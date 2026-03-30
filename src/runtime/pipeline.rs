use super::highway::{HighwayPlan, LANE_COUNT};
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
use crate::manifest::schema::Tier;
use crate::types::{CompilerError, ComponentAnalysis, ComponentId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
        let stream_router = WTStreamRouter::with_component_tiers(
            Arc::new(Mutex::new(WebTransportMuxer::new())),
            component_tiers,
        );

        Ok(Self {
            highway,
            analyses,
            scheduler,
            inter_lane,
            stream_router,
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
        &mut self,
        chunks: &[LaneRenderedChunk],
    ) -> Result<Vec<WebTransportFrame>, RuntimePipelineError> {
        Ok(self.stream_router.mux_lane_chunks(chunks)?)
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

        let mut pipeline = FourLaneRuntimePipeline::new(
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
