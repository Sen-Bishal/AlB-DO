// SPDX-License-Identifier: MIT
// albedo-client — the Tier-C client runtime (A3).
//
// This is the browser mirror of the SSR `h` builtin in
// `src/runtime/quickjs_engine.rs`. Tier-C components are transpiled with the
// SAME JSX pragma (`h` / `h.Fragment`, see `jsx_options.pragma` in
// quickjs_engine.rs), so one transpiled module runs on both sides — but the
// two `h`s do opposite things:
//
//   * server `h` eagerly invokes function components and concatenates HTML
//     strings (synchronous SSR, no state to retain);
//   * client `h` (here) builds a virtual node and DEFERS component invocation
//     until the reconciler can install a hook-state cell for the instance.
//     That deferral is what lets `useState`/`useEffect` run in the browser.
//
// The lifecycle is hydrate-then-diff, Preact-style:
//   1. hydrate — walk the vnode tree in lockstep with the server-rendered DOM,
//      ADOPTING existing nodes (no re-paint) and attaching event listeners;
//   2. setState — re-invoke the owning component with its retained hooks, diff
//      the new vnode subtree against the live instance tree, and patch only
//      what changed. No server round-trip — the whole update is local.
//
// The runtime installs itself on `globalThis` (classic script, no module
// graph): `globalThis.h` so transpiled component code resolves its pragma, and
// `globalThis.__ALBEDO_HYDRATE_ISLAND` — the entry the ≤2KB hydration bootstrap
// (`src/hydration/script.rs`) already calls per island on its trigger.
//
// Shipped size target is ~3KB min+gzip; this source is the readable form.
(function (global) {
  'use strict';
  if (global.__albedoClient) {
    return;
  }

  var TEXT = '#text';

  // --- hook dispatch state -------------------------------------------------
  // `currentFiber` is the component instance being (re)rendered; `hookIndex`
  // walks its hook cells in call order. Rules-of-Hooks (no conditional hooks)
  // is what keeps this positional indexing sound — the same invariant the
  // server-side extractor enforces in `src/transforms/hooks.rs`.
  var currentFiber = null;
  var hookIndex = 0;

  // `currentContextMap` is the set of active context providers during a tree
  // walk: contextId -> the provider instance carrying the live `value` prop.
  // It is snapshotted onto each component instance at mount so a *partial*
  // re-render (a deep consumer's own setState, entered straight through
  // `reconcile`) still resolves the right value without re-walking from the
  // provider. `contextIdSeq` hands out stable per-context ids.
  var currentContextMap = null;
  var contextIdSeq = 0;

  // Effects collected during a render commit, flushed after the DOM settles.
  var pendingEffects = [];

  // Components whose state changed and that owe a re-render.
  var dirtyQueue = [];
  var flushScheduled = false;

  var schedule =
    typeof global.queueMicrotask === 'function'
      ? function (fn) { global.queueMicrotask(fn); }
      : typeof global.Promise === 'function'
        ? function (fn) { global.Promise.resolve().then(fn); }
        : function (fn) { fn(); };

  // --- hyperscript ---------------------------------------------------------

  function normalizeChildren(children, out) {
    for (var i = 0; i < children.length; i++) {
      var child = children[i];
      if (child === null || child === undefined || child === false || child === true) {
        continue;
      }
      if (Array.isArray(child)) {
        normalizeChildren(child, out);
        continue;
      }
      if (typeof child === 'object' && child.__vnode) {
        out.push(child);
        continue;
      }
      out.push({ __vnode: true, type: TEXT, text: String(child), props: null, children: null });
    }
  }

  function h(type, props) {
    var rest = [];
    for (var i = 2; i < arguments.length; i++) {
      rest.push(arguments[i]);
    }
    var children = [];
    normalizeChildren(rest, children);
    // `vnode.children` (above) is for the reconciler to walk when `type` is a
    // host tag — it appends real DOM nodes from it and never looks at props.
    // A *component* is invoked, not walked, and reads its children the React
    // way: off `props.children`. This mirrors the SSR `h` in
    // `quickjs_engine.rs`, which does the same merge for the same reason —
    // without it, a component built through the classic `h(Component, props,
    // child)` call shape, or through the automatic JSX runtime (which strips
    // `children` out of `props` before it ever reaches here — see
    // `react_host.rs`'s `__albedo_jsx`), sees `props.children === undefined`
    // even though it was given children. `Link` below is a real instance of
    // this: it forwards `props.children` and, without this merge, silently
    // renders an empty anchor.
    var finalProps = props || null;
    if (isComponent(type)) {
      finalProps = Object.assign({}, props || {});
      if (children.length === 1) {
        finalProps.children = children[0];
      } else if (children.length > 1) {
        finalProps.children = children;
      }
    }
    return {
      __vnode: true,
      type: type,
      props: finalProps,
      children: children,
      key: finalProps && finalProps.key != null ? finalProps.key : null,
    };
  }

  // Fragment is a sentinel component type the reconciler special-cases to mean
  // "render children with no wrapping element".
  function Fragment(props) {
    return props ? props.children : null;
  }
  h.Fragment = Fragment;

  function isComponent(type) {
    return typeof type === 'function' && type !== Fragment;
  }

  // --- hooks ---------------------------------------------------------------

  function useState(initial) {
    var fiber = currentFiber;
    var index = hookIndex++;
    var hooks = fiber.hooks;
    if (hooks.length <= index) {
      hooks[index] = { state: typeof initial === 'function' ? initial() : initial };
    }
    var hook = hooks[index];
    var setState = function (next) {
      var value = typeof next === 'function' ? next(hook.state) : next;
      if (value === hook.state) {
        return;
      }
      hook.state = value;
      enqueue(fiber);
    };
    return [hook.state, setState];
  }

  function useEffect(effect, deps) {
    var fiber = currentFiber;
    var index = hookIndex++;
    var hooks = fiber.hooks;
    var prev = hooks[index];
    var changed = !prev || !deps || depsChanged(prev.deps, deps);
    var cell = { effect: changed ? effect : null, deps: deps, cleanup: prev ? prev.cleanup : null };
    hooks[index] = cell;
    if (changed) {
      pendingEffects.push(cell);
    }
  }

  function useRef(initial) {
    var fiber = currentFiber;
    var index = hookIndex++;
    var hooks = fiber.hooks;
    if (hooks.length <= index) {
      hooks[index] = { current: initial };
    }
    return hooks[index];
  }

  function useMemo(factory, deps) {
    var fiber = currentFiber;
    var index = hookIndex++;
    var hooks = fiber.hooks;
    var prev = hooks[index];
    // No deps array → recompute every render (React semantics). With deps,
    // reuse the memoized value while they are referentially equal.
    if (prev && deps && !depsChanged(prev.deps, deps)) {
      return prev.value;
    }
    var value = factory();
    hooks[index] = { value: value, deps: deps };
    return value;
  }

  function useCallback(callback, deps) {
    // useCallback(fn, deps) is exactly useMemo(() => fn, deps); it consumes the
    // same single hook slot via the delegated useMemo call.
    return useMemo(function () { return callback; }, deps);
  }

  // createContext returns a context object whose `.Provider` is a sentinel
  // component the reconciler special-cases (like Fragment): instead of running
  // a component body it publishes `props.value` to descendants and renders its
  // child. `_id` keys the provider in the context map; `_defaultValue` is what
  // `useContext` returns when no Provider is mounted above the consumer.
  function createContext(defaultValue) {
    var id = ++contextIdSeq;
    function Provider(props) {
      // Vestigial: the reconciler renders a Provider via its children on the
      // vnode, never by calling this body. Kept so the type is a valid
      // function component for `isComponent`/JSX and for any direct call.
      return props ? props.children : null;
    }
    Provider.__albedoContextId = id;
    return { __albedoContext: true, _id: id, _defaultValue: defaultValue, Provider: Provider };
  }

  // useContext does NOT consume a positional hook slot — it reads the provider
  // map snapshotted onto the fiber, so its result is independent of hook call
  // order and stays correct across conditional renders. The value is read live
  // from the provider instance's current vnode, so a consumer re-rendering for
  // any reason observes the provider's latest `value`.
  function useContext(context) {
    var fiber = currentFiber;
    var map = (fiber && fiber.contextMap) || currentContextMap;
    if (map && context) {
      var provider = map[context._id];
      if (provider && provider.vnode && provider.vnode.props && 'value' in provider.vnode.props) {
        return provider.vnode.props.value;
      }
    }
    return context ? context._defaultValue : undefined;
  }

  // The context id this vnode type provides, or null if it is not a Provider.
  function providerContextId(type) {
    return (typeof type === 'function' && type.__albedoContextId) || null;
  }

  // A child context map = the parent map plus this provider, keyed by id. A
  // fresh object per provider keeps sibling subtrees isolated and lets each
  // instance keep its own snapshot.
  function childContextMap(parent, id, providerInst) {
    var next = {};
    if (parent) {
      for (var k in parent) {
        next[k] = parent[k];
      }
    }
    next[id] = providerInst;
    return next;
  }

  function depsChanged(a, b) {
    if (!a || !b || a.length !== b.length) {
      return true;
    }
    for (var i = 0; i < a.length; i++) {
      if (a[i] !== b[i]) {
        return true;
      }
    }
    return false;
  }

  function runEffects() {
    var effects = pendingEffects;
    pendingEffects = [];
    for (var i = 0; i < effects.length; i++) {
      var cell = effects[i];
      if (typeof cell.cleanup === 'function') {
        try { cell.cleanup(); } catch (err) { reportError(err); }
      }
      if (typeof cell.effect === 'function') {
        try {
          var ret = cell.effect();
          cell.cleanup = typeof ret === 'function' ? ret : null;
        } catch (err) { reportError(err); }
      }
    }
  }

  // --- scheduler -----------------------------------------------------------

  function enqueue(fiber) {
    if (fiber.dirty) {
      return;
    }
    fiber.dirty = true;
    dirtyQueue.push(fiber);
    if (!flushScheduled) {
      flushScheduled = true;
      schedule(flush);
    }
  }

  function flush() {
    flushScheduled = false;
    var queue = dirtyQueue;
    dirtyQueue = [];
    for (var i = 0; i < queue.length; i++) {
      var fiber = queue[i];
      fiber.dirty = false;
      if (fiber.unmounted) {
        continue;
      }
      reconcile(fiber.parentDom, fiber, fiber.vnode);
    }
    runEffects();
  }

  // --- multi-node instance plumbing ------------------------------------------
  //
  // Every instance owns some number of real DOM nodes: exactly one for text,
  // a host element, or a component (which just delegates to whatever its
  // render returned) — but zero or many for a Fragment or a context Provider,
  // which paint nothing of their own and are transparent groups of whatever
  // their children produce. `instance.isGroup` marks the latter; its nodes
  // live in its `childInstances`, exactly like a host element's, except there
  // is no element of its own to hold them. These helpers are the only code
  // that needs to know any of this — mount, hydrate, reconcile and unmount
  // all treat "an instance" as "a thing with DOM nodes" without caring how
  // many it has or where they came from.

  function collectInstanceNodes(instance, out) {
    if (!instance) {
      return;
    }
    if (instance.isGroup) {
      for (var i = 0; i < instance.childInstances.length; i++) {
        collectInstanceNodes(instance.childInstances[i], out);
      }
      return;
    }
    if (instance.renderedInstance) {
      collectInstanceNodes(instance.renderedInstance, out);
      return;
    }
    if (instance.dom) {
      out.push(instance.dom);
    }
  }

  function firstInstanceNode(instance) {
    var nodes = [];
    collectInstanceNodes(instance, nodes);
    return nodes.length ? nodes[0] : null;
  }

  // Insert every node `instance` owns into `parentDom`, in order, right
  // before `beforeNode` — or at the end when `beforeNode` is null/undefined.
  function insertInstance(parentDom, instance, beforeNode) {
    var nodes = [];
    collectInstanceNodes(instance, nodes);
    for (var i = 0; i < nodes.length; i++) {
      if (beforeNode) {
        parentDom.insertBefore(nodes[i], beforeNode);
      } else {
        parentDom.appendChild(nodes[i]);
      }
    }
  }

  // Remove every node `instance` owns from wherever it currently lives.
  function removeInstance(instance) {
    var nodes = [];
    collectInstanceNodes(instance, nodes);
    for (var i = 0; i < nodes.length; i++) {
      if (nodes[i].parentNode) {
        nodes[i].parentNode.removeChild(nodes[i]);
      }
    }
  }

  // Mount a flat vnode list into `container`, in order. Shared by a host
  // element (container = the element itself) and a group (container = the
  // surrounding real DOM parent, since a group owns no element of its own).
  function mountChildren(vnodes, container) {
    var instances = [];
    for (var i = 0; i < vnodes.length; i++) {
      var ci = instantiate(vnodes[i], container);
      insertInstance(container, ci, null);
      instances.push(ci);
    }
    return instances;
  }

  // Hydrate a flat vnode list against `container`'s existing DOM, starting at
  // `startDom` and walking real siblings — not `childNodes[i]` — because one
  // vnode can consume zero (an empty nested group), one (text/host/component)
  // or many (a nested Fragment/Provider) DOM nodes before the next vnode's
  // slice begins.
  function hydrateChildren(vnodes, container, startDom) {
    var instances = [];
    var cursor = startDom;
    for (var i = 0; i < vnodes.length; i++) {
      var ci = hydrateInstance(cursor, vnodes[i], container);
      instances.push(ci);
      var consumed = [];
      collectInstanceNodes(ci, consumed);
      for (var j = 0; j < consumed.length; j++) {
        cursor = cursor ? cursor.nextSibling : null;
      }
    }
    return instances;
  }

  // Diff an old/new pair of flat child-instance/vnode lists against
  // `container`. Index-aligned: matching indices reconcile in place, a
  // shorter new list drops the tail, a longer one appends new instances at
  // the end. Reordering existing children mid-list is not attempted — same
  // documented scope as the rest of this reconciler.
  function reconcileChildList(container, oldChildren, newVnodes) {
    var count = Math.max(oldChildren.length, newVnodes.length);
    var next = [];
    for (var i = 0; i < count; i++) {
      var child = reconcile(container, oldChildren[i] || null, newVnodes[i] || null);
      if (child) {
        next.push(child);
      }
    }
    return next;
  }

  // --- instantiate (mount path: create real DOM) ---------------------------

  function instantiate(vnode, parentDom) {
    if (vnode.type === TEXT) {
      return { vnode: vnode, dom: global.document.createTextNode(vnode.text) };
    }
    if (vnode.type === Fragment) {
      return {
        vnode: vnode,
        isGroup: true,
        parentDom: parentDom,
        childInstances: mountChildren(vnode.children, parentDom),
      };
    }
    var ctxId = providerContextId(vnode.type);
    if (ctxId != null) {
      var pinst = { vnode: vnode, isGroup: true, isProvider: true, parentDom: parentDom, contextMap: currentContextMap };
      var prevMap = currentContextMap;
      currentContextMap = childContextMap(prevMap, ctxId, pinst);
      pinst.childInstances = mountChildren(vnode.children, parentDom);
      currentContextMap = prevMap;
      return pinst;
    }
    if (isComponent(vnode.type)) {
      var inst = { vnode: vnode, component: vnode.type, hooks: [], parentDom: parentDom, contextMap: currentContextMap };
      var rendered = renderComponent(inst);
      inst.renderedInstance = instantiate(rendered, parentDom);
      return inst;
    }
    var dom = createHostElement(vnode.type, parentDom);
    updateDomProps(dom, null, vnode.props);
    return { vnode: vnode, dom: dom, childInstances: mountChildren(vnode.children, dom) };
  }

  // --- hydrate (adopt server-rendered DOM, no re-paint) --------------------

  function hydrateInstance(dom, vnode, parentDom) {
    if (vnode.type === TEXT) {
      if (dom && dom.nodeType === 3) {
        if (dom.nodeValue !== vnode.text) {
          dom.nodeValue = vnode.text;
        }
        return { vnode: vnode, dom: dom };
      }
      return mountReplace(vnode, parentDom, dom);
    }
    if (vnode.type === Fragment) {
      return {
        vnode: vnode,
        isGroup: true,
        parentDom: parentDom,
        childInstances: hydrateChildren(vnode.children, parentDom, dom),
      };
    }
    var ctxId = providerContextId(vnode.type);
    if (ctxId != null) {
      var pinst = { vnode: vnode, isGroup: true, isProvider: true, parentDom: parentDom, contextMap: currentContextMap };
      var prevMap = currentContextMap;
      currentContextMap = childContextMap(prevMap, ctxId, pinst);
      pinst.childInstances = hydrateChildren(vnode.children, parentDom, dom);
      currentContextMap = prevMap;
      return pinst;
    }
    if (isComponent(vnode.type)) {
      var inst = { vnode: vnode, component: vnode.type, hooks: [], parentDom: parentDom, contextMap: currentContextMap };
      var rendered = renderComponent(inst);
      inst.renderedInstance = hydrateInstance(dom, rendered, parentDom);
      return inst;
    }
    // Host element. If the server node doesn't line up with the expected tag,
    // fall back to a clean mount rather than silently mis-adopting.
    if (!dom || dom.nodeType !== 1 || !tagMatches(dom, vnode.type)) {
      return mountReplace(vnode, parentDom, dom);
    }
    updateDomProps(dom, null, vnode.props);
    return { vnode: vnode, dom: dom, childInstances: hydrateChildren(vnode.children, dom, dom.firstChild) };
  }

  function mountReplace(vnode, parentDom, existingDom) {
    var inst = instantiate(vnode, parentDom);
    if (parentDom && existingDom && existingDom.parentNode === parentDom) {
      insertInstance(parentDom, inst, existingDom);
      existingDom.parentNode.removeChild(existingDom);
    } else if (parentDom) {
      insertInstance(parentDom, inst, null);
    }
    return inst;
  }

  // --- reconcile (update path: diff instance tree vs new vnode) ------------

  function reconcile(parentDom, instance, vnode) {
    if (instance == null) {
      var created = instantiate(vnode, parentDom);
      insertInstance(parentDom, created, null);
      return created;
    }
    if (vnode == null) {
      unmount(instance);
      removeInstance(instance);
      return null;
    }
    if (instance.vnode.type !== vnode.type) {
      // Read the old instance's leading node BEFORE it's touched, so the
      // replacement lands in the same slot even when either side owns zero
      // or many nodes (a group). Known boundary: if the OLD instance is an
      // empty group (no node to anchor on) this falls back to appending at
      // the end of `parentDom`, which is only wrong if the empty group has
      // real trailing siblings there — the same "no mid-list reordering"
      // scope this reconciler already documents elsewhere.
      var anchor = firstInstanceNode(instance);
      var replacement = instantiate(vnode, parentDom);
      insertInstance(parentDom, replacement, anchor);
      unmount(instance);
      removeInstance(instance);
      return replacement;
    }
    if (vnode.type === TEXT) {
      if (instance.vnode.text !== vnode.text) {
        instance.dom.nodeValue = vnode.text;
      }
      instance.vnode = vnode;
      return instance;
    }
    if (vnode.type === Fragment) {
      instance.childInstances = reconcileChildList(parentDom, instance.childInstances || [], vnode.children);
      instance.vnode = vnode;
      instance.parentDom = parentDom;
      return instance;
    }
    var rctxId = providerContextId(vnode.type);
    if (rctxId != null) {
      // Refresh the provider vnode first so consumers read the new `value`,
      // then reconcile the child subtree under the updated context map. Basing
      // the map on the instance's own snapshot (not the global) keeps a partial
      // re-render entered straight at this provider correct.
      instance.vnode = vnode;
      instance.parentDom = parentDom;
      var pPrevMap = currentContextMap;
      currentContextMap = childContextMap(instance.contextMap || pPrevMap, rctxId, instance);
      instance.childInstances = reconcileChildList(parentDom, instance.childInstances || [], vnode.children);
      currentContextMap = pPrevMap;
      return instance;
    }
    if (isComponent(vnode.type)) {
      instance.vnode = vnode;
      instance.parentDom = parentDom;
      // Restore this component's context snapshot so any subtree mounted during
      // the re-render inherits the providers that were active above it, even
      // when we entered through a deep partial re-render with a stale global.
      var cPrevMap = currentContextMap;
      currentContextMap = instance.contextMap || cPrevMap;
      var rendered = renderComponent(instance);
      instance.renderedInstance = reconcile(parentDom, instance.renderedInstance, rendered);
      currentContextMap = cPrevMap;
      return instance;
    }
    updateDomProps(instance.dom, instance.vnode.props, vnode.props);
    instance.childInstances = reconcileChildList(instance.dom, instance.childInstances || [], vnode.children);
    instance.vnode = vnode;
    return instance;
  }

  // --- component invocation ------------------------------------------------

  function renderComponent(instance) {
    var prevFiber = currentFiber;
    var prevIndex = hookIndex;
    currentFiber = instance;
    hookIndex = 0;
    try {
      return instance.component(instance.vnode.props || {});
    } finally {
      currentFiber = prevFiber;
      hookIndex = prevIndex;
    }
  }

  function unmount(instance) {
    if (!instance) {
      return;
    }
    instance.unmounted = true;
    if (instance.hooks) {
      for (var i = 0; i < instance.hooks.length; i++) {
        var hook = instance.hooks[i];
        if (hook && typeof hook.cleanup === 'function') {
          try { hook.cleanup(); } catch (err) { reportError(err); }
        }
      }
    }
    if (instance.renderedInstance) {
      unmount(instance.renderedInstance);
    }
    // A host element that received a ref hands back `null` on the way out, so
    // a consumer never holds a node that has left the document. Not a group
    // (Fragment/Provider): those never carry a `ref` prop that means anything.
    if (instance.childInstances && !instance.isGroup && instance.vnode && instance.vnode.props) {
      attachRef(instance.vnode.props.ref, null);
    }
    if (instance.childInstances) {
      for (var j = 0; j < instance.childInstances.length; j++) {
        unmount(instance.childInstances[j]);
      }
    }
  }

  // --- DOM prop application ------------------------------------------------

  function updateDomProps(dom, oldProps, newProps) {
    oldProps = oldProps || {};
    newProps = newProps || {};
    var key;
    for (key in oldProps) {
      if (!hasOwn(oldProps, key) || key === 'children' || key === 'key') {
        continue;
      }
      if (!(key in newProps) || newProps[key] !== oldProps[key]) {
        applyProp(dom, key, oldProps[key], undefined);
      }
    }
    for (key in newProps) {
      if (!hasOwn(newProps, key) || key === 'children' || key === 'key') {
        continue;
      }
      if (oldProps[key] !== newProps[key]) {
        applyProp(dom, key, oldProps[key], newProps[key]);
      }
    }
  }

  var SVG_NS = 'http://www.w3.org/2000/svg';

  // JSX prop → attribute name.
  //
  // 🔑 **This table is checked against `runtime::jsx_attributes`'s by a
  // Rust test** (`the_client_runtime_table_matches_this_one`). It cannot be
  // generated — this file is hand-written JavaScript served to the browser —
  // so drift is caught rather than prevented. It matters because hydration
  // *adopts* the server's DOM: a client that spells one attribute differently
  // does not produce a cosmetic difference, it produces a stray attribute on an
  // adopted node, or a re-mount.
  //
  // Keyed on the exact prop name, with no SVG/HTML branch, because the mapping
  // needs no context: `strokeWidth` is meaningless on a `<div>`. Attributes
  // already spelled camelCase in SVG (`viewBox`, `preserveAspectRatio`) map to
  // themselves and are deliberately absent — `setAttribute` on a namespaced
  // element is case-preserving.
  var JSX_ATTRIBUTE_RENAMES = {
    className: 'class',
    htmlFor: 'for',
    defaultChecked: 'checked',
    defaultValue: 'value',
    alignmentBaseline: 'alignment-baseline',
    baselineShift: 'baseline-shift',
    clipPath: 'clip-path',
    clipRule: 'clip-rule',
    colorInterpolation: 'color-interpolation',
    colorInterpolationFilters: 'color-interpolation-filters',
    dominantBaseline: 'dominant-baseline',
    fillOpacity: 'fill-opacity',
    fillRule: 'fill-rule',
    floodColor: 'flood-color',
    floodOpacity: 'flood-opacity',
    fontFamily: 'font-family',
    fontSize: 'font-size',
    fontSizeAdjust: 'font-size-adjust',
    fontStretch: 'font-stretch',
    fontStyle: 'font-style',
    fontVariant: 'font-variant',
    fontWeight: 'font-weight',
    imageRendering: 'image-rendering',
    letterSpacing: 'letter-spacing',
    lightingColor: 'lighting-color',
    markerEnd: 'marker-end',
    markerMid: 'marker-mid',
    markerStart: 'marker-start',
    paintOrder: 'paint-order',
    pointerEvents: 'pointer-events',
    shapeRendering: 'shape-rendering',
    stopColor: 'stop-color',
    stopOpacity: 'stop-opacity',
    strokeDasharray: 'stroke-dasharray',
    strokeDashoffset: 'stroke-dashoffset',
    strokeLinecap: 'stroke-linecap',
    strokeLinejoin: 'stroke-linejoin',
    strokeMiterlimit: 'stroke-miterlimit',
    strokeOpacity: 'stroke-opacity',
    strokeWidth: 'stroke-width',
    textAnchor: 'text-anchor',
    textDecoration: 'text-decoration',
    textRendering: 'text-rendering',
    unicodeBidi: 'unicode-bidi',
    vectorEffect: 'vector-effect',
    wordSpacing: 'word-spacing',
    writingMode: 'writing-mode',
  };


  // 🔑 `document.createElement('svg')` produces an `HTMLUnknownElement`, not an
  // SVG element — it renders **nothing**, silently. Every icon package in npm
  // is `createElement('svg', …)`, so without this the Tier-C npm path would
  // deliver a chunk that loads, a component that mounts, and a blank square
  // where the icon should be. The namespace is inherited from the parent (SVG
  // has no closing marker in the vnode tree) and entered at the `<svg>` tag,
  // matching how every VDOM does it.
  function createHostElement(type, parentDom) {
    var inSvg =
      type === 'svg' ||
      (parentDom && parentDom.namespaceURI === SVG_NS && type !== 'foreignObject');
    if (inSvg) {
      return global.document.createElementNS(SVG_NS, type);
    }
    return global.document.createElement(type);
  }

  // The attribute name to write, given the JSX prop name.
  function attributeNameFor(key) {
    return hasOwn(JSX_ATTRIBUTE_RENAMES, key) ? JSX_ATTRIBUTE_RENAMES[key] : key;
  }

  // A React-style `ref` on a host element: hand the ref the real DOM node.
  //
  // Both callable refs (`ref={node => ...}`) and object refs
  // (`ref={useRef(null)}`) are supported, which is exactly the pair
  // `forwardRef`/`useImperativeHandle` in the react host module
  // (`bundler::client_npm`) produce and consume.
  function attachRef(ref, value) {
    if (typeof ref === 'function') {
      try {
        ref(value);
      } catch (err) {
        reportError(err);
      }
      return;
    }
    if (ref && typeof ref === 'object') {
      ref.current = value;
    }
  }

  function applyProp(dom, key, oldValue, newValue) {
    // 🔑 `ref` is a binding, not an attribute. Without this arm a forwarded
    // ref reaches `setAttribute` and lands in the DOM as
    // `ref="[object Object]"` while nothing ever receives the node — which is
    // why externalising React's `forwardRef` is only honest with this half
    // present. Detach-then-attach so a ref that moved between elements is
    // cleared on the old one first.
    if (key === 'ref') {
      attachRef(oldValue, null);
      attachRef(newValue, dom);
      return;
    }
    // Event handler prop `onX` → DOM listener. The lowercased remainder is the
    // event type (`onClick` → `click`), matching React's convention.
    if (key.length > 2 && key[0] === 'o' && key[1] === 'n' && key[2] >= 'A' && key[2] <= 'Z') {
      var eventType = key.slice(2).toLowerCase();
      if (typeof oldValue === 'function') {
        dom.removeEventListener(eventType, oldValue);
      }
      if (typeof newValue === 'function') {
        dom.addEventListener(eventType, newValue);
      }
      return;
    }
    // JSX prop → attribute name, from the table both server renderers read.
    key = attributeNameFor(key);
    if (newValue === undefined || newValue === null || newValue === false) {
      dom.removeAttribute(key);
      return;
    }
    if (newValue === true) {
      dom.setAttribute(key, '');
      return;
    }
    dom.setAttribute(key, String(newValue));
  }

  // --- helpers -------------------------------------------------------------

  function hasOwn(obj, key) {
    return Object.prototype.hasOwnProperty.call(obj, key);
  }

  function tagMatches(dom, type) {
    var name = dom.tagName || dom.nodeName;
    return typeof name === 'string' && name.toLowerCase() === String(type).toLowerCase();
  }

  function reportError(err) {
    if (global.console && typeof global.console.error === 'function') {
      global.console.error('[albedo-client]', err);
    }
  }

  // --- public entry points -------------------------------------------------

  var registry = Object.create(null);

  function registerComponent(key, component) {
    registry[String(key)] = component;
  }

  // Hydrate `vnode` against `root` treating `root` itself as the component's
  // output node (the island marker element). This is the production entry the
  // bootstrap reaches through `__ALBEDO_HYDRATE_ISLAND`.
  function hydrateIsland(vnode, root) {
    var instance = hydrateInstance(root, vnode, root.parentNode || root);
    root.__albedoRoot = instance;
    runEffects();
    return instance;
  }

  // Hydrate `vnode` against the single child of `container` (the simple form
  // used by tests and by callers that own a wrapper element).
  function hydrate(vnode, container) {
    var instance = hydrateInstance(container.firstChild, vnode, container);
    container.__albedoRoot = instance;
    runEffects();
    return instance;
  }

  function hydrateIslandDescriptor(island) {
    if (!island) {
      return;
    }
    var component = registry[String(island.component_id)] || registry[island.module_path];
    if (typeof component !== 'function') {
      return;
    }
    var root = island.target;
    if (!root && global.document && typeof global.document.querySelector === 'function') {
      root = global.document.querySelector('[data-albedo-island="' + island.component_id + '"]');
    }
    if (!root) {
      return;
    }
    if (root.getAttribute && root.getAttribute('data-albedo-hydrated') === 'true') {
      return;
    }
    if (root.setAttribute) {
      root.setAttribute('data-albedo-hydrated', 'true');
    }
    hydrateIsland(h(component, island.props || {}), root);
  }

  var api = {
    h: h,
    Fragment: Fragment,
    useState: useState,
    useEffect: useEffect,
    useRef: useRef,
    useMemo: useMemo,
    useCallback: useCallback,
    useContext: useContext,
    createContext: createContext,
    hydrate: hydrate,
    hydrateIsland: hydrateIsland,
    registerComponent: registerComponent,
  };

  // Phase L · `<Link href>` in a client island.
  //
  // The server half of this lives in `quickjs_engine`'s runtime prelude, and
  // both exist for the same reason: `<Link>` is rewritten to `<a
  // data-albedo-link>` by the pure-Rust evaluator's JSX walker only.
  // `transforms::link` is a metadata pass that does not touch the AST, so a
  // compiled island — whose JSX lowers to `h(Link, …)` — has no `Link` binding
  // at all and throws `Link is not defined` on its first render.
  //
  // Without this, fixing only the server side would give an island that
  // server-renders correctly and then blows up the moment it hydrates, which
  // is a worse failure than not mounting: the markup is already on screen.
  //
  // `data-albedo-link` is set last, matching the attribute order both
  // renderers emit, so hydration adopts the server's DOM instead of replacing
  // it. `albedo-link-forms.js` is what actually hooks the attribute to
  // intercept the click.
  function Link(props) {
    var merged = {};
    var source = props || {};
    for (var key in source) {
      if (Object.prototype.hasOwnProperty.call(source, key) && key !== 'children') {
        merged[key] = source[key];
      }
    }
    delete merged['data-albedo-link'];
    merged['data-albedo-link'] = true;
    return h('a', merged, source.children);
  }

  // The one export in the shared React host table (`runtime::react_host`) whose
  // implementation cannot be shared: an "element" is a vnode here and an
  // `AlbedoHtml` string wrapper on the server. `isValidElement` routes through
  // this global so the table stays single-sourced.
  global.__albedo_is_element = function (value) {
    return value !== null && typeof value === 'object' && value.__vnode === true;
  };

  global.__albedoClient = api;
  global.h = h;
  global.Link = Link;
  global.Fragment = Fragment;
  global.useState = useState;
  global.useEffect = useEffect;
  global.useRef = useRef;
  global.useMemo = useMemo;
  global.useCallback = useCallback;
  global.useContext = useContext;
  global.createContext = createContext;
  global.__ALBEDO_HYDRATE_ISLAND = hydrateIslandDescriptor;

  // Drain any islands the ≤2KB bootstrap (`src/hydration/script.rs`) enqueued
  // before this (async-loaded) runtime finished defining the entry above. The
  // bootstrap schedules each island on its trigger (idle/visible/interaction);
  // if its `run` fires before `__ALBEDO_HYDRATE_ISLAND` exists it pushes the
  // descriptor onto `__ALBEDO_HYDRATE_QUEUE` instead of dropping it. We flush
  // that backlog now, then replace the queue with a live shim so any later
  // push hydrates immediately — closing the script-load ordering race that
  // otherwise left effect-only islands (idle trigger) dead on the page.
  var pending = global.__ALBEDO_HYDRATE_QUEUE;
  if (pending && typeof pending.length === 'number') {
    for (var qi = 0; qi < pending.length; qi++) {
      try {
        hydrateIslandDescriptor(pending[qi]);
      } catch (_e) {}
    }
  }
  global.__ALBEDO_HYDRATE_QUEUE = {
    push: function (island) {
      hydrateIslandDescriptor(island);
    },
  };
})(typeof globalThis !== 'undefined' ? globalThis : this);
