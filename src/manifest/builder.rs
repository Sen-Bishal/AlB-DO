// CiCD reviewer rules

use super::metadata::{
    hoist_head_tags_from_body, metadata_from_const_expr, render_head_metadata, DYNAMIC_HEAD_MARKER,
};
use super::schema::{
    AssetManifest, DataDep, DataSource, DomPosition, HtmlShell, HydrationMode, RenderedNode,
    RouteActionEntry, RouteManifest, RouteMetadata, Tier, TierBNode, TierCNode, WTStreamSlot,
};
use crate::effects::EffectProfile;
use crate::graph::ComponentGraph;
use crate::ir::opcode::OpcodeFrame;
use crate::ir::wire::encode_frame;
use crate::routing::{discover_routes, DiscoveredRoute};
use crate::runtime::broadcast::BroadcastRegistry;
use crate::runtime::compiled::{
    render_entry_with_bindings, render_entry_with_broadcast, CompiledProject, RenderOptions,
};
use crate::runtime::eval::ComponentProject;
use crate::runtime::session::SessionId;
use crate::runtime::slot_store::{SessionSlotView, SlotStore};
use crate::runtime::webtransport::{
    WTRenderMode, WTStreamRouter, WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES,
    WT_STREAM_SLOT_PREFETCH, WT_STREAM_SLOT_SHELL,
};
use crate::types::{Component, ComponentId};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct StaticRenderProject {
    root: PathBuf,
    project: ComponentProject,
}

/// Phase P · CompiledProject wrapped around the same source tree the
/// Phase J render project uses. Held alongside `static_render_project`
/// so the builder can pre-render Tier-B components with full Phase K
/// hook compile (BindEvent + SetTextRef + initial SlotSet) at build
/// time, embedding the result into the manifest.
struct CompiledRenderProject {
    root: PathBuf,
    project: CompiledProject,
}

struct ShellPlaceholder {
    order: u32,
    html: String,
}

/// Sentinel `parent_placeholder` for a Tier-C island mounted in a layout. It
/// marks the node so `collect_shell_placeholders` does NOT emit it into the
/// `<children />` slot — the island's anchor is the inline placeholder the
/// layout render emits at its authored position instead.
const LAYOUT_ISLAND_PARENT: &str = "__albedo_layout_island__";

#[derive(Debug, Clone)]
pub struct ComponentTierMetadata {
    pub tier: Tier,
    pub hydration_mode: HydrationMode,
    pub effect_profile: EffectProfile,
}

pub struct ManifestBuilder<'a> {
    graph: &'a ComponentGraph,
    components: HashMap<ComponentId, Component>,
    metadata: HashMap<ComponentId, ComponentTierMetadata>,
    tier_b_timeout_ms: u64,
    working_dir: Option<PathBuf>,
    static_render_project: Option<StaticRenderProject>,
    /// Phase P · Phase K render path. Built from the same source tree
    /// as `static_render_project` when possible. `None` when
    /// CompiledProject construction fails (e.g. parse error in a
    /// component); falls back to Phase J static render.
    compiled_render_project: Option<CompiledRenderProject>,
    /// Phase P · file-based-routing discovery, when `<root>/routes/`
    /// exists. Used to populate `RouteManifest.layout_chain` (and
    /// later `error_component` / `loading_component` once Stream E.2
    /// extends the discovery struct).
    discovered_routes: Vec<DiscoveredRoute>,
    /// FORGE — build-time materialised shared-slot seeds (`topic → JSON
    /// bytes`), read from the substrate in [`Self::new`]. `render_tier_b_inline`
    /// pre-seeds its registry with these so the Stream-B pre-render bakes the
    /// persisted rows into Tier-B HTML instead of the empty `b"null"`
    /// placeholder. Empty unless the `forge` feature is on and a `forge.db`
    /// exists at the project root.
    forge_topic_seeds: Vec<(String, Vec<u8>)>,
}

impl<'a> ManifestBuilder<'a> {
    pub fn new(
        graph: &'a ComponentGraph,
        metadata: HashMap<ComponentId, ComponentTierMetadata>,
        tier_b_timeout_ms: u64,
    ) -> Self {
        let working_dir = std::env::current_dir().ok();
        let components = graph
            .components()
            .into_iter()
            .map(|component| (component.id, component))
            .collect::<HashMap<_, _>>();
        let static_render_project =
            build_static_render_project(&components, working_dir.as_deref());
        let compiled_render_project = build_compiled_render_project(&static_render_project);
        let discovered_routes =
            discover_routes_from_components(&components, working_dir.as_deref());
        let forge_topic_seeds = materialize_forge_seeds(working_dir.as_deref());

        Self {
            graph,
            components,
            metadata,
            tier_b_timeout_ms,
            working_dir,
            static_render_project,
            compiled_render_project,
            discovered_routes,
            forge_topic_seeds,
        }
    }

    pub fn build_route_manifest(
        &self,
        route: &str,
        root_component: ComponentId,
        assets: &AssetManifest,
    ) -> RouteManifest {
        let mut tier_a_root = Vec::new();
        let mut tier_b = Vec::new();
        let mut tier_c = Vec::new();
        let mut order_counter = 0u32;

        // Tier-C children are standalone hydration islands, anchored at their
        // own placeholder — not inlined into a Tier-A parent's static HTML
        // (which would render them twice: once inline, once at the island).
        // Install the island-name set so the tier-blind static renderer emits
        // a hole for them during `traverse`'s `render_static` calls.
        let island_names = self.tier_c_component_names();
        let _island_guard = crate::runtime::eval::core::install_island_skip_set(&island_names);

        self.traverse(
            root_component,
            None,
            false,
            &mut tier_a_root,
            &mut tier_b,
            &mut tier_c,
            &mut order_counter,
            assets,
        );

        // Phase P · per-route metadata. Topics are global today (every
        // route knows every topic the project references) — the
        // session over-subscribes a few extra mpsc::Sender clones,
        // which is cheap. Per-route precision is a follow-up.
        let shared_slot_topics = self
            .compiled_render_project
            .as_ref()
            .map(|cp| cp.project.shared_slot_topics())
            .unwrap_or_default();

        // Action IDs come from CompiledComponent.action_handlers,
        // which is empty until Stream C ships. The lookup path is
        // future-proofed here so Stream C doesn't need a second
        // schema bump.
        let action_ids = self.collect_route_action_ids();

        // Layout chain comes from discover_routes when `<root>/routes/`
        // exists. Empty otherwise.
        let layout_chain = self.layout_chain_for_route(route);

        // Layout-island reachability — a Tier-C island mounted in a layout isn't
        // reached by the route-entry `traverse` above, so collect those islands
        // here and fold them into the route's `tier_c`. They carry a sentinel
        // parent so `collect_shell_placeholders` skips them; their anchor is the
        // inline placeholder the layout render emits (via `layout_island_map`).
        let (layout_islands, layout_island_map) =
            self.collect_layout_islands(&layout_chain, &mut order_counter, assets);
        tier_c.extend(layout_islands);
        // Phase P · Stream E.2 — pick the per-route error / loading
        // boundary. `discover_routes` already chose the nearest one
        // (longest matching URL prefix); we translate path → component
        // name via the same matcher Stream E.1 tightened.
        let error_component = self.error_component_for_route(route);
        let loading_component = self.loading_component_for_route(route);

        // Gate 2 · B — resolve the route's `<head>` metadata. Slice 1 is
        // the static `export const metadata` base. Slice 2 then hoists
        // JSX-rendered `<title>`/`<meta>` out of the rendered tier-node
        // HTML (React-19 style), stripping them in place so the served
        // body no longer carries them, and merges them over the static
        // base (JSX wins — last-writer).
        let mut metadata = self.extract_static_metadata(root_component);
        let hoisted = hoist_head_from_nodes(&mut tier_a_root, &mut tier_b);
        metadata.merge(hoisted);

        // Slice 3 — does the route's leaf module export `generateMetadata`? If
        // so the shell carries a head marker (not the static title/meta) and the
        // serve path resolves the real metadata per request.
        let dynamic_metadata = self.detect_dynamic_metadata(root_component);

        let shell = self.build_shell(
            route,
            assets,
            &tier_a_root,
            &tier_b,
            &tier_c,
            &layout_chain,
            &layout_island_map,
            &metadata,
            dynamic_metadata.is_some(),
        );

        RouteManifest {
            route: route.to_string(),
            shell,
            tier_a_root,
            tier_b,
            tier_c,
            shared_slot_topics,
            action_ids,
            layout_chain,
            error_component,
            loading_component,
            metadata,
            dynamic_metadata,
        }
    }

    /// Slice 3 — return the leaf component's name when its module exports a
    /// `generateMetadata` function (the boot-plan key the serve path invokes),
    /// or `None` when the route's head is fully static. A `generateMetadata`
    /// that exists but isn't actually exported is a harmless false positive:
    /// the per-request eval finds no export and the static head stands.
    fn detect_dynamic_metadata(&self, root_component: ComponentId) -> Option<String> {
        let compiled = self.compiled_render_project.as_ref()?;
        let component = self.components.get(&root_component)?;
        let entry = self.component_entry_for_project(component, compiled.root.as_path())?;
        let module = compiled.project.module(entry.as_str())?;
        module
            .functions
            .contains_key("generateMetadata")
            .then(|| component.name.clone())
    }

    /// Gate 2 · B (slice 1) — read the route leaf component's
    /// `export const metadata = { ... }` object literal and lower it to
    /// a [`RouteMetadata`]. Returns the default (empty) metadata when the
    /// compiled project is unavailable, the module can't be resolved, or
    /// no `metadata` const is exported — all of which leave the shell's
    /// historical `<head>` unchanged.
    fn extract_static_metadata(&self, root_component: ComponentId) -> RouteMetadata {
        let Some(compiled) = self.compiled_render_project.as_ref() else {
            return RouteMetadata::default();
        };
        let Some(component) = self.components.get(&root_component) else {
            return RouteMetadata::default();
        };
        let Some(entry) = self.component_entry_for_project(component, compiled.root.as_path())
        else {
            return RouteMetadata::default();
        };
        let Some(module) = compiled.project.module(entry.as_str()) else {
            return RouteMetadata::default();
        };
        module
            .module_constants
            .iter()
            .find(|(name, _)| name == "metadata")
            .map(|(_, expr)| metadata_from_const_expr(expr))
            .unwrap_or_default()
    }

    /// Phase P · Stream E.2 — resolve `routes/.../error.tsx` (if any)
    /// for `route` to a component name the streaming handler can
    /// render when a Tier-C node fails. `discover_routes_from_components`
    /// returns the file path; component_name_for_rel_path translates
    /// to the registered component's name. `None` when no error.tsx
    /// covers this route.
    fn error_component_for_route(&self, route: &str) -> Option<String> {
        let discovered = self
            .discovered_routes
            .iter()
            .find(|r| r.url_path == route)?;
        let rel = discovered.error_boundary.as_ref()?;
        self.component_name_for_rel_path(rel.as_path())
    }

    /// Phase P · Stream E.2 — same shape as `error_component_for_route`
    /// for `loading.tsx`.
    fn loading_component_for_route(&self, route: &str) -> Option<String> {
        let discovered = self
            .discovered_routes
            .iter()
            .find(|r| r.url_path == route)?;
        let rel = discovered.loading.as_ref()?;
        self.component_name_for_rel_path(rel.as_path())
    }

    /// Collect TS-action handler names + their wire IDs for this
    /// route. Empty until Stream C lands the `action()` extractor
    /// (which adds `action_handlers` to `CompiledComponent`). Kept as
    /// a method so the call site in `build_route_manifest` reads
    /// uniformly even before Stream C wires real data here.
    fn collect_route_action_ids(&self) -> Vec<RouteActionEntry> {
        Vec::new()
    }

    /// Resolve the layout chain for `route` by looking up the route's
    /// discovery entry and translating each `layout_chain` path into
    /// the matching component name. Skips entries that don't resolve
    /// — graceful for projects without file-based routing.
    fn layout_chain_for_route(&self, route: &str) -> Vec<String> {
        let Some(discovered) = self.discovered_routes.iter().find(|r| r.url_path == route) else {
            return Vec::new();
        };

        discovered
            .layout_chain
            .iter()
            .filter_map(|layout_rel| self.component_name_for_rel_path(layout_rel.as_path()))
            .collect()
    }

    /// Map a `routes/...` relative path back to a component name by
    /// tail-matching against `Component.file_path`. Tolerant of
    /// `/` vs `\` separator differences.
    ///
    /// Phase P · Stream E.1 — the match requires the path tail to
    /// begin at `/routes/<rel>` so a needle of `layout.tsx` matches
    /// only `<root>/routes/layout.tsx` and NOT `<root>/routes/nested/layout.tsx`.
    /// A second pass accepts a bare-relative match (`routes/<rel>`
    /// without a leading slash) for projects whose file_path
    /// strings are stored relative to the workspace. Without these
    /// constraints, HashMap iteration order would let any deeper
    /// `layout.tsx` win against the root needle.
    fn component_name_for_rel_path(&self, rel: &Path) -> Option<String> {
        let needle = rel.to_string_lossy().replace('\\', "/");
        let absolute_suffix = format!("/routes/{}", needle);
        let relative_suffix = format!("routes/{}", needle);
        for component in self.components.values() {
            let normalised = component.file_path.replace('\\', "/");
            if normalised.ends_with(absolute_suffix.as_str())
                || normalised == relative_suffix
                || normalised.ends_with(&format!("/{}", relative_suffix))
            {
                return Some(component.name.clone());
            }
        }
        None
    }

    pub fn build_assets_manifest(&self) -> AssetManifest {
        let mut chunks = HashMap::new();
        let mut css = Vec::new();

        for component in self.components.values() {
            if component.file_path.ends_with(".css") {
                css.push(component.file_path.replace('\\', "/"));
            }

            let Some(metadata) = self.metadata.get(&component.id) else {
                continue;
            };

            if metadata.tier == Tier::C {
                chunks.insert(
                    component.name.clone(),
                    format!(
                        "/_albedo/chunks/{}.{}.js",
                        slugify(component.name.as_str()),
                        format!("{:016x}", component.source_hash)
                    ),
                );
            }
        }

        css.sort();
        css.dedup();

        AssetManifest {
            chunks,
            css,
            runtime: "/_albedo/runtime.js".to_string(),
        }
    }

    pub fn build_build_id(&self) -> String {
        let mut components = self.components.values().collect::<Vec<_>>();
        components.sort_by(|left, right| left.id.as_u64().cmp(&right.id.as_u64()));

        let mut basis = String::new();
        for component in components {
            basis.push_str(component.file_path.as_str());
            basis.push(':');
            basis.push_str(format!("{:016x}", component.source_hash).as_str());
            basis.push(';');
        }

        format!("{:016x}", fnv1a_64(basis.as_bytes()))
    }

    pub fn build_wt_stream_slots(&self) -> Vec<WTStreamSlot> {
        let mut by_slot: BTreeMap<u8, BTreeSet<u64>> = BTreeMap::new();

        for (component_id, metadata) in &self.metadata {
            if !matches!(metadata.tier, Tier::B | Tier::C) {
                continue;
            }

            let shell_slot = WTStreamRouter::stream_slot_for(metadata.tier, WTRenderMode::Shell);
            by_slot
                .entry(shell_slot)
                .or_default()
                .insert(component_id.as_u64());

            let patch_slot = WTStreamRouter::stream_slot_for(metadata.tier, WTRenderMode::Patch);
            by_slot
                .entry(patch_slot)
                .or_default()
                .insert(component_id.as_u64());
        }

        by_slot
            .into_iter()
            .map(|(slot, component_ids)| WTStreamSlot {
                slot,
                label: stream_slot_label(slot).to_string(),
                component_ids: component_ids.into_iter().collect(),
            })
            .collect()
    }

    fn build_shell(
        &self,
        route: &str,
        assets: &AssetManifest,
        tier_a_root: &[RenderedNode],
        tier_b: &[TierBNode],
        tier_c: &[TierCNode],
        layout_chain: &[String],
        layout_island_map: &std::collections::HashMap<String, String>,
        metadata: &RouteMetadata,
        dynamic_metadata: bool,
    ) -> HtmlShell {
        // Build the route's inner body content first (everything that
        // would land between <body> and </body> pre-E.1). This is the
        // "leaf" content the layout chain wraps. The leaf HTML is a set
        // of slot placeholders here; the rendered component HTML (where
        // slice-2 JSX head tags were hoisted out of) lives on the tier
        // nodes and is injected at serve time.
        let mut inner = String::new();
        let mut placeholders = self.collect_shell_placeholders(tier_a_root, tier_b, tier_c);
        placeholders.sort_by_key(|entry| entry.order);
        for placeholder in placeholders {
            inner.push_str(&placeholder.html);
        }

        // Phase P · Stream E.1 — apply the layout chain outermost-out.
        // When `layout_chain` is empty, `wrap_in_layouts` is a no-op
        // and the body_open shape stays identical to pre-E.1 — no
        // observable change for routes without a `routes/layout.tsx`.
        let wrapped = self.wrap_in_layouts(inner, layout_chain, layout_island_map);

        let mut doctype_and_head = String::from(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
        );
        // Tier-B injection bootstrap. The streamed Tier-B `<script>__albedo_inject(...)</script>`
        // calls are CLASSIC scripts (run during parse), but the real
        // `__albedo_inject` is defined in `runtime.js`, a `type="module"`
        // script (deferred — runs AFTER the document parses). Without an early
        // stub the body's inject calls fire before the function exists and are
        // lost, leaving every Tier-B placeholder blank. This classic, head-level
        // stub buffers calls into a queue that `installLegacyHtmlInjector`
        // drains once the module loads. Must precede any inject call → first
        // thing in <head>.
        doctype_and_head.push_str(TIER_B_INJECT_BOOTSTRAP);
        // The `<title>`/`<meta>` block. For a route with `generateMetadata`,
        // emit a marker the serve path replaces per request with the resolved
        // (static-merged-with-dynamic) head; otherwise bake the static block.
        // Author-declared title wins; otherwise the historical `ALBEDO {route}`
        // fallback keeps untouched routes byte-identical to pre-B builds.
        if dynamic_metadata {
            doctype_and_head.push_str(DYNAMIC_HEAD_MARKER);
        } else {
            doctype_and_head.push_str(&render_head_metadata(route, metadata));
        }
        for css_path in &assets.css {
            doctype_and_head.push_str(&format!(
                "<link rel=\"stylesheet\" href=\"{}\">",
                escape_html(css_path)
            ));
        }
        // Phase P · Stream E.3 — inject scoped CSS for every
        // `.module.css` file referenced by any component on this
        // route. The scoped output is keyed by file (not by
        // component) so a CSS file imported from multiple components
        // ships exactly once per route. Concatenated into one
        // `<style>` block to minimise extra requests and avoid a
        // FOUC between Tier-A render and Tier-B hydration.
        let scoped_css = self.collect_scoped_module_css_for_route(tier_a_root, tier_b, tier_c);
        if !scoped_css.is_empty() {
            doctype_and_head.push_str("<style data-albedo-css-modules>");
            doctype_and_head.push_str(&scoped_css);
            doctype_and_head.push_str("</style>");
        }
        doctype_and_head.push_str("</head>");

        let mut body_open = String::from("<body>");
        body_open.push_str(&wrapped);

        // Phase P · post-P wire-through — inline every Tier-B node's
        // pre-rendered opcode frame as a `<script type="application/x-albedo-frame">`
        // so bakabox's bootstrap can apply BindEvent / SetTextRef /
        // initial-SlotSet instructions on the first paint. Without
        // this the production server only ships opcodes when the WT
        // patches lane is active; static / HTTP-only deploys (and
        // any pre-WT load) had no way to make clicks fire.
        for node in tier_b {
            if node.initial_opcode_frame.is_empty() {
                continue;
            }
            body_open.push_str(&format!(
                "<script type=\"application/x-albedo-frame\" data-base64=\"{}\"></script>",
                base64_encode(&node.initial_opcode_frame)
            ));
        }

        HtmlShell {
            doctype_and_head,
            body_open,
            body_close: "</body></html>".to_string(),
            shim_script: default_shim_script(!tier_b.is_empty() || !tier_c.is_empty()),
        }
    }

    /// Phase P · Stream E.3 — concatenate scoped CSS for every
    /// `.module.css` file referenced by any component on this
    /// route. Walks every TIer-A / Tier-B / Tier-C node on the
    /// route, resolves each to its `module_spec`, and accumulates
    /// the file-keyed scoped CSS from the project's registry. Dedup
    /// happens at the file_key level inside `scoped_css_for_module`,
    /// so a single file imported from multiple components emits
    /// once. Empty string when no component on the route imports a
    /// `.module.css` (the common case for back-compat routes).
    fn collect_scoped_module_css_for_route(
        &self,
        tier_a_root: &[RenderedNode],
        tier_b: &[TierBNode],
        tier_c: &[TierCNode],
    ) -> String {
        let Some(compiled) = self.compiled_render_project.as_ref() else {
            return String::new();
        };
        let registry = compiled.project.css_modules();
        if registry.file_count() == 0 {
            return String::new();
        }

        // Collect unique `module_spec`s referenced on this route.
        // We resolve each component's name back to its file path,
        // then to a project-relative spec via the same shape
        // `CompiledProject::wrap` uses.
        let mut module_specs: BTreeSet<String> = BTreeSet::new();
        let mut record = |component_name: &str| {
            if let Some(component) = self.components.values().find(|c| c.name == component_name) {
                if let Some(spec) =
                    self.component_entry_for_project(component, compiled.root.as_path())
                {
                    module_specs.insert(spec);
                }
            }
        };
        for node in tier_a_root {
            record(&node.component_id);
        }
        for node in tier_b {
            record(&node.component_id);
        }
        for node in tier_c {
            record(&node.component_id);
        }

        // For each module on the route, ask the registry for the
        // de-duplicated scoped CSS. Top-level dedup across modules
        // happens via the BTreeSet of file_keys we accumulate here.
        let mut seen_keys: BTreeSet<&str> = BTreeSet::new();
        let mut out = String::new();
        for spec in &module_specs {
            for body in registry.scoped_css_for_module(spec) {
                if seen_keys.insert(body) {
                    out.push_str(body);
                }
            }
        }
        out
    }

    /// Phase P · Stream E.1 — wrap a route's inner body content in
    /// the rendered HTML of every layout in `layout_chain`, composing
    /// outermost → leaf.
    ///
    /// `layout_chain` is the source-order chain `discover_routes`
    /// produced (outermost first, leaf-parent last). The wrap walks
    /// the chain in REVERSE so the innermost layout absorbs the
    /// route content first, then the next layer absorbs THAT result,
    /// and so on, until the outermost layout owns the whole tree.
    ///
    /// Each layout component is rendered statically with empty props
    /// — layout files declare `<children />` to mark the inner
    /// substitution point, which the renderer emits as
    /// [`crate::runtime::eval::LAYOUT_CHILDREN_SENTINEL`]. The wrap
    /// pass `str::replace`s that sentinel with the accumulated inner
    /// HTML to compose the next layer.
    ///
    /// Layouts whose component name doesn't resolve to a known
    /// component (missing file, unresolved import) are silently
    /// skipped — matching the rest of the manifest builder's
    /// graceful-degradation contract. A missing layout shouldn't
    /// fail the whole build; it just leaves the chain shorter than
    /// the discovered file suggested.
    fn wrap_in_layouts(
        &self,
        leaf_html: String,
        layout_chain: &[String],
        layout_island_map: &std::collections::HashMap<String, String>,
    ) -> String {
        use crate::runtime::eval::LAYOUT_CHILDREN_SENTINEL;

        if layout_chain.is_empty() {
            return leaf_html;
        }

        // Install the layout-island placeholder map for the duration of every
        // `render_layout_html` below, so a Tier-C island in the layout emits its
        // real placeholder div inline (instead of nothing) at its authored spot.
        let _island_anchor_guard =
            crate::runtime::eval::core::install_layout_island_placeholders(layout_island_map);

        let mut accumulated = leaf_html;
        for layout_name in layout_chain.iter().rev() {
            let Some(layout_html) = self.render_layout_html(layout_name) else {
                tracing::warn!(
                    target: "albedo.manifest.layout",
                    layout = %layout_name,
                    "layout component not found or failed to render; \
                     route shipped without this layout in the chain"
                );
                continue;
            };
            if !layout_html.contains(LAYOUT_CHILDREN_SENTINEL) {
                // Layout source has no `<children />` — degrade to the
                // pre-E.1 shape rather than dropping the route's
                // content on the floor. The layout's rendered HTML
                // wins; the inner content is appended so it's still
                // observable in the shell. Surface a tracing warn so
                // the build log flags the misconfiguration.
                tracing::warn!(
                    target: "albedo.manifest.layout",
                    layout = %layout_name,
                    "layout component has no <children /> intrinsic; \
                     appending inner content rather than substituting"
                );
                accumulated = format!("{}{}", layout_html, accumulated);
                continue;
            }
            accumulated = layout_html.replace(LAYOUT_CHILDREN_SENTINEL, &accumulated);
        }
        accumulated
    }

    /// Resolve a layout component name to its statically-rendered
    /// HTML. Returns `None` when the component isn't registered or
    /// rendering fails — caller's job to decide what to do with that.
    fn render_layout_html(&self, layout_name: &str) -> Option<String> {
        let component = self.components.values().find(|c| c.name == layout_name)?;
        self.render_static_component_html(component)
    }

    /// Layout-island reachability — collect every Tier-C island reachable from a
    /// layout in `layout_chain` and lower it to a `TierCNode`. These islands are
    /// NOT walked by the route-entry `traverse` (a layout is rendered standalone
    /// with the `<children />` sentinel, off the route's component subtree), so
    /// without this pass an island mounted in `layout.tsx` ships no hydration
    /// block on serve.
    ///
    /// Returns the nodes plus the `name → placeholder_id` map the renderer
    /// consults (via `install_layout_island_placeholders`) to emit each island's
    /// inline placeholder while the layout is rendered. Each node carries the
    /// [`LAYOUT_ISLAND_PARENT`] sentinel so `collect_shell_placeholders` leaves
    /// it out of the `<children />` slot — its anchor is the inline div instead.
    fn collect_layout_islands(
        &self,
        layout_chain: &[String],
        order_counter: &mut u32,
        assets: &AssetManifest,
    ) -> (Vec<TierCNode>, std::collections::HashMap<String, String>) {
        use std::collections::HashSet;

        let mut nodes = Vec::new();
        let mut map = std::collections::HashMap::new();
        let mut emitted: HashSet<ComponentId> = HashSet::new();
        let mut visited: HashSet<ComponentId> = HashSet::new();

        for layout_name in layout_chain {
            let Some(layout_id) = self
                .components
                .iter()
                .find(|(_, c)| &c.name == layout_name)
                .map(|(id, _)| *id)
            else {
                continue;
            };

            // Depth-first walk of the layout's render subtree. Recurse through
            // Tier-A/Tier-B so an island nested inside the layout's static markup
            // is still found; a Tier-C node is the island boundary — its own
            // subtree belongs to the island and isn't walked here. `visited`
            // guards against dependency cycles; `emitted` dedups an island shared
            // across multiple layouts in the chain.
            let mut stack = self.sorted_children(layout_id);
            while let Some(id) = stack.pop() {
                if !visited.insert(id) {
                    continue;
                }
                match self.tier_of(id) {
                    Some(Tier::C) => {
                        if !emitted.insert(id) {
                            continue;
                        }
                        let node = self.build_tier_c_node(
                            id,
                            Some(LAYOUT_ISLAND_PARENT.to_string()),
                            order_counter,
                            assets,
                        );
                        map.insert(node.component_id.clone(), node.placeholder_id.clone());
                        nodes.push(node);
                    }
                    Some(Tier::A) | Some(Tier::B) => {
                        for child in self.sorted_children(id) {
                            stack.push(child);
                        }
                    }
                    None => {}
                }
            }
        }

        (nodes, map)
    }

    fn traverse(
        &self,
        id: ComponentId,
        parent_placeholder: Option<String>,
        // True iff a Tier-A ancestor has already inlined this subtree
        // into its `render_static` HTML. When set, Tier-A nodes here
        // skip pushing themselves to `tier_a_root` (the dedup contract
        // for Phase J's static slicer). Independent of `parent_placeholder`,
        // which controls body-anchor placement for Tier-B/C islands.
        inlined_under_tier_a: bool,
        tier_a_root: &mut Vec<RenderedNode>,
        tier_b: &mut Vec<TierBNode>,
        tier_c: &mut Vec<TierCNode>,
        order_counter: &mut u32,
        assets: &AssetManifest,
    ) {
        let Some(metadata) = self.metadata.get(&id) else {
            return;
        };

        match metadata.tier {
            Tier::A => {
                // Static slicer dedup contract (Phase J):
                //
                // A Tier-A component's `render_static` already inlines every
                // Tier-A descendant into one HTML string. Pushing those
                // descendants to `tier_a_root` again — and emitting their
                // own `__SLOT_` placeholders in `body_open` — produces
                // duplicate work for the stitcher and a manifest that
                // double-counts the same DOM bytes.
                //
                // The `inlined_under_tier_a` flag tracks whether a Tier-A
                // ancestor has already absorbed our HTML; when set, we
                // skip the push. `parent_placeholder` is left untouched
                // so Tier-B/C descendants (which are NOT inlined into
                // Tier-A's HTML — they remain separate async islands)
                // keep their original body-anchor placement.
                if !inlined_under_tier_a {
                    tier_a_root.push(self.render_static(
                        id,
                        parent_placeholder.clone(),
                        order_counter,
                    ));
                }
                for child in self.sorted_children(id) {
                    let child_inlined =
                        inlined_under_tier_a || self.tier_of(child) == Some(Tier::A);
                    self.traverse(
                        child,
                        parent_placeholder.clone(),
                        child_inlined,
                        tier_a_root,
                        tier_b,
                        tier_c,
                        order_counter,
                        assets,
                    );
                }
            }
            Tier::B => {
                let component = self.component_or_panic(id);
                let placeholder_id = format!(
                    "__b_{}_{}",
                    slugify(component.name.as_str()),
                    component.id.as_u64()
                );
                let mut node = self.build_tier_b_node(
                    id,
                    parent_placeholder,
                    placeholder_id.clone(),
                    order_counter,
                );

                self.collect_tier_a_children(
                    id,
                    &placeholder_id,
                    &mut node.tier_a_children,
                    order_counter,
                );
                tier_b.push(node);

                for child in self.sorted_children(id) {
                    if self.tier_of(child) == Some(Tier::A) {
                        continue;
                    }
                    self.traverse(
                        child,
                        Some(placeholder_id.clone()),
                        false,
                        tier_a_root,
                        tier_b,
                        tier_c,
                        order_counter,
                        assets,
                    );
                }
            }
            Tier::C => {
                tier_c.push(self.build_tier_c_node(id, parent_placeholder, order_counter, assets));
            }
        }
    }

    fn collect_tier_a_children(
        &self,
        root: ComponentId,
        parent_placeholder: &str,
        output: &mut Vec<RenderedNode>,
        order_counter: &mut u32,
    ) {
        for child in self.sorted_children(root) {
            match self.tier_of(child) {
                Some(Tier::A) => {
                    output.push(self.render_static(
                        child,
                        Some(parent_placeholder.to_string()),
                        order_counter,
                    ));
                    self.collect_tier_a_children(child, parent_placeholder, output, order_counter);
                }
                Some(Tier::B) | Some(Tier::C) | None => {}
            }
        }
    }

    fn build_tier_b_node(
        &self,
        id: ComponentId,
        parent_placeholder: Option<String>,
        placeholder_id: String,
        order_counter: &mut u32,
    ) -> TierBNode {
        let component = self.component_or_panic(id);
        let metadata = self.metadata_or_panic(id);

        // Phase P · pre-render through CompiledProject + Phase K so
        // the manifest carries real HTML + the BindEvent / SetTextRef
        // / initial-SlotSet payload instead of a fallback placeholder.
        // Falls back to `None` + the existing fallback_html if the
        // compile path isn't available or render errors transiently.
        let (initial_html, initial_opcode_frame) = self
            .render_tier_b_inline(component)
            .map(|(html, bytes)| (Some(html), bytes))
            .unwrap_or_else(|| (None, Vec::new()));

        TierBNode {
            component_id: component.name.clone(),
            placeholder_id,
            render_fn: format!("render::{}", component.name),
            static_props: json!({
                "component_id": component.id.as_u64(),
                "component_name": component.name,
            }),
            dynamic_prop_keys: self.dynamic_prop_keys_for_component(component),
            data_deps: self.data_deps_for_component(component, metadata),
            tier_a_children: Vec::new(),
            position: DomPosition {
                parent_placeholder,
                slot: "default".to_string(),
                order: next_order(order_counter),
            },
            timeout_ms: self.tier_b_timeout_ms.max(1),
            fallback_html: Some(format!(
                "<div data-albedo-fallback=\"{}\"></div>",
                escape_html(component.name.as_str())
            )),
            initial_html,
            initial_opcode_frame,
        }
    }

    /// Phase P · render `component` through `CompiledProject` + Phase
    /// K hook compile and return `(html, bincode-encoded OpcodeFrame)`.
    /// `None` when the compiled project isn't available or the render
    /// errors (e.g. parse failure surfaced as runtime error). The
    /// caller substitutes `fallback_html` in that case.
    ///
    /// Build-time render uses a fresh empty `SlotStore` + a fresh
    /// `BroadcastRegistry`. Topics referenced by `useSharedSlot`
    /// calls auto-register with empty seed values via
    /// `auto_subscribe` — the streaming handler later overrides
    /// those when real requests come in. The subscriber sender is a
    /// dummy `mpsc::Sender` whose receiver is dropped; broadcast
    /// fan-out uses `try_send` so this never blocks.
    fn render_tier_b_inline(&self, component: &Component) -> Option<(String, Vec<u8>)> {
        let compiled = self.compiled_render_project.as_ref()?;
        let entry = self.component_entry_for_project(component, compiled.root.as_path())?;

        let session = SessionId::random();
        let slot_store = Arc::new(SlotStore::new());
        let slots = SessionSlotView::new(session, slot_store);
        let broadcast = BroadcastRegistry::new();
        // FORGE — pre-seed the topics the substrate materialised at build
        // start, so a `useSharedSlot` read during this pre-render sees the
        // persisted rows rather than the empty placeholder. No-op when the
        // seed list is empty (non-forge builds, or no `forge.db`).
        for (topic, bytes) in &self.forge_topic_seeds {
            broadcast.topic(topic.as_str(), bytes.clone());
        }
        // Buffer = 16: deep enough to absorb the auto-subscribe
        // initial-SlotSet burst for projects with many topics, while
        // staying small enough that a runaway producer dies fast.
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let opts = RenderOptions { hook_compile: true };

        let output = render_entry_with_broadcast(
            &compiled.project,
            entry.as_str(),
            &Value::Object(Default::default()),
            &slots,
            &broadcast,
            tx,
            &opts,
        )
        .ok()?;

        if output.html.trim().is_empty() {
            return None;
        }

        // Wrap the opcode vector in an `OpcodeFrame` and bincode-
        // encode. The streaming handler later ships these bytes
        // verbatim on the WT patches lane — same wire format the
        // runtime emits for live patches, so the client decoder
        // doesn't care whether the bytes came from build-time
        // pre-render or runtime fan-out.
        let frame_bytes = if output.opcodes.is_empty() {
            Vec::new()
        } else {
            let frame = OpcodeFrame {
                frame_id: 0,
                component_id: Some(component.id.as_u64()),
                instructions: output.opcodes,
            };
            encode_frame(&frame).ok()?
        };

        Some((output.html, frame_bytes))
    }

    fn build_tier_c_node(
        &self,
        id: ComponentId,
        parent_placeholder: Option<String>,
        order_counter: &mut u32,
        assets: &AssetManifest,
    ) -> TierCNode {
        let component = self.component_or_panic(id);
        let metadata = self.metadata_or_panic(id);

        let bundle_path = assets
            .chunks
            .get(component.name.as_str())
            .cloned()
            .unwrap_or_else(|| format!("/_albedo/chunks/{}.js", slugify(component.name.as_str())));

        TierCNode {
            component_id: component.name.clone(),
            placeholder_id: format!(
                "__c_{}_{}",
                slugify(component.name.as_str()),
                component.id.as_u64()
            ),
            bundle_path,
            initial_props: json!({
                "component_id": component.id.as_u64(),
                "component_name": component.name,
            }),
            hydration_mode: metadata.hydration_mode.into_streaming(),
            position: DomPosition {
                parent_placeholder,
                slot: "default".to_string(),
                order: next_order(order_counter),
            },
            side_effects: metadata.effect_profile.side_effects,
        }
    }

    fn render_static(
        &self,
        id: ComponentId,
        parent_placeholder: Option<String>,
        order_counter: &mut u32,
    ) -> RenderedNode {
        let component = self.component_or_panic(id);
        let placeholder_id = format!(
            "__a_{}_{}",
            slugify(component.name.as_str()),
            component.id.as_u64()
        );
        let html = self
            .render_static_component_html(component)
            .unwrap_or_else(|| {
                tracing::warn!(
                    target: "albedo.manifest.render",
                    component = %component.name,
                    "static render failed; falling back to text-stripped placeholder markup"
                );
                self.render_static_fallback_html(component)
            });

        RenderedNode {
            component_id: component.name.clone(),
            placeholder_id,
            html,
            position: DomPosition {
                parent_placeholder,
                slot: "default".to_string(),
                order: next_order(order_counter),
            },
        }
    }

    fn collect_shell_placeholders(
        &self,
        tier_a_root: &[RenderedNode],
        tier_b: &[TierBNode],
        tier_c: &[TierCNode],
    ) -> Vec<ShellPlaceholder> {
        let mut placeholders = Vec::new();

        for node in tier_a_root {
            if node.position.parent_placeholder.is_none() {
                placeholders.push(ShellPlaceholder {
                    order: node.position.order,
                    html: format!("<!--__SLOT_{}-->", node.placeholder_id),
                });
            }
        }

        for node in tier_b {
            if node.position.parent_placeholder.is_none() {
                placeholders.push(ShellPlaceholder {
                    order: node.position.order,
                    html: format!(
                        "<div id=\"{}\" data-albedo-tier=\"b\"></div>",
                        escape_html(node.placeholder_id.as_str())
                    ),
                });
            }
        }

        for node in tier_c {
            if node.position.parent_placeholder.is_none() {
                placeholders.push(ShellPlaceholder {
                    order: node.position.order,
                    html: format!(
                        "<div id=\"{}\" data-albedo-tier=\"c\"></div>",
                        escape_html(node.placeholder_id.as_str())
                    ),
                });
            }
        }

        placeholders
    }

    fn render_static_component_html(&self, component: &Component) -> Option<String> {
        let empty_props = Value::Object(Default::default());

        // Phase P · Stream E.3 — route the Tier-A static render
        // through the CompiledProject path (with hook_compile off)
        // so `styles.foo` resolves to the scoped class name via the
        // CSS-module registry installed by `render_entry_with_bindings`.
        // Falls back to the legacy static ComponentProject when no
        // compiled project is available (test fixtures, etc.).
        if let Some(compiled) = self.compiled_render_project.as_ref() {
            let entry = self.component_entry_for_project(component, compiled.root.as_path())?;
            let session = SessionId::random();
            let slot_store = Arc::new(SlotStore::new());
            let slots = SessionSlotView::new(session, slot_store);
            let opts = RenderOptions {
                hook_compile: false,
            };
            if let Ok(output) = render_entry_with_bindings(
                &compiled.project,
                entry.as_str(),
                &empty_props,
                &slots,
                &opts,
            ) {
                let html = output.html;
                if !html.trim().is_empty() {
                    return Some(html);
                }
            }
        }

        let render_project = self.static_render_project.as_ref()?;
        let entry = self.component_entry_for_project(component, render_project.root.as_path())?;
        render_project
            .project
            .render_entry(entry.as_str(), &empty_props)
            .ok()
            .filter(|html| !html.trim().is_empty())
    }

    fn render_static_fallback_html(&self, component: &Component) -> String {
        let content = self
            .best_effort_static_content(component)
            .unwrap_or_else(|| component.name.clone());
        format!(
            "<section data-albedo-static=\"{}\" data-component-id=\"{}\">{}</section>",
            escape_html(component.name.as_str()),
            component.id.as_u64(),
            escape_html(content.as_str())
        )
    }

    fn best_effort_static_content(&self, component: &Component) -> Option<String> {
        let path = self.resolve_component_path(component.file_path.as_str())?;
        let source = std::fs::read_to_string(path).ok()?;
        let mut text = String::new();
        let mut in_tag = false;
        let mut saw_tag = false;

        for ch in source.chars() {
            match ch {
                '<' => {
                    in_tag = true;
                    saw_tag = true;
                }
                '>' => {
                    in_tag = false;
                }
                _ => {
                    if saw_tag && !in_tag && !ch.is_control() {
                        text.push(ch);
                    }
                }
            }
            if text.len() >= 160 {
                break;
            }
        }

        let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        }
    }

    fn component_entry_for_project(&self, component: &Component, root: &Path) -> Option<String> {
        let absolute = self.resolve_component_path(component.file_path.as_str())?;
        let relative = absolute.strip_prefix(root).ok()?;
        Some(relative.to_string_lossy().replace('\\', "/"))
    }

    fn resolve_component_path(&self, file_path: &str) -> Option<PathBuf> {
        let path = PathBuf::from(file_path);
        if path.is_absolute() {
            Some(path)
        } else {
            self.working_dir.as_ref().map(|cwd| cwd.join(path))
        }
    }

    fn data_deps_for_component(
        &self,
        component: &Component,
        metadata: &ComponentTierMetadata,
    ) -> Vec<DataDep> {
        let mut deps = Vec::new();

        if metadata.effect_profile.io {
            deps.push(DataDep {
                key: "request_context".to_string(),
                source: DataSource::RequestContext {
                    key: "path".to_string(),
                },
            });
        }

        if metadata.effect_profile.asynchronous {
            deps.push(DataDep {
                key: "async_state".to_string(),
                source: DataSource::Cache {
                    cache_key_template: format!(
                        "component:{}:{}",
                        slugify(component.name.as_str()),
                        component.id.as_u64()
                    ),
                    ttl_s: 5,
                },
            });
        }

        deps
    }

    fn dynamic_prop_keys_for_component(&self, component: &Component) -> Vec<String> {
        let mut keys = Vec::new();
        let module_path = component.file_path.replace('\\', "/");
        if module_path.contains('[') && module_path.contains(']') {
            // A `[slug]` route component receives the parsed route params as a
            // single `params` object prop (`{ slug }`) — the Next-idiomatic
            // shape `async function Page({ params })` expects. The serve path
            // resolves this key via `RequestContext::resolve("params")`, which
            // assembles the matched params map into a JSON object.
            keys.push("params".to_string());
        }
        keys
    }

    fn tier_of(&self, id: ComponentId) -> Option<Tier> {
        self.metadata.get(&id).map(|entry| entry.tier)
    }

    /// Names of every Tier-C component — the hydration islands a Tier-A parent
    /// must NOT inline. Drives the `install_island_skip_set` guard around the
    /// static render pass.
    fn tier_c_component_names(&self) -> std::collections::HashSet<String> {
        self.components
            .iter()
            .filter(|(id, _)| self.tier_of(**id) == Some(Tier::C))
            .map(|(_, component)| component.name.clone())
            .collect()
    }

    fn sorted_children(&self, id: ComponentId) -> Vec<ComponentId> {
        let mut children = self
            .graph
            .get_dependencies(&id)
            .into_iter()
            // Module-only dependencies (data/util) are linked on the server via
            // the manifest's dependency edges but have no JSX to render — never
            // walk them as renderable children (would static-render a data file).
            .filter(|child| {
                !self
                    .components
                    .get(child)
                    .is_some_and(|component| component.is_module_only)
            })
            .collect::<Vec<_>>();
        children.sort_unstable_by_key(|component_id| component_id.as_u64());
        children
    }

    fn component_or_panic(&self, id: ComponentId) -> &Component {
        self.components
            .get(&id)
            .unwrap_or_else(|| panic!("missing component '{:?}' while building manifest", id))
    }

    fn metadata_or_panic(&self, id: ComponentId) -> &ComponentTierMetadata {
        self.metadata
            .get(&id)
            .unwrap_or_else(|| panic!("missing tier metadata for component '{:?}'", id))
    }
}

/// FORGE — materialise the shared-slot seeds from `forge.db` at the project
/// root, at build time, so the Stream-B pre-render bakes persisted rows into
/// Tier-B HTML. Runs on a dedicated thread with its own current-thread runtime:
/// `albedo build` drives the manifest build synchronously, but dev hot-reload
/// rebuilds from *inside* the serve runtime, where a bare `block_on` would
/// panic. Any failure (no `forge.db`, open/query error) degrades to an empty
/// seed set, leaving non-FORGE behaviour unchanged.
#[cfg(feature = "forge")]
fn materialize_forge_seeds(working_dir: Option<&std::path::Path>) -> Vec<(String, Vec<u8>)> {
    let db_path = working_dir.map_or_else(|| PathBuf::from("forge.db"), |dir| dir.join("forge.db"));
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return Vec::new();
                };
                runtime.block_on(async {
                    let Ok(substrate) =
                        crate::forge::LibSqlSubstrate::open_local(&db_path).await
                    else {
                        return Vec::new();
                    };
                    if crate::forge::skeleton::bootstrap_schema(&substrate)
                        .await
                        .is_err()
                    {
                        return Vec::new();
                    }
                    crate::forge::skeleton::materialize_seeds(&substrate)
                        .await
                        .unwrap_or_default()
                })
            })
            .join()
            .unwrap_or_default()
    })
}

/// Non-FORGE builds carry no substrate; the seed set is always empty.
#[cfg(not(feature = "forge"))]
fn materialize_forge_seeds(_working_dir: Option<&std::path::Path>) -> Vec<(String, Vec<u8>)> {
    Vec::new()
}

/// Phase P · build the Phase K render project from the same component
/// source tree the Phase J static render uses. Returns `None` when
/// `static_render_project` is absent (no resolvable source files) or
/// when `CompiledProject::wrap` errors (parse failure surfaced as
/// extraction error). The manifest builder degrades to placeholder
/// fallback HTML in that case.
fn build_compiled_render_project(
    static_render_project: &Option<StaticRenderProject>,
) -> Option<CompiledRenderProject> {
    let static_project = static_render_project.as_ref()?;
    // `ComponentProject` derives `Clone`; cloning here costs one
    // pass over the parsed modules. The alternative — moving the
    // project from `static_render_project` into the compiled one —
    // would force `static_render_project` to disappear, breaking
    // the Phase J fallback path that the existing builder relies
    // on for Tier-A static renders.
    let project_clone = static_project.project.clone();
    let compiled = match CompiledProject::wrap(project_clone) {
        Ok(compiled) => compiled,
        Err(err) => {
            tracing::warn!(
                target: "albedo.manifest.build",
                error = %err,
                "CompiledProject::wrap failed; static renders fall back to the legacy \
                 ComponentProject path (CSS-module class names will not resolve)"
            );
            return None;
        }
    };
    Some(CompiledRenderProject {
        root: static_project.root.clone(),
        project: compiled,
    })
}

/// Phase P · resolve the project's `<root>/routes/` directory by
/// looking for `/routes/` segments in any component file path, then
/// run `discover_routes` against it. The CLI build path lands the
/// resulting `DiscoveredRoute` list here so the manifest can carry
/// layout chain (and later error/loading) metadata without the
/// builder needing the dev contract.
///
/// Returns an empty vector when no `routes/` segment is found or
/// when discovery errors — the manifest then ships without
/// file-based routing metadata, which is correct for projects that
/// use config-driven routing only.
fn discover_routes_from_components(
    components: &HashMap<ComponentId, Component>,
    working_dir: Option<&Path>,
) -> Vec<DiscoveredRoute> {
    let Some(routes_dir) = infer_routes_dir(components, working_dir) else {
        return Vec::new();
    };
    discover_routes(&routes_dir)
        .map(|d| d.routes)
        .unwrap_or_default()
}

/// Look for any component file under `<some>/routes/<*>` and return
/// the canonical absolute `<some>/routes/` directory. Tolerant of
/// `\` vs `/` separators on Windows.
fn infer_routes_dir(
    components: &HashMap<ComponentId, Component>,
    working_dir: Option<&Path>,
) -> Option<PathBuf> {
    for component in components.values() {
        let normalised = component.file_path.replace('\\', "/");
        let Some(idx) = normalised.find("/routes/") else {
            continue;
        };
        // `/routes/` is 8 chars; we want the path including `routes/`
        // (no trailing slash) as the discovery root.
        let prefix = &normalised[..idx + "/routes".len()];
        let Some(absolute) = resolve_component_path(prefix, working_dir) else {
            continue;
        };
        if absolute.is_dir() {
            return Some(absolute);
        }
    }
    None
}

fn build_static_render_project(
    components: &HashMap<ComponentId, Component>,
    working_dir: Option<&Path>,
) -> Option<StaticRenderProject> {
    let mut module_files = components
        .values()
        .filter_map(|component| resolve_component_path(component.file_path.as_str(), working_dir))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();

    if module_files.is_empty() {
        return None;
    }

    module_files.sort();
    module_files.dedup();

    let mut root = module_files
        .first()
        .and_then(|path| path.parent().map(Path::to_path_buf))?;
    for path in module_files.iter().skip(1) {
        let parent = path.parent()?;
        root = common_ancestor(root, parent)?;
    }

    let project = match ComponentProject::load_from_dir(&root) {
        Ok(project) => project,
        Err(err) => {
            tracing::warn!(
                target: "albedo.manifest.build",
                root = %root.display(),
                error = %err,
                "ComponentProject::load_from_dir failed; static/Tier-A rendering \
                 will be unavailable for this build"
            );
            return None;
        }
    };
    Some(StaticRenderProject { root, project })
}

fn resolve_component_path(file_path: &str, working_dir: Option<&Path>) -> Option<PathBuf> {
    let path = PathBuf::from(file_path);
    if path.is_absolute() {
        Some(path)
    } else {
        working_dir.map(|cwd| cwd.join(path))
    }
}

fn common_ancestor(mut left: PathBuf, right: &Path) -> Option<PathBuf> {
    while !right.starts_with(&left) {
        if !left.pop() {
            return None;
        }
    }
    Some(left)
}

/// Classic, head-level stub that buffers `__albedo_inject` / `__albedo_hydrate`
/// calls until the deferred `runtime.js` module installs the real handlers and
/// drains the queue (see `installLegacyHtmlInjector` in `assets/albedo-runtime.js`).
/// Runs synchronously during head parse, so it is defined before any streamed
/// Tier-B injection script in the body executes.
const TIER_B_INJECT_BOOTSTRAP: &str = "<script>(function(){var w=window;\
w.__albedo_inject=function(){(w.__ALBEDO_INJECT_QUEUE=w.__ALBEDO_INJECT_QUEUE||[]).push(arguments);};\
w.__albedo_hydrate=function(){(w.__ALBEDO_HYDRATE_QUEUE=w.__ALBEDO_HYDRATE_QUEUE||[]).push(arguments);};})();</script>";

fn default_shim_script(enable_wt_bootstrap: bool) -> String {
    let mut script = "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>".to_string();
    if enable_wt_bootstrap {
        script.push_str(
            "<script type=\"module\" async src=\"/_albedo/wt-bootstrap.js\" data-albedo-wt-bootstrap=\"1\"></script>",
        );
    }
    // Phase L · client-side Link / form-action / Navigate
    // interception. Always shipped — even Tier-A-only routes may
    // carry `<Link>` elements for SPA navigation. Module-script
    // execution follows document order, so this loads after
    // `runtime.js` and finds `__ALBEDO_RUNTIME` already wired by the
    // time its IIFE runs.
    script.push_str("<script type=\"module\" src=\"/_albedo/link-forms.js\"></script>");
    script
}

fn stream_slot_label(slot: u8) -> &'static str {
    match slot {
        WT_STREAM_SLOT_CONTROL => WTRenderMode::Control.as_str(),
        WT_STREAM_SLOT_SHELL => WTRenderMode::Shell.as_str(),
        WT_STREAM_SLOT_PATCHES => WTRenderMode::Patch.as_str(),
        WT_STREAM_SLOT_PREFETCH => WTRenderMode::Prefetch.as_str(),
        _ => "unknown",
    }
}

fn next_order(counter: &mut u32) -> u32 {
    let current = *counter;
    *counter = counter.saturating_add(1);
    current
}

/// Phase P · post-P wire-through — tiny RFC 4648 base64 encoder.
/// Mirrors `src/bin/albedo.rs::base64_encode` so the dev path and
/// production manifest emit identical bootstrap script payloads.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '_' || ch == '-' {
            out.push('_');
        }
    }
    if out.is_empty() {
        "component".to_string()
    } else {
        out
    }
}

/// Gate 2 · B (slice 2) — hoist JSX-rendered `<title>`/`<meta>` out of
/// every rendered tier node's HTML into one merged [`RouteMetadata`],
/// stripping the tags from the node HTML in place. Covers Tier-A roots,
/// Tier-B pre-rendered HTML, and the Tier-A children nested under Tier-B.
/// The body that ships to the browser therefore no longer carries the
/// hoisted tags — they re-emit in the document `<head>`.
fn hoist_head_from_nodes(
    tier_a_root: &mut [RenderedNode],
    tier_b: &mut [TierBNode],
) -> RouteMetadata {
    fn hoist_into(html: &mut String, merged: &mut RouteMetadata) {
        let (stripped, found) = hoist_head_tags_from_body(html);
        if !found.is_empty() {
            *html = stripped;
            merged.merge(found);
        }
    }

    let mut merged = RouteMetadata::default();
    for node in tier_a_root.iter_mut() {
        hoist_into(&mut node.html, &mut merged);
    }
    for node in tier_b.iter_mut() {
        for child in node.tier_a_children.iter_mut() {
            hoist_into(&mut child.html, &mut merged);
        }
        if let Some(html) = node.initial_html.as_mut() {
            hoist_into(html, &mut merged);
        }
    }
    merged
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{default_shim_script, stream_slot_label, TIER_B_INJECT_BOOTSTRAP};
    use crate::runtime::webtransport::{
        WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES, WT_STREAM_SLOT_PREFETCH,
        WT_STREAM_SLOT_SHELL,
    };

    #[test]
    fn tier_b_inject_bootstrap_is_a_classic_stub_defining_both_globals() {
        // Must be a CLASSIC script (no type=module) so it runs during parse,
        // before any streamed Tier-B inject call — and must define both the
        // inject and hydrate buffering globals the runtime module drains.
        assert!(TIER_B_INJECT_BOOTSTRAP.starts_with("<script>"));
        assert!(!TIER_B_INJECT_BOOTSTRAP.contains("type=\"module\""));
        assert!(TIER_B_INJECT_BOOTSTRAP.contains("__albedo_inject"));
        assert!(TIER_B_INJECT_BOOTSTRAP.contains("__albedo_hydrate"));
        assert!(TIER_B_INJECT_BOOTSTRAP.contains("__ALBEDO_INJECT_QUEUE"));
        assert!(TIER_B_INJECT_BOOTSTRAP.contains("__ALBEDO_HYDRATE_QUEUE"));
    }

    #[test]
    fn test_default_shim_script_includes_wt_bootstrap_for_streaming_routes() {
        let script = default_shim_script(true);
        assert!(script.contains("/_albedo/runtime.js"));
        assert!(script.contains("/_albedo/wt-bootstrap.js"));
        assert!(script.contains("data-albedo-wt-bootstrap"));
    }

    #[test]
    fn test_default_shim_script_omits_wt_bootstrap_for_tier_a_only_routes() {
        let script = default_shim_script(false);
        assert!(script.contains("/_albedo/runtime.js"));
        assert!(!script.contains("/_albedo/wt-bootstrap.js"));
    }

    #[test]
    fn test_default_shim_script_always_includes_link_forms_asset() {
        // Phase L · `link-forms.js` ships on every route, Tier-A
        // included, because `<Link>` SPA navigation works without
        // any WT bootstrap. Both shim variants must reference it.
        let with_wt = default_shim_script(true);
        let without_wt = default_shim_script(false);
        assert!(with_wt.contains("/_albedo/link-forms.js"));
        assert!(without_wt.contains("/_albedo/link-forms.js"));
    }

    #[test]
    fn test_stream_slot_label_maps_expected_slots() {
        assert_eq!(stream_slot_label(WT_STREAM_SLOT_CONTROL), "control");
        assert_eq!(stream_slot_label(WT_STREAM_SLOT_SHELL), "shell");
        assert_eq!(stream_slot_label(WT_STREAM_SLOT_PATCHES), "patch");
        assert_eq!(stream_slot_label(WT_STREAM_SLOT_PREFETCH), "prefetch");
    }

    /// Layout-island reachability — a Tier-C island mounted in a layout (not on
    /// the route) is lifted into the route's islands so it ships a hydration
    /// block, anchored inline in the layout (sentinel parent → kept out of the
    /// `<children />` slot) with its name → placeholder-id recorded for the
    /// renderer. Guards the "discovered → built → dropped at serve" regression.
    #[test]
    fn collect_layout_islands_lifts_a_tier_c_island_from_a_layout() {
        use super::{ComponentTierMetadata, ManifestBuilder, LAYOUT_ISLAND_PARENT};
        use crate::effects::EffectProfile;
        use crate::manifest::schema::{AssetManifest, HydrationMode, Tier};
        use crate::types::{Component, ComponentId};
        use crate::RenderCompiler;
        use std::collections::HashMap;

        let mut compiler = RenderCompiler::new();

        let mut layout = Component::new(ComponentId::new(0), "RootLayout".to_string());
        layout.file_path = "src/routes/layout.tsx".to_string();

        // An effect-bearing Tier-C island, mounted by the layout's masthead.
        let mut panel = Component::new(ComponentId::new(0), "Panel".to_string());
        panel.file_path = "src/components/Panel.tsx".to_string();
        panel.effect_profile.side_effects = true;

        let layout_id = compiler.add_component(layout);
        let panel_id = compiler.add_component(panel);
        compiler.add_dependency(layout_id, panel_id).unwrap();

        let mut metadata = HashMap::new();
        metadata.insert(
            layout_id,
            ComponentTierMetadata {
                tier: Tier::A,
                hydration_mode: HydrationMode::None,
                effect_profile: EffectProfile::default(),
            },
        );
        metadata.insert(
            panel_id,
            ComponentTierMetadata {
                tier: Tier::C,
                hydration_mode: HydrationMode::OnIdle,
                effect_profile: EffectProfile {
                    side_effects: true,
                    ..EffectProfile::default()
                },
            },
        );

        let builder = ManifestBuilder::new(compiler.graph(), metadata, 1000);
        let assets = AssetManifest::default();
        let mut order = 0u32;

        let (nodes, map) =
            builder.collect_layout_islands(&["RootLayout".to_string()], &mut order, &assets);

        assert_eq!(nodes.len(), 1, "the layout's Tier-C island is lifted");
        let node = &nodes[0];
        assert_eq!(node.component_id, "Panel");
        assert!(
            node.placeholder_id.starts_with("__c_panel_"),
            "placeholder id is the standard tier-c scheme; got {}",
            node.placeholder_id
        );
        assert_eq!(
            node.position.parent_placeholder.as_deref(),
            Some(LAYOUT_ISLAND_PARENT),
            "sentinel parent keeps it out of the <children /> slot"
        );
        assert!(
            node.side_effects,
            "effect flag carried so fix #2 keeps it on the A3 hydration path"
        );
        assert_eq!(
            map.get("Panel"),
            Some(&node.placeholder_id),
            "renderer map points the island name at its inline placeholder id"
        );

        // A layout with no island lifts nothing — no spurious blocks.
        let (empty_nodes, empty_map) =
            builder.collect_layout_islands(&[], &mut order, &assets);
        assert!(empty_nodes.is_empty() && empty_map.is_empty());
    }
}
