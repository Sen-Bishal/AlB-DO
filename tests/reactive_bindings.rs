//! Step 3 spike — fine-grained reactive bindings ("stop hydrating components").
//!
//! Proves the whole loop on the simplest real case: a `useState` counter TSX
//! compiles — through the SAME Phase K binding emitter the Tier-B path uses — to
//! static HTML + a `{slot}`/`on*` binding frame + a handler thunk. A browser
//! click then runs that handler LOCALLY (against a plain JS state object) and
//! patches the bound text node in place via the opcode contract, with:
//!
//!   * ZERO network round-trip (the handler never reaches the server), and
//!   * ZERO component hydration (no VDOM, no re-render — only the one text node that reads state is
//!     touched, and it is the SAME server-rendered node).
//!
//! Following the repo's JS-test discipline (`tests/client_hydration.rs`), the
//! driver runs under QuickJS against a compact DOM shim. A focused bakabox VM
//! stub stands in for the full opcode patcher exactly as the DOM shim stands in
//! for a real DOM: it implements the same `SetTextRef` / `BindEvent` / `SlotSet`
//! contract the production `albedo-runtime.js` bakabox does.

use dom_render_compiler::runtime::eval::{CompiledProject, SessionSlotView};
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use rquickjs::{Context, Runtime};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

const REACTIVE_DRIVER: &str = include_str!("../assets/albedo-reactive.js");

fn hook_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hook_compile")
        .join(name)
}

// A minimal DOM (element/text nodes with stable identity, synchronous event
// dispatch, a `fetch` spy to assert "zero round-trip" as a hard invariant) plus
// a focused bakabox VM stub implementing the three opcodes binding mode uses.
const DOM_AND_VM: &str = r#"
globalThis.__net = 0;
globalThis.fetch = function () { globalThis.__net++; return {}; };
globalThis.console = { error: function () {}, debug: function () {} };

function makeText(text) {
  return { nodeType: 3, nodeValue: text, parentNode: null };
}

function makeElement(tag) {
  var node = {
    nodeType: 1,
    tagName: tag.toUpperCase(),
    childNodes: [],
    attributes: {},
    listeners: {},
    parentNode: null,
  };
  node.appendChild = function (child) { child.parentNode = node; node.childNodes.push(child); return child; };
  node.setAttribute = function (k, v) { node.attributes[k] = String(v); };
  node.getAttribute = function (k) {
    return Object.prototype.hasOwnProperty.call(node.attributes, k) ? node.attributes[k] : null;
  };
  node.addEventListener = function (t, fn) { (node.listeners[t] || (node.listeners[t] = [])).push(fn); };
  node.__dispatch = function (t) {
    var l = (node.listeners[t] || []).slice();
    var ev = { type: t, target: node };
    for (var i = 0; i < l.length; i++) { l[i](ev); }
  };
  Object.defineProperty(node, 'firstChild', {
    get: function () { return node.childNodes.length ? node.childNodes[0] : null; },
  });
  return node;
}

globalThis.document = { createElement: makeElement, createTextNode: makeText };

// Focused bakabox stand-in: same node/slot maps and the same SetTextRef /
// BindEvent / SlotSet semantics as `assets/albedo-runtime.js`, minus the binary
// frame decode and the streaming-lane opcodes binding mode never uses.
globalThis.makeVm = function (doc) {
  var vm = {
    nodes: {},          // stableId -> element
    events: {},         // eventId -> name
    slots: {},          // slotId -> [stableId]
    eventDispatcher: function () {},
  };
  vm.seedNodesFromDocument = function (root) {
    var stack = [root];
    while (stack.length) {
      var n = stack.pop();
      if (n && n.nodeType === 1) {
        var raw = n.getAttribute('data-albedo-id');
        if (raw !== null) { vm.nodes[raw] = n; }
        for (var i = n.childNodes.length - 1; i >= 0; i--) { stack.push(n.childNodes[i]); }
      }
    }
  };
  vm.applyInstruction = function (op) {
    switch (op.op) {
      case 'InitInternTable':
        if (op.table.kind === 'Event') {
          for (var i = 0; i < op.table.entries.length; i++) {
            vm.events[op.table.entries[i].id] = op.table.entries[i].value;
          }
        }
        return;
      case 'SetTextRef':
        (vm.slots[op.slotId] || (vm.slots[op.slotId] = [])).push(op.stableId);
        return;
      case 'BindEvent':
        var el = vm.nodes[op.stableId];
        var name = vm.events[op.eventId];
        if (el && name) {
          el.addEventListener(name, function (ev) { vm.eventDispatcher(op.proxyId, ev); });
        }
        return;
      case 'SlotSet':
        var sites = vm.slots[op.slotId] || [];
        var text = (typeof op.value === 'string') ? op.value : String(op.value);
        for (var s = 0; s < sites.length; s++) {
          var node = vm.nodes[sites[s]];
          if (!node) continue;
          // In-place text patch: mutate the existing text node, mirroring a real
          // DOM `textContent` write that keeps the same node.
          if (node.firstChild && node.firstChild.nodeType === 3) {
            node.firstChild.nodeValue = text;
          } else {
            node.textContent = text;
          }
        }
        return;
      default:
        return;
    }
  };
  return vm;
};
"#;

const SCENARIO: &str = r#"
var payload = globalThis.__PAYLOAD;

// Build the DOM the shell would have served: the counter button, stamped with
// the same stable id the binding frame references, holding the SSR text "0".
var ev = payload.events[0];
var body = document.createElement('div');
var button = document.createElement('button');
button.setAttribute('data-albedo-id', String(ev.stableId));
var serverText = document.createTextNode('0');
button.appendChild(serverText);
body.appendChild(button);
globalThis.__serverText = serverText;

var vm = globalThis.makeVm(document);
globalThis.__albedoReactive.installReactiveRuntime({ vm: vm, payload: payload, root: body });

var afterInstall = button.firstChild.nodeValue;   // untouched: "0"
button.__dispatch(ev.event);                       // local click → setN(0 + 1)
var afterClick1 = button.firstChild.nodeValue;     // "1"
button.__dispatch(ev.event);                       // state persists → "2"
var afterClick2 = button.firstChild.nodeValue;

JSON.stringify({
  afterInstall: afterInstall,
  afterClick1: afterClick1,
  afterClick2: afterClick2,
  sameNode: button.firstChild === globalThis.__serverText,
  network: globalThis.__net,
});
"#;

#[test]
fn counter_drives_fine_grained_text_patch_with_zero_network() {
    let project =
        CompiledProject::load_from_dir(hook_fixture("counter")).expect("counter fixture compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);

    let payload = project
        .build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots)
        .expect("reactive payload builds");

    // The payload is exactly the fine-grained surface: one text binding, one
    // event binding, one handler thunk — no whole-component island.
    assert_eq!(payload.texts.len(), 1, "counter has one {{n}} text binding");
    assert_eq!(payload.events.len(), 1, "counter has one onClick");
    assert_eq!(payload.handlers.len(), 1, "counter has one handler thunk");
    assert!(
        payload.handlers[0].1.contains("__emit"),
        "handler thunk must emit slot writes; got: {}",
        payload.handlers[0].1
    );

    let payload_json = serde_json::to_string(&payload).expect("payload serializes");

    let runtime = Runtime::new().expect("quickjs runtime");
    let context = Context::full(&runtime).expect("quickjs context");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_AND_VM)
            .expect("DOM + VM shim evaluates");
        ctx.eval::<(), _>(REACTIVE_DRIVER)
            .expect("reactive driver evaluates");
        let bootstrap = format!("globalThis.__PAYLOAD = {payload_json};");
        ctx.eval::<(), _>(bootstrap.as_str())
            .expect("payload injects");
        ctx.eval::<String, _>(SCENARIO).expect("scenario evaluates")
    });

    let value: Value = serde_json::from_str(&summary).expect("scenario summary is JSON");

    // The shell text is left untouched on install — no re-render, no hydration.
    assert_eq!(value["afterInstall"], "0");
    // The local click ran the handler and patched the bound text node...
    assert_eq!(value["afterClick1"], "1");
    // ...and state persisted across clicks (0→1→2), all in the browser.
    assert_eq!(value["afterClick2"], "2");
    // The SAME server-rendered text node was mutated in place — a fine-grained
    // patch, not a subtree teardown.
    assert_eq!(
        value["sameNode"], true,
        "text node must be patched in place"
    );
    // The whole interaction was local.
    assert_eq!(value["network"], 0, "binding mode must not round-trip");
}

// Attrs rung: a `{slot}` read in an attribute position (`className={cls}`).
// Exercises the driver's REAL built-in `makeVm` (not the test stub) so the
// production SetAttrRef→setAttribute path is what's proven. `className` must
// bind to the HTML `class` attribute the server actually rendered.
const ATTR_SCENARIO: &str = r#"
var payload = globalThis.__PAYLOAD;
var attr = payload.attrs[0];

var body = document.createElement('div');
var button = document.createElement('button');
button.setAttribute('data-albedo-id', String(attr.stableId));
button.setAttribute('class', 'off');           // server-rendered initial
button.appendChild(document.createTextNode('toggle'));
body.appendChild(button);
globalThis.__btn = button;

var vm = globalThis.__albedoReactive.makeVm(document);
globalThis.__albedoReactive.installReactiveRuntime({ vm: vm, payload: payload, root: body });

var afterInstall = button.getAttribute('class');   // untouched: "off"
button.__dispatch(payload.events[0].event);        // setCls("on")
var afterClick = button.getAttribute('class');     // "on"

JSON.stringify({
  attrName: attr.attr,
  afterInstall: afterInstall,
  afterClick: afterClick,
  sameNode: body.firstChild === globalThis.__btn,
  network: globalThis.__net,
});
"#;

#[test]
fn class_name_slot_drives_fine_grained_attr_patch() {
    let project = CompiledProject::load_from_dir(hook_fixture("attr_toggle"))
        .expect("attr_toggle fixture compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);

    let payload = project
        .build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots)
        .expect("reactive payload builds");

    // `className={cls}` produced one attribute binding, named with the HTML
    // attribute (`class`), plus the click handler.
    assert_eq!(payload.attrs.len(), 1, "one className binding expected");
    assert_eq!(
        payload.attrs[0].attr, "class",
        "className must bind to HTML `class`"
    );
    assert_eq!(payload.events.len(), 1, "one onClick expected");

    let payload_json = serde_json::to_string(&payload).expect("payload serializes");

    let runtime = Runtime::new().expect("quickjs runtime");
    let context = Context::full(&runtime).expect("quickjs context");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_AND_VM)
            .expect("DOM + VM shim evaluates");
        ctx.eval::<(), _>(REACTIVE_DRIVER)
            .expect("reactive driver evaluates");
        let bootstrap = format!("globalThis.__PAYLOAD = {payload_json};");
        ctx.eval::<(), _>(bootstrap.as_str())
            .expect("payload injects");
        ctx.eval::<String, _>(ATTR_SCENARIO)
            .expect("attr scenario evaluates")
    });

    let value: Value = serde_json::from_str(&summary).expect("scenario summary is JSON");

    assert_eq!(value["attrName"], "class");
    assert_eq!(
        value["afterInstall"], "off",
        "install must not touch the attribute"
    );
    assert_eq!(
        value["afterClick"], "on",
        "click must patch the bound attribute locally"
    );
    assert_eq!(
        value["sameNode"], true,
        "attribute patched on the same element"
    );
    assert_eq!(value["network"], 0, "attr binding mode must not round-trip");
}

// Derived rung: a `{n * 2}` expression alongside a bare `{n}` read. The bare
// read patches via the slot directly; the derived expression is recomputed
// client-side from state whenever `n` changes. Exercises the real driver makeVm.
const DERIVED_SCENARIO: &str = r#"
var payload = globalThis.__PAYLOAD;
var btnId = payload.events[0].stableId;
var rawId = payload.texts[0].stableId;
var dblId = payload.derived[0].stableId;

var body = document.createElement('div');
var button = document.createElement('button');
button.setAttribute('data-albedo-id', String(btnId));
var rawSpan = document.createElement('span');
rawSpan.setAttribute('data-albedo-id', String(rawId));
rawSpan.appendChild(document.createTextNode('1'));     // SSR: n = 1
var dblSpan = document.createElement('span');
dblSpan.setAttribute('data-albedo-id', String(dblId));
dblSpan.appendChild(document.createTextNode('2'));     // SSR: n * 2 = 2
body.appendChild(button);
body.appendChild(rawSpan);
body.appendChild(dblSpan);

var vm = globalThis.__albedoReactive.makeVm(document);
globalThis.__albedoReactive.installReactiveRuntime({ vm: vm, payload: payload, root: body });

var before = { raw: rawSpan.firstChild.nodeValue, dbl: dblSpan.firstChild.nodeValue };
button.__dispatch('click');                            // setN(1 + 1) = 2
var after1 = { raw: rawSpan.firstChild.nodeValue, dbl: dblSpan.firstChild.nodeValue };
button.__dispatch('click');                            // setN(2 + 1) = 3
var after2 = { raw: rawSpan.firstChild.nodeValue, dbl: dblSpan.firstChild.nodeValue };

JSON.stringify({
  before: before,
  after1: after1,
  after2: after2,
  network: globalThis.__net,
});
"#;

#[test]
fn derived_expression_recomputes_from_state_on_change() {
    let project =
        CompiledProject::load_from_dir(hook_fixture("derived")).expect("derived fixture compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);

    let payload = project
        .build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots)
        .expect("reactive payload builds");

    // One bare text read (`{n}`), one derived expression (`{n * 2}`) that depends
    // on the same slot, and one handler.
    assert_eq!(payload.texts.len(), 1, "one bare {{n}} read");
    assert_eq!(payload.derived.len(), 1, "one derived {{n * 2}} expression");
    assert_eq!(
        payload.derived[0].dep_slots.len(),
        1,
        "derived depends on n's slot"
    );
    assert_eq!(
        payload.derived[0].dep_slots[0], payload.texts[0].slot_id,
        "derived must depend on the same slot the bare read binds"
    );
    assert!(
        payload.derived[0].attr.is_none(),
        "this derived binding is a text node"
    );

    let payload_json = serde_json::to_string(&payload).expect("payload serializes");

    let runtime = Runtime::new().expect("quickjs runtime");
    let context = Context::full(&runtime).expect("quickjs context");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_AND_VM)
            .expect("DOM + VM shim evaluates");
        ctx.eval::<(), _>(REACTIVE_DRIVER)
            .expect("reactive driver evaluates");
        let bootstrap = format!("globalThis.__PAYLOAD = {payload_json};");
        ctx.eval::<(), _>(bootstrap.as_str())
            .expect("payload injects");
        ctx.eval::<String, _>(DERIVED_SCENARIO)
            .expect("derived scenario evaluates")
    });

    let value: Value = serde_json::from_str(&summary).expect("scenario summary is JSON");

    // Install leaves the SSR values untouched.
    assert_eq!(value["before"]["raw"], "1");
    assert_eq!(value["before"]["dbl"], "2");
    // First click: n=2 → bare read "2", derived n*2 = "4".
    assert_eq!(value["after1"]["raw"], "2");
    assert_eq!(
        value["after1"]["dbl"], "4",
        "derived must recompute n * 2 locally"
    );
    // Second click: n=3 → "3" and "6".
    assert_eq!(value["after2"]["raw"], "3");
    assert_eq!(value["after2"]["dbl"], "6");
    assert_eq!(value["network"], 0, "derived recompute must not round-trip");
}

#[test]
fn usememo_local_resolves_to_a_derived_binding() {
    // `{doubled}` references a `const doubled = useMemo(() => n * 2, [n])`. The
    // analysis must substitute the memo body so the binding depends on n's slot
    // and recomputes `n * 2` client-side — identical behaviour to inline `{n*2}`.
    let project =
        CompiledProject::load_from_dir(hook_fixture("usememo")).expect("usememo fixture compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);

    let payload = project
        .build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots)
        .expect("reactive payload builds");

    assert_eq!(payload.texts.len(), 1, "one bare {{n}} read");
    assert_eq!(
        payload.derived.len(),
        1,
        "the {{doubled}} useMemo local resolves to a derived binding"
    );
    assert_eq!(
        payload.derived[0].dep_slots,
        vec![payload.texts[0].slot_id],
        "the resolved memo body must depend on n's slot"
    );
    assert!(
        payload.derived[0].thunk.contains("* 2"),
        "the thunk must carry the substituted memo body (n * 2); got: {}",
        payload.derived[0].thunk
    );

    let payload_json = serde_json::to_string(&payload).expect("payload serializes");

    let runtime = Runtime::new().expect("quickjs runtime");
    let context = Context::full(&runtime).expect("quickjs context");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_AND_VM)
            .expect("DOM + VM shim evaluates");
        ctx.eval::<(), _>(REACTIVE_DRIVER)
            .expect("reactive driver evaluates");
        let bootstrap = format!("globalThis.__PAYLOAD = {payload_json};");
        ctx.eval::<(), _>(bootstrap.as_str())
            .expect("payload injects");
        ctx.eval::<String, _>(DERIVED_SCENARIO)
            .expect("derived scenario evaluates")
    });

    let value: Value = serde_json::from_str(&summary).expect("scenario summary is JSON");

    assert_eq!(value["before"]["dbl"], "2");
    assert_eq!(
        value["after1"]["dbl"], "4",
        "useMemo local must recompute n * 2 locally"
    );
    assert_eq!(value["after2"]["dbl"], "6");
    assert_eq!(value["network"], 0);
}
