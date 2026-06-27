//! Gate 1 D — adversarial-input robustness harness for the untrusted wire
//! decoders.
//!
//! ## Why this exists
//!
//! Every decoder reachable from the network is an attack surface: a malicious
//! or merely corrupt client can hand it arbitrary bytes. The contract for all
//! of them is the same — **any** input is either a successfully decoded value
//! or a typed `Err`. It must never:
//!   * panic (a panic on the request path is a crashed worker, or — behind the
//!     `catch_unwind` backstop — a 500 the operator can't explain);
//!   * allocate unboundedly from an attacker-chosen length prefix (a decode
//!     bomb: a few bytes on the wire requesting gigabytes of heap → OOM).
//!
//! ## What this covers (the untrusted boundary, in `dom-render-compiler`)
//!
//! | decoder                  | reached from                                  |
//! |--------------------------|-----------------------------------------------|
//! | `decode_action_envelope` | `POST /_albedo/action` body (2 MiB-capped)    |
//! | `decode_frame`           | WT inbound opcode frames (muxer-budgeted)     |
//! | `decode_intern_table`    | WT inbound intern tables (muxer-budgeted)     |
//!
//! The HTTP-head parser (`albedo dev`) and the resource-exhaustion limits live
//! on the binary/server side and are covered by their own tests; this file owns
//! the codec layer.
//!
//! ## Why a hand-rolled harness and not just `cargo fuzz`
//!
//! The repo already ships `cargo fuzz` targets (`fuzz/fuzz_targets`), but
//! libFuzzer needs a sanitizer toolchain that does not run on the Windows MSVC
//! host this project is developed on — so those targets have never actually
//! executed here. This harness is plain `cargo test`: deterministic (seeded),
//! cross-platform, and CI-runnable on every PR. The fuzz targets remain the
//! deep, coverage-guided option for Linux/CI; this is the always-on gate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use dom_render_compiler::ir::action::{decode_action_envelope, ActionEnvelope, ActionEventKind};
use dom_render_compiler::ir::opcode::{
    InternEntry, InternTable, InternTableKind, Instruction, OpcodeFrame, StableId, TagId,
};
use dom_render_compiler::ir::wire::{
    decode_frame, decode_intern_table, encode_frame, encode_intern_table,
};

// ── Allocation accounting ────────────────────────────────────────────────
//
// A counting allocator wrapping the system allocator. We never *cap* (a cap
// would turn a runaway request into an uncatchable `handle_alloc_error` abort);
// instead we record the live high-water mark so a test can assert a decoder did
// not balloon the heap from a forged length. One `#[global_allocator]` governs
// this whole test binary.

struct CountingAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `f`, returning the peak *additional* live bytes allocated during it.
/// Single-threaded by construction (the harness never spawns), so the global
/// counters attribute cleanly to the closure under test.
fn peak_alloc_during<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let start_live = LIVE.load(Ordering::Relaxed);
    PEAK.store(start_live, Ordering::Relaxed);
    let result = f();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(start_live);
    (result, peak)
}

// ── Deterministic PRNG ───────────────────────────────────────────────────
//
// xorshift64*, std-only, seeded. Deterministic so a failure is a reproducible
// seed + iteration index, never a flaky "it crashed once on CI".

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Avoid the zero fixed-point.
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }
}

// ── The no-panic driver ──────────────────────────────────────────────────

/// Feeds `input` to `decoder` under `catch_unwind` and fails the test with the
/// offending bytes if it unwinds. Returns whatever the decoder returned (Ok or
/// typed Err — both are acceptable; only a panic is a defect).
fn assert_no_panic<T>(
    label: &str,
    decoder: &(impl Fn(&[u8]) -> T + std::panic::RefUnwindSafe),
    input: &[u8],
) {
    // Silence the default panic hook for the duration so a *captured* panic
    // doesn't spam the test log; we re-raise it as a clean assertion below.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(|| decoder(input));
    std::panic::set_hook(prev);

    if outcome.is_err() {
        panic!(
            "{label} PANICKED on adversarial input ({} bytes): {:02x?}",
            input.len(),
            input
        );
    }
}

/// Hammer a decoder with: pure random bytes, mutated valid seeds, and a curated
/// corpus of classic codec-killers. Inputs are kept small (≤ 4 KiB) so the
/// random pass can never itself OOM — the bounded-allocation property is proven
/// separately by the decode-bomb tests with crafted length prefixes.
fn hammer<T>(
    label: &str,
    decoder: impl Fn(&[u8]) -> T + std::panic::RefUnwindSafe,
    valid_seeds: &[Vec<u8>],
    rng_seed: u64,
) {
    // 1. Curated adversarial corpus — the inputs that historically break codecs.
    let mut corpus: Vec<Vec<u8>> = vec![
        vec![],                       // empty
        vec![0x00],                   // single zero
        vec![0xFF],                   // single high byte
        vec![0xFF; 16],               // all-ones, short
        vec![0xFF; 256],              // all-ones, longer
        vec![0x00; 256],              // all-zeros
        vec![0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], // bincode u64 varint marker + max
        vec![0xFE, 0xFF, 0xFF, 0xFF, 0xFF],                         // bincode u32 varint marker + max
    ];

    // 2. Truncations of every valid seed — the classic "valid prefix, missing
    //    tail" that trips length-then-body decoders.
    for seed in valid_seeds {
        for cut in 0..seed.len() {
            corpus.push(seed[..cut].to_vec());
        }
        corpus.push(seed.clone());
    }

    for input in &corpus {
        assert_no_panic(label, &decoder, input);
    }

    // 3. Pure random + bit-flipped seeds, many iterations, deterministic seed.
    let mut rng = Rng::new(rng_seed);
    for _ in 0..20_000 {
        let input = if valid_seeds.is_empty() || rng.next_u64() & 1 == 0 {
            // Pure random buffer, length 0..=512.
            let len = rng.below(513);
            (0..len).map(|_| rng.byte()).collect::<Vec<u8>>()
        } else {
            // A valid seed with a handful of bytes corrupted.
            let mut buf = valid_seeds[rng.below(valid_seeds.len())].clone();
            if !buf.is_empty() {
                let flips = 1 + rng.below(4);
                for _ in 0..flips {
                    let idx = rng.below(buf.len());
                    buf[idx] = rng.byte();
                }
            }
            buf
        };
        assert_no_panic(label, &decoder, &input);
    }
}

// ── Valid seed builders ──────────────────────────────────────────────────

fn valid_action_envelopes() -> Vec<Vec<u8>> {
    use dom_render_compiler::ir::action::encode_action_envelope;
    [
        ActionEnvelope { action_id: 0, event_kind: ActionEventKind::Click as u8, payload: vec![] },
        ActionEnvelope {
            action_id: u32::MAX,
            event_kind: ActionEventKind::Input as u8,
            payload: b"event.target.value".to_vec(),
        },
        ActionEnvelope {
            action_id: 42,
            event_kind: ActionEventKind::Submit as u8,
            payload: br#"{"_csrf":"abc","name":"x"}"#.to_vec(),
        },
    ]
    .iter()
    .map(|e| encode_action_envelope(e).expect("seed encodes"))
    .collect()
}

fn valid_frames() -> Vec<Vec<u8>> {
    let frames = [
        OpcodeFrame { frame_id: 0, component_id: None, instructions: vec![] },
        OpcodeFrame {
            frame_id: 7,
            component_id: Some(3),
            instructions: vec![
                Instruction::Create { tag_id: TagId(0), stable_id: StableId(1) },
                Instruction::Append { parent_id: StableId(0), child_id: StableId(1) },
            ],
        },
    ];
    frames.iter().map(|f| encode_frame(f).expect("seed encodes")).collect()
}

fn valid_intern_tables() -> Vec<Vec<u8>> {
    let tables = [
        InternTable { kind: InternTableKind::Tag, entries: vec![] },
        InternTable {
            kind: InternTableKind::Attr,
            entries: vec![
                InternEntry { id: 0, value: "class".into() },
                InternEntry { id: 1, value: "id".into() },
            ],
        },
    ];
    tables.iter().map(|t| encode_intern_table(t).expect("seed encodes")).collect()
}

// ── No-panic tests ───────────────────────────────────────────────────────

#[test]
fn action_envelope_decoder_never_panics() {
    hammer(
        "decode_action_envelope",
        |b| decode_action_envelope(b),
        &valid_action_envelopes(),
        0xAC71_04E5_E01B_2244,
    );
}

#[test]
fn frame_decoder_never_panics() {
    hammer("decode_frame", |b| decode_frame(b), &valid_frames(), 0xF2A3_E5C1_0B47_9D6E);
}

#[test]
fn intern_table_decoder_never_panics() {
    hammer(
        "decode_intern_table",
        |b| decode_intern_table(b),
        &valid_intern_tables(),
        0x17E2_88C4_55AA_3300,
    );
}

// ── Decode-bomb (bounded allocation) ─────────────────────────────────────
//
// The action envelope is decoded straight from a `POST /_albedo/action` body.
// The HTTP layer caps the body at 2 MiB, but that bounds bytes *received*, not
// bytes the decoder may try to *allocate* from a forged length prefix. These
// tests forge a length prefix far larger than the body and assert the decoder
// stays bounded — the canonical decode-bomb defense.

/// bincode-2 unsigned variable-int encoding, matching `ir::wire::config()`'s
/// `with_variable_int_encoding()`. Validated against the real codec by
/// `bincode_varint_helper_matches_codec` below, so the forged bytes are exact.
fn bincode_varint(n: u64) -> Vec<u8> {
    if n <= 250 {
        vec![n as u8]
    } else if n <= u64::from(u16::MAX) {
        let mut v = vec![251];
        v.extend_from_slice(&(n as u16).to_le_bytes());
        v
    } else if n <= u64::from(u32::MAX) {
        let mut v = vec![252];
        v.extend_from_slice(&(n as u32).to_le_bytes());
        v
    } else {
        let mut v = vec![253];
        v.extend_from_slice(&n.to_le_bytes());
        v
    }
}

#[test]
fn bincode_varint_helper_matches_codec() {
    use dom_render_compiler::ir::action::encode_action_envelope;
    // The envelope wire is [action_id varint][event_kind u8][payload_len varint]
    // [payload bytes]; with action_id=1 and event_kind=0 the 2-byte prefix is
    // [1, 0], so the bytes between the prefix and the payload are exactly
    // varint(payload.len()).
    for len in [0usize, 2, 250, 251, 300, 70_000] {
        let env = ActionEnvelope { action_id: 1, event_kind: 0, payload: vec![0u8; len] };
        let bytes = encode_action_envelope(&env).expect("encode");
        let on_wire = &bytes[2..bytes.len() - len];
        assert_eq!(on_wire, bincode_varint(len as u64).as_slice(), "varint mismatch at len={len}");
    }
}

#[test]
fn action_envelope_decode_is_allocation_bounded() {
    // Forge an envelope claiming a 256 MiB payload with ZERO payload bytes
    // present. A 7-byte body must not turn into a 256 MiB heap request.
    let claimed: u64 = 256 * 1024 * 1024;
    let mut bomb = vec![1u8, 0u8]; // action_id=1, event_kind=0
    bomb.extend(bincode_varint(claimed));

    let (result, peak) = peak_alloc_during(|| decode_action_envelope(&bomb));

    assert!(
        peak < 16 * 1024 * 1024,
        "decode_action_envelope allocated {peak} bytes from a {}-byte body claiming {claimed} \
         payload bytes — decode bomb: allocation must be bounded by the input, not the length \
         prefix",
        bomb.len(),
    );
    assert!(
        result.is_err(),
        "an envelope claiming {claimed} payload bytes with none present must decode to Err"
    );
}
