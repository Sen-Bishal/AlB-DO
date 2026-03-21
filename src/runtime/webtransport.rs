use super::renderer::{RouteStreamChunk, RouteStreamChunkKind};
use crate::types::ComponentId;

pub const WEBTRANSPORT_STREAM_COUNT: usize = 4;

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

pub struct WebTransportMuxer {
    next_sequence: [u64; WEBTRANSPORT_STREAM_COUNT],
    deferred_round_robin: usize,
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
            deferred_round_robin: 0,
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
            RouteStreamChunkKind::ShellHtml | RouteStreamChunkKind::HeadTag => 0,
            RouteStreamChunkKind::DeferredHtml => {
                let stream = 1 + (self.deferred_round_robin % 2);
                self.deferred_round_robin += 1;
                stream
            }
            RouteStreamChunkKind::HydrationPayload => 3,
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
                kind: RouteStreamChunkKind::DeferredHtml,
                content: "B".to_string(),
            },
            RouteStreamChunk {
                kind: RouteStreamChunkKind::HydrationPayload,
                content: "{\"ok\":true}".to_string(),
            },
        ]);

        assert_eq!(frames[0].stream_id, 0);
        assert_eq!(frames[1].stream_id, 1);
        assert_eq!(frames[2].stream_id, 2);
        assert_eq!(frames[3].stream_id, 3);
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
}
