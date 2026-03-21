use crate::types::ComponentId;
use crossbeam::queue::ArrayQueue;
use std::f64::consts::PI;

pub const LANE_COUNT: usize = 4;
const TAU: f64 = 2.0 * PI;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhaseResult {
    pub phase: f64,
    pub priority: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LaneMessage {
    pub from_lane: usize,
    pub component_id: ComponentId,
    pub phase_result: PhaseResult,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LaneTarget {
    pub lane: usize,
    pub phase: f64,
    pub priority: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LagrangeWeights {
    pub phase: f64,
    pub priority: f64,
    pub load: f64,
}

impl Default for LagrangeWeights {
    fn default() -> Self {
        Self {
            phase: 1.0,
            priority: 1.0,
            load: 0.25,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    Routed { lane: usize },
    DroppedNoRoute,
    DroppedQueueFull { lane: usize },
}

#[derive(Debug, Clone, Copy)]
pub struct PiArchKernel {
    weights: LagrangeWeights,
}

impl Default for PiArchKernel {
    fn default() -> Self {
        Self::new(LagrangeWeights::default())
    }
}

impl PiArchKernel {
    pub fn new(weights: LagrangeWeights) -> Self {
        Self { weights }
    }

    pub fn route_message(
        &self,
        message: &LaneMessage,
        targets: &[LaneTarget],
        lane_loads: &[usize; LANE_COUNT],
    ) -> Option<usize> {
        let mut best_lane = None;
        let mut best_score = f64::INFINITY;

        for target in targets {
            if target.lane >= LANE_COUNT {
                continue;
            }

            let score = self.lagrange_score(message, target, lane_loads[target.lane] as f64);
            if score < best_score {
                best_score = score;
                best_lane = Some(target.lane);
                continue;
            }

            if (score - best_score).abs() <= f64::EPSILON {
                if let Some(current_lane) = best_lane {
                    if target.lane < current_lane {
                        best_lane = Some(target.lane);
                    }
                }
            }
        }

        best_lane
    }

    fn lagrange_score(&self, message: &LaneMessage, target: &LaneTarget, lane_load: f64) -> f64 {
        let phase_distance =
            angular_phase_distance(message.phase_result.phase, target.phase) / PI.max(f64::EPSILON);
        let priority_delta = (target.priority - message.phase_result.priority).abs();

        (self.weights.phase * phase_distance)
            + (self.weights.priority * priority_delta)
            + (self.weights.load * lane_load)
    }
}

pub struct PiArchLayer {
    kernel: PiArchKernel,
    lane_queues: Vec<ArrayQueue<LaneMessage>>,
}

impl PiArchLayer {
    pub fn new(capacity_per_lane: usize, kernel: PiArchKernel) -> Self {
        let mut lane_queues = Vec::with_capacity(LANE_COUNT);
        for _ in 0..LANE_COUNT {
            lane_queues.push(ArrayQueue::new(capacity_per_lane.max(1)));
        }

        Self {
            kernel,
            lane_queues,
        }
    }

    pub fn lane_loads(&self) -> [usize; LANE_COUNT] {
        let mut loads = [0usize; LANE_COUNT];
        for (idx, queue) in self.lane_queues.iter().enumerate() {
            loads[idx] = queue.len();
        }
        loads
    }

    pub fn dispatch(&self, message: LaneMessage, targets: &[LaneTarget]) -> DispatchOutcome {
        let lane_loads = self.lane_loads();
        let Some(lane) = self.kernel.route_message(&message, targets, &lane_loads) else {
            return DispatchOutcome::DroppedNoRoute;
        };

        if self.lane_queues[lane].push(message).is_ok() {
            DispatchOutcome::Routed { lane }
        } else {
            DispatchOutcome::DroppedQueueFull { lane }
        }
    }

    pub fn pop_lane(&self, lane: usize) -> Option<LaneMessage> {
        self.lane_queues.get(lane).and_then(ArrayQueue::pop)
    }

    pub fn drain_lane<F>(&self, lane: usize, mut on_message: F) -> usize
    where
        F: FnMut(LaneMessage),
    {
        let Some(queue) = self.lane_queues.get(lane) else {
            return 0;
        };

        let mut drained = 0usize;
        while let Some(message) = queue.pop() {
            on_message(message);
            drained += 1;
        }
        drained
    }
}

fn normalize_phase(phase: f64) -> f64 {
    phase.rem_euclid(TAU)
}

fn angular_phase_distance(left: f64, right: f64) -> f64 {
    let left = normalize_phase(left);
    let right = normalize_phase(right);
    let absolute = (left - right).abs();
    absolute.min(TAU - absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(phase: f64, priority: f64) -> LaneMessage {
        LaneMessage {
            from_lane: 0,
            component_id: ComponentId::new(1),
            phase_result: PhaseResult { phase, priority },
        }
    }

    #[test]
    fn test_kernel_routing_is_deterministic_for_identical_inputs() {
        let kernel = PiArchKernel::default();
        let message = message(0.3, 5.0);
        let targets = vec![
            LaneTarget {
                lane: 2,
                phase: 0.5,
                priority: 5.2,
            },
            LaneTarget {
                lane: 1,
                phase: 0.4,
                priority: 5.1,
            },
        ];
        let lane_loads = [0, 0, 0, 0];

        let a = kernel.route_message(&message, &targets, &lane_loads);
        let b = kernel.route_message(&message, &targets, &lane_loads);
        assert_eq!(a, b);
        assert_eq!(a, Some(1));
    }

    #[test]
    fn test_kernel_prefers_less_loaded_lane_when_signal_quality_matches() {
        let kernel = PiArchKernel::new(LagrangeWeights {
            phase: 1.0,
            priority: 1.0,
            load: 1.0,
        });
        let message = message(1.0, 2.0);
        let targets = vec![
            LaneTarget {
                lane: 1,
                phase: 1.0,
                priority: 2.0,
            },
            LaneTarget {
                lane: 2,
                phase: 1.0,
                priority: 2.0,
            },
        ];
        let lane_loads = [0, 10, 1, 0];

        let lane = kernel.route_message(&message, &targets, &lane_loads);
        assert_eq!(lane, Some(2));
    }

    #[test]
    fn test_layer_dispatch_and_drain_lane() {
        let layer = PiArchLayer::new(8, PiArchKernel::default());
        let outcome = layer.dispatch(
            message(0.2, 1.0),
            &[LaneTarget {
                lane: 3,
                phase: 0.3,
                priority: 1.1,
            }],
        );
        assert_eq!(outcome, DispatchOutcome::Routed { lane: 3 });

        let mut collected = Vec::new();
        let drained = layer.drain_lane(3, |msg| collected.push(msg.component_id));
        assert_eq!(drained, 1);
        assert_eq!(collected, vec![ComponentId::new(1)]);
    }
}
