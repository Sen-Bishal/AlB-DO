//CiCD reviewer rules

use super::schema::{
    AssetManifest, DataDep, DataSource, DomPosition, HtmlShell, HydrationMode, RenderedNode,
    RouteManifest, Tier, TierBNode, TierCNode, WTStreamSlot,
};
use crate::effects::EffectProfile;
use crate::graph::ComponentGraph;
use crate::runtime::ast_eval::ComponentProject;
use crate::runtime::webtransport::{
    WTRenderMode, WTStreamRouter, WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES,
    WT_STREAM_SLOT_PREFETCH, WT_STREAM_SLOT_SHELL,
};
use crate::types::{Component, ComponentId};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

struct StaticRenderProject {
    root: PathBuf,
    project: ComponentProject,
}

struct ShellPlaceholder {
    order: u32,
    html: String,
}

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

        Self {
            graph,
            components,
            metadata,
            tier_b_timeout_ms,
            working_dir,
            static_render_project,
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

        self.traverse(
            root_component,
            None,
            &mut tier_a_root,
            &mut tier_b,
            &mut tier_c,
            &mut order_counter,
            assets,
        );

        RouteManifest {
            route: route.to_string(),
            shell: self.build_shell(route, assets, &tier_a_root, &tier_b, &tier_c),
            tier_a_root,
            tier_b,
            tier_c,
        }
    }

    pub fn build_assets_manifest(&self) -> AssetManifest {
        let mut chunks = HashMap::new();
        let mut css = Vec::new();

        for component in self.components.values() {
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

            if component.file_path.ends_with(".css") {
                css.push(component.file_path.replace('\\', "/"));
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
    ) -> HtmlShell {
        let mut doctype_and_head = String::from(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
        );
        doctype_and_head.push_str(&format!("<title>ALBEDO {}</title>", escape_html(route)));
        for css_path in &assets.css {
            doctype_and_head.push_str(&format!(
                "<link rel=\"stylesheet\" href=\"{}\">",
                escape_html(css_path)
            ));
        }
        doctype_and_head.push_str("</head>");

        let mut body_open = String::from("<body>");
        let mut placeholders = self.collect_shell_placeholders(tier_a_root, tier_b, tier_c);
        placeholders.sort_by_key(|entry| entry.order);
        for placeholder in placeholders {
            body_open.push_str(&placeholder.html);
        }

        HtmlShell {
            doctype_and_head,
            body_open,
            body_close: "</body></html>".to_string(),
            shim_script: default_shim_script(!tier_b.is_empty() || !tier_c.is_empty()),
        }
    }

    fn traverse(
        &self,
        id: ComponentId,
        parent_placeholder: Option<String>,
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
                if parent_placeholder.is_none() {
                    tier_a_root.push(self.render_static(
                        id,
                        parent_placeholder.clone(),
                        order_counter,
                    ));
                }
                for child in self.sorted_children(id) {
                    self.traverse(
                        child,
                        parent_placeholder.clone(),
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
        }
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
            .unwrap_or_else(|| self.render_static_fallback_html(component));

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
        let render_project = self.static_render_project.as_ref()?;
        let entry = self.component_entry_for_project(component, render_project.root.as_path())?;
        let empty_props = Value::Object(Default::default());
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
            keys.push("path".to_string());
        }
        keys
    }

    fn tier_of(&self, id: ComponentId) -> Option<Tier> {
        self.metadata.get(&id).map(|entry| entry.tier)
    }

    fn sorted_children(&self, id: ComponentId) -> Vec<ComponentId> {
        let mut children = self
            .graph
            .get_dependencies(&id)
            .into_iter()
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

    let project = ComponentProject::load_from_dir(&root).ok()?;
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

fn default_shim_script(enable_wt_bootstrap: bool) -> String {
    let mut script = "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>".to_string();
    if enable_wt_bootstrap {
        script.push_str(
            "<script type=\"module\" async src=\"/_albedo/wt-bootstrap.js\" data-albedo-wt-bootstrap=\"1\"></script>",
        );
    }
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
    use super::{default_shim_script, stream_slot_label};
    use crate::runtime::webtransport::{
        WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES, WT_STREAM_SLOT_PREFETCH,
        WT_STREAM_SLOT_SHELL,
    };

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
    fn test_stream_slot_label_maps_expected_slots() {
        assert_eq!(stream_slot_label(WT_STREAM_SLOT_CONTROL), "control");
        assert_eq!(stream_slot_label(WT_STREAM_SLOT_SHELL), "shell");
        assert_eq!(stream_slot_label(WT_STREAM_SLOT_PATCHES), "patch");
        assert_eq!(stream_slot_label(WT_STREAM_SLOT_PREFETCH), "prefetch");
    }
}
