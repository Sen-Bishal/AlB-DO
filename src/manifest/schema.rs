use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Tier {
    A,
    B,
    C,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HydrationMode {
    Immediate,
    LazyViewport,
    LazyInteraction,
    LazyIdle,
    None,
    OnVisible,
    OnIdle,
    OnInteraction,
}

impl HydrationMode {
    pub fn into_streaming(self) -> Self {
        match self {
            Self::Immediate => Self::Immediate,
            Self::LazyViewport | Self::OnVisible => Self::LazyViewport,
            Self::LazyInteraction | Self::OnInteraction => Self::LazyInteraction,
            Self::LazyIdle | Self::OnIdle => Self::LazyIdle,
            Self::None => Self::None,
        }
    }
}

/// Describes which components are assigned to a given WebTransport stream slot.
///
/// Emitted into [`RenderManifestV2::wt_streams`] at build time so the dev CLI,
/// `albedo trace`, and the WT client bootstrap can all agree on the slot-to-component
/// mapping without re-running tier analysis at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WTStreamSlot {
    /// Stream slot index (0 = control, 1 = shell, 2 = patches, 3 = prefetch).
    pub slot: u8,
    /// Human-readable label matching `WTRenderMode::as_str()`.
    pub label: String,
    /// Component IDs that stream on this slot.
    pub component_ids: Vec<u64>,
}

/// The full manifest written to disk at build time and loaded at server startup.
///
/// `schema_version` + legacy component fields are retained for backward compatibility
/// with existing tooling while the new route schedule is rolled out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderManifestV2 {
    pub version: u32,
    pub build_id: String,
    pub routes: HashMap<String, RouteManifest>,
    pub assets: AssetManifest,
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub components: Vec<ComponentManifestEntry>,
    #[serde(default)]
    pub parallel_batches: Vec<Vec<u64>>,
    #[serde(default)]
    pub critical_path: Vec<u64>,
    #[serde(default)]
    pub vendor_chunks: Vec<VendorChunk>,
    /// WebTransport stream slot assignments, populated at build time.
    ///
    /// Slot indices follow the `WT_STREAM_SLOT_*` constants in `runtime/webtransport.rs`:
    /// slot 0 = control, 1 = shell, 2 = patches, 3 = prefetch.
    /// Empty when the build predates WT support or when no Tier B/C components exist.
    #[serde(default)]
    pub wt_streams: Vec<WTStreamSlot>,
}

impl RenderManifestV2 {
    pub const SCHEMA_VERSION: &'static str = "2.0";
    pub const VERSION: u32 = 2;

    pub fn legacy_defaults() -> Self {
        Self {
            version: Self::VERSION,
            build_id: String::new(),
            routes: HashMap::new(),
            assets: AssetManifest::default(),
            schema_version: Self::SCHEMA_VERSION.to_string(),
            generated_at: String::new(),
            components: Vec::new(),
            parallel_batches: Vec::new(),
            critical_path: Vec::new(),
            vendor_chunks: Vec::new(),
            wt_streams: Vec::new(),
        }
    }
}

/// Per-route streaming schedule produced at compile time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteManifest {
    pub route: String,
    pub shell: HtmlShell,
    pub tier_a_root: Vec<RenderedNode>,
    pub tier_b: Vec<TierBNode>,
    pub tier_c: Vec<TierCNode>,
    /// Phase P · broadcast topics referenced by any component on this
    /// route. The streaming handler auto-subscribes each WT session
    /// to these at render time so JSX-side `useSharedSlot("topic")`
    /// resolves without explicit subscribe.
    #[serde(default)]
    pub shared_slot_topics: Vec<String>,
    /// Phase P · TS-side action handler names + their wire
    /// `action_id`s for this route. Populated once Stream C lands
    /// the `action()` extractor; the field exists now so manifests
    /// produced by intermediate builds round-trip cleanly.
    #[serde(default)]
    pub action_ids: Vec<RouteActionEntry>,
    /// Phase P · ordered layout chain (outermost → leaf) for this
    /// route. Each entry is a component name resolved through
    /// `discover_routes::DiscoveredRoute.layout_chain`. Render-side
    /// composition (Stream E.1) wraps the route's HTML in each
    /// layout's HTML in order.
    #[serde(default)]
    pub layout_chain: Vec<String>,
    /// Phase P · component name of the `error.tsx` boundary for this
    /// route, if any. Streaming handler serves this when a Tier-C
    /// resolution fails. Stream E.2 populates this; field added now
    /// so the schema is stable.
    #[serde(default)]
    pub error_component: Option<String>,
    /// Phase P · component name of the `loading.tsx` placeholder for
    /// this route, if any. Streaming handler serves this while
    /// Tier-C resolves. Stream E.2 populates this.
    #[serde(default)]
    pub loading_component: Option<String>,
    /// Gate 2 · B — resolved document metadata for this route's
    /// `<head>`. Composed (last-writer-wins) from three layered
    /// sources: the static `export const metadata` object, a dynamic
    /// `generateMetadata()` evaluated per request, and JSX-hoisted
    /// `<title>`/`<meta>` tags. `Default` (all empty) preserves the
    /// historical shell `<head>` exactly — the `ALBEDO {route}` title
    /// fallback still applies — so routes that author no metadata are
    /// byte-identical to pre-B builds.
    #[serde(default)]
    pub metadata: RouteMetadata,
    /// Gate 2 · B slice 3 — when the route's leaf component module exports a
    /// `generateMetadata(props)` function, this carries the boot-plan key
    /// (the leaf component name) the serve path invokes per request to resolve
    /// dynamic `<head>` metadata. `None` (the common case) means the route's
    /// head is fully static and the pre-baked shell stands unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_metadata: Option<String>,
}

/// Gate 2 · B — the resolved per-route document metadata destined for
/// the shell `<head>`. Authoring-surface agnostic: the builder lowers
/// `export const metadata` / `generateMetadata()` / JSX head tags into
/// this one shape, then the shell renders it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Resolved `<meta>` tags in author order (both `name=` and
    /// `property=` flavours; `description` is NOT duplicated here — it
    /// rides the `description` field and the shell emits its tag).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meta: Vec<MetaTag>,
}

impl RouteMetadata {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none() && self.meta.is_empty()
    }

    /// Layer `other` on top of `self` (last-writer-wins): any scalar
    /// `other` sets overrides; meta tags append. This is how the static
    /// base composes with the dynamic and JSX-hoisted overrides.
    pub fn merge(&mut self, other: RouteMetadata) {
        if other.title.is_some() {
            self.title = other.title;
        }
        if other.description.is_some() {
            self.description = other.description;
        }
        self.meta.extend(other.meta);
    }
}

/// One resolved `<meta>` tag. `attr` is the key-carrying attribute —
/// `"name"` for standard + twitter tags, `"property"` for Open Graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetaTag {
    pub attr: String,
    pub key: String,
    pub content: String,
}

/// Phase P · one TS-authored action handler discovered on a route.
/// `action_id` is `FNV-1a-32(name)` — the same hash the form
/// extractor's `allocate_form_action_id` produces, so the wire
/// envelope's `action_id` looks the route up directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteActionEntry {
    pub name: String,
    pub action_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderedNode {
    pub component_id: String,
    pub placeholder_id: String,
    pub html: String,
    pub position: DomPosition,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TierBNode {
    pub component_id: String,
    pub placeholder_id: String,
    pub render_fn: String,
    pub static_props: Value,
    pub dynamic_prop_keys: Vec<String>,
    pub data_deps: Vec<DataDep>,
    pub tier_a_children: Vec<RenderedNode>,
    pub position: DomPosition,
    pub timeout_ms: u64,
    pub fallback_html: Option<String>,
    /// Phase P · pre-rendered initial HTML for this Tier-B component,
    /// produced at build time by `render_entry_with_broadcast` against
    /// a fresh empty slot store. The streaming handler inlines this
    /// into the shell instead of the placeholder fallback. `None`
    /// when the build pipeline couldn't render (missing source,
    /// transient error) — falls back to `fallback_html`.
    #[serde(default)]
    pub initial_html: Option<String>,
    /// Phase P · bincode-encoded `OpcodeFrame` carrying the initial
    /// hydration payload (`BindEvent` + `SetTextRef` + initial
    /// `SlotSet`). The streaming handler ships these bytes verbatim
    /// on the WT patches lane so bakabox wires up the island on
    /// first paint. Empty when no Phase K hooks / events fired.
    /// Encoding via `crate::ir::wire::encode_frame` matches the
    /// runtime wire format — no schema drift possible.
    #[serde(default)]
    pub initial_opcode_frame: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TierCNode {
    pub component_id: String,
    pub placeholder_id: String,
    pub bundle_path: String,
    pub initial_props: Value,
    pub hydration_mode: HydrationMode,
    pub position: DomPosition,
    /// True when this component carries an effect hook (`useEffect` /
    /// `useLayoutEffect` / `useInsertionEffect`) or another mount-time side
    /// effect. Sourced from the tiering analysis' `EffectProfile::side_effects`,
    /// so the serve path never has to re-parse source to learn it. Consumed by
    /// the reactive serve-wire builder: a side-effecting island is excluded from
    /// fine-grained binding mode (whose descriptor has no notion of effects) and
    /// falls back to full A3 hydration, where its effect actually runs.
    #[serde(default)]
    pub side_effects: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DomPosition {
    pub parent_placeholder: Option<String>,
    pub slot: String,
    pub order: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataDep {
    pub key: String,
    pub source: DataSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataSource {
    DbQuery {
        query: String,
        param_keys: Vec<String>,
    },
    HttpFetch {
        url_template: String,
        method: String,
    },
    Cache {
        cache_key_template: String,
        ttl_s: u64,
    },
    RequestContext {
        key: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HtmlShell {
    pub doctype_and_head: String,
    pub body_open: String,
    pub body_close: String,
    pub shim_script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetManifest {
    pub chunks: HashMap<String, String>,
    pub css: Vec<String>,
    pub runtime: String,
}

impl Default for AssetManifest {
    fn default() -> Self {
        Self {
            chunks: HashMap::new(),
            css: Vec::new(),
            runtime: "/_albedo/runtime.js".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComponentManifestEntry {
    pub id: u64,
    pub name: String,
    pub module_path: String,
    pub tier: Tier,
    pub weight_bytes: u64,
    pub priority: f64,
    pub dependencies: Vec<u64>,
    pub can_defer: bool,
    pub hydration_mode: HydrationMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VendorChunk {
    pub chunk_name: String,
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StaticSliceArtifactEntry {
    pub component_id: u64,
    pub module_path: String,
    pub source_hash: u64,
    pub eligible: bool,
    pub ineligibility_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StaticSliceArtifactManifest {
    pub version: String,
    pub manifest_schema_version: String,
    pub manifest_generated_at: String,
    pub entry_component_id: Option<u64>,
    pub slices: Vec<StaticSliceArtifactEntry>,
}

impl StaticSliceArtifactManifest {
    pub const VERSION: &'static str = "1.0";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrecompiledRuntimeModuleEntry {
    pub component_id: u64,
    pub module_path: String,
    pub source_hash: u64,
    pub compiled_script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrecompiledRuntimeModuleSkip {
    pub component_id: u64,
    pub module_path: String,
    pub source_hash: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrecompiledRuntimeModulesArtifact {
    pub version: String,
    pub engine: String,
    pub manifest_schema_version: String,
    pub manifest_generated_at: String,
    pub modules: Vec<PrecompiledRuntimeModuleEntry>,
    pub skipped: Vec<PrecompiledRuntimeModuleSkip>,
}

impl PrecompiledRuntimeModulesArtifact {
    pub const VERSION: &'static str = "1.0";
    pub const ENGINE_QUICKJS: &'static str = "quickjs";
}

#[cfg(test)]
mod tests {
    use super::HydrationMode;

    #[test]
    fn test_hydration_mode_none_stays_none_for_streaming() {
        assert_eq!(HydrationMode::None.into_streaming(), HydrationMode::None);
    }
}
