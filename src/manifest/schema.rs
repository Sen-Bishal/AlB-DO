use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Tier {
    A,
    B,
    C,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum HydrationMode {
    None,
    OnVisible,
    OnIdle,
    OnInteraction,
    Immediate,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderManifestV2 {
    pub schema_version: String,
    pub generated_at: String,
    pub components: Vec<ComponentManifestEntry>,
    pub parallel_batches: Vec<Vec<u64>>,
    pub critical_path: Vec<u64>,
    pub vendor_chunks: Vec<VendorChunk>,
}

impl RenderManifestV2 {
    pub const SCHEMA_VERSION: &'static str = "2.0";
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
