//! Wire-format conformance gate between the Rust runtime and the bakabox
//! client.
//!
//! This test encodes the canonical wire frame produced by
//! [`dom_render_compiler::ir::canonical_v1_frame`] and compares the bytes
//! against a checked-in fixture at
//! `tests/fixtures/wire/v4_canonical_frame.bin`. The bakabox JS decoder
//! consumes the same fixture (see `assets/albedo-bincode.js` and its test
//! suite, landing in Phase C). If the Rust side emits different bytes than
//! the fixture, either:
//!
//! - The wire format changed without a [`LOCKED_WIRE_VERSION`] bump (a
//!   silent break), in which case fix the cause and re-run; OR
//! - The wire format change is intentional, in which case set
//!   `UPDATE_BAKABOX_FIXTURE=1` when running this test, commit the new
//!   fixture, bump `LOCKED_WIRE_VERSION`, and coordinate a bakabox release.
//!
//! Treat the failure as a wire break, not a flaky test.

use dom_render_compiler::ir::{canonical_v1_frame, decode_frame, encode_frame};
use std::env;
use std::fs;
use std::path::PathBuf;

/// Path to the checked-in bakabox conformance fixture, relative to the
/// crate root. The bakabox JS test harness reads from the same path.
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("wire")
        .join("v4_canonical_frame.bin")
}

/// Returns true when the test was invoked with `UPDATE_BAKABOX_FIXTURE=1`.
/// In that mode the test rewrites the fixture instead of comparing against
/// it. This is the only sanctioned way to regenerate the bytes; do not
/// hand-edit the binary.
fn update_mode() -> bool {
    matches!(env::var("UPDATE_BAKABOX_FIXTURE"), Ok(value) if value == "1")
}

/// Encodes the canonical frame and either rewrites or compares against the
/// fixture file, depending on `update_mode()`. Returns the encoded bytes so
/// downstream tests can use them without re-encoding.
fn encode_and_lock_fixture() -> Vec<u8> {
    let frame = canonical_v1_frame();
    let bytes = encode_frame(&frame).expect("canonical frame must encode");
    let path = fixture_path();

    if update_mode() || !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("must create fixture directory");
        }
        fs::write(&path, &bytes).expect("must write fixture file");
        if !update_mode() {
            panic!(
                "bakabox conformance fixture was missing at {}; it has been \
                 generated. Re-run the test to verify, then commit the file.",
                path.display()
            );
        }
        return bytes;
    }

    let expected = fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "must read bakabox conformance fixture at {}: {err}",
            path.display()
        )
    });

    assert_eq!(
        bytes,
        expected,
        "BAKABOX WIRE BREAK: canonical frame bytes diverged from the \
         checked-in fixture. Either restore the wire format to the locked \
         version, or — if this change is intentional — bump \
         LOCKED_WIRE_VERSION, re-run with UPDATE_BAKABOX_FIXTURE=1, and \
         coordinate a bakabox release."
    );

    bytes
}

#[test]
fn canonical_frame_bytes_match_fixture() {
    let _ = encode_and_lock_fixture();
}

#[test]
fn fixture_round_trips_through_decoder() {
    let bytes = encode_and_lock_fixture();
    let (decoded, consumed) =
        decode_frame(&bytes).expect("fixture bytes must decode cleanly");
    assert_eq!(
        consumed,
        bytes.len(),
        "decoder must consume the entire fixture"
    );
    assert_eq!(decoded, canonical_v1_frame());
}
