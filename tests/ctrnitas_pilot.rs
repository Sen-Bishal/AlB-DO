//! TEMPORARY — CTRNI'TAS pilot measurement. Delete after reading.
//!
//! Q: how often does the static row-projection classifier return a class
//! STRICTLY more conservative than the truth (the row's server HTML really is a
//! pure function of its record)? That gap is the headroom speculation would buy.

use dom_render_compiler::transforms::shared_slot_lists::{
    classify_shared_slot_lists_source, RowProjection,
};

const HDR: &str = "import { useSharedSlot } from \"albedo\";\n";

/// (name, body-of-component, ground truth for topic "cart")
fn corpus() -> Vec<(&'static str, String, RowProjection)> {
    use RowProjection::{PerRecord, PositionStable, WholeView};
    let c = |s: &str| format!("{HDR}export default function C() {{\n  const cart = useSharedSlot(\"cart\");\n  return (<div>{s}</div>);\n}}\n");
    vec![
        ("01 plain product row",
         c(r#"<ul>{cart.map((p) => <li key={p.id}>{p.name} — {p.price}</li>)}</ul>"#),
         PerRecord),
        ("02 destructured record",
         c(r#"<ul>{cart.map(({ id, name, price }) => <li key={id}>{name} {price}</li>)}</ul>"#),
         PerRecord),
        ("03 block body, local computation",
         c(r#"<ul>{cart.map((p) => { const net = p.price * 0.9; return <li key={p.id}>{net}</li>; })}</ul>"#),
         PerRecord),
        ("04 numbered rows (index used)",
         c(r#"<ol>{cart.map((p, i) => <li key={p.id}>#{i + 1} {p.name}</li>)}</ol>"#),
         PositionStable),
        ("05 row reads collection length",
         c(r#"<ul>{cart.map((p) => <li key={p.id}>{p.name} of {cart.length}</li>)}</ul>"#),
         WholeView),
        ("06 named callback",
         format!("{HDR}function renderRow(p) {{ return <li key={{p.id}}>{{p.name}}</li>; }}\nexport default function C() {{\n  const cart = useSharedSlot(\"cart\");\n  return (<ul>{{cart.map(renderRow)}}</ul>);\n}}\n"),
         PerRecord),
        ("07 remove button closes over the array",
         c(r#"<ul>{cart.map((p) => <li key={p.id}>{p.name}<button onClick={() => setCart(cart.filter((x) => x.id !== p.id))}>x</button></li>)}</ul>"#),
         PerRecord),
        ("08 third param (array) used",
         c(r#"<ul>{cart.map((p, i, all) => <li key={p.id}>{p.name} {all.length}</li>)}</ul>"#),
         WholeView),
        ("09 index param present but unused",
         c(r#"<ul>{cart.map((p, i) => <li key={p.id}>{p.name}</li>)}</ul>"#),
         PerRecord),
        // CORRECTED. I first labelled this PerRecord, reading the per-topic
        // collapse in `RowProjection::min` as over-conservatism. It isn't:
        // `scanListAnchors` keys the live binding on `topicSlotId(topic)` and
        // registers only the FIRST anchor, so a topic has one projection, not
        // one per site. WholeView is the correct class here, not headroom.
        ("10 two sites: clean list + summary reading cart.length",
         c(r#"<div><ul>{cart.map((p) => <li key={p.id}>{p.name}</li>)}</ul><aside>{cart.map((p) => <b>{p.name} of {cart.length}</b>)}</aside></div>"#),
         WholeView),
        ("11 nested map over the record's own array",
         c(r#"<ul>{cart.map((p) => <li key={p.id}>{p.tags.map((t) => <span>{t}</span>)}</li>)}</ul>"#),
         PerRecord),
        ("12 arrow with default param",
         c(r#"<ul>{cart.map((p = {}) => <li>{p.name}</li>)}</ul>"#),
         PerRecord),
        ("13 function-expression callback",
         c(r#"<ul>{cart.map(function (p) { return <li key={p.id}>{p.name}</li>; })}</ul>"#),
         PerRecord),
        ("14 row calls a module-level formatter",
         format!("{HDR}const fmt = (n) => `$${{n}}`;\nexport default function C() {{\n  const cart = useSharedSlot(\"cart\");\n  return (<ul>{{cart.map((p) => <li key={{p.id}}>{{fmt(p.price)}}</li>)}}</ul>);\n}}\n"),
         PerRecord),
        ("15 empty-state guard before the list",
         c(r#"<ul>{cart.length === 0 ? <li>empty</li> : cart.map((p) => <li key={p.id}>{p.name}</li>)}</ul>"#),
         PerRecord),
    ]
}

fn rank(c: RowProjection) -> u8 {
    match c {
        RowProjection::WholeView => 0,
        RowProjection::PositionStable => 1,
        RowProjection::PerRecord => 2,
    }
}

#[test]
fn ctrnitas_pilot_provable_vs_actual() {
    let mut agree = 0usize;
    let mut conservative = 0usize;
    let mut unsound = 0usize;
    println!("\n{:<46} {:<15} {:<15} {}", "case", "classifier", "truth", "verdict");
    println!("{}", "-".repeat(96));
    for (name, src, truth) in corpus() {
        let got = classify_shared_slot_lists_source("C.tsx", &src)
            .get("cart")
            .copied()
            .unwrap_or(RowProjection::WholeView);
        let verdict = match rank(got).cmp(&rank(truth)) {
            std::cmp::Ordering::Equal => {
                agree += 1;
                "agree"
            }
            std::cmp::Ordering::Less => {
                conservative += 1;
                "** CONSERVATIVE (headroom)"
            }
            std::cmp::Ordering::Greater => {
                unsound += 1;
                "!! UNSOUND"
            }
        };
        println!("{name:<46} {got:<15?} {truth:<15?} {verdict}");
    }
    let n = agree + conservative + unsound;
    println!("{}", "-".repeat(96));
    println!("n = {n} · agree {agree} · conservative {conservative} · unsound {unsound}");
    println!(
        "provable-vs-actual gap = {:.0}% of sites are misclassified DOWNWARD\n",
        100.0 * conservative as f64 / n as f64
    );
    assert_eq!(unsound, 0, "classifier claimed a class stronger than truth");
}
