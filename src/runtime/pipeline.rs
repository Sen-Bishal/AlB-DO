use super::dirty_bitmap::DirtyBitmap;
use super::frame::{frame_tick, FrameArena, FrameMetrics, FrameReport};
use super::highway::{phase_to_lane, HighwayPlan, LANE_COUNT};
use super::hot_set::{HotSetError, RenderPriority};
use super::pi_arch::{
    DispatchOutcome, LaneMessage, LaneTarget, PhaseResult, PiArchKernel, PiArchLayer,
};
use super::scheduler::{OvertakeZoneScheduler, SchedulerConfig, SchedulerFrameStats};
use super::webtransport::{
    LaneRenderedChunk, WTRenderMode, WTStreamRouter, WebTransportError, WebTransportFrame,
    WebTransportMuxer,
};
use crate::graph::ComponentGraph;
use crate::ir::columns::IrColumns;
use crate::manifest::schema::Tier;
use crate::types::{CompilerError, ComponentAnalysis, ComponentId};
use rustc_hash::FxHashMap;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum RuntimePipelineError {
    #[error(transparent)]
    Compiler(#[from] CompilerError),
    #[error(transparent)]
    HotSet(#[from] HotSetError),
    #[error(transparent)]
    WebTransport(#[from] WebTransportError),
}

pub struct FourLaneRuntimePipeline {
    highway: HighwayPlan,
    analyses: HashMap<ComponentId, ComponentAnalysis>,
    scheduler: OvertakeZoneScheduler,
    inter_lane: PiArchLayer,
    stream_router: WTStreamRouter,
    frame_arena: FrameArena,
    columns: IrColumns,
    dirty_bitmap: DirtyBitmap,
    frame_metrics: FrameMetrics,
}

impl FourLaneRuntimePipeline {
    pub fn new(
        graph: &ComponentGraph,
        analyses: HashMap<ComponentId, ComponentAnalysis>,
        component_tiers: HashMap<ComponentId, Tier>,
        hot_set_entries: &[(ComponentId, RenderPriority)],
        scheduler_config: SchedulerConfig,
        lane_queue_capacity: usize,
    ) -> Result<Self, RuntimePipelineError> {
        let highway = HighwayPlan::build(graph, &analyses)?;
        let scheduler = OvertakeZoneScheduler::with_hot_set(scheduler_config, hot_set_entries)?;
        let inter_lane = PiArchLayer::new(lane_queue_capacity.max(1), PiArchKernel::default());
        let stream_router =
            WTStreamRouter::with_component_tiers(WebTransportMuxer::new(), component_tiers);
        let frame_arena = FrameArena::with_capacity(analyses.len());

        let mut columns = IrColumns::from_graph(graph, &analyses);
        let phase_by_id: FxHashMap<u64, f64> = analyses
            .iter()
            .map(|(id, analysis)| (id.as_u64(), analysis.phase))
            .collect();
        columns.sort_by_lane(|id| {
            phase_by_id
                .get(&id)
                .copied()
                .map_or(0, phase_to_lane)
        });
        let dirty_bitmap = DirtyBitmap::with_capacity(columns.len());
        let frame_metrics = FrameMetrics::default();

        Ok(Self {
            highway,
            analyses,
            scheduler,
            inter_lane,
            stream_router,
            frame_arena,
            columns,
            dirty_bitmap,
            frame_metrics,
        })
    }

    pub fn submit_analyzer_result(&self, component_id: ComponentId) -> bool {
        self.scheduler.submit_analyzer_result(component_id)
    }

    pub fn mark_hot_dirty(&self, component_id: ComponentId) -> bool {
        self.scheduler.mark_hot_dirty(component_id)
    }

    pub fn run_scheduler_frame(&self) -> SchedulerFrameStats {
        self.scheduler.run_frame()
    }

    pub fn dispatch_cross_lane_dependency_signals(&self) -> usize {
        let mut routed = 0usize;

        for edge in &self.highway.cross_lane_dependencies {
            let Some(source_analysis) = self.analyses.get(&edge.dependency) else {
                continue;
            };
            let Some(target_analysis) = self.analyses.get(&edge.dependent) else {
                continue;
            };

            let message = LaneMessage {
                from_lane: edge.from_lane,
                component_id: edge.dependency,
                phase_result: PhaseResult {
                    phase: source_analysis.phase,
                    priority: source_analysis.priority,
                },
            };

            let target = LaneTarget {
                lane: edge.to_lane,
                phase: target_analysis.phase,
                priority: target_analysis.priority,
            };

            if let DispatchOutcome::Routed { .. } = self.inter_lane.dispatch(message, &[target]) {
                routed += 1;
            }
        }

        routed
    }

    pub fn drain_inter_lane_messages(&self, lane: usize) -> Vec<LaneMessage> {
        let mut drained = Vec::new();
        if lane >= LANE_COUNT {
            return drained;
        }
        self.inter_lane
            .drain_lane(lane, |message| drained.push(message));
        drained
    }

    pub fn drain_render_queue_to_lane_chunks(&self) -> Vec<LaneRenderedChunk> {
        let mut chunks = Vec::new();
        while let Some(component_id) = self.scheduler.pop_render_ready() {
            let chunk = self.stream_router.route_component_chunk(
                component_id,
                WTRenderMode::Patch,
                format!("component:{}", component_id.as_u64()),
            );
            chunks.push(chunk);
        }
        chunks
    }

    pub fn mux_lane_chunks(
        &self,
        chunks: &[LaneRenderedChunk],
    ) -> Result<Vec<WebTransportFrame>, RuntimePipelineError> {
        Ok(self.stream_router.mux_lane_chunks(chunks)?)
    }

    /// Marks a component dirty by its id, looking up its column index via
    /// [`IrColumns::index_of`]. Returns `true` iff the id is known and the
    /// bit was not already set.
    pub fn mark_component_dirty(&self, component_id: ComponentId) -> bool {
        let Some(column_idx) = self.columns.index_of(component_id.as_u64()) else {
            return false;
        };
        let Ok(idx) = usize::try_from(column_idx) else {
            return false;
        };
        self.dirty_bitmap.mark(idx)
    }

    /// Marks a column slot directly. Exposed for fuzz harnesses and soak
    /// tests that drive the bitmap without going through `ComponentId`.
    pub fn mark_column_dirty(&self, column_idx: usize) -> bool {
        self.dirty_bitmap.mark(column_idx)
    }

    /// Cycle-5 RAF entry point.
    ///
    /// Drives a single reconciliation tick: drain the owned bitmap, partition
    /// the dirty set by lane via `columns.lane_offsets()`, emit per-lane patch
    /// buffers through a rayon join fan-out, and allocate one monotone
    /// sequence per non-empty lane against the shared WebTransport muxer.
    ///
    /// Zero allocations on the hot path — the pipeline's [`FrameArena`]
    /// owns every buffer and is cleared, not freed, between ticks. The
    /// returned [`FrameReport`] is also folded into [`FrameMetrics`] for
    /// downstream percentile observability.
    pub fn tick_frame(&mut self) -> FrameReport {
        let report = frame_tick(
            &self.columns,
            &self.dirty_bitmap,
            &self.stream_router.muxer,
            &mut self.frame_arena,
        );
        self.frame_metrics.record(&report);
        report
    }

    /// Read-only access to the frame arena's lane patch buffers, used by
    /// transports that want to forward the raw bytes after a `tick_frame`.
    pub fn frame_arena(&self) -> &FrameArena {
        &self.frame_arena
    }

    pub fn columns(&self) -> &IrColumns {
        &self.columns
    }

    pub fn dirty_bitmap(&self) -> &DirtyBitmap {
        &self.dirty_bitmap
    }

    pub fn frame_metrics(&self) -> &FrameMetrics {
        &self.frame_metrics
    }

    /// Emits the current metrics window as a tracing event. Kept separate
    /// from [`Self::tick_frame`] so the emit cadence is the caller's choice.
    pub fn emit_frame_metrics_summary(&self) {
        self.frame_metrics.emit_summary();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Component;
    use std::f64::consts::PI;
    use std::time::Duration;

    fn analysis(id: ComponentId, phase: f64, priority: f64) -> ComponentAnalysis {
        ComponentAnalysis {
            id,
            priority,
            estimated_time_ms: 1.0,
            phase,
            topological_level: 0,
        }
    }

    #[test]
    fn test_pipeline_runs_scheduler_and_muxes_lane_frames() {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(Component::new(ComponentId::new(0), "A".to_string()));
        let id_b = graph.add_component(Component::new(ComponentId::new(0), "B".to_string()));
        graph.add_dependency(id_a, id_b).unwrap();

        let mut analyses = HashMap::new();
        analyses.insert(id_a, analysis(id_a, PI + 0.2, 1.0));
        analyses.insert(id_b, analysis(id_b, 0.1, 2.0));
        let component_tiers = HashMap::from([(id_a, Tier::C), (id_b, Tier::B)]);

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            component_tiers,
            &[(id_b, RenderPriority::Critical)],
            SchedulerConfig {
                overtake_budget: Duration::from_secs(1),
                overtake_interval: 2,
                ..SchedulerConfig::default()
            },
            32,
        )
        .unwrap();

        pipeline.mark_hot_dirty(id_b);
        pipeline.submit_analyzer_result(id_a);
        let frame = pipeline.run_scheduler_frame();
        assert_eq!(frame.hot_set_rendered, 1);
        assert_eq!(frame.analyzer_forwarded, 1);

        let chunks = pipeline.drain_render_queue_to_lane_chunks();
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.lane
                    == super::super::webtransport::WT_STREAM_SLOT_PATCHES as usize)
        );
        let frames = pipeline.mux_lane_chunks(&chunks).unwrap();
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn test_pipeline_tick_frame_drives_owned_bitmap_to_lane_patches() {
        use crate::runtime::highway::LANE_COUNT;

        let graph = ComponentGraph::new();
        let ids: Vec<ComponentId> = (0..4)
            .map(|i| {
                graph.add_component(Component::new(ComponentId::new(0), format!("C{i}")))
            })
            .collect();

        // Phase values spread across the [0, 2π) circle so phase_to_lane
        // drops one component per lane.
        let phases = [0.1_f64, 1.7, 3.4, 5.0];
        let mut analyses = HashMap::new();
        for (id, phase) in ids.iter().zip(phases.iter()) {
            analyses.insert(*id, analysis(*id, *phase, 1.0));
        }
        let component_tiers = ids
            .iter()
            .map(|id| (*id, Tier::B))
            .collect::<HashMap<_, _>>();

        let mut pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            component_tiers,
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap();

        assert_eq!(pipeline.columns().len(), 4);
        assert_eq!(pipeline.dirty_bitmap().capacity(), 4);

        let offsets = pipeline.columns().lane_offsets();
        for lane in 0..LANE_COUNT {
            let start = offsets.get(lane).copied().unwrap_or(0);
            let end = offsets.get(lane.saturating_add(1)).copied().unwrap_or(0);
            assert_eq!(
                end.saturating_sub(start),
                1,
                "each lane should receive exactly one component (lane={lane})"
            );
        }

        for id in &ids {
            assert!(
                pipeline.mark_component_dirty(*id),
                "marking a known id must succeed"
            );
        }

        let report = pipeline.tick_frame();
        assert_eq!(report.dirty_count, 4);
        assert_eq!(report.frames_pushed, LANE_COUNT);
        assert!(report.lane_sequences.iter().all(Option::is_some));
        assert_eq!(pipeline.frame_metrics().total_ticks(), 1);
        assert!(pipeline.frame_metrics().sample_count() >= 1);

        let baseline_capacity = pipeline.frame_arena().scratch_capacity();
        for id in &ids {
            pipeline.mark_component_dirty(*id);
        }
        let _ = pipeline.tick_frame();
        assert_eq!(
            pipeline.frame_arena().scratch_capacity(),
            baseline_capacity,
            "subsequent ticks must not reallocate the arena"
        );
        assert_eq!(pipeline.frame_metrics().total_ticks(), 2);
    }

    #[test]
    fn test_pipeline_mark_component_dirty_rejects_unknown_id() {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(Component::new(ComponentId::new(0), "A".to_string()));

        let mut analyses = HashMap::new();
        analyses.insert(id_a, analysis(id_a, 0.1, 1.0));

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            HashMap::from([(id_a, Tier::B)]),
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap();

        assert!(!pipeline.mark_component_dirty(ComponentId::new(u64::MAX)));
        assert!(pipeline.mark_component_dirty(id_a));
    }

    #[test]
    fn test_pipeline_dispatches_cross_lane_signals() {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(Component::new(ComponentId::new(0), "A".to_string()));
        let id_b = graph.add_component(Component::new(ComponentId::new(0), "B".to_string()));
        graph.add_dependency(id_a, id_b).unwrap();

        let mut analyses = HashMap::new();
        analyses.insert(id_a, analysis(id_a, PI + 0.2, 1.0));
        analyses.insert(id_b, analysis(id_b, 0.1, 2.0));
        let component_tiers = HashMap::from([(id_a, Tier::B), (id_b, Tier::C)]);

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            component_tiers,
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap();

        let routed = pipeline.dispatch_cross_lane_dependency_signals();
        assert!(routed >= 1);
        let drained = pipeline.drain_inter_lane_messages(2);
        assert!(!drained.is_empty());
    }
}
