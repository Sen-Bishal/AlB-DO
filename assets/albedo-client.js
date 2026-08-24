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
    //
    // 🔑 **A host tag gets the same treatment, and for a different consumer.**
    // The reconciler never needs `props.children` for a host tag — it walks
    // `vnode.children`. *Packages* do. Radix's `Slot` reads
    // `slottableElement.props.children` off whatever the author passed to
    // `asChild` (usually a plain `<button>`), and its `getElementRef` calls
    // `Object.getOwnPropertyDescriptor(element.props, 'ref')`, which throws on
    // the `null` this used to produce. React folds children into props for
    // every element type; so does this now. `updateDomProps` already skips
    // `children` and `key`, so nothing reaches the DOM as an attribute.
    //
    // This is the host-tag half of the component-vnode fix above — one bug,
    // found from opposite ends.
    var finalProps = Object.assign({}, props || {});
    if (children.length === 1) {
      finalProps.children = children[0];
    } else if (children.length > 1) {
      finalProps.children = children;
    }
    return {
      __vnode: true,
      type: type,
      props: finalProps,
      children: children,
      key: finalProps.key != null ? finalProps.key : null,
    };
  }

  // `cloneElement` — `TODO.md` 9.2. The third export in the shared React host
  // table (`runtime::react_host`) with two implementations, after
  // `isValidElement` and `createPortal`.
  //
  // Here it is genuinely cheap, because a vnode is still a description: rebuild
  // it through `h` so the clone's children normalization and props shaping are
  // the *same* code a fresh render uses, rather than a second copy of those
  // rules that can drift. The server's half has to work much harder — see
  // `__albedo_clone_element` in `quickjs_engine.rs`.
  //
  // Prop precedence is React's: config over the element's own props, then
  // variadic children over `config.children` over the element's own.
  function cloneElement(element, config) {
    var extra = [];
    for (var i = 2; i < arguments.length; i++) {
      extra.push(arguments[i]);
    }
    // Not a vnode, or a text node — which has no props to merge and whose
    // `'#text'` type would be built as a literal `<#text>` tag by `h`.
    if (element === null || typeof element !== 'object'
        || element.__vnode !== true || element.type === TEXT) {
      return element;
    }

    var merged = {};
    var base = element.props || {};
    var key;
    for (key in base) {
      if (hasOwn(base, key)) {
        merged[key] = base[key];
      }
    }
    if (config) {
      for (key in config) {
        if (hasOwn(config, key)) {
          merged[key] = config[key];
        }
      }
    }

    // `h` re-folds children into props, so hand them to it positionally and
    // let it do that once. `element.children` is the already-normalized array,
    // which is what `h` would produce from the same input anyway.
    var childArgs = extra;
    if (childArgs.length === 0) {
      childArgs = hasOwn(merged, 'children') ? [merged.children] : [element.children];
    }
    delete merged.children;

    return h.apply(null, [element.type, merged].concat(childArgs));
  }

  // Fragment is a sentinel component type the reconciler special-cases to mean
  // "render children with no wrapping element".
  function Fragment(props) {
    return props ? props.children : null;
  }
  h.Fragment = Fragment;

  // `createPortal(children, container)` — TODO.md 9.3.
  //
  // A portal is a group (§ multi-node instance plumbing) with one difference
  // that decides every other line: **it owns no nodes in its parent.** Its
  // children live in `container`, somewhere else in the document entirely.
  //
  // 🔑 That single property is what makes it fit the existing machinery instead
  // of fighting it. `collectInstanceNodes` returns nothing for a portal, so:
  //   * `insertInstance` moves nothing — a parent re-order never drags portal
  //     content back into the parent;
  //   * `firstInstanceNode` skips it as an anchor, which is correct: it has no
  //     position among its siblings;
  //   * `hydrateChildren`'s cursor does not advance past it — correct, because
  //     the server rendered nothing there (see below).
  // Only `removeInstance` needs to know a portal exists, because its nodes must
  // be removed from `container` rather than from the parent.
  //
  // ## Why there is nothing to hydrate
  //
  // React's own server renderer THROWS on a portal — *"Portals are not
  // currently supported by the server renderer. Render them conditionally so
  // that they only appear on the client render."* (verified in
  // `react-dom/cjs/react-dom-server-legacy.browser.development.js`). So no
  // React app has ever had server markup for portal content, Radix only renders
  // `<Dialog.Portal>` when the dialog is open for exactly that reason, and
  // Albedo's server renderers emit nothing rather than throwing — strictly more
  // permissive, and it can never produce markup that disagrees with the client.
  //
  // Hydration therefore MOUNTS the portal fresh instead of adopting. There is
  // no server DOM to adopt, and trying to would consume a sibling that belongs
  // to the portal's neighbour.
  function Portal(props) {
    return props ? props.children : null;
  }

  function createPortal(children, container) {
    var kids = [];
    normalizeChildren([children], kids);
    return {
      __vnode: true,
      type: Portal,
      props: { container: container },
      children: kids,
      key: null,
    };
  }

  // A component is allowed to render nothing — `if (!open) return null` is the
  // most ordinary idiom in React, and Radix's `Presence` is exactly it. But a
  // bare `null` has no `.type`, and `instantiate`/`hydrateInstance` read that
  // field on their first line, so a null render crashed the reconciler outright.
  //
  // 🔑 Rather than scatter null checks, a null render becomes a real vnode with
  // its own sentinel type — the same shape Fragment and Portal already use.
  // Every downstream site then takes its normal path, and `Empty` behaves like
  // a group with no children: it owns zero DOM nodes, so `collectInstanceNodes`
  // reports nothing for it and the sibling/anchor/hydration-cursor logic skips
  // it without knowing it exists.
  function Empty() {
    return null;
  }

  function emptyVnode() {
    return { __vnode: true, type: Empty, props: null, children: [], key: null };
  }

  // What a component actually returned, as something the reconciler can walk.
  //
  // Mirrors `normalizeChildren`'s rules deliberately: a value is either markup,
  // nothing, or text, and it should not matter whether it arrived as a child or
  // as a component's return value. That also makes `return "hello"` work, which
  // React allows and which previously reached `createHostElement(undefined)`.
  function normalizeRender(rendered) {
    if (rendered === null || rendered === undefined || typeof rendered === 'boolean') {
      return emptyVnode();
    }
    if (typeof rendered === 'object' && rendered.__vnode) {
      return rendered;
    }
    if (Array.isArray(rendered)) {
      // An array return is a fragment in all but name.
      var kids = [];
      normalizeChildren(rendered, kids);
      return { __vnode: true, type: Fragment, props: null, children: kids, key: null };
    }
    return { __vnode: true, type: TEXT, text: String(rendered), props: null, children: null };
  }

  function isComponent(type) {
    return typeof type === 'function'
      && type !== Fragment
      && type !== Portal
      && type !== Empty;
  }

  // The DOM node a portal's children go into. `document.body` is React's
  // default and the one Radix passes when a consumer gives no `container`.
  function portalContainer(vnode) {
    var declared = vnode.props ? vnode.props.container : null;
    return declared || (global.document ? global.document.body : null);
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

  // useId — the client half of the pair documented in `quickjs_engine.rs`'s
  // prelude. Same formula, same inputs: the island's module_path as the scope
  // (identical to the string the server rendered under) and a counter over
  // useId calls in component-invocation order, which is parent-first
  // depth-first on both sides.
  //
  // It takes a hook SLOT rather than recomputing, because the counter only
  // matches the server's on the FIRST pass. A re-render (any setState) walks
  // the same components again, and a recomputed id would change under the DOM
  // that already carries it — breaking exactly the aria wiring the hook exists
  // to establish.
  var idScope = 'r';
  var idCounter = 0;

  function idSlug(entry) {
    var raw = entry === undefined || entry === null ? '' : String(entry);
    var slug = '';
    for (var i = 0; i < raw.length; i++) {
      var ch = raw[i];
      var ok = (ch >= 'a' && ch <= 'z') || (ch >= 'A' && ch <= 'Z') || (ch >= '0' && ch <= '9');
      slug += ok ? ch : '-';
    }
    slug = slug.replace(/-+/g, '-').replace(/^-|-$/g, '');
    return slug.length > 0 ? slug : 'r';
  }

  function beginIdScope(entry) {
    idCounter = 0;
    idScope = idSlug(entry);
  }

  function useId() {
    var fiber = currentFiber;
    var index = hookIndex++;
    var hooks = fiber.hooks;
    if (hooks.length <= index) {
      hooks[index] = { id: 'albedo-' + idScope + '-' + (idCounter++).toString(36) };
    }
    return hooks[index].id;
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
    // A portal's nodes are in another container, so it contributes NOTHING to
    // its parent's node list. Every caller of this function is asking "what
    // does this instance occupy here?", and the answer for a portal is
    // "nothing" — see `createPortal`.
    if (instance.isPortal) {
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
    if (instance && instance.isPortal) {
      // Collect from the portal's CHILDREN, since the portal itself reports no
      // nodes. Each node is removed via its own `parentNode`, so this lands in
      // the portal's container rather than the parent.
      for (var p = 0; p < (instance.childInstances || []).length; p++) {
        collectInstanceNodes(instance.childInstances[p], nodes);
      }
    } else {
      collectInstanceNodes(instance, nodes);
    }
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
      // Where anything created at this slot must land: the first DOM node owned
      // by a LATER old sibling, or the end of the container when there is none.
      //
      // 🔑 This is only consulted when the slot's own instance owns no nodes,
      // and that case stopped being exotic when a component was allowed to
      // render nothing. `reconcile` alone cannot compute it — an empty instance
      // has no position of its own, so the answer lives with its siblings and
      // has to be passed in. Without it, a `null` that becomes content appends
      // at the END of its parent instead of reappearing where it belongs, which
      // is the boundary `reconcile` used to document as known-wrong.
      var anchor = null;
      for (var j = i + 1; j < oldChildren.length && !anchor; j++) {
        anchor = firstInstanceNode(oldChildren[j]);
      }
      var child = reconcile(container, oldChildren[i] || null, newVnodes[i] || null, anchor);
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
    if (vnode.type === Empty) {
      return { vnode: vnode, isGroup: true, parentDom: parentDom, childInstances: [] };
    }
    if (vnode.type === Portal) {
      var pcontainer = portalContainer(vnode);
      return {
        vnode: vnode,
        isGroup: true,
        isPortal: true,
        parentDom: parentDom,
        container: pcontainer,
        childInstances: pcontainer ? mountChildren(vnode.children, pcontainer) : [],
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
    if (vnode.type === Empty) {
      // Renders nothing, so it adopts nothing and leaves `dom` for the next
      // sibling — the server rendered nothing here either.
      return { vnode: vnode, isGroup: true, parentDom: parentDom, childInstances: [] };
    }
    if (vnode.type === Portal) {
      // No server markup exists for a portal (see `createPortal`), so this is a
      // MOUNT, not an adopt — and it deliberately ignores `dom`, which belongs
      // to whatever sibling comes next.
      return instantiate(vnode, parentDom);
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

  function reconcile(parentDom, instance, vnode, anchorHint) {
    if (instance == null) {
      var created = instantiate(vnode, parentDom);
      insertInstance(parentDom, created, anchorHint || null);
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
      // The old instance's own leading node when it has one; otherwise the
      // sibling anchor, which is the empty-instance case.
      var anchor = firstInstanceNode(instance) || anchorHint || null;
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
    if (vnode.type === Empty) {
      instance.vnode = vnode;
      instance.parentDom = parentDom;
      return instance;
    }
    if (vnode.type === Portal) {
      // Children diff against the CONTAINER, never `parentDom`. A container
      // that changed identity between renders is a different destination, so
      // the old content is torn down rather than migrated — React re-creates
      // in that case too.
      var nextContainer = portalContainer(vnode);
      if (nextContainer !== instance.container) {
        unmount(instance);
        removeInstance(instance);
        return instantiate(vnode, parentDom);
      }
      instance.childInstances = nextContainer
        ? reconcileChildList(nextContainer, instance.childInstances || [], vnode.children)
        : [];
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
      // Forwarded: a component's rendered subtree sits in the COMPONENT's slot,
      // so when that subtree goes from nothing to something it must land where
      // the component is, not at the end of the parent.
      instance.renderedInstance = reconcile(
        parentDom,
        instance.renderedInstance,
        rendered,
        anchorHint
      );
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
      // Normalised HERE, at the single seam every component return passes
      // through, so `instantiate`, `hydrateInstance` and `reconcile` never see
      // a value that is not a vnode.
      return normalizeRender(instance.component(instance.vnode.props || {}));
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

  // 🔑 The attributes where a JSX boolean becomes the WORD, not a bare name.
  //
  // HTML has two unrelated kinds of attribute that both take `true` in JSX. A
  // real boolean attribute (`disabled`, `checked`, `hidden`) signals by being
  // *present* — its value is ignored, and `false` means remove it. An enumerated
  // attribute signals by its *value*, and its value space is the two literal
  // strings `"true"` and `"false"` — so a bare `aria-expanded` is the empty
  // string, which is neither, and assistive technology reads it as not expanded.
  // `false` is a value here too: `aria-hidden="false"` is what keeps an
  // ancestor's `aria-hidden="true"` from being inherited over a subtree, and is
  // not the same as saying nothing.
  //
  // The `aria-` prefix is a rule rather than a list of the sixty-odd ARIA
  // booleans, so a new ARIA attribute cannot rot into inert markup. This table
  // is only the non-`aria-` remainder — React's `BOOLEANISH_STRING` set. It is
  // set-equal to `runtime::jsx_attributes::ENUMERATED_BOOLEAN_ATTRIBUTES` and a
  // Rust test asserts that, for the same reason the rename table above has one:
  // hydration ADOPTS the server's node, so a client that spells one attribute
  // differently silently rewrites it on the way in.
  //
  // Keys are lowercase; lookups lowercase first. HTML attribute names are
  // case-insensitive and `setAttribute` lowercases them on an HTML element, so
  // `contentEditable` and `contenteditable` are one attribute.
  var ENUMERATED_BOOLEAN_ATTRIBUTES = {
    contenteditable: true,
    draggable: true,
    spellcheck: true,
    autoreverse: true,
    externalresourcesrequired: true,
    focusable: true,
    preservealpha: true,
  };

  var ARIA_PREFIX = 'aria-';

  function isEnumeratedBooleanAttribute(name) {
    var lower = String(name).toLowerCase();
    return (
      lower.slice(0, ARIA_PREFIX.length) === ARIA_PREFIX ||
      hasOwn(ENUMERATED_BOOLEAN_ATTRIBUTES, lower)
    );
  }


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
    // Booleans first, and BEFORE the removal branch: an enumerated attribute's
    // `false` is a value it must carry (`aria-hidden="false"`), not a reason to
    // remove it. The attribute name decides, from the same table both server
    // renderers read — a divergence here would not fail loudly, it would rewrite
    // the attribute on an adopted node the instant hydration applied props.
    if (typeof newValue === 'boolean' && isEnumeratedBooleanAttribute(key)) {
      dom.setAttribute(key, newValue ? 'true' : 'false');
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
    // Enter the same `useId` scope the server rendered this island under.
    // `module_path` is the string it passed as `entry`.
    beginIdScope(island.module_path || island.component_id);
    hydrateIsland(h(component, island.props || {}), root);
  }

  var api = {
    h: h,
    Fragment: Fragment,
    useState: useState,
    useEffect: useEffect,
    useRef: useRef,
    useId: useId,
    createPortal: createPortal,
    Portal: Portal,
    Empty: Empty,
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

  // The second such export, and the same arrangement: `createPortal` builds a
  // vnode here and returns empty markup on the server (there is no server
  // rendering of portal content — see `createPortal` above). One table row in
  // `runtime::react_host`, one global name, two implementations.
  global.__albedo_createPortal = createPortal;

  // The third, same arrangement. On the server an element is finished HTML, so
  // cloning there is a re-render of one tag rather than a rebuild of a
  // description; the two cannot share a body. One table row, one global name.
  global.__albedo_clone_element = cloneElement;

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
