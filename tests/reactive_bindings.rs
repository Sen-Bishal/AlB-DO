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

// Focused bakabox stand-in: the same node/slot maps and the same
// InitInternTable / SetTextRef / SetAttrRef / SetHtmlRef / BindEvent / SlotSet
// binding contract `assets/albedo-runtime.js` implements, minus the binary
// frame decode and the streaming-lane opcodes binding mode never uses. This is
// the explicit test double for the (retired) production `makeVm`: production
// now runs `installReactiveRuntime` against the real bakabox — proven under
// Node in `tests/bakabox/reactive-unify.test.mjs` — while these QuickJS tests
// keep exercising the Rust payload → driver → handler loop against a
// contract-faithful VM.
globalThis.makeVm = function (doc) {
  var vm = {
    nodes: {},          // stableId -> element
    events: {},         // eventId -> name
    slots: {},          // slotId -> [stableId]
    lists: {},          // slotId -> keyed-list anchor element
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
        (vm.slots[op.slotId] || (vm.slots[op.slotId] = [])).push({ kind: 'text', stableId: op.stableId });
        return;
      case 'SetAttrRef':
        (vm.slots[op.slotId] || (vm.slots[op.slotId] = [])).push({ kind: 'attr', stableId: op.stableId, attr: op.attr });
        return;
      case 'SetHtmlRef':
        (vm.slots[op.slotId] || (vm.slots[op.slotId] = [])).push({ kind: 'html', stableId: op.stableId });
        return;
      case 'SetListRef':
        vm.lists[op.slotId] = vm.nodes[op.stableId];
        return;
      case 'ReconcileList':
        // Faithful-enough reflection for the driver contract: the real keyed
        // reconcile (identity/order) is proven in tests/bakabox/keyed-reconcile.
        // Here we just assert the driver produced the right ordered rows.
        var anchor = vm.lists[op.slotId];
        if (anchor) {
          var joined = '';
          for (var r = 0; r < op.rows.length; r++) { joined += op.rows[r].html; }
          anchor.innerHTML = joined;
        }
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
          var site = sites[s];
          var node = vm.nodes[site.stableId];
          if (!node) continue;
          if (site.kind === 'attr') {
            node.setAttribute(site.attr, text);
          } else if (site.kind === 'html') {
            node.innerHTML = text;
          } else if (node.firstChild && node.firstChild.nodeType === 3) {
            // In-place text patch: mutate the existing text node, mirroring a real
            // DOM `textContent` write that keeps the same node.
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

var vm = globalThis.makeVm(document);
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

var vm = globalThis.makeVm(document);
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

// Conditionals rung: `{open && <p className="panel">…}` gates a STATIC subtree
// on a boolean slot. The compiler renders it as a `display:contents` wrapper
// whose innerHTML the client toggles between the branch HTML and empty when the
// button flips `open`. Exercises the real driver makeVm (the production
// SetHtmlRef → innerHTML path), with zero network.
const CONDITIONAL_SCENARIO: &str = r#"
var payload = globalThis.__PAYLOAD;
var btnId = payload.events[0].stableId;
var cond = payload.derived[0];          // the html:true conditional binding
var wrapId = cond.stableId;

var body = document.createElement('div');
var button = document.createElement('button');
button.setAttribute('data-albedo-id', String(btnId));
button.appendChild(document.createTextNode('toggle'));
var wrapper = document.createElement('span');
wrapper.setAttribute('data-albedo-id', String(wrapId));
wrapper.innerHTML = '';                 // SSR: open=false → nothing inside
body.appendChild(button);
body.appendChild(wrapper);
globalThis.__wrap = wrapper;

var vm = globalThis.makeVm(document);
globalThis.__albedoReactive.installReactiveRuntime({ vm: vm, payload: payload, root: body });

var afterInstall = wrapper.innerHTML;   // "" — install paints the falsy branch
button.__dispatch('click');             // open -> true
var afterOpen = wrapper.innerHTML;      // the <p class="panel"> branch HTML
button.__dispatch('click');             // open -> false
var afterClose = wrapper.innerHTML;     // "" again

JSON.stringify({
  isHtml: cond.html === true,
  afterInstall: afterInstall,
  afterOpen: afterOpen,
  afterClose: afterClose,
  sameNode: body.childNodes[1] === globalThis.__wrap,
  network: globalThis.__net,
});
"#;

#[test]
fn conditional_subtree_toggles_via_innerhtml_with_zero_network() {
    let project = CompiledProject::load_from_dir(hook_fixture("conditional"))
        .expect("conditional fixture compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);

    let payload = project
        .build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots)
        .expect("reactive payload builds");

    // The conditional lowers to exactly one derived binding marked `html`,
    // depending on `open`'s slot, plus the toggle handler. No text/attr binding
    // (the subtree is static), so this is purely the structural rung.
    assert!(
        payload.texts.is_empty(),
        "no bare text reads in this component"
    );
    assert!(payload.attrs.is_empty(), "no attribute slot reads either");
    assert_eq!(payload.derived.len(), 1, "one conditional binding");
    assert!(
        payload.derived[0].html,
        "the conditional binding must be flagged html (innerHTML toggle)"
    );
    assert_eq!(payload.events.len(), 1, "one onClick toggle");
    // The SSR HTML carries the display:contents wrapper so the client has a
    // stable anchor to toggle.
    assert!(
        payload.html.contains("display:contents"),
        "served HTML must wrap the conditional in a display:contents span; got: {}",
        payload.html
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
        ctx.eval::<String, _>(CONDITIONAL_SCENARIO)
            .expect("conditional scenario evaluates")
    });

    let value: Value = serde_json::from_str(&summary).expect("scenario summary is JSON");

    assert_eq!(value["isHtml"], true, "binding marked as html");
    assert_eq!(
        value["afterInstall"], "",
        "falsy branch shows nothing on install"
    );
    // Flipping `open` true recomputes the conditional and swaps in the branch.
    let opened = value["afterOpen"].as_str().unwrap_or_default();
    assert!(
        opened.contains("Now you see me") && opened.contains("panel"),
        "opening must inject the static branch HTML; got: {opened}"
    );
    // Flipping back hides it again — fine-grained, same wrapper node.
    assert_eq!(value["afterClose"], "", "closing removes the subtree again");
    assert_eq!(
        value["sameNode"], true,
        "toggle reuses the same wrapper element"
    );
    assert_eq!(
        value["network"], 0,
        "conditional toggle must not round-trip"
    );
}

#[test]
fn dynamic_conditional_branch_falls_back_to_island() {
    // A slot-reactive conditional whose branch reads state (`{open && <p>{count}</p>}`)
    // is NOT representable fine-grained — the appearing subtree carries its own
    // bindings. The renderer flags a structural fallback and the payload build
    // errors, so `build_reactive_blocks` skips it and the route keeps its
    // correct A3 whole-component island. Proving the deliberate fallback is the
    // safety guarantee: binding mode never ships a stale conditional.
    let project = CompiledProject::load_from_dir(hook_fixture("conditional_dynamic"))
        .expect("conditional_dynamic fixture compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);

    let result =
        project.build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots);
    assert!(
        result.is_err(),
        "a state-reading conditional branch must force the A3 fallback, not ship a binding payload"
    );

    // The fallback flag must be cleared by the time the build returns, so the
    // NEXT (eligible) component on the same thread isn't poisoned into a false
    // fallback.
    let next = CompiledProject::load_from_dir(hook_fixture("conditional"))
        .expect("conditional fixture compiles");
    let store2 = Arc::new(SlotStore::new());
    let slots2 = SessionSlotView::new(SessionId::random(), store2);
    assert!(
        next.build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots2)
            .is_ok(),
        "the structural-fallback flag must not leak into the next render"
    );
}

// Keyed-lists rung. `{items.map(item => <li>{item.label}</li>)}` over a
// `useState` array lowers to one `html: true` derived binding whose recompute
// reads the array slot and joins a per-item HTML template. Clicking "add"
// appends an item locally; the client regenerates the list's innerHTML from
// state, growing the DOM — data-driven structural reactivity, zero network.
const LIST_SCENARIO: &str = r#"
var payload = globalThis.__PAYLOAD;
var btnId = payload.events[0].stableId;
var list = payload.derived[0];          // the html:true list binding
var wrapId = list.stableId;

var body = document.createElement('div');
var button = document.createElement('button');
button.setAttribute('data-albedo-id', String(btnId));
button.appendChild(document.createTextNode('add'));
var ul = document.createElement('ul');
var wrapper = document.createElement('span');
wrapper.setAttribute('data-albedo-id', String(wrapId));
wrapper.innerHTML = '';                 // install paints the SSR list
ul.appendChild(wrapper);
body.appendChild(button);
body.appendChild(ul);
globalThis.__wrap = wrapper;

var vm = globalThis.makeVm(document);
globalThis.__albedoReactive.installReactiveRuntime({ vm: vm, payload: payload, root: body });

var afterInstall = wrapper.innerHTML;   // <li>a</li><li>b</li>
button.__dispatch('click');             // setItems([...items, {label:'c'}])
var afterAdd = wrapper.innerHTML;        // <li>a</li><li>b</li><li>c</li>

JSON.stringify({
  isHtml: list.html === true,
  afterInstall: afterInstall,
  afterAdd: afterAdd,
  sameNode: body.childNodes[1].childNodes[0] === globalThis.__wrap,
  network: globalThis.__net,
});
"#;

#[test]
fn keyed_list_rerenders_innerhtml_with_zero_network() {
    let project =
        CompiledProject::load_from_dir(hook_fixture("list")).expect("list fixture compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);

    let payload = project
        .build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots)
        .expect("reactive payload builds");

    // The list lowers to exactly one derived binding marked `html`, depending on
    // the `items` slot, plus the append handler. The per-item `{item.label}` is
    // inside the list template — NOT a component-level text/derived binding.
    assert!(
        payload.texts.is_empty(),
        "no component-level bare text reads"
    );
    assert!(
        payload.attrs.is_empty(),
        "no component-level attribute reads"
    );
    assert_eq!(payload.derived.len(), 1, "one list binding");
    assert!(
        payload.derived[0].html,
        "the list binding must be flagged html (innerHTML re-render)"
    );
    assert_eq!(payload.events.len(), 1, "one onClick append handler");
    assert!(
        payload.html.contains("display:contents"),
        "served HTML must wrap the list in a display:contents span; got: {}",
        payload.html
    );
    // SSR first paint already renders the two initial items. (The server-side
    // `<li>`s carry `data-albedo-id` stamps from the normal render path; the
    // client template below emits clean markup — the install-paint reconciles
    // them. So match on the item text, not the exact tag.)
    assert!(
        payload.html.contains(">a</li>") && payload.html.contains(">b</li>"),
        "SSR must render the initial list items; got: {}",
        payload.html
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
        ctx.eval::<String, _>(LIST_SCENARIO)
            .expect("list scenario evaluates")
    });

    let value: Value = serde_json::from_str(&summary).expect("scenario summary is JSON");

    assert_eq!(value["isHtml"], true, "binding marked as html");
    // Install paints the two SSR items locally (no network).
    let installed = value["afterInstall"].as_str().unwrap_or_default();
    assert!(
        installed.contains("<li class=\"row\">a</li>")
            && installed.contains("<li class=\"row\">b</li>"),
        "install must paint the initial list; got: {installed}"
    );
    // Appending grows the list — the new item appears, regenerated from state.
    let added = value["afterAdd"].as_str().unwrap_or_default();
    assert!(
        added.contains("<li class=\"row\">c</li>") && added.matches("<li").count() == 3,
        "appending must re-render the list with the new item; got: {added}"
    );
    assert_eq!(
        value["sameNode"], true,
        "re-render reuses the same wrapper node"
    );
    assert_eq!(value["network"], 0, "list mutation must not round-trip");
}

// Keyed-lists rung · delta-sink lane. `{items.map(item => <li key={item.id}>…)}`
// carries an explicit key, so it lowers to a `lists` binding (not a `derived`
// html thunk): the SSR rows are `data-albedo-key`-stamped, the wrapper is a
// `SetListRef` anchor, and appending drives keyed reconciliation (`ReconcileList`).
const KEYED_LIST_SCENARIO: &str = r#"
var payload = globalThis.__PAYLOAD;
var btnId = payload.events[0].stableId;
var lb = payload.lists[0];

var body = document.createElement('div');
var button = document.createElement('button');
button.setAttribute('data-albedo-id', String(btnId));
button.appendChild(document.createTextNode('add'));
var ul = document.createElement('ul');
var wrapper = document.createElement('span');
wrapper.setAttribute('data-albedo-id', String(lb.stableId));
ul.appendChild(wrapper);
body.appendChild(button);
body.appendChild(ul);

var vm = globalThis.makeVm(document);
globalThis.__albedoReactive.installReactiveRuntime({ vm: vm, payload: payload, root: body });

var afterInstall = wrapper.innerHTML;   // reconciled rows for [a,b]
button.__dispatch('click');             // setItems([...items, {id:3,label:'c'}])
var afterAdd = wrapper.innerHTML;        // reconciled rows for [a,b,c]

JSON.stringify({ afterInstall: afterInstall, afterAdd: afterAdd, network: globalThis.__net });
"#;

#[test]
fn keyed_list_with_key_prop_drives_reconcile_sink() {
    let project = CompiledProject::load_from_dir(hook_fixture("list_keyed"))
        .expect("list_keyed fixture compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);

    let payload = project
        .build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots)
        .expect("reactive payload builds");

    // A keyed list lowers to `lists` (delta sink), NOT `derived` (coarse innerHTML).
    assert_eq!(payload.lists.len(), 1, "one keyed list binding");
    assert!(
        payload.derived.is_empty(),
        "keyed list must not also produce a coarse derived binding"
    );
    assert_eq!(payload.events.len(), 1, "one onClick append handler");
    let thunk = &payload.lists[0].rows_thunk;
    assert!(thunk.contains("key:"), "rows thunk must project a key; got: {thunk}");
    assert!(thunk.contains("html:"), "rows thunk must project row html; got: {thunk}");
    // SSR HTML carries the wrapper anchor and per-row keys.
    assert!(
        payload.html.contains("display:contents"),
        "served HTML wraps the list in a display:contents span; got: {}",
        payload.html
    );
    assert!(
        payload.html.contains("data-albedo-key=\"1\"")
            && payload.html.contains("data-albedo-key=\"2\""),
        "SSR rows must be stamped with data-albedo-key; got: {}",
        payload.html
    );

    let payload_json = serde_json::to_string(&payload).expect("payload serializes");
    let runtime = Runtime::new().expect("quickjs runtime");
    let context = Context::full(&runtime).expect("quickjs context");
    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_AND_VM).expect("DOM + VM shim evaluates");
        ctx.eval::<(), _>(REACTIVE_DRIVER).expect("reactive driver evaluates");
        let bootstrap = format!("globalThis.__PAYLOAD = {payload_json};");
        ctx.eval::<(), _>(bootstrap.as_str()).expect("payload injects");
        ctx.eval::<String, _>(KEYED_LIST_SCENARIO).expect("scenario evaluates")
    });

    let value: Value = serde_json::from_str(&summary).expect("scenario summary is JSON");
    let after_install = value["afterInstall"].as_str().unwrap_or_default();
    assert!(
        after_install.contains(">a</li>")
            && after_install.contains(">b</li>")
            && !after_install.contains(">c</li>"),
        "install reconciles rows a,b; got: {after_install}"
    );
    assert!(
        after_install.contains("data-albedo-key=\"1\""),
        "reconciled client rows carry data-albedo-key; got: {after_install}"
    );
    let after_add = value["afterAdd"].as_str().unwrap_or_default();
    assert!(
        after_add.contains(">a</li>")
            && after_add.contains(">b</li>")
            && after_add.contains(">c</li>"),
        "after append, rows a,b,c present; got: {after_add}"
    );
    assert_eq!(value["network"], 0, "keyed list reconcile must not round-trip");
}

#[test]
fn event_reading_handler_falls_back_to_island() {
    // A handler that reads its DOM event argument (`onInput={(e) =>
    // setCount(e.target.value.length)}`) is NOT representable in binding mode:
    // the client thunk wires `__state`/setters/captured props but never the
    // event. `build_reactive_payload` must decline so `build_reactive_blocks`
    // skips it and the component keeps its correct A3 island (where the real
    // closure runs with the native event). This is the safety guarantee that
    // killed MarginNote's silent no-op: binding mode never serve-wires a handler
    // it can't execute.
    let project = CompiledProject::load_from_dir(hook_fixture("event_reading_handler"))
        .expect("event_reading_handler fixture compiles");
    let store = Arc::new(SlotStore::new());
    let slots = SessionSlotView::new(SessionId::random(), store);

    let result =
        project.build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots);
    assert!(
        result.is_err(),
        "an event-reading handler must force the A3 fallback, not ship a binding payload"
    );

    // A parameterless handler on the same thread must still serve-wire — the
    // decline is specific to reading the event, not a blanket poison.
    let next = CompiledProject::load_from_dir(hook_fixture("counter"))
        .expect("counter fixture compiles");
    let slots2 = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
    assert!(
        next.build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots2)
            .is_ok(),
        "a parameterless handler must still be serve-wireable after the decline"
    );
}

#[test]
fn dynamic_list_item_falls_back_to_island() {
    // A list whose per-item subtree carries its own handler (`<li onClick=…>`)
    // is NOT representable as inert innerHTML — regenerating it would drop the
    // per-item listeners. The renderer flags a structural fallback and the
    // payload build errors, so `build_reactive_blocks` skips it and the route
    // keeps its correct A3 whole-component island. This is the safety guarantee:
    // binding mode never ships a list it would silently break.
    let project = CompiledProject::load_from_dir(hook_fixture("list_dynamic"))
        .expect("list_dynamic fixture compiles");
    let store = Arc::new(SlotStore::new());
    let slots = SessionSlotView::new(SessionId::random(), store);

    let result =
        project.build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots);
    assert!(
        result.is_err(),
        "a list item with its own handler must force the A3 fallback, not ship a binding payload"
    );

    // The fallback flag must be cleared by the time the build returns, so the
    // next (eligible) component on the same thread isn't poisoned.
    let next = CompiledProject::load_from_dir(hook_fixture("list")).expect("list fixture compiles");
    let slots2 = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
    assert!(
        next.build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots2)
            .is_ok(),
        "the structural-fallback flag must not leak into the next render"
    );
}
