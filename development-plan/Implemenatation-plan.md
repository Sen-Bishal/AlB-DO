# Phase A — Opcode Set + Binary Frame Format

Server-driven UI instruction set and wire encoding for AlBDO. All DOM mutations are expressed as compact opcodes that the server serializes with Bincode v2 and the client decodes into slot-table operations — no JS function shipping, no HTML string diffing.

## Design Decisions

1. **Intern table shipping** — Bootstrap via the `InitInternTable` opcode on the control stream (full table, single shot). Incremental updates ship via a separate `PatchInternTable { kind, ops: Vec<InternPatchOp> }` opcode, where each op is `Set { id, value }` or `Remove { id }`. Two opcodes instead of one mode-flagged variant: the opcode discriminant unambiguously distinguishes bootstrap from patch, and the bootstrap path stays small (no per-entry tag byte). Hot-reload of new components ships `PatchInternTable` without re-shipping the entire table.

2. **`value_ref` / `text_ref` semantics** — `SetAttr` and `SetText` carry **inline `Vec<u8>`** payloads (static values; the encoder stays single-pass and the decoder is stateless for these). Reactive bindings — i.e. `useState`-driven values — use the Alt-D opcode triple: `SetTextRef { stable_id, slot_id }` and `SetAttrRef { stable_id, attr_id, slot_id }` register a binding once at create-time, and subsequent updates arrive as `SlotSet { slot_id, value }` carrying only the new bytes against `slot_id`. The client maintains a `Map<SlotId, BindingSite>` and re-applies on every `SlotSet`. `InternEntry.value` is `String` (UTF-8 only — the intern table holds tag/attr/event identifiers, not arbitrary bytes); `Vec<u8>` is reserved for inline attribute/text payloads where binary is legal.

3. **`instruction_range` in `Patch`** — `InstructionRange { start, end }` is a half-open byte range (`start..end`) into the **concatenation** of all WebTransport STREAM messages that share an `OpcodeFrame::frame_id`. Reassembly via `WebTransportMuxer::reassemble_binary_stream` happens BEFORE `wire::decode_frame` is invoked — per-message decoding is unsupported. Cross-frame references are out of scope for v1. Fields are private; construction goes through `InstructionRange::try_new(start, end) -> Result<Self, RangeError>` so the `start <= end` invariant holds at every wire boundary; the decoder uses `try_new` on every decode.

4. **Alternative D — slot refs in instruction args** — Folded into v1. The `SetTextRef` / `SetAttrRef` / `SlotSet` opcodes are part of the Phase-A wire surface even though only Phase E (hooks) emits them. Cost: ~3 extra bincode discriminants now. Cost of deferring: a wire-format v2 the moment `useState` lands, forcing every deployed client to upgrade.

5. **Frame boundary semantics** — One `OpcodeFrame` MAY span multiple `WebTransportFrame`s (STREAM messages). All such messages carry the same `frame_id`; receiver concatenates by `sequence` order before opcode decoding. Producers MUST allocate `frame_id` via `WebTransportMuxer::allocate_sequence` so a split frame remains attributable. Per-message frame boundaries would constrain the encoder for no observable client benefit and are not supported.

6. **Codec trait surface** — `wire.rs` exposes `WireEncode` / `WireDecode` traits with blanket impls over `bincode::Encode + bincode::Decode<()>`. The free functions `encode_frame` / `decode_frame` / `encode_intern_table` / `decode_intern_table` remain as ergonomic wrappers but delegate to the traits. The trait surface is the contract boundary — a future swap from bincode to FlatBuffers requires changes only in `wire.rs`.

7. **Payload kind on reassemble** — `WebTransportError::PayloadKindMismatch { stream_id, sequence, expected, found }` is returned when a frame's payload kind contradicts the reassemble path's declared kind (text frame on `reassemble_binary_stream`, or vice versa). Surfacing as an error — not a silent drop — so producer routing bugs cannot vanish bytes on the receiver.

### Proposed Changes

IR Module — Opcode Definitions

`src/ir/opcode.rs`
Core instruction enum and supporting ID types. All IDs are u16-interned values that reference tables shipped via `InitInternTable` on the control stream at session init, with `PatchInternTable` for incremental updates. `InstructionRange` is private-fielded with `try_new` validation.

IR Module — Wire Encoding

`src/ir/wire.rs`
Bincode v2 encode/decode entry points, exposed both as free functions and via the `WireEncode` / `WireDecode` traits. Keeps the serialization layer behind a clean trait boundary so swapping to FlatBuffers later requires changes only in this file.

Runtime Module — WebTransport reassemble

`src/runtime/webtransport.rs`
Both `reassemble_stream` (text) and `reassemble_binary_stream` return `WebTransportError::PayloadKindMismatch` on payload-kind contradictions. `reassemble_binary_stream` is the path Phase B's emitter will exercise via the patches stream.

Fuzz Targets

`fuzz/fuzz_targets/decode_frame.rs` and `fuzz/fuzz_targets/decode_intern_table.rs`
`cargo fuzz` harnesses over the wire decoders. Run for ≥ 5 minutes per target before declaring Phase A locked.

Note: Make a python script to showcase our engine
