use super::renderer::{RouteStreamChunk, RouteStreamChunkKind};
use crate::manifest::schema::Tier;
use crate::types::ComponentId;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const WEBTRANSPORT_STREAM_COUNT: usize = 4;
pub const WT_STREAM_SLOT_CONTROL: u8 = 0;
pub const WT_STREAM_SLOT_SHELL: u8 = 1;
pub const WT_STREAM_SLOT_PATCHES: u8 = 2;
pub const WT_STREAM_SLOT_PREFETCH: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRenderedChunk {
    pub lane: usize,
    pub component_id: Option<ComponentId>,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTransportFrame {
    pub stream_id: u8,
    pub sequence: u64,
    pub component_id: Option<ComponentId>,
    pub payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WTRenderMode {
    Control,
    Shell,
    Patch,
    Prefetch,
}

#[derive(Debug)]
pub struct WTComponentStream {
    pub component_id: ComponentId,
    pub stream_slot: u8,
    pub render_mode: WTRenderMode,
    pub sequence: AtomicU64,
}

impl WTComponentStream {
    pub fn new(component_id: ComponentId, stream_slot: u8, render_mode: WTRenderMode) -> Self {
        Self {
            component_id,
            stream_slot,
            render_mode,
            sequence: AtomicU64::new(0),
        }
    }

    pub fn next_patch_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::AcqRel)
    }

    pub fn patch_sequence(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }
}

impl Clone for WTComponentStream {
    fn clone(&self) -> Self {
        Self {
            component_id: self.component_id,
            stream_slot: self.stream_slot,
            render_mode: self.render_mode,
            sequence: AtomicU64::new(self.patch_sequence()),
        }
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum WebTransportError {
    #[error("invalid stream id: {stream_id}")]
    InvalidStreamId { stream_id: usize },
    #[error("sequence gap in stream {stream_id}: expected sequence {expected}, received {actual}")]
    SequenceGap {
        stream_id: u8,
        expected: u64,
        actual: u64,
    },
}

pub struct WTStreamRouter {
    pub muxer: Arc<Mutex<WebTransportMuxer>>,
    pub component_map: DashMap<ComponentId, WTComponentStream>,
    component_tiers: DashMap<ComponentId, Tier>,
}

impl WTStreamRouter {
    pub fn new(muxer: Arc<Mutex<WebTransportMuxer>>) -> Self {
        Self {
            muxer,
            component_map: DashMap::new(),
            component_tiers: DashMap::new(),
        }
    }

    pub fn with_component_tiers(
        muxer: Arc<Mutex<WebTransportMuxer>>,
        component_tiers: impl IntoIterator<Item = (ComponentId, Tier)>,
    ) -> Self {
        let router = Self::new(muxer);
        for (component_id, tier) in component_tiers {
            router.register_component_tier(component_id, tier);
        }
        router
    }

    pub fn register_component_tier(&self, component_id: ComponentId, tier: Tier) {
        self.component_tiers.insert(component_id, tier);
    }

    pub fn tier_for_component(&self, component_id: ComponentId) -> Option<Tier> {
        self.component_tiers.get(&component_id).map(|entry| *entry)
    }

    pub fn patch_sequence_for(&self, component_id: ComponentId) -> Option<u64> {
        self.component_map
            .get(&component_id)
            .map(|entry| entry.patch_sequence())
    }

    pub fn stream_slot_for(tier: Tier, render_mode: WTRenderMode) -> u8 {
        match render_mode {
            WTRenderMode::Control => WT_STREAM_SLOT_CONTROL,
            WTRenderMode::Shell => match tier {
                Tier::A => WT_STREAM_SLOT_CONTROL,
                Tier::B | Tier::C => WT_STREAM_SLOT_SHELL,
            },
            WTRenderMode::Patch => match tier {
                Tier::A => WT_STREAM_SLOT_CONTROL,
                Tier::B | Tier::C => WT_STREAM_SLOT_PATCHES,
            },
            WTRenderMode::Prefetch => WT_STREAM_SLOT_PREFETCH,
        }
    }

    pub fn route_component_chunk(
        &self,
        component_id: ComponentId,
        render_mode: WTRenderMode,
        payload: impl Into<String>,
    ) -> LaneRenderedChunk {
        let tier = self.tier_for_component(component_id).unwrap_or(Tier::B);
        let slot = Self::stream_slot_for(tier, render_mode);
        self.upsert_component_stream(component_id, slot, render_mode);

        LaneRenderedChunk {
            lane: slot as usize,
            component_id: Some(component_id),
            payload: payload.into(),
        }
    }

    pub fn route_global_chunk(
        &self,
        render_mode: WTRenderMode,
        payload: impl Into<String>,
    ) -> LaneRenderedChunk {
        let slot = Self::stream_slot_for(Tier::B, render_mode);
        LaneRenderedChunk {
            lane: slot as usize,
            component_id: None,
            payload: payload.into(),
        }
    }

    pub fn mux_lane_chunks(
        &self,
        chunks: &[LaneRenderedChunk],
    ) -> Result<Vec<WebTransportFrame>, WebTransportError> {
        let mut muxer = self.muxer.lock().expect("webtransport muxer lock poisoned");
        muxer.mux_lane_chunks(chunks)
    }

    fn upsert_component_stream(
        &self,
        component_id: ComponentId,
        stream_slot: u8,
        render_mode: WTRenderMode,
    ) {
        if let Some(mut stream) = self.component_map.get_mut(&component_id) {
            if stream.stream_slot != stream_slot || stream.render_mode != render_mode {
                stream.stream_slot = stream_slot;
                stream.render_mode = render_mode;
                stream.sequence.store(0, Ordering::Release);
            }
            if render_mode == WTRenderMode::Patch {
                stream.next_patch_sequence();
            }
            return;
        }

        let stream = WTComponentStream::new(component_id, stream_slot, render_mode);
        if render_mode == WTRenderMode::Patch {
            stream.next_patch_sequence();
        }
        self.component_map.insert(component_id, stream);
    }
}

pub struct WebTransportMuxer {
    next_sequence: [u64; WEBTRANSPORT_STREAM_COUNT],
}

impl Default for WebTransportMuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl WebTransportMuxer {
    pub fn new() -> Self {
        Self {
            next_sequence: [0_u64; WEBTRANSPORT_STREAM_COUNT],
        }
    }

    pub fn mux_lane_chunks(
        &mut self,
        chunks: &[LaneRenderedChunk],
    ) -> Result<Vec<WebTransportFrame>, WebTransportError> {
        let mut frames = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            if chunk.lane >= WEBTRANSPORT_STREAM_COUNT {
                return Err(WebTransportError::InvalidStreamId {
                    stream_id: chunk.lane,
                });
            }
            frames.push(self.make_frame(chunk.lane, chunk.component_id, chunk.payload.clone()));
        }
        Ok(frames)
    }

    pub fn mux_route_chunks(&mut self, chunks: &[RouteStreamChunk]) -> Vec<WebTransportFrame> {
        let mut frames = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let stream_id = self.stream_for_route_chunk(chunk.kind);
            frames.push(self.make_frame(stream_id, None, chunk.content.clone()));
        }
        frames
    }

    pub fn reassemble_stream(
        stream_id: u8,
        frames: &[WebTransportFrame],
    ) -> Result<String, WebTransportError> {
        if stream_id as usize >= WEBTRANSPORT_STREAM_COUNT {
            return Err(WebTransportError::InvalidStreamId {
                stream_id: stream_id as usize,
            });
        }

        let mut selected = frames
            .iter()
            .filter(|frame| frame.stream_id == stream_id)
            .cloned()
            .collect::<Vec<_>>();
        selected.sort_unstable_by_key(|frame| frame.sequence);

        let mut expected = 0_u64;
        let mut output = String::new();
        for frame in selected {
            if frame.sequence != expected {
                return Err(WebTransportError::SequenceGap {
                    stream_id,
                    expected,
                    actual: frame.sequence,
                });
            }
            output.push_str(frame.payload.as_str());
            expected += 1;
        }
        Ok(output)
    }

    fn make_frame(
        &mut self,
        stream_id: usize,
        component_id: Option<ComponentId>,
        payload: String,
    ) -> WebTransportFrame {
        let sequence = self.next_sequence[stream_id];
        self.next_sequence[stream_id] = sequence + 1;

        WebTransportFrame {
            stream_id: stream_id as u8,
            sequence,
            component_id,
            payload,
        }
    }

    fn stream_for_route_chunk(&mut self, kind: RouteStreamChunkKind) -> usize {
        match kind {
            RouteStreamChunkKind::ShellHtml => WT_STREAM_SLOT_SHELL as usize,
            RouteStreamChunkKind::DeferredHtml | RouteStreamChunkKind::HydrationPayload => {
                WT_STREAM_SLOT_PATCHES as usize
            }
            RouteStreamChunkKind::HeadTag => WT_STREAM_SLOT_PREFETCH as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mux_lane_chunks_assigns_monotonic_sequence_per_stream() {
        let mut muxer = WebTransportMuxer::new();
        let frames = muxer
            .mux_lane_chunks(&[
                LaneRenderedChunk {
                    lane: 0,
                    component_id: Some(ComponentId::new(1)),
                    payload: "a".to_string(),
                },
                LaneRenderedChunk {
                    lane: 2,
                    component_id: Some(ComponentId::new(2)),
                    payload: "b".to_string(),
                },
                LaneRenderedChunk {
                    lane: 0,
                    component_id: Some(ComponentId::new(3)),
                    payload: "c".to_string(),
                },
            ])
            .unwrap();

        assert_eq!(frames[0].stream_id, 0);
        assert_eq!(frames[0].sequence, 0);
        assert_eq!(frames[1].stream_id, 2);
        assert_eq!(frames[1].sequence, 0);
        assert_eq!(frames[2].stream_id, 0);
        assert_eq!(frames[2].sequence, 1);
    }

    #[test]
    fn test_mux_route_chunks_maps_shell_deferred_and_hydration_streams() {
        let mut muxer = WebTransportMuxer::new();
        let frames = muxer.mux_route_chunks(&[
            RouteStreamChunk {
                kind: RouteStreamChunkKind::ShellHtml,
                content: "<main>".to_string(),
            },
            RouteStreamChunk {
                kind: RouteStreamChunkKind::DeferredHtml,
                content: "A".to_string(),
            },
            RouteStreamChunk {
                kind: RouteStreamChunkKind::HydrationPayload,
                content: "{\"ok\":true}".to_string(),
            },
            RouteStreamChunk {
                kind: RouteStreamChunkKind::HeadTag,
                content: "<link rel=\"prefetch\" href=\"/next.js\">".to_string(),
            },
        ]);

        assert_eq!(frames[0].stream_id, 1);
        assert_eq!(frames[2].stream_id, 2);
        assert_eq!(frames[3].stream_id, 3);
        assert_eq!(frames[1].stream_id, 2);
    }

    #[test]
    fn test_reassemble_stream_detects_sequence_gaps() {
        let frames = vec![
            WebTransportFrame {
                stream_id: 1,
                sequence: 0,
                component_id: None,
                payload: "A".to_string(),
            },
            WebTransportFrame {
                stream_id: 1,
                sequence: 2,
                component_id: None,
                payload: "B".to_string(),
            },
        ];

        let err = WebTransportMuxer::reassemble_stream(1, &frames).unwrap_err();
        assert!(matches!(err, WebTransportError::SequenceGap { .. }));
    }

    #[test]
    fn test_reassemble_stream_isolated_per_stream() {
        let frames = vec![
            WebTransportFrame {
                stream_id: 0,
                sequence: 0,
                component_id: None,
                payload: "shell".to_string(),
            },
            WebTransportFrame {
                stream_id: 1,
                sequence: 1,
                component_id: None,
                payload: "gap".to_string(),
            },
        ];

        let shell = WebTransportMuxer::reassemble_stream(0, &frames).unwrap();
        assert_eq!(shell, "shell");
    }

    #[test]
    fn test_router_maps_component_tiers_to_stream_slots() {
        let router = WTStreamRouter::with_component_tiers(
            Arc::new(Mutex::new(WebTransportMuxer::new())),
            [
                (ComponentId::new(10), Tier::A),
                (ComponentId::new(11), Tier::B),
                (ComponentId::new(12), Tier::C),
            ],
        );

        let a = router.route_component_chunk(ComponentId::new(10), WTRenderMode::Patch, "a");
        let b = router.route_component_chunk(ComponentId::new(11), WTRenderMode::Patch, "b");
        let c = router.route_component_chunk(ComponentId::new(12), WTRenderMode::Shell, "c");

        assert_eq!(a.lane, WT_STREAM_SLOT_CONTROL as usize);
        assert_eq!(b.lane, WT_STREAM_SLOT_PATCHES as usize);
        assert_eq!(c.lane, WT_STREAM_SLOT_SHELL as usize);
    }

    #[test]
    fn test_router_tracks_per_component_patch_sequence() {
        let router = WTStreamRouter::new(Arc::new(Mutex::new(WebTransportMuxer::new())));
        router.register_component_tier(ComponentId::new(21), Tier::B);
        router.register_component_tier(ComponentId::new(22), Tier::C);

        router.route_component_chunk(ComponentId::new(21), WTRenderMode::Patch, "p1");
        router.route_component_chunk(ComponentId::new(21), WTRenderMode::Patch, "p2");
        router.route_component_chunk(ComponentId::new(22), WTRenderMode::Patch, "q1");

        assert_eq!(router.patch_sequence_for(ComponentId::new(21)), Some(2));
        assert_eq!(router.patch_sequence_for(ComponentId::new(22)), Some(1));
    }

    #[test]
    fn test_router_muxes_routed_chunks_with_muxer_sequences() {
        let router = WTStreamRouter::new(Arc::new(Mutex::new(WebTransportMuxer::new())));
        router.register_component_tier(ComponentId::new(31), Tier::B);

        let chunks = vec![
            router.route_component_chunk(ComponentId::new(31), WTRenderMode::Shell, "<div>"),
            router.route_component_chunk(ComponentId::new(31), WTRenderMode::Patch, "patch-1"),
            router.route_component_chunk(ComponentId::new(31), WTRenderMode::Patch, "patch-2"),
        ];

        let frames = router.mux_lane_chunks(&chunks).unwrap();
        assert_eq!(frames[0].stream_id, WT_STREAM_SLOT_SHELL);
        assert_eq!(frames[1].stream_id, WT_STREAM_SLOT_PATCHES);
        assert_eq!(frames[1].sequence, 0);
        assert_eq!(frames[2].stream_id, WT_STREAM_SLOT_PATCHES);
        assert_eq!(frames[2].sequence, 1);
    }
}
