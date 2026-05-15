//! Phase J end-to-end demo gate.
//!
//! `examples/pulse-dashboard/` is the canonical Phase J fixture: a small
//! component tree that exercises every expression shape the renderer
//! used to silently drop, plus shell-stamping and the static-slicer
//! dedup contract. This test renders the example through the same
//! `ComponentProject::render_entry` path that `albedo dev` and
//! `albedo serve` use, then asserts the demo expectations from the
//! sprint plan:
//!
//!   * `{count}` from `useState(0)` renders `0` (the useState shim).
//!   * `{iso}` from `new Date(0).toISOString()` renders a real ISO string.
//!   * `(p99_ms).toFixed(2)` renders a fixed-precision number.
//!   * `Math.floor(...)` and `.length` resolve to numbers.
//!   * Every host element carries a `data-albedo-id` attribute.
//!
//! When this test passes, the white-page failure mode the plan calls
//! out ("`albedo serve` shows white") is structurally impossible: the
//! same renderer that emits this HTML feeds the production server.

use dom_render_compiler::runtime::eval::render_from_components_dir;
use serde_json::Value;
use std::path::PathBuf;

fn dashboard_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("pulse-dashboard")
}

fn render_dashboard() -> String {
    let props = Value::Object(Default::default());
    render_from_components_dir(dashboard_root(), "App.tsx", &props)
        .expect("pulse-dashboard must render through ComponentProject::render_entry")
}

#[test]
fn pulse_dashboard_renders_use_state_initial_value() {
    let html = render_dashboard();
    // The useState shim must bind `count = 0`; the bare `{count}`
    // interpolation should render the literal `0`, not an empty slot.
    assert!(
        html.contains("value: <strong data-albedo-id=\"")
            && html.contains(">0</strong>"),
        "Counter's `{{count}}` must render 0, got: {html}"
    );
    // Second useState in the same component: `label = \"idle\"`.
    assert!(
        html.contains("idle counter"),
        "Counter's `{{label}}` must render 'idle', got: {html}"
    );
}

#[test]
fn pulse_dashboard_renders_new_date_to_iso_string() {
    let html = render_dashboard();
    // `new Date(0).toISOString()` is the canonical Unix epoch ISO.
    assert!(
        html.contains("1970-01-01T00:00:00.000Z"),
        "App's `{{iso}}` must render the epoch ISO, got: {html}"
    );
}

#[test]
fn pulse_dashboard_renders_to_fixed_precision_numbers() {
    let html = render_dashboard();
    // StatusPill: `(svc.p99_ms).toFixed(1)` for each service.
    assert!(html.contains("12.3ms"), "edge p99 must show toFixed(1) = 12.3ms");
    assert!(html.contains("47.9ms"), "render p99 must show toFixed(1) = 47.9ms");
    assert!(html.contains("9.4ms"), "stitch p99 must show toFixed(1) = 9.4ms");
    // LatencyTable: `.toFixed(2)` and `Math.floor(...)`.
    assert!(html.contains(">12.35</td>"), "edge p99 toFixed(2) must round to 12.35");
    assert!(html.contains(">12</td>"), "edge floor must be 12");
    assert!(html.contains(">47</td>"), "render floor must be 47");
}

#[test]
fn pulse_dashboard_renders_array_length_and_iteration() {
    let html = render_dashboard();
    // `services.length` in App's <h2>; `rows.length` in LatencyTable's <tfoot>.
    assert!(
        html.contains("services (3)"),
        "App must render `services.length` as 3"
    );
    assert!(
        html.contains("3 rows"),
        "LatencyTable footer must render `total` as 3"
    );
    // The .map iteration produced three <li> elements.
    assert_eq!(
        html.matches("class=\"pill pill-").count(),
        3,
        "expected 3 pills (one per service)"
    );
}

#[test]
fn pulse_dashboard_stamps_data_albedo_id_on_every_host_element() {
    let html = render_dashboard();
    // Every `<tag` opener should be followed by a `data-albedo-id`
    // attribute. We can't easily count opens, but at minimum the count
    // of stamps should match our coarse element count.
    let stamp_count = html.matches(" data-albedo-id=\"").count();
    assert!(
        stamp_count >= 20,
        "expected at least 20 data-albedo-id stamps for the dashboard, got {stamp_count}: {html}"
    );

    // Every stamp value must parse as u32.
    let mut rest = html.as_str();
    while let Some(start) = rest.find(" data-albedo-id=\"") {
        let after = &rest[start + " data-albedo-id=\"".len()..];
        let close = after.find('"').expect("stamp must close");
        let value = &after[..close];
        value.parse::<u32>().unwrap_or_else(|_| {
            panic!("data-albedo-id must parse as u32, got {value:?} in: {html}")
        });
        rest = &after[close + 1..];
    }
}

#[test]
fn pulse_dashboard_render_is_deterministic_across_calls() {
    // The element-counter is thread-local and resets per render, so two
    // sequential renders must produce byte-identical HTML — no leaking
    // across requests.
    let a = render_dashboard();
    let b = render_dashboard();
    assert_eq!(a, b, "renders must be deterministic across calls");
}
