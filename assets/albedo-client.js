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
    return {
      __vnode: true,
      type: type,
      props: props || null,
      children: children,
      key: props && props.key != null ? props.key : null,
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

  // --- instantiate (mount path: create real DOM) ---------------------------

  function instantiate(vnode, parentDom) {
    if (vnode.type === TEXT) {
      return { vnode: vnode, dom: global.document.createTextNode(vnode.text) };
    }
    if (vnode.type === Fragment) {
      // A fragment has no DOM of its own; for v1 it adopts its single child's
      // node. Multi-child fragments at a reconcilable boundary are a known gap
      // (no anchor node to diff against) — handled in a later slice.
      var only = singleFragmentChild(vnode);
      var childInst = instantiate(only, parentDom);
      return { vnode: vnode, dom: childInst.dom, fragmentChild: childInst };
    }
    if (isComponent(vnode.type)) {
      var inst = { vnode: vnode, component: vnode.type, hooks: [], parentDom: parentDom };
      var rendered = renderComponent(inst);
      inst.renderedInstance = instantiate(rendered, parentDom);
      inst.dom = inst.renderedInstance.dom;
      return inst;
    }
    var dom = global.document.createElement(vnode.type);
    updateDomProps(dom, null, vnode.props);
    var childInstances = [];
    for (var i = 0; i < vnode.children.length; i++) {
      var ci = instantiate(vnode.children[i], dom);
      dom.appendChild(ci.dom);
      childInstances.push(ci);
    }
    return { vnode: vnode, dom: dom, childInstances: childInstances };
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
      var only = singleFragmentChild(vnode);
      var childInst = hydrateInstance(dom, only, parentDom);
      return { vnode: vnode, dom: childInst.dom, fragmentChild: childInst };
    }
    if (isComponent(vnode.type)) {
      var inst = { vnode: vnode, component: vnode.type, hooks: [], parentDom: parentDom };
      var rendered = renderComponent(inst);
      inst.renderedInstance = hydrateInstance(dom, rendered, parentDom);
      inst.dom = inst.renderedInstance.dom;
      return inst;
    }
    // Host element. If the server node doesn't line up with the expected tag,
    // fall back to a clean mount rather than silently mis-adopting.
    if (!dom || dom.nodeType !== 1 || !tagMatches(dom, vnode.type)) {
      return mountReplace(vnode, parentDom, dom);
    }
    updateDomProps(dom, null, vnode.props);
    var childInstances = [];
    var domChildren = dom.childNodes;
    for (var i = 0; i < vnode.children.length; i++) {
      var childDom = domChildren ? domChildren[i] : null;
      childInstances.push(hydrateInstance(childDom, vnode.children[i], dom));
    }
    return { vnode: vnode, dom: dom, childInstances: childInstances };
  }

  function mountReplace(vnode, parentDom, existingDom) {
    var inst = instantiate(vnode, parentDom);
    if (parentDom && existingDom && existingDom.parentNode === parentDom) {
      parentDom.replaceChild(inst.dom, existingDom);
    } else if (parentDom) {
      parentDom.appendChild(inst.dom);
    }
    return inst;
  }

  // --- reconcile (update path: diff instance tree vs new vnode) ------------

  function reconcile(parentDom, instance, vnode) {
    if (instance == null) {
      var created = instantiate(vnode, parentDom);
      parentDom.appendChild(created.dom);
      return created;
    }
    if (vnode == null) {
      unmount(instance);
      if (instance.dom && instance.dom.parentNode) {
        instance.dom.parentNode.removeChild(instance.dom);
      }
      return null;
    }
    if (instance.vnode.type !== vnode.type) {
      var replacement = instantiate(vnode, parentDom);
      parentDom.replaceChild(replacement.dom, instance.dom);
      unmount(instance);
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
      instance.fragmentChild = reconcile(parentDom, instance.fragmentChild, singleFragmentChild(vnode));
      instance.dom = instance.fragmentChild.dom;
      instance.vnode = vnode;
      return instance;
    }
    if (isComponent(vnode.type)) {
      instance.vnode = vnode;
      instance.parentDom = parentDom;
      var rendered = renderComponent(instance);
      instance.renderedInstance = reconcile(parentDom, instance.renderedInstance, rendered);
      instance.dom = instance.renderedInstance.dom;
      return instance;
    }
    updateDomProps(instance.dom, instance.vnode.props, vnode.props);
    reconcileChildren(instance, vnode);
    instance.vnode = vnode;
    return instance;
  }

  function reconcileChildren(instance, vnode) {
    var oldChildren = instance.childInstances || [];
    var newVnodes = vnode.children;
    var count = Math.max(oldChildren.length, newVnodes.length);
    var next = [];
    for (var i = 0; i < count; i++) {
      var child = reconcile(instance.dom, oldChildren[i] || null, newVnodes[i] || null);
      if (child) {
        next.push(child);
      }
    }
    instance.childInstances = next;
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
    if (instance.fragmentChild) {
      unmount(instance.fragmentChild);
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

  function applyProp(dom, key, oldValue, newValue) {
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

  function singleFragmentChild(vnode) {
    if (vnode.children.length === 1) {
      return vnode.children[0];
    }
    throw new Error('[albedo-client] multi-child Fragment is not yet reconcilable on the client');
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
    hydrate: hydrate,
    hydrateIsland: hydrateIsland,
    registerComponent: registerComponent,
  };

  global.__albedoClient = api;
  global.h = h;
  global.Fragment = Fragment;
  global.useState = useState;
  global.useEffect = useEffect;
  global.__ALBEDO_HYDRATE_ISLAND = hydrateIslandDescriptor;
})(typeof globalThis !== 'undefined' ? globalThis : this);
