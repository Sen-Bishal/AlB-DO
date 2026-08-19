// A3 slice 1 — proof that the Tier-C client runtime (`assets/albedo-client.js`)
// hydrates server-rendered markup and runs `useState`/`useEffect` locally with
// zero network round-trip.
//
// Following the repo's established JS-test discipline (see
// `tests/hydration_integration_tests.rs`), the runtime is driven under QuickJS
// against a compact DOM shim. The shim's nodes carry stable identity so the
// test can assert the server `<button>` is ADOPTED (same object) on hydrate and
// PATCHED IN PLACE (same object) on update — not recreated — which is the whole
// point of hydration.

use dom_render_compiler::runtime::quickjs_engine::compile_client_island_module;
use rquickjs::{Context, Runtime};

const CLIENT_RUNTIME: &str = include_str!("../assets/albedo-client.js");

// Minimal DOM the client runtime exercises: element/text nodes with stable
// identity, child lists, attributes, and synchronous event dispatch. A
// synchronous `queueMicrotask` makes the scheduler deterministic, and a `fetch`
// spy lets the test assert "zero round-trip" as a hard invariant rather than a
// vibe.
const DOM_SHIM: &str = r#"
globalThis.__net = 0;
globalThis.fetch = function () { globalThis.__net++; return {}; };
globalThis.queueMicrotask = function (fn) { fn(); };
globalThis.console = { error: function () {} };
globalThis.__effectLog = [];

// `nextSibling`, live off the parent's `childNodes` rather than cached, so it
// stays correct across appendChild/insertBefore/removeChild/replaceChild with
// no bookkeeping at each call site — same as a real DOM node. Multi-child
// Fragment hydration (`hydrateChildren` in albedo-client.js) walks this to
// find where one vnode's DOM slice ends and the next begins.
function withSiblingLink(node) {
  Object.defineProperty(node, 'nextSibling', {
    get: function () {
      if (!node.parentNode) { return null; }
      var siblings = node.parentNode.childNodes;
      var i = siblings.indexOf(node);
      return i >= 0 && i + 1 < siblings.length ? siblings[i + 1] : null;
    },
  });
  return node;
}

function makeText(text) {
  return withSiblingLink({ nodeType: 3, nodeValue: text, parentNode: null });
}

function makeElement(tag) {
  var node = {
    nodeType: 1,
    tagName: tag.toUpperCase(),
    nodeName: tag.toUpperCase(),
    childNodes: [],
    attributes: {},
    listeners: {},
    parentNode: null,
  };
  node.appendChild = function (child) {
    child.parentNode = node;
    node.childNodes.push(child);
    return child;
  };
  node.removeChild = function (child) {
    var i = node.childNodes.indexOf(child);
    if (i >= 0) { node.childNodes.splice(i, 1); }
    child.parentNode = null;
    return child;
  };
  node.replaceChild = function (newChild, oldChild) {
    var i = node.childNodes.indexOf(oldChild);
    if (i >= 0) { node.childNodes[i] = newChild; newChild.parentNode = node; oldChild.parentNode = null; }
    return oldChild;
  };
  node.insertBefore = function (newChild, ref) {
    var i = node.childNodes.indexOf(ref);
    if (i < 0) { i = node.childNodes.length; }
    node.childNodes.splice(i, 0, newChild);
    newChild.parentNode = node;
    return newChild;
  };
  node.setAttribute = function (k, v) { node.attributes[k] = String(v); };
  node.removeAttribute = function (k) { delete node.attributes[k]; };
  node.getAttribute = function (k) {
    return Object.prototype.hasOwnProperty.call(node.attributes, k) ? node.attributes[k] : null;
  };
  node.addEventListener = function (t, fn) { (node.listeners[t] || (node.listeners[t] = [])).push(fn); };
  node.removeEventListener = function (t, fn) {
    var l = node.listeners[t];
    if (l) { var i = l.indexOf(fn); if (i >= 0) { l.splice(i, 1); } }
  };
  node.__dispatch = function (t) {
    var l = (node.listeners[t] || []).slice();
    var ev = { type: t, target: node };
    for (var i = 0; i < l.length; i++) { l[i](ev); }
  };
  Object.defineProperty(node, 'firstChild', {
    get: function () { return node.childNodes.length ? node.childNodes[0] : null; },
  });
  return withSiblingLink(node);
}

globalThis.document = { createElement: makeElement, createTextNode: makeText };

// The hydration bootstrap (src/hydration/script.rs) locates each island root by
// `document.querySelector('[data-albedo-island="ID"]')`; the shim implements
// just that selector against a registered document root.
globalThis.__domRoot = null;
globalThis.document.querySelector = function (sel) {
  var m = /^\[data-albedo-island="(.+)"\]$/.exec(sel);
  if (!m || !globalThis.__domRoot) { return null; }
  var want = m[1];
  var stack = [globalThis.__domRoot];
  while (stack.length) {
    var n = stack.pop();
    if (n.nodeType === 1) {
      if (n.getAttribute('data-albedo-island') === want) { return n; }
      for (var i = n.childNodes.length - 1; i >= 0; i--) { stack.push(n.childNodes[i]); }
    }
  }
  return null;
};
"#;

// The component, authored exactly as the JSX pragma transpile would emit it:
// `h`, `useState`, `useEffect` referenced as globals the client runtime installs.
// Server markup is built by hand to stand in for what the SSR `h` produced:
// `<button>count: 0</button>`.
const SCENARIO: &str = r#"
function Counter(props) {
  var s = useState(props.start || 0);
  var n = s[0], set = s[1];
  useEffect(function () { globalThis.__effectLog.push(n); return function () {}; }, [n]);
  return h('button', { onClick: function () { set(n + 1); } }, 'count: ' + n);
}

var container = document.createElement('div');
var button = document.createElement('button');
button.appendChild(document.createTextNode('count: 0'));
container.appendChild(button);

globalThis.__serverButton = button;

__albedoClient.hydrate(h(Counter, { start: 0 }), container);

var afterHydrateText = button.firstChild.nodeValue;
var adoptedOnHydrate = container.firstChild === globalThis.__serverButton;
var effectsAfterHydrate = globalThis.__effectLog.length;

button.__dispatch('click');

var afterClickText = button.firstChild.nodeValue;
var sameNodeAfterClick = container.firstChild === globalThis.__serverButton;
var effectsAfterClick = globalThis.__effectLog.length;

JSON.stringify({
  afterHydrateText: afterHydrateText,
  adoptedOnHydrate: adoptedOnHydrate,
  effectsAfterHydrate: effectsAfterHydrate,
  afterClickText: afterClickText,
  sameNodeAfterClick: sameNodeAfterClick,
  effectsAfterClick: effectsAfterClick,
  effectLog: globalThis.__effectLog,
  network: globalThis.__net,
});
"#;

#[test]
fn client_runtime_hydrates_and_updates_counter_with_zero_network() {
    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME).expect("client runtime should evaluate");
        ctx.eval::<String, _>(SCENARIO).expect("scenario should evaluate")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("scenario summary should be JSON");

    // Hydration adopts the server node and leaves the painted text untouched.
    assert_eq!(value["afterHydrateText"], "count: 0");
    assert_eq!(value["adoptedOnHydrate"], true, "server <button> must be adopted, not recreated");

    // useEffect ran client-side exactly once on mount.
    assert_eq!(value["effectsAfterHydrate"], 1);

    // The local click drove state → re-render → in-place text patch, with the
    // same DOM node preserved (a diff, not a teardown).
    assert_eq!(value["afterClickText"], "count: 1");
    assert_eq!(value["sameNodeAfterClick"], true, "update must patch in place, not replace the node");

    // useEffect re-ran because its dependency [n] changed.
    assert_eq!(value["effectsAfterClick"], 2);
    assert_eq!(value["effectLog"], serde_json::json!([0, 1]));

    // The whole interaction was local — nothing touched the network.
    assert_eq!(value["network"], 0, "Tier-C interaction must not round-trip to the server");
}

// The production entry the ≤2KB bootstrap actually calls: a registered
// component, an island root located by `data-albedo-island`, hydrated through
// `__ALBEDO_HYDRATE_ISLAND(descriptor)`.
const ISLAND_SCENARIO: &str = r#"
function Panel(props) {
  var s = useState(0);
  var n = s[0], set = s[1];
  return h('button', { 'data-albedo-island': '7', onClick: function () { set(n + 1); } }, 'hits: ' + n);
}
__albedoClient.registerComponent('7', Panel);

var body = document.createElement('div');
var panel = document.createElement('button');
panel.setAttribute('data-albedo-island', '7');
panel.appendChild(document.createTextNode('hits: 0'));
body.appendChild(panel);

globalThis.__domRoot = body;
globalThis.__serverPanel = panel;

__ALBEDO_HYDRATE_ISLAND({ component_id: 7, module_path: 'components/panel', props: {} });

var hydratedAttr = panel.getAttribute('data-albedo-hydrated');

panel.__dispatch('click');

// A second descriptor call must be a no-op once the root is marked hydrated.
__ALBEDO_HYDRATE_ISLAND({ component_id: 7, module_path: 'components/panel', props: {} });

JSON.stringify({
  hydratedAttr: hydratedAttr,
  afterClickText: panel.firstChild.nodeValue,
  sameNode: globalThis.__domRoot.firstChild === globalThis.__serverPanel,
  network: globalThis.__net,
});
"#;

#[test]
fn hydrate_island_descriptor_drives_the_bootstrap_facing_entry() {
    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME).expect("client runtime should evaluate");
        ctx.eval::<String, _>(ISLAND_SCENARIO).expect("island scenario should evaluate")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("island summary should be JSON");

    // The descriptor entry marks the root hydrated, then a local click updates
    // state on the adopted node — no network, idempotent on a second call.
    assert_eq!(value["hydratedAttr"], "true");
    assert_eq!(value["afterClickText"], "hits: 1");
    assert_eq!(value["sameNode"], true, "island root must be patched in place");
    assert_eq!(value["network"], 0);
}

// A3.2 — a REAL TSX island, transpiled by our own pipeline
// (`compile_client_island_module`), ships to the browser, self-registers, and
// hydrates through the descriptor with server-seeded props. This is the
// transpile→ship→hydrate path end-to-end, minus the HTTP shell.
const COUNTER_TSX: &str = r#"
import { useState } from "react";

export default function Counter(props) {
  const [n, setN] = useState(props.start || 0);
  return <button data-albedo-island="42" onClick={() => setN(n + 1)}>{"count: " + n}</button>;
}
"#;

const TSX_ISLAND_SCENARIO: &str = r#"
var body = document.createElement('div');
var panel = document.createElement('button');
panel.setAttribute('data-albedo-island', '42');
panel.appendChild(document.createTextNode('count: 5'));
body.appendChild(panel);

globalThis.__domRoot = body;
globalThis.__serverPanel = panel;

__ALBEDO_HYDRATE_ISLAND({ component_id: 42, module_path: 'components/counter', props: { start: 5 } });

var afterHydrate = panel.firstChild.nodeValue;
panel.__dispatch('click');

JSON.stringify({
  afterHydrate: afterHydrate,
  afterClick: panel.firstChild.nodeValue,
  sameNode: body.firstChild === globalThis.__serverPanel,
  network: globalThis.__net,
});
"#;

#[test]
fn transpiled_tsx_island_ships_and_hydrates_with_seeded_props() {
    let island_script = compile_client_island_module("components/counter", COUNTER_TSX, 42)
        .expect("counter island should compile to a browser module");

    // The browser module must be self-contained: no server-only module helpers,
    // no leftover ESM syntax — just a self-registering IIFE.
    assert!(island_script.contains("registerComponent(\"42\""));
    assert!(!island_script.contains("__albedo_import"), "no server import helpers in client JS");
    assert!(!island_script.contains("import "), "no ESM import syntax in client JS");

    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME).expect("client runtime should evaluate");
        ctx.eval::<(), _>(island_script.as_str()).expect("transpiled island should evaluate");
        ctx.eval::<String, _>(TSX_ISLAND_SCENARIO).expect("scenario should evaluate")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("summary should be JSON");

    // Server-seeded props (start: 5) drove the hydrated state; the local click
    // advanced it, in place, with no network.
    assert_eq!(value["afterHydrate"], "count: 5");
    assert_eq!(value["afterClick"], "count: 6");
    assert_eq!(value["sameNode"], true);
    assert_eq!(value["network"], 0);
}

// B (Gate 2) — the rest of the React hook family runs client-side under the same
// fiber/hook-slot discipline as `useState`/`useEffect`:
//   • `useRef`   — a stable mutable cell that survives re-renders (proven by a
//                  render counter that climbs instead of resetting to 1).
//   • `useMemo`  — recomputes ONLY when its deps change (a factory-call counter
//                  stays flat across an unrelated state update, ticks on a
//                  relevant one).
//   • `useCallback` — keeps a referentially-stable function while deps are equal,
//                  returns a fresh one when they change.
const HOOK_FAMILY_SCENARIO: &str = r#"
globalThis.__memoCalls = 0;
globalThis.__cbs = [];

function Widget(props) {
  var a = useState(0);
  var n = a[0], setN = a[1];
  var b = useState('x');
  var label = b[0], setLabel = b[1];

  // useRef: one cell, mutated every render. If useRef handed back a fresh
  // {current: 0} each time, this would never exceed 1.
  var renderCount = useRef(0);
  renderCount.current = renderCount.current + 1;

  // useMemo: depends only on n. The factory must not fire when only `label`
  // changes.
  var doubled = useMemo(function () {
    globalThis.__memoCalls = globalThis.__memoCalls + 1;
    return n * 2;
  }, [n]);

  // useCallback: depends only on n. Identity must hold across an n-independent
  // re-render, break when n changes.
  var cb = useCallback(function () { return n; }, [n]);
  globalThis.__cbs.push(cb);

  globalThis.__setN = setN;
  globalThis.__setLabel = setLabel;
  globalThis.__renderRef = renderCount;

  return h('button', {}, 'n:' + n + ' x2:' + doubled + ' L:' + label);
}

var container = document.createElement('div');
var button = document.createElement('button');
button.appendChild(document.createTextNode('n:0 x2:0 L:x'));
container.appendChild(button);

__albedoClient.hydrate(h(Widget, {}), container);

var afterHydrate = {
  text: button.firstChild.nodeValue,
  memoCalls: globalThis.__memoCalls,
  renders: globalThis.__renderRef.current,
  cbCount: globalThis.__cbs.length,
};

// Update an UNRELATED state (label). n is unchanged → useMemo must NOT recompute
// and useCallback must keep the same function identity.
globalThis.__setLabel('y');
var afterLabel = {
  text: button.firstChild.nodeValue,
  memoCalls: globalThis.__memoCalls,
  renders: globalThis.__renderRef.current,
  cbStable: globalThis.__cbs[globalThis.__cbs.length - 1] === globalThis.__cbs[globalThis.__cbs.length - 2],
};

// Update n → useMemo recomputes, useCallback returns a fresh function.
globalThis.__setN(5);
var afterN = {
  text: button.firstChild.nodeValue,
  memoCalls: globalThis.__memoCalls,
  renders: globalThis.__renderRef.current,
  cbChanged: globalThis.__cbs[globalThis.__cbs.length - 1] !== globalThis.__cbs[globalThis.__cbs.length - 2],
};

JSON.stringify({
  afterHydrate: afterHydrate,
  afterLabel: afterLabel,
  afterN: afterN,
  network: globalThis.__net,
});
"#;

#[test]
fn client_runtime_runs_useref_usememo_usecallback() {
    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME).expect("client runtime should evaluate");
        ctx.eval::<String, _>(HOOK_FAMILY_SCENARIO).expect("scenario should evaluate")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("scenario summary should be JSON");

    // Mount: one render, memo computed once, one callback captured.
    assert_eq!(value["afterHydrate"]["text"], "n:0 x2:0 L:x");
    assert_eq!(value["afterHydrate"]["memoCalls"], 1);
    assert_eq!(value["afterHydrate"]["renders"], 1);
    assert_eq!(value["afterHydrate"]["cbCount"], 1);

    // Unrelated update: ref cell persisted (renders climbs to 2), memo did NOT
    // recompute (still 1), callback identity held.
    assert_eq!(value["afterLabel"]["text"], "n:0 x2:0 L:y");
    assert_eq!(value["afterLabel"]["memoCalls"], 1, "useMemo must not recompute when its deps are unchanged");
    assert_eq!(value["afterLabel"]["renders"], 2, "useRef cell must survive re-render");
    assert_eq!(value["afterLabel"]["cbStable"], true, "useCallback must keep identity when deps are equal");

    // Relevant update: memo recomputed (2), callback identity broke, value patched.
    assert_eq!(value["afterN"]["text"], "n:5 x2:10 L:y");
    assert_eq!(value["afterN"]["memoCalls"], 2, "useMemo must recompute when deps change");
    assert_eq!(value["afterN"]["renders"], 3);
    assert_eq!(value["afterN"]["cbChanged"], true, "useCallback must return a fresh function when deps change");

    // The whole sequence was local — no round-trip.
    assert_eq!(value["network"], 0);
}

// B (Gate 2) — `useContext` resolves the nearest Provider's value on the client,
// the last hook in the React family. Three invariants the slice must hold:
//   1. a consumer reads the Provider `value`, NOT the createContext default
//      (default "light" vs provider "dark");
//   2. a consumer re-rendering on its OWN state still resolves context (proves
//      the per-fiber context snapshot, not a transient render-time stack);
//   3. changing the Provider value (held in an ancestor's state) propagates to
//      every consumer below it.
const CONTEXT_SCENARIO: &str = r#"
var ThemeContext = createContext('light');

function ThemeLabel(props) {
  var theme = useContext(ThemeContext);
  return h('span', {}, theme);
}

function Toggle(props) {
  // A consumer with its OWN local state. Re-rendering on this state must keep
  // resolving the context value through the fiber's snapshot.
  var s = useState(0);
  var n = s[0], set = s[1];
  var theme = useContext(ThemeContext);
  globalThis.__bump = function () { set(n + 1); };
  return h('button', {}, theme + ':' + n);
}

function App(props) {
  var s = useState('dark');
  var theme = s[0], setTheme = s[1];
  globalThis.__setTheme = setTheme;
  return h(ThemeContext.Provider, { value: theme },
    h('div', {}, h(ThemeLabel, {}), h(Toggle, {})));
}

var container = document.createElement('div');
var outer = document.createElement('div');
var span = document.createElement('span');
span.appendChild(document.createTextNode('dark'));
var button = document.createElement('button');
button.appendChild(document.createTextNode('dark:0'));
outer.appendChild(span);
outer.appendChild(button);
container.appendChild(outer);

__albedoClient.hydrate(h(App, {}), container);

var afterHydrate = { label: span.firstChild.nodeValue, button: button.firstChild.nodeValue };

// (2) Consumer's own state advances — context value must be retained.
globalThis.__bump();
var afterBump = { label: span.firstChild.nodeValue, button: button.firstChild.nodeValue };

// (3) Provider value changes via the ancestor's state — propagates to both
// consumers; Toggle keeps its own n (now 1).
globalThis.__setTheme('light');
var afterTheme = { label: span.firstChild.nodeValue, button: button.firstChild.nodeValue };

JSON.stringify({
  afterHydrate: afterHydrate,
  afterBump: afterBump,
  afterTheme: afterTheme,
  network: globalThis.__net,
});
"#;

#[test]
fn client_runtime_resolves_usecontext_through_provider() {
    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME).expect("client runtime should evaluate");
        ctx.eval::<String, _>(CONTEXT_SCENARIO).expect("scenario should evaluate")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("scenario summary should be JSON");

    // (1) Both consumers read the Provider value ("dark"), not the default.
    assert_eq!(value["afterHydrate"]["label"], "dark");
    assert_eq!(value["afterHydrate"]["button"], "dark:0");

    // (2) Toggle's local state advanced; context value held across the partial
    // re-render; the label (untouched) stayed put.
    assert_eq!(value["afterBump"]["button"], "dark:1");
    assert_eq!(value["afterBump"]["label"], "dark");

    // (3) Provider value change propagated to both consumers; Toggle kept n=1.
    assert_eq!(value["afterTheme"]["label"], "light");
    assert_eq!(value["afterTheme"]["button"], "light:1");

    // Pure client-side — no round-trip.
    assert_eq!(value["network"], 0);
}

// Regression: `h()` must fold positional children back into `props.children`
// for a component vnode — a component is invoked, not walked, and reads its
// children the React way. Three call shapes converge on `h`, and all three
// must agree:
//   * classic single child     — `h(Component, props, child)`
//   * classic multiple children — `h(Component, props, child, child)`
//   * the automatic JSX runtime's shape — `children` pre-folded into the
//     config object by the compiler, then pulled back out and handed to `h`
//     positionally by `react_host.rs`'s `__albedo_jsx` (simulated here rather
//     than pulled in, since that prelude is plain JS text generated by the
//     Rust side, not something this test links against).
const CHILDREN_SCENARIO: &str = r#"
function Label(props) {
  return h('span', null, props.children);
}

function jsxLike(type, config) {
  var props = {};
  var children;
  for (var k in config) {
    if (k === 'children') { children = config[k]; } else { props[k] = config[k]; }
  }
  if (Array.isArray(children)) { return h.apply(null, [type, props].concat(children)); }
  return h(type, props, children);
}

var single = document.createElement('div');
__albedoClient.hydrate(h(Label, null, 'solo'), single);

var multi = document.createElement('div');
__albedoClient.hydrate(h(Label, null, 'a', 'b'), multi);

var viaJsx = document.createElement('div');
__albedoClient.hydrate(jsxLike(Label, { children: 'jsx' }), viaJsx);

var none = document.createElement('div');
__albedoClient.hydrate(h(Label, null), none);

JSON.stringify({
  singleTag: single.firstChild.tagName,
  singleText: single.firstChild.firstChild.nodeValue,
  multiTexts: [multi.firstChild.childNodes[0].nodeValue, multi.firstChild.childNodes[1].nodeValue],
  jsxText: viaJsx.firstChild.firstChild.nodeValue,
  noneHasChild: none.firstChild.firstChild !== null,
});
"#;

#[test]
fn h_folds_positional_children_into_props_for_component_vnodes() {
    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME).expect("client runtime should evaluate");
        ctx.eval::<String, _>(CHILDREN_SCENARIO).expect("scenario should evaluate")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("scenario summary should be JSON");

    assert_eq!(value["singleTag"], "SPAN");
    assert_eq!(value["singleText"], "solo", "a single positional child must reach props.children");
    assert_eq!(
        value["multiTexts"],
        serde_json::json!(["a", "b"]),
        "multiple positional children must reach props.children as an array"
    );
    assert_eq!(
        value["jsxText"], "jsx",
        "children folded back out of the automatic-runtime config object must still reach props.children"
    );
    assert_eq!(
        value["noneHasChild"], false,
        "a childless component must not fabricate a props.children"
    );
}

// Regression: a top-level component whose render is a multi-child Fragment —
// exactly `scaffold/src/components/Counter.tsx` (`<>` wrapping four
// siblings) — must hydrate. `inject_island_marker`
// (`src/runtime/renderer/manifest.rs`) stamps `data-albedo-island` onto the
// FIRST tag of the island's own SSR string, because a Fragment's output has
// no wrapper element; the other siblings are real DOM siblings of that first
// tag, not its children. This scenario reproduces that shape exactly:
// `root` (what `hydrateIsland` is handed) is the first of four sibling
// elements, not a container.
const MULTI_CHILD_FRAGMENT_ISLAND_SCENARIO: &str = r#"
function Panel(props) {
  var s = useState(0);
  var n = s[0], set = s[1];
  return h(
    Fragment,
    null,
    h('p', null, 'eyebrow'),
    h('h2', null, 'title'),
    h('button', { onClick: function () { set(n + 1); } }, 'press'),
    h('span', null, 'tally: ' + n)
  );
}

var container = document.createElement('div');
var p = document.createElement('p');
p.appendChild(document.createTextNode('eyebrow'));
var h2 = document.createElement('h2');
h2.appendChild(document.createTextNode('title'));
var button = document.createElement('button');
button.appendChild(document.createTextNode('press'));
var span = document.createElement('span');
span.appendChild(document.createTextNode('tally: 0'));
container.appendChild(p);
container.appendChild(h2);
container.appendChild(button);
container.appendChild(span);

// `root` is `p` — the first sibling, exactly what the marker injector stamps.
__albedoClient.hydrateIsland(h(Panel, {}), p);

var adoptedBeforeClick = [
  container.childNodes[0] === p,
  container.childNodes[1] === h2,
  container.childNodes[2] === button,
  container.childNodes[3] === span,
];
var tallyBeforeClick = span.firstChild.nodeValue;

button.__dispatch('click');

JSON.stringify({
  childCount: container.childNodes.length,
  adoptedBeforeClick: adoptedBeforeClick,
  tallyBeforeClick: tallyBeforeClick,
  tallyAfterClick: span.firstChild.nodeValue,
  sameNodesAfterClick: container.childNodes[0] === p && container.childNodes[3] === span,
});
"#;

#[test]
fn multi_child_fragment_island_hydrates_and_updates() {
    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME).expect("client runtime should evaluate");
        ctx.eval::<String, _>(MULTI_CHILD_FRAGMENT_ISLAND_SCENARIO)
            .expect("scenario should evaluate")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("scenario summary should be JSON");

    assert_eq!(value["childCount"], 4, "all four Fragment siblings must be present");
    assert_eq!(
        value["adoptedBeforeClick"],
        serde_json::json!([true, true, true, true]),
        "every sibling must be ADOPTED from the server markup, not recreated"
    );
    assert_eq!(value["tallyBeforeClick"], "tally: 0");
    assert_eq!(value["tallyAfterClick"], "tally: 1", "state update must patch the fourth sibling");
    assert_eq!(
        value["sameNodesAfterClick"], true,
        "the update must patch in place, not tear down and remount the group"
    );
}

// Regression: the Fragment's own child COUNT changing (not just content)
// across a re-render — tail growth and tail shrink through
// `reconcileChildList`, exercised via a real click, not a hand call into
// internals.
const FRAGMENT_GROW_SHRINK_SCENARIO: &str = r#"
function GrowShrink(props) {
  var s = useState(2);
  var n = s[0], set = s[1];
  var kids = [h('button', { onClick: function () { set(n === 2 ? 4 : 2); } }, 'toggle')];
  for (var i = 0; i < n; i++) {
    kids.push(h('li', null, 'item' + i));
  }
  return h.apply(null, [Fragment, null].concat(kids));
}

var container = document.createElement('div');
var button = document.createElement('button');
button.appendChild(document.createTextNode('toggle'));
var li0 = document.createElement('li'); li0.appendChild(document.createTextNode('item0'));
var li1 = document.createElement('li'); li1.appendChild(document.createTextNode('item1'));
container.appendChild(button);
container.appendChild(li0);
container.appendChild(li1);

__albedoClient.hydrateIsland(h(GrowShrink, {}), button);

var countAfterHydrate = container.childNodes.length;

button.__dispatch('click'); // 2 -> 4: two new <li> must be appended

var countAfterGrow = container.childNodes.length;
var textsAfterGrow = [];
for (var i = 1; i < container.childNodes.length; i++) {
  textsAfterGrow.push(container.childNodes[i].firstChild.nodeValue);
}

button.__dispatch('click'); // 4 -> 2: the trailing two <li> must be removed

JSON.stringify({
  countAfterHydrate: countAfterHydrate,
  countAfterGrow: countAfterGrow,
  textsAfterGrow: textsAfterGrow,
  countAfterShrink: container.childNodes.length,
});
"#;

#[test]
fn fragment_child_count_grows_and_shrinks_across_reconcile() {
    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME).expect("client runtime should evaluate");
        ctx.eval::<String, _>(FRAGMENT_GROW_SHRINK_SCENARIO)
            .expect("scenario should evaluate")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("scenario summary should be JSON");

    assert_eq!(value["countAfterHydrate"], 3, "button + 2 items");
    assert_eq!(value["countAfterGrow"], 5, "button + 4 items after growing");
    assert_eq!(
        value["textsAfterGrow"],
        serde_json::json!(["item0", "item1", "item2", "item3"]),
        "new items must append in order after the existing ones"
    );
    assert_eq!(value["countAfterShrink"], 3, "button + 2 items after shrinking back");
}

// Regression: a Fragment nested INSIDE a host element's children — one JSX
// child slot expanding into two DOM nodes flanked by siblings on both
// sides — must hydrate with every sibling correctly adopted, proving the
// cursor walk (not a `childNodes[i]` index) is what host-element hydration
// uses too.
const NESTED_FRAGMENT_SCENARIO: &str = r#"
function Nested(props) {
  return h(
    'div',
    { id: 'wrap' },
    h('span', null, 'lead'),
    h(Fragment, null, h('i', null, 'a'), h('i', null, 'b')),
    h('span', null, 'trail')
  );
}

var container = document.createElement('div');
var wrap = document.createElement('div');
var lead = document.createElement('span'); lead.appendChild(document.createTextNode('lead'));
var ia = document.createElement('i'); ia.appendChild(document.createTextNode('a'));
var ib = document.createElement('i'); ib.appendChild(document.createTextNode('b'));
var trail = document.createElement('span'); trail.appendChild(document.createTextNode('trail'));
wrap.appendChild(lead);
wrap.appendChild(ia);
wrap.appendChild(ib);
wrap.appendChild(trail);
container.appendChild(wrap);

__albedoClient.hydrate(h(Nested, {}), container);

var tags = [];
for (var i = 0; i < wrap.childNodes.length; i++) { tags.push(wrap.childNodes[i].tagName); }

JSON.stringify({
  tags: tags,
  sameLead: wrap.childNodes[0] === lead,
  sameIa: wrap.childNodes[1] === ia,
  sameIb: wrap.childNodes[2] === ib,
  sameTrail: wrap.childNodes[3] === trail,
});
"#;

#[test]
fn fragment_nested_inside_host_element_children_hydrates_all_siblings() {
    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME).expect("client runtime should evaluate");
        ctx.eval::<String, _>(NESTED_FRAGMENT_SCENARIO).expect("scenario should evaluate")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("scenario summary should be JSON");

    assert_eq!(value["tags"], serde_json::json!(["SPAN", "I", "I", "SPAN"]));
    assert_eq!(value["sameLead"], true);
    assert_eq!(value["sameIa"], true, "the fragment's first child must be adopted, not recreated");
    assert_eq!(value["sameIb"], true, "the fragment's second child must be adopted, not recreated");
    assert_eq!(value["sameTrail"], true, "the sibling after the fragment must line up correctly");
}

#[test]
fn client_island_rejects_unbundled_imports_loudly() {
    // A non-framework import has no client binding yet — it must fail loudly
    // rather than emit a browser module that references undefined helpers.
    let source = "import { z } from \"zod\";\nexport default function F() { return <i>{z.name}</i>; }";
    let err = compile_client_island_module("components/f", source, 7)
        .expect_err("unbundled npm import should be rejected");
    let message = format!("{err}");
    assert!(message.contains("zod"), "error should name the offending import: {message}");
}
