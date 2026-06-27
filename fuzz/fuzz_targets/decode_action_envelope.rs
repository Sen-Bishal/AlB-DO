// Gate 1 D — action-envelope decoder fuzz target.
//
// Drives `ir::action::decode_action_envelope` with arbitrary bytes. This is the
// one wire decoder fed straight from untrusted network input (the
// `POST /_albedo/action` body), so it is the highest-value fuzz target. The
// decoder must never panic and never allocate beyond the bounded decode config
// (`ir::wire::MAX_WIRE_DECODE_BYTES`); any input is either a decoded
// `ActionEnvelope` or a `WireError::Decode`.
//
// The always-on, cross-platform counterpart is `tests/adversarial_input.rs`
// (plain `cargo test`); this target is the deep, coverage-guided run for
// Linux/CI where libFuzzer is available. Run:
//   cargo fuzz run decode_action_envelope -- -max_total_time=300 -rss_limit_mb=512
// (the rss limit makes a decode-bomb regression surface as an OOM, not a hang).

#![no_main]

use dom_render_compiler::ir::action::decode_action_envelope;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = decode_action_envelope(data);
});
