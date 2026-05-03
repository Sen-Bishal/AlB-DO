// Phase A — intern-table decoder fuzz target.
//
// Drives `wire::decode_intern_table` with arbitrary bytes. The decoder must
// never panic; any input either decodes to an `InternTable` or returns
// `WireError::Decode`.
//
// PHASE 2 (B-emitter) — Pinaki: same locking discipline as `decode_frame`
// — clean for ≥ 5 minutes locally before the first Phase-B caller.
// — Bishal-albdo@may-2026

#![no_main]

use dom_render_compiler::ir::wire;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = wire::decode_intern_table(data);
});
