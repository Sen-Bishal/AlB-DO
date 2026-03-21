use crate::effects::EffectProfile;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ComponentId(u64);

impl ComponentId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    pub id: ComponentId,
    pub name: String,
    pub weight: f64,
    pub bitrate: f64,
    pub dependencies: HashSet<ComponentId>,
    pub is_above_fold: bool,
    pub is_interactive: bool,
    pub is_lcp_candidate: bool,
    pub effect_profile: EffectProfile,
    pub source_hash: u64,
    pub file_path: String,
    pub line_number: usize,
}

impl Component {
    pub fn new(id: ComponentId, name: String) -> Self {
        Self {
            id,
            name,
            weight: 0.0,
            bitrate: 100.0,
            dependencies: HashSet::new(),
            is_above_fold: false,
            is_interactive: false,
            is_lcp_candidate: false,
            effect_profile: EffectProfile::default(),
            source_hash: 0,
            file_path: String::new(),
            line_number: 0,
        }
    }

    pub fn calculate_adjusted_bitrate(&self) -> f64 {
        let mut adjusted = self.bitrate;

        if self.is_above_fold {
            adjusted *= 5.0;
        }
        if self.is_interactive {
            adjusted *= 3.0;
        }
        if self.is_lcp_candidate {
            adjusted *= 10.0;
        }

        if self.weight > 1000.0 {
            adjusted *= 0.5;
        }

        adjusted
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(align(64))]
pub struct ComponentAnalysis {
    pub id: ComponentId,
    pub priority: f64,
    pub estimated_time_ms: f64,
    pub phase: f64,
    pub topological_level: usize,
}

impl ComponentAnalysis {
    pub fn new(id: ComponentId) -> Self {
        Self {
            id,
            priority: 0.0,
            estimated_time_ms: 0.0,
            phase: 0.0,
            topological_level: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderBatch {
    pub level: usize,
    pub components: Vec<ComponentId>,
    pub estimated_time_ms: f64,
    pub can_defer: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OptimizationResult {
    pub version: String,
    pub generated_at: String,
    pub critical_path: Vec<ComponentId>,
    pub parallel_batches: Vec<RenderBatch>,
    pub metrics: OptimizationMetrics,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OptimizationMetrics {
    pub total_components: usize,
    pub total_weight_kb: f64,
    pub optimization_time_ms: u128,
    pub estimated_improvement_ms: f64,
}

pub struct IdGenerator {
    counter: AtomicU64,
}

impl IdGenerator {
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }

    pub fn next(&self) -> ComponentId {
        ComponentId(self.counter.fetch_add(1, Ordering::SeqCst))
    }
}

impl Default for IdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CompilerError {
    #[error("Circular dependency detected: {0:?}")]
    CircularDependency(Vec<ComponentId>),

    #[error("Component not found: {0:?}")]
    ComponentNotFound(ComponentId),

    #[error("Invalid component graph: {0}")]
    InvalidGraph(String),

    #[error("Analysis failed: {0}")]
    AnalysisFailed(String),
}

pub type Result<T> = std::result::Result<T, CompilerError>;
