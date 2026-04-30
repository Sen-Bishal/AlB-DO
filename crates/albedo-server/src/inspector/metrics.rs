//! Background metrics aggregator for the inspector.
//!
//! Consumes the same `RenderEvent`s the SSE stream emits and folds them into
//! per-component counters / latency rings. The frontend polls a snapshot every
//! few seconds via `GET /__albedo/api/metrics` — keeping the aggregator
//! pull-friendly means we never have to wait on a serializer in the publish
//! path.

use super::events::{EventTier, RenderEvent};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

const LATENCY_RING_CAPACITY: usize = 256;
const TREND_RING_CAPACITY: usize = 60;
const DEFAULT_ASSUMED_ALPHA: f32 = 0.7;
/// Frontend warns the operator once measured cascade ratio drifts more than
/// this fraction away from the assumed `alpha`.
const ALPHA_DELTA_WARNING: f32 = 0.20;

#[derive(Debug)]
struct ComponentCounter {
    name: String,
    tier: EventTier,
    render_count: u64,
    cascade_total: u64,
    last_seen_ms: u64,
    durations_us: VecDeque<u64>,
}

impl ComponentCounter {
    fn new(name: String, tier: EventTier) -> Self {
        Self {
            name,
            tier,
            render_count: 0,
            cascade_total: 0,
            last_seen_ms: 0,
            durations_us: VecDeque::with_capacity(LATENCY_RING_CAPACITY),
        }
    }

    fn record(&mut self, event: &RenderEvent) {
        self.render_count = self.render_count.saturating_add(1);
        self.cascade_total = self
            .cascade_total
            .saturating_add(event.cascade_count() as u64);
        self.last_seen_ms = event.timestamp_ms;
        if self.durations_us.len() == LATENCY_RING_CAPACITY {
            self.durations_us.pop_front();
        }
        self.durations_us.push_back(event.duration_us);
    }

    fn p95_us(&self) -> u64 {
        if self.durations_us.is_empty() {
            return 0;
        }
        let mut sorted: Vec<u64> = self.durations_us.iter().copied().collect();
        sorted.sort_unstable();
        let last = sorted.len() - 1;
        let idx = (last * 95 + 50) / 100;
        sorted[idx.min(last)]
    }
}

#[derive(Debug)]
struct AggregatorInner {
    components: HashMap<u64, ComponentCounter>,
    total_renders: u64,
    total_cascade: u64,
    trend: VecDeque<f32>,
    last_trend_ms: u64,
}

impl AggregatorInner {
    fn record(&mut self, event: &RenderEvent) {
        let entry = self
            .components
            .entry(event.component_id)
            .or_insert_with(|| ComponentCounter::new(event.component_name.clone(), event.tier));
        entry.record(event);
        self.total_renders = self.total_renders.saturating_add(1);
        self.total_cascade = self
            .total_cascade
            .saturating_add(event.cascade_count() as u64);

        // Sample the collective albedo into the trend ring at most once per
        // ~250ms. Keeps the sparkline shape smooth without recomputing on
        // every event in a burst.
        if event.timestamp_ms.saturating_sub(self.last_trend_ms) >= 250 {
            self.last_trend_ms = event.timestamp_ms;
            let albedo = collective_albedo(self.total_renders, self.total_cascade);
            if self.trend.len() == TREND_RING_CAPACITY {
                self.trend.pop_front();
            }
            self.trend.push_back(albedo);
        }
    }
}

/// Thread-safe wrapper around the metrics state. Cheap to clone an Arc to.
#[derive(Debug)]
pub struct MetricsAggregator {
    inner: Mutex<AggregatorInner>,
    assumed_alpha: f32,
}

impl Default for MetricsAggregator {
    fn default() -> Self {
        Self::new(DEFAULT_ASSUMED_ALPHA)
    }
}

impl MetricsAggregator {
    pub fn new(assumed_alpha: f32) -> Self {
        Self {
            inner: Mutex::new(AggregatorInner {
                components: HashMap::new(),
                total_renders: 0,
                total_cascade: 0,
                trend: VecDeque::with_capacity(TREND_RING_CAPACITY),
                last_trend_ms: 0,
            }),
            assumed_alpha,
        }
    }

    pub fn record(&self, event: &RenderEvent) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.record(event);
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let Ok(guard) = self.inner.lock() else {
            return MetricsSnapshot::empty(self.assumed_alpha);
        };

        let collective = collective_albedo(guard.total_renders, guard.total_cascade);
        let measured_alpha = if guard.total_renders == 0 {
            0.0
        } else {
            guard.total_cascade as f32 / guard.total_renders as f32
        };
        let alpha_delta = (measured_alpha - self.assumed_alpha).abs();
        let alpha_warning =
            self.assumed_alpha > 0.0 && (alpha_delta / self.assumed_alpha) > ALPHA_DELTA_WARNING;

        let mut hottest: Vec<HotComponent> = guard
            .components
            .iter()
            .map(|(id, counter)| HotComponent {
                id: *id,
                name: counter.name.clone(),
                tier: counter.tier,
                render_count: counter.render_count,
            })
            .collect();
        hottest.sort_by_key(|c| std::cmp::Reverse(c.render_count));
        hottest.truncate(5);

        let mut slowest: Vec<SlowComponent> = guard
            .components
            .iter()
            .map(|(id, counter)| SlowComponent {
                id: *id,
                name: counter.name.clone(),
                tier: counter.tier,
                p95_us: counter.p95_us(),
                render_count: counter.render_count,
            })
            .collect();
        slowest.sort_by_key(|c| std::cmp::Reverse(c.p95_us));
        slowest.truncate(5);

        let mut tier = TierBreakdown::default();
        for counter in guard.components.values() {
            match counter.tier {
                EventTier::A => tier.a = tier.a.saturating_add(counter.render_count),
                EventTier::B => tier.b = tier.b.saturating_add(counter.render_count),
                EventTier::C => tier.c = tier.c.saturating_add(counter.render_count),
            }
        }

        let trend: Vec<f32> = guard.trend.iter().copied().collect();

        MetricsSnapshot {
            collective_albedo: collective,
            cascade_ratio: measured_alpha,
            assumed_alpha: self.assumed_alpha,
            measured_alpha,
            alpha_delta,
            alpha_warning,
            total_renders: guard.total_renders,
            tracked_components: guard.components.len(),
            tier_breakdown: tier,
            hottest,
            slowest,
            trend,
            // Lane utilization is sourced from the runtime pipeline's
            // FrameMetrics, which is not wired into this crate yet. Surface
            // zeros so the heatmap renders a quiet "no signal" state instead
            // of fabricating numbers.
            lane_utilization: [0.0; 4],
        }
    }
}

fn collective_albedo(total_renders: u64, total_cascade: u64) -> f32 {
    if total_renders == 0 {
        return 1.0;
    }
    let cascade_ratio = total_cascade as f32 / total_renders as f32;
    (1.0 - cascade_ratio.min(1.0)).clamp(0.0, 1.0)
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub collective_albedo: f32,
    pub cascade_ratio: f32,
    pub assumed_alpha: f32,
    pub measured_alpha: f32,
    pub alpha_delta: f32,
    pub alpha_warning: bool,
    pub total_renders: u64,
    pub tracked_components: usize,
    pub tier_breakdown: TierBreakdown,
    pub hottest: Vec<HotComponent>,
    pub slowest: Vec<SlowComponent>,
    pub trend: Vec<f32>,
    pub lane_utilization: [f32; 4],
}

impl MetricsSnapshot {
    pub fn empty(assumed_alpha: f32) -> Self {
        Self {
            collective_albedo: 1.0,
            cascade_ratio: 0.0,
            assumed_alpha,
            measured_alpha: 0.0,
            alpha_delta: 0.0,
            alpha_warning: false,
            total_renders: 0,
            tracked_components: 0,
            tier_breakdown: TierBreakdown::default(),
            hottest: Vec::new(),
            slowest: Vec::new(),
            trend: Vec::new(),
            lane_utilization: [0.0; 4],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TierBreakdown {
    pub a: u64,
    pub b: u64,
    pub c: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HotComponent {
    pub id: u64,
    pub name: String,
    pub tier: EventTier,
    pub render_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlowComponent {
    pub id: u64,
    pub name: String,
    pub tier: EventTier,
    pub p95_us: u64,
    pub render_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: u64, name: &str, dur_us: u64, cascade: usize, ts: u64) -> RenderEvent {
        RenderEvent {
            component_id: id,
            component_name: name.to_string(),
            tier: EventTier::B,
            duration_us: dur_us,
            timestamp_ms: ts,
            cascade_children: vec![0; cascade],
            note: None,
        }
    }

    #[test]
    fn snapshot_records_counts_and_ranks_hottest() {
        let agg = MetricsAggregator::default();
        for i in 0..10 {
            agg.record(&ev(1, "TodoList", 100 + i * 10, 1, 1_000 + i * 100));
        }
        for i in 0..3 {
            agg.record(&ev(2, "Header", 40, 0, 2_000 + i * 100));
        }
        let snap = agg.snapshot();
        assert_eq!(snap.total_renders, 13);
        assert_eq!(snap.hottest.first().map(|c| c.id), Some(1));
        assert!(snap.cascade_ratio > 0.0);
    }

    #[test]
    fn alpha_warning_fires_when_measured_drifts() {
        let agg = MetricsAggregator::new(0.2);
        for _ in 0..20 {
            agg.record(&ev(1, "TodoList", 100, 5, 1_000));
        }
        let snap = agg.snapshot();
        assert!(snap.alpha_warning, "20% drift should warn");
    }
}
