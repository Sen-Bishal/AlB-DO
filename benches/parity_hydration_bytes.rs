//! Phase P · Stream G — Hydration cost per Tier-B island.
//!
//! Measures the **bytes the browser downloads to make a single
//! component interactive**: the bincode-encoded `OpcodeFrame` that
//! Stream B baked into the manifest. This is the per-island
//! incremental hydration cost over the Tier-A static shell.
//!
//! ⚠️ **This bench used to add the wrapper module JS
//! (`__albedo__/wrappers/*.mjs`) to that total, and that was wrong** —
//! no browser ever loaded a wrapper, so those bytes were never
//! downloaded by anyone. They are no longer emitted at all (see
//! `bundler::emit::emit_bundle_artifacts_to_dir_internal`), and the
//! term is gone from the total here. The reported number went *down*
//! as a result, which is the direction that costs us nothing — but it
//! was reported as a download cost when it was not one, and that is
//! the same defect the tier report had before item 4.6.
//!
//! The number maps onto "how big is your React island bundle" for
//! Next.js / Remix. React's smallest hydrated counter shipping the
//! framework runtime is typically 40+ KB; ALBEDO's per-component cost
//! is the opcode frame, bincode-encoded and typically under 200 bytes
//! for a counter.
//!
//! ⬜ **Still not counted, and it dominates:** the shared framework
//! runtime every page pays regardless of tiering. Quote this number
//! only alongside that one.
//!
//! Reproduce with:
//!   cargo bench --bench parity_hydration_bytes

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dom_render_compiler::ir::opcode::{
    Instruction, OpcodeFrame, ProxyId, SlotId, StableId, TagId,
};
use dom_render_compiler::ir::wire::encode_frame;

/// Synthesise the opcode frame Phase K emits for a useState counter
/// — one BindEvent (click → setter), one SetTextRef (count display).
/// Same shape Stream B's `render_tier_b_inline` produces for a
/// useState component at build time.
fn counter_frame() -> OpcodeFrame {
    OpcodeFrame {
        frame_id: 0,
        component_id: Some(1),
        instructions: vec![
            Instruction::BindEvent {
                stable_id: StableId(1),
                event_id: dom_render_compiler::ir::opcode::EventId(0),
                proxy_id: ProxyId(0xdead_beef),
            },
            Instruction::SetTextRef {
                stable_id: StableId(2),
                slot_id: SlotId(0xfeed_face),
            },
            Instruction::SlotSet {
                slot_id: SlotId(0xfeed_face),
                value: b"0".to_vec(),
            },
        ],
    }
}

/// A form-action component: one BindEvent (submit) + a couple of
/// SetText opcodes for field-error spans. Phase L stamps these.
fn form_frame() -> OpcodeFrame {
    OpcodeFrame {
        frame_id: 0,
        component_id: Some(2),
        instructions: vec![
            Instruction::BindEvent {
                stable_id: StableId(1),
                event_id: dom_render_compiler::ir::opcode::EventId(2),
                proxy_id: ProxyId(0xcafe_0001),
            },
            Instruction::Create {
                tag_id: TagId(3),
                stable_id: StableId(10),
            },
            Instruction::SetText {
                stable_id: StableId(10),
                text: b"".to_vec(),
            },
        ],
    }
}

/// A list-rendering component: more SetText / Append opcodes, fewer
/// event bindings. Representative of a chat message list or feed.
fn list_frame() -> OpcodeFrame {
    let mut instructions = vec![Instruction::BindEvent {
        stable_id: StableId(1),
        event_id: dom_render_compiler::ir::opcode::EventId(0),
        proxy_id: ProxyId(0xa11c_0001),
    }];
    for i in 0..10u32 {
        instructions.push(Instruction::Create {
            tag_id: TagId(4),
            stable_id: StableId(100 + i),
        });
        instructions.push(Instruction::SetText {
            stable_id: StableId(100 + i),
            text: format!("item {i}").into_bytes(),
        });
        instructions.push(Instruction::Append {
            parent_id: StableId(1),
            child_id: StableId(100 + i),
        });
    }
    OpcodeFrame {
        frame_id: 0,
        component_id: Some(3),
        instructions,
    }
}

fn report_island_bytes(label: &str, frame: &OpcodeFrame) {
    let encoded = encode_frame(frame).expect("encode frame");
    let opcode_bytes = encoded.len();
    eprintln!("  {label:<10} opcodes {ops:>4} B", ops = opcode_bytes);
}

fn print_hydration_summary() {
    eprintln!();
    eprintln!("─── Phase P · G — Hydration bytes per island (opcode frame) ───");
    report_island_bytes("counter", &counter_frame());
    report_island_bytes("form", &form_frame());
    report_island_bytes("list", &list_frame());
    eprintln!();
    eprintln!(
        "  Reference: React 18 minimal counter bundle (Next.js `app/`)\n  \
         typically lands at 42–48 KB gzipped per route. Compare like-for-like."
    );
    eprintln!();
}

fn bench_hydration(c: &mut Criterion) {
    print_hydration_summary();

    // Microbenchmark the opcode encoding. The numbers above are the
    // deliverable; this timing is here to round out the bench harness output.
    c.bench_function("hydration_opcode_encode", |b| {
        let frame = counter_frame();
        b.iter(|| {
            let encoded = encode_frame(&frame).expect("encode");
            black_box(encoded);
        });
    });
}

criterion_group!(benches, bench_hydration);
criterion_main!(benches);
