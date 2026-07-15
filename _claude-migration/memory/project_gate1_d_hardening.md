---
name: project-gate1-d-hardening
description: "Gate 1 D robustness pass — untrusted-input threat model, the decode-bomb fix, dev-parser DoS fix, and the always-on adversarial harness"
metadata: 
  node_type: memory
  type: project
  originSessionId: ab0131d5-3e03-4eec-9a60-44c1fb8d7be5
---

Gate 1/2 **D** "remove hot-path `.unwrap()`/`panic!` (serve/parse/decode)" — done **2026-06-27**, threat-model-driven, all **UNCOMMITTED** (user owns commits). Built like a real security pass: threat model → verification harness first → fix what it finds → regression-lock. Extends [[project_b_hooks_and_head]] / sits under [[project_endgame]] Gate sequencing.

## Trust boundary (where untrusted bytes enter OUR code)
- **Prod (`albedo serve`, axum/hyper):** hyper parses HTTP (not ours). Our untrusted decoders: `decode_action_envelope` (`POST /_albedo/action` body, capped 2 MiB by `MAX_REQUEST_BODY_BYTES` in `crates/albedo-server/src/server.rs`), WT opcode/intern frame decoders (muxer-budgeted), CSRF/JSON payload (lenient `serde_json::from_slice(..).ok()` — safe), routing.
- **Dev (`albedo dev`, hand-rolled `TcpStream`):** `read_http_request_head` in `src/bin/albedo.rs` (the `albedo` bin source lives at `src/bin/albedo.rs` but is built by `crates/albedo-server/Cargo.toml`'s `[[bin]] path="../../src/bin/albedo.rs"` → test with `cargo test -p albedo-server --bin albedo`).

## The two real vulnerabilities found + fixed
1. **Decode bomb (process-kill severity).** The wire codec `config()` (`src/ir/wire.rs`) was pinned `.with_no_limit()`; bincode then trusts a `Vec`/`String` length prefix and pre-allocates it. The harness's random pass instantly drove a **2.3-exabyte** allocation → uncatchable OOM-abort (NOT a `panic!`, which is why the existing libFuzzer "never panics" targets would've missed it on rss-limited runs and why it never fired on Windows where the fuzzer never ran). **Fix:** split encode/decode configs — `config()` (encode, the conformance/`LOCKED_WIRE_VERSION` source of truth) is **untouched** (so NO wire-format break, NO version bump — valid bytes decode identically); new bounded `decode_config()` (`MAX_WIRE_DECODE_BYTES = 8 MiB`, same LE+varint knobs) backs the `WireDecode` blanket impl → forged length → typed `WireError::Decode`. Key insight: the original "no limit because the WT muxer budgets it" rationale never covered the action path (HTTP POST bypasses the muxer), and a decoder must never trust a length prefix regardless of upstream budgets.
2. **Dev HTTP-head DoS.** `read_http_request_head` used unbounded `BufReader::read_line` → an endless header line (no terminating blank line) grows a `String` until OOM. **Fix:** `Take`-cap total head (`MAX_REQUEST_HEAD_BYTES = 64 KiB`) + per-line cap (`MAX_REQUEST_LINE_BYTES = 16 KiB`) + header-count cap (`MAX_REQUEST_HEADER_COUNT = 128`); refactored the parse loop into a generic, unit-testable `parse_http_request_head<R: Read>(&mut BufReader<R>)`.

## Triage result for the handlers (certified clean, no fix needed)
- `streaming.rs` + `routing.rs`: **every** `unwrap/expect/panic` is inside `#[cfg(test)]` — prod paths are panic-free by construction.
- `action.rs`: prod uses defensive `Response::builder()...unwrap_or_else(|_| ...)` (can't panic).
- `routing.rs` route-pattern slice ops (`&segment[5..len-2]` etc.) are **guard-proven safe** (the `starts_with(prefix) && ends_with(suffix)` pair forces a min length making the range valid; empty names caught by `validate_param_name`) and run on trusted patterns, not request URLs.
- `public_assets.rs`: `sanitize_public_path` defends path-traversal (rejects `ParentDir`/`RootDir`/`Prefix`/`..`-prefix/null byte) with dedicated tests.
- So the genuine exposure was the two unbounded allocators above; the "remove unwraps" framing was mostly already satisfied.

## Verification infrastructure (the durable artifact)
- **`tests/adversarial_input.rs`** — cross-platform, `cargo test`-runnable (NOT nightly/libFuzzer), so it runs in CI on every PR. Counting `#[global_allocator]` (peak high-water, no cap → no uncatchable abort) + seeded xorshift mutator + adversarial corpus (empty/0xFF/forged varint markers) + per-seed truncations, all under `std::panic::catch_unwind`. Asserts never-panics for `decode_action_envelope`/`decode_frame`/`decode_intern_table` (20k iters each) + a bounded-allocation test (forge a 256 MiB-claiming 7-byte envelope → peak < 16 MiB AND `Err`). Includes `bincode_varint` helper validated against the real codec.
- **`fuzz/fuzz_targets/decode_action_envelope.rs`** — new cargo-fuzz target (mirrors the existing `decode_frame`/`decode_intern_table`). **Windows caveat:** `cargo fuzz`/libFuzzer needs a sanitizer toolchain that doesn't run on this Windows MSVC host — these targets have never executed here. Run on Linux/CI: `cargo fuzz run decode_action_envelope -- -rss_limit_mb=512` (rss limit makes a bomb regression surface as OOM).

## Status / files touched (all uncommitted)
`src/ir/wire.rs` (encode/decode config split), `src/bin/albedo.rs` (bounded head parser + 4 tests), `tests/adversarial_input.rs` (new), `fuzz/{Cargo.toml,fuzz_targets/decode_action_envelope.rs}` (new), `TODO.md`. Regression: 406 lib + 118 server-lib + 26 bin + wire/conformance/broadcast/reactive all green. **Note:** `cargo test --workspace` OOMs the Windows paging file (os error 1455 — too many parallel rustc/link jobs); use `-j2` or per-crate runs.

## Remaining (optional, lower-severity)
- A structured `read_http_request_head` cargo-fuzz target (parser is bounded + unit-tested in the meantime).
- Exhaustive whole-workspace unwrap audit beyond the untrusted boundary (build-time/manifest unwraps operate on trusted developer input — out of this threat model).
