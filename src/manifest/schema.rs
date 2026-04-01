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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TierCNode {
    pub component_id: String,
    pub placeholder_id: String,
    pub bundle_path: String,
    pub initial_props: Value,
    pub hydration_mode: HydrationMode,
    pub position: DomPosition,
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
