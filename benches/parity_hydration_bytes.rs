//! Phase P · Stream G — Hydration cost per Tier-B island.
//!
//! Measures the **bytes the browser downloads to make a single
//! component interactive**: the wrapper module JS (`__albedo__/wrappers/*.mjs`)
//! plus the bincode-encoded `OpcodeFrame` that Stream B baked into
//! the manifest. This is the per-island incremental hydration cost
//! over the Tier-A static shell.
//!
//! The number maps onto "how big is your React island bundle" for
//! Next.js / Remix. React's smallest hydrated counter shipping the
//! framework runtime is typically 40+ KB; ALBEDO's wrapper is a
//! 4-line trampoline (Stream F.1 design note) so the per-component
//! cost is dominated by the opcode frame, which is bincode-encoded
//! and typically under 200 bytes for a counter.
//!
//! Reproduce with:
//!   cargo bench --bench parity_hydration_bytes

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dom_render_compiler::bundler::rewrite::build_wrapper_module_source;
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

fn report_island_bytes(label: &str, source_module: &str, frame: &OpcodeFrame) {
    let wrapper = build_wrapper_module_source(source_module);
    let encoded = encode_frame(frame).expect("encode frame");
    let opcode_bytes = encoded.len();
    let total = wrapper.len() + opcode_bytes;
    eprintln!(
        "  {label:<10} wrapper {wrapper:>4} B · opcodes {ops:>4} B · total {total:>5} B",
        wrapper = wrapper.len(),
        ops = opcode_bytes,
        total = total,
    );
}

fn print_hydration_summary() {
    eprintln!();
    eprintln!("─── Phase P · G — Hydration bytes per island (wrapper JS + opcode frame) ───");
    report_island_bytes(
        "counter",
        "src/components/Counter.tsx",
        &counter_frame(),
    );
    report_island_bytes("form", "src/routes/login.tsx", &form_frame());
    report_island_bytes("list", "src/routes/feed.tsx", &list_frame());
    eprintln!();
    eprintln!(
        "  Reference: React 18 minimal counter bundle (Next.js `app/`)\n  \
         typically lands at 42–48 KB gzipped per route. Compare like-for-like."
    );
    eprintln!();
}

fn bench_hydration(c: &mut Criterion) {
    print_hydration_summary();

    // Microbenchmark the wrapper-bytes computation + opcode encoding.
    // The numbers above are the deliverable; this timing is here to
    // round out the bench harness output.
    c.bench_function("hydration_wrapper_plus_opcode_encode", |b| {
        let frame = counter_frame();
        b.iter(|| {
            let wrapper = build_wrapper_module_source("src/Counter.tsx");
            let encoded = encode_frame(&frame).expect("encode");
            black_box((wrapper, encoded));
        });
    });
}

criterion_group!(benches, bench_hydration);
criterion_main!(benches);
