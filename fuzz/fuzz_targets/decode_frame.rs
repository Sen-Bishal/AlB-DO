// Phase A — opcode-frame decoder fuzz target.
//
// Drives `wire::decode_frame` with arbitrary bytes. The decoder must never
// panic; any input is either a successfully decoded `OpcodeFrame` or a
// `WireError::Decode`.
//
// PHASE 2 (B-emitter) — Pinaki: do NOT register an emitter call site
// against `decode_frame` until this target has run clean for at least 5
// minutes locally. Once Phase B ships, every decoder fix is a wire break.
// — Bishal-albdo@may-2026

#![no_main]

use dom_render_compiler::ir::wire;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = wire::decode_frame(data);
});
