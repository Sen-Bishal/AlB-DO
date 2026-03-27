use crate::manifest::schema::RenderManifestV2;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadErrorKind {
    ModuleMissing,
    DependencyCycle,
    InvalidEntryExport,
    UnsupportedSyntax,
    EngineFailure,
}

impl fmt::Display for LoadErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::ModuleMissing => "module_missing",
            Self::DependencyCycle => "dependency_cycle",
            Self::InvalidEntryExport => "invalid_entry_export",
            Self::UnsupportedSyntax => "unsupported_syntax",
            Self::EngineFailure => "engine_failure",
        };
        write!(f, "{label}")
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    #[error("InitError: {0}")]
    InitError(String),
    #[error("LoadError[{kind}]: {message}")]
    LoadError {
        kind: LoadErrorKind,
        message: String,
    },
    #[error("RenderError: {0}")]
    RenderError(String),
    #[error("PropsError: {0}")]
    PropsError(String),
}

impl RuntimeError {
    pub fn init(message: impl Into<String>) -> Self {
        Self::InitError(message.into())
    }

    pub fn load(kind: LoadErrorKind, message: impl Into<String>) -> Self {
        Self::LoadError {
            kind,
            message: message.into(),
        }
    }

    pub fn render(message: impl Into<String>) -> Self {
        Self::RenderError(message.into())
    }

    pub fn props(message: impl Into<String>) -> Self {
        Self::PropsError(message.into())
    }
}

pub type RuntimeResult<T> = std::result::Result<T, RuntimeError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutput {
    pub html: String,
    pub eval_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderStreamOutput {
    pub shell_html: String,
    pub deferred_chunks: Vec<String>,
    pub eval_ms: u128,
}

#[derive(Debug, Clone, Default)]
pub struct BootstrapPayload {
    pub dom_shim_js: String,
    pub runtime_helpers_js: String,
    pub preloaded_libraries: Vec<BootstrapLibrary>,
}

#[derive(Debug, Clone)]
pub struct BootstrapLibrary {
    pub specifier: String,
    pub code: String,
}

pub fn stable_source_hash(source: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in source.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub trait RuntimeEngine {
    fn init(&mut self, bootstrap: &BootstrapPayload) -> RuntimeResult<()>;
    fn load_module(&mut self, specifier: &str, code: &str) -> RuntimeResult<()>;
    fn load_precompiled_module(
        &mut self,
        specifier: &str,
        compiled_script: &str,
        source_hash: u64,
    ) -> RuntimeResult<()>;
    fn render_component(&mut self, entry: &str, props_json: &str) -> RuntimeResult<RenderOutput>;
    fn render_component_stream(
        &mut self,
        entry: &str,
        props_json: &str,
    ) -> RuntimeResult<RenderStreamOutput> {
        let rendered = self.render_component(entry, props_json)?;
        Ok(RenderStreamOutput {
            shell_html: rendered.html,
            deferred_chunks: Vec::new(),
            eval_ms: rendered.eval_ms,
        })
    }
    fn warm(&mut self) -> RuntimeResult<()>;
    fn prewarm(&mut self) {
        let _ = self.init(&BootstrapPayload::default());
    }
    fn is_initialized(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct ManifestSentinelRing {
    current_manifest: Arc<RwLock<Arc<RenderManifestV2>>>,
    slots: Vec<RwLock<Option<Arc<RenderManifestV2>>>>,
    head: AtomicUsize,
}

impl ManifestSentinelRing {
    pub fn new(initial_manifest: Arc<RenderManifestV2>, ring_size: usize) -> Self {
        let normalized_size = ring_size.max(1);
        let slots = (0..normalized_size)
            .map(|_| RwLock::new(None))
            .collect::<Vec<_>>();

        Self {
            current_manifest: Arc::new(RwLock::new(initial_manifest)),
            slots,
            head: AtomicUsize::new(0),
        }
    }

    pub fn acquire(&self) -> Arc<RenderManifestV2> {
        let manifest = self
            .current_manifest
            .read()
            .expect("manifest lock poisoned")
            .clone();
        let slot = self.next_slot();
        if let Ok(mut guard) = self.slots[slot].write() {
            *guard = Some(manifest.clone());
        }
        manifest
    }

    pub fn publish(&self, new_manifest: Arc<RenderManifestV2>) {
        if let Ok(mut guard) = self.current_manifest.write() {
            *guard = new_manifest;
        }
    }

    pub fn clear_slot(&self, slot: usize) {
        if let Some(slot_ref) = self.slots.get(slot) {
            if let Ok(mut guard) = slot_ref.write() {
                *guard = None;
            }
        }
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    fn next_slot(&self) -> usize {
        let next = self.head.fetch_add(1, Ordering::AcqRel);
        next % self.slots.len()
    }
}
