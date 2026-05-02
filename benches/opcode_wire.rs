//! Benchmark: Bincode opcode wire format vs JSON string equivalents.
//!
//! Measures three things:
//!   1. Encoded payload size (bytes)
//!   2. Encode throughput (ns/op)
//!   3. Decode throughput (ns/op)
//!
//! Run with:
//!   cargo bench --bench opcode_wire

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dom_render_compiler::ir::opcode::*;
use dom_render_compiler::ir::wire;

/// Builds a realistic OpcodeFrame representing a small component tree:
///   <div class="app">
///     <h1>Hello, world!</h1>
///     <button onclick=proxy(1)>Click me</button>
///     <div data-slot="counter">0</div>
///     <!-- suspense placeholder -->
///   </div>
fn sample_frame() -> OpcodeFrame {
    OpcodeFrame {
        frame_id: 1,
        component_id: Some(42),
        instructions: vec![
            Instruction::Create { tag_id: TagId(0), stable_id: StableId(1) },    // div
            Instruction::SetAttr { stable_id: StableId(1), attr_id: AttrId(0), value: b"app".to_vec() },
            Instruction::Create { tag_id: TagId(1), stable_id: StableId(2) },    // h1
            Instruction::SetText { stable_id: StableId(2), text: b"Hello, world!".to_vec() },
            Instruction::Append { parent_id: StableId(1), child_id: StableId(2) },
            Instruction::Create { tag_id: TagId(2), stable_id: StableId(3) },    // button
            Instruction::SetText { stable_id: StableId(3), text: b"Click me".to_vec() },
            Instruction::BindEvent { stable_id: StableId(3), event_id: EventId(0), proxy_id: ProxyId(1) },
            Instruction::Append { parent_id: StableId(1), child_id: StableId(3) },
            Instruction::Create { tag_id: TagId(0), stable_id: StableId(4) },    // div
            Instruction::SetAttr { stable_id: StableId(4), attr_id: AttrId(1), value: b"counter".to_vec() },
            Instruction::BindSlot { stable_id: StableId(4), slot_id: SlotId(0) },
            Instruction::SetText { stable_id: StableId(4), text: b"0".to_vec() },
            Instruction::Append { parent_id: StableId(1), child_id: StableId(4) },
            Instruction::Placeholder { stable_id: StableId(5), suspense_id: SuspenseId(100) },
            Instruction::Append { parent_id: StableId(1), child_id: StableId(5) },
        ],
    }
}

/// The equivalent HTML string that a pre-Phase-A server would have sent.
fn sample_html_string() -> String {
    r#"<div class="app"><h1>Hello, world!</h1><button>Click me</button><div data-slot="counter">0</div><!--suspense:100--></div>"#.to_string()
}

/// JSON representation of the same payload (how a React-style VDOM diff
/// might serialize it).
fn sample_json_payload() -> serde_json::Value {
    serde_json::json!([
        {"op":"create","tag":"div","id":1},
        {"op":"set_attr","id":1,"attr":"class","value":"app"},
        {"op":"create","tag":"h1","id":2},
        {"op":"set_text","id":2,"text":"Hello, world!"},
        {"op":"append","parent":1,"child":2},
        {"op":"create","tag":"button","id":3},
        {"op":"set_text","id":3,"text":"Click me"},
        {"op":"bind_event","id":3,"event":"click","proxy":1},
        {"op":"append","parent":1,"child":3},
        {"op":"create","tag":"div","id":4},
        {"op":"set_attr","id":4,"attr":"data-slot","value":"counter"},
        {"op":"bind_slot","id":4,"slot":0},
        {"op":"set_text","id":4,"text":"0"},
        {"op":"append","parent":1,"child":4},
        {"op":"placeholder","id":5,"suspense":100},
        {"op":"append","parent":1,"child":5}
    ])
}

fn bench_encode(c: &mut Criterion) {
    let frame = sample_frame();
    let json_val = sample_json_payload();
    let html = sample_html_string();

    // Print sizes once for comparison
    let bincode_bytes = wire::encode_frame(&frame).unwrap();
    let json_bytes = serde_json::to_vec(&json_val).unwrap();
    println!("\n  === Wire Size Comparison ===");
    println!("  Bincode opcode : {:>4} bytes", bincode_bytes.len());
    println!("  JSON opcode    : {:>4} bytes", json_bytes.len());
    println!("  Raw HTML string: {:>4} bytes", html.len());
    println!(
        "  Bincode is {:.1}x smaller than JSON",
        json_bytes.len() as f64 / bincode_bytes.len() as f64
    );
    println!(
        "  Bincode is {:.1}x smaller than HTML\n",
        html.len() as f64 / bincode_bytes.len() as f64
    );

    let mut group = c.benchmark_group("encode");
    group.bench_function("bincode_opcode_frame", |b| {
        b.iter(|| wire::encode_frame(black_box(&frame)).unwrap())
    });
    group.bench_function("json_opcode_payload", |b| {
        b.iter(|| serde_json::to_vec(black_box(&json_val)).unwrap())
    });
    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let frame = sample_frame();
    let json_val = sample_json_payload();

    let bincode_bytes = wire::encode_frame(&frame).unwrap();
    let json_bytes = serde_json::to_vec(&json_val).unwrap();

    let mut group = c.benchmark_group("decode");
    group.bench_function("bincode_opcode_frame", |b| {
        b.iter(|| wire::decode_frame(black_box(&bincode_bytes)).unwrap())
    });
    group.bench_function("json_opcode_payload", |b| {
        b.iter(|| serde_json::from_slice::<serde_json::Value>(black_box(&json_bytes)).unwrap())
    });
    group.finish();
}

fn bench_roundtrip(c: &mut Criterion) {
    let frame = sample_frame();
    let json_val = sample_json_payload();

    let mut group = c.benchmark_group("roundtrip");
    group.bench_function("bincode_encode_decode", |b| {
        b.iter(|| {
            let bytes = wire::encode_frame(black_box(&frame)).unwrap();
            let _ = wire::decode_frame(black_box(&bytes)).unwrap();
        })
    });
    group.bench_function("json_serialize_deserialize", |b| {
        b.iter(|| {
            let bytes = serde_json::to_vec(black_box(&json_val)).unwrap();
            let _: serde_json::Value = serde_json::from_slice(black_box(&bytes)).unwrap();
        })
    });
    group.finish();
}

criterion_group!(benches, bench_encode, bench_decode, bench_roundtrip);
criterion_main!(benches);
