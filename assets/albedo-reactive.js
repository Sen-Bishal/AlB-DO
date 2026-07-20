// SPDX-License-Identifier: MIT
// albedo-reactive — the Tier-C "binding mode" driver (step 3, fine-grained
// reactivity).
//
// This is the runtime half of "stop hydrating components". The compiler emits,
// per route, the same `SetTextRef` / `BindEvent` bindings the Tier-B opcode path
// already produces, PLUS each `on*` handler lowered to a client thunk
// `(function(__state,__emit){...})`. This driver wires those bindings against the
// already-rendered static HTML and, on interaction, runs the handler LOCALLY —
// reading/writing a plain JS state object — and feeds the resulting `SlotSet`
// opcodes to the existing bakabox patcher.
//
// There is no VDOM, no component hydration, and no server round-trip: only the
// DOM text/attribute nodes that actually read state ever change. The handler
// thunks are produced by `CompiledProject::build_reactive_payload` and are only
// emitted for handlers the analysis proved client-satisfiable (steps 1–2).

(function (global) {
  'use strict';

  // Display form of a slot value. Matches bakabox: a slot's painted text is the
  // value verbatim (numbers stringified, null/undefined blanked).
  function formatSlotValue(v) {
    if (typeof v === 'string') return v;
    if (v === null || v === undefined) return '';
    return String(v);
  }

  // bakabox's `SlotSet` carries UTF-8 bytes (it `TextDecoder`s them). In a real
  // browser we hand it bytes; under a DOM/VM shim that lacks `TextEncoder` we
  // pass the string through and let the shim consume it directly.
  function encodeSlotValue(text) {
    if (typeof TextEncoder !== 'undefined') {
      return new TextEncoder().encode(text);
    }
    return text;
  }

  // Normalise the handler table to `{ proxyId: source }`. The Rust payload
  // serialises `Vec<(u32, String)>` as an array of `[pid, src]` pairs; accept
  // either that or a plain object.
  function normaliseHandlers(raw) {
    var out = {};
    if (!raw) return out;
    if (Array.isArray(raw)) {
      for (var i = 0; i < raw.length; i++) {
        out[String(raw[i][0])] = raw[i][1];
      }
      return out;
    }
    for (var key in raw) {
      if (Object.prototype.hasOwnProperty.call(raw, key)) {
        out[String(key)] = raw[key];
      }
    }
    return out;
  }

  // Escape a string for a double-quoted HTML attribute value. Mirrors the
  // Rust `escape_html` so a row key stamped here matches the SSR-stamped one.
  //
  // Deliberately regex-free: this driver is inlined RAW into a classic
  // `<script>`, and the inline-script escaper rewrites `</` → `<\/`, which would
  // corrupt a `/</g` regex literal into `/<\/g` and break the whole IIFE. A
  // character switch has no such sequence. (Payload thunks escape via regex
  // safely because they travel as JSON strings, where `\/` decodes back to `/`.)
  function escapeHtmlAttr(v) {
    var s = String(v);
    var out = '';
    for (var i = 0; i < s.length; i++) {
      var c = s.charAt(i);
      if (c === '&') out += '&amp;';
      else if (c === '<') out += '&lt;';
      else if (c === '>') out += '&gt;';
      else if (c === '"') out += '&quot;';
      else if (c === "'") out += '&#39;';
      else out += c;
    }
    return out;
  }

  // Inject `data-albedo-key="KEY"` into a row's opening tag, right after the
  // tag name — the client mirror of Rust `stamp_row_key`. Rows are single host
  // elements, so the HTML starts with `<tag`.
  function stampRowKey(html, key) {
    if (typeof html !== 'string' || html.charAt(0) !== '<') return html;
    var i = 1;
    while (i < html.length) {
      var c = html.charAt(i);
      if (c === ' ' || c === '\t' || c === '\n' || c === '\r' || c === '>' || c === '/') break;
      i++;
    }
    return html.slice(0, i) + ' data-albedo-key="' + escapeHtmlAttr(key) + '"' + html.slice(i);
  }

  function compileThunk(src) {
    if (typeof src === 'function') return src;
    // `src` is already `(function(__state,__emit){...})`. `new Function`
    // avoids leaking names into the enclosing scope and needs no indirect eval.
    return new Function('return (' + src + ');')();
  }

  // Monotonic per-install counter feeding each install's slot-id scope. One VM
  // backs every island on a route (see `drainReactiveQueue`), so this is what
  // keeps their slot keyspaces disjoint.
  var installSeq = 0;

  /**
   * Wire a route's reactive payload against `opts.vm` (a bakabox-compatible
   * patcher exposing `seedNodesFromDocument`, `applyInstruction`, and a settable
   * `eventDispatcher`). `opts.root` is the DOM subtree the shell painted;
   * `opts.payload` is the `ReactivePayload`; `opts.state` is the optional initial
   * slot state (defaults to empty — the first interaction falls back to each
   * `useState` initial, which already matches the server-rendered text).
   */
  function installReactiveRuntime(opts) {
    var vm = opts.vm;
    var payload = opts.payload || {};
    var state = opts.state || {};
    var texts = payload.texts || [];
    var attrs = payload.attrs || [];
    var derived = payload.derived || [];
    var lists = payload.lists || [];
    var events = payload.events || [];
    var handlers = {};
    var sources = normaliseHandlers(payload.handlers);
    for (var pid in sources) {
      if (Object.prototype.hasOwnProperty.call(sources, pid)) {
        handlers[pid] = compileThunk(sources[pid]);
      }
    }

    // One VM now backs every island on the route, so slot ids share a keyspace.
    // A payload's slot ids are only component-local (two islands can both use
    // slot 0; the synthetic derived ids `d0…` collide outright), so prefix every
    // VM-facing slot id with a per-install scope. Internal bookkeeping below
    // (state, dirty, depToDerived) stays keyed on the RAW ids the handler thunks
    // emit — `vmSlot` is applied only at the `vm.applyInstruction` boundary.
    var slotScope = 'r' + (installSeq++) + ':';
    function vmSlot(id) {
      return slotScope + id;
    }

    // Adopt the server-painted nodes by their `data-albedo-id` stamps.
    if (typeof vm.seedNodesFromDocument === 'function') {
      vm.seedNodesFromDocument(opts.root);
    }

    // bakabox resolves a `BindEvent`'s `eventId` through its Event intern table;
    // build one covering every event name this route binds.
    var eventId = {};
    var internEntries = [];
    var nextId = 1;
    for (var i = 0; i < events.length; i++) {
      var name = events[i].event;
      if (eventId[name] === undefined) {
        eventId[name] = nextId++;
        internEntries.push({ id: eventId[name], value: name });
      }
    }
    if (internEntries.length) {
      vm.applyInstruction({ op: 'InitInternTable', table: { kind: 'Event', entries: internEntries } });
    }

    // Derived bindings: each `{slot-expr}` gets a synthetic slot so the patcher
    // re-applies it like any other binding; `depToDerived` maps each real slot to
    // the derived entries that must recompute when it changes.
    var derivedFns = [];     // [{ synthId, fn }]
    var depToDerived = {};   // realSlotId -> [index into derivedFns]
    for (var d = 0; d < derived.length; d++) {
      var dv = derived[d];
      var synthId = vmSlot('d' + d);
      if (dv.html) {
        // Conditionals rung: the recomputed value is raw branch HTML applied as
        // innerHTML to a `display:contents` wrapper (toggling a static subtree).
        vm.applyInstruction({ op: 'SetHtmlRef', stableId: dv.stableId, slotId: synthId });
      } else if (dv.attr) {
        vm.applyInstruction({ op: 'SetAttrRef', stableId: dv.stableId, attr: dv.attr, slotId: synthId });
      } else {
        vm.applyInstruction({ op: 'SetTextRef', stableId: dv.stableId, slotId: synthId });
      }
      derivedFns.push({ synthId: synthId, fn: compileThunk(dv.thunk) });
      for (var k = 0; k < dv.depSlots.length; k++) {
        var dep = dv.depSlots[k];
        (depToDerived[dep] || (depToDerived[dep] = [])).push(d);
      }
    }

    // Keyed-list bindings drive the delta sink's keyed reconciliation. Each gets
    // a per-install synthetic slot; `SetListRef` marks its wrapper as the anchor
    // (seeding row identity from the SSR `data-albedo-key` rows), and a recompute
    // rebuilds the ordered `{key, html}` row set from live state, which the sink
    // reconciles minimally (insert/remove/patch/move). `depToList` maps a real
    // slot to the list entries that must reconcile when it changes.
    var listFns = [];      // [{ listSlot, fn }]
    var depToList = {};    // realSlotId -> [index into listFns]
    function reconcileList(entry) {
      var rows = entry.fn(state);
      if (!Array.isArray(rows)) return;
      var out = [];
      for (var r = 0; r < rows.length; r++) {
        var rowKey = formatSlotValue(rows[r].key);
        out.push({ key: rowKey, html: stampRowKey(rows[r].html, rowKey) });
      }
      vm.applyInstruction({ op: 'ReconcileList', slotId: entry.listSlot, rows: out });
    }
    for (var l = 0; l < lists.length; l++) {
      var lb = lists[l];
      var listSlot = vmSlot('L' + l);
      vm.applyInstruction({ op: 'SetListRef', stableId: lb.stableId, slotId: listSlot });
      listFns.push({ listSlot: listSlot, fn: compileThunk(lb.rowsThunk) });
      var listDeps = lb.depSlots || [];
      for (var ld = 0; ld < listDeps.length; ld++) {
        (depToList[listDeps[ld]] || (depToList[listDeps[ld]] = [])).push(l);
      }
    }

    // The dispatcher is a ROUTER, not a replacement. For a proxy this island
    // owns, run the proven-client handler locally (emit each state write as a
    // `SlotSet`, recompute dependent derived bindings) with no network. For any
    // other proxy, fall through to the prior dispatcher — on a real bakabox
    // that is the server-action POST, so binding-mode and server actions can
    // coexist on the one VM. bakabox captures `this.eventDispatcher` at
    // `BindEvent` time, and this island binds its events below, so its
    // listeners resolve to this router while earlier islands keep theirs.
    var priorDispatcher = vm.eventDispatcher;
    vm.eventDispatcher = function (proxyId, event) {
      var thunk = handlers[String(proxyId)];
      if (typeof thunk !== 'function') {
        if (typeof priorDispatcher === 'function') priorDispatcher(proxyId, event);
        return;
      }
      var dirty = {};
      thunk(state, function (slotId, value) {
        state[slotId] = value;
        dirty[slotId] = true;
        vm.applyInstruction({
          op: 'SlotSet',
          slotId: vmSlot(slotId),
          value: encodeSlotValue(formatSlotValue(value)),
        });
      });
      // Recompute derived bindings whose dependency slots changed.
      var pending = {};
      for (var slot in dirty) {
        var list = depToDerived[slot];
        if (list) { for (var i = 0; i < list.length; i++) { pending[list[i]] = true; } }
      }
      for (var idx in pending) {
        var entry = derivedFns[idx];
        vm.applyInstruction({
          op: 'SlotSet',
          slotId: entry.synthId,
          value: encodeSlotValue(formatSlotValue(entry.fn(state))),
        });
      }
      // Reconcile keyed lists whose dependency slots changed.
      var pendingLists = {};
      for (var lslot in dirty) {
        var lentries = depToList[lslot];
        if (lentries) { for (var m = 0; m < lentries.length; m++) { pendingLists[lentries[m]] = true; } }
      }
      for (var lidx in pendingLists) { reconcileList(listFns[lidx]); }
    };

    // Register text + attr bindings before events so the first interaction's
    // SlotSet already has its target sites.
    for (var t = 0; t < texts.length; t++) {
      vm.applyInstruction({ op: 'SetTextRef', stableId: texts[t].stableId, slotId: vmSlot(texts[t].slotId) });
    }
    for (var a = 0; a < attrs.length; a++) {
      vm.applyInstruction({
        op: 'SetAttrRef',
        stableId: attrs[a].stableId,
        attr: attrs[a].attr,
        slotId: vmSlot(attrs[a].slotId),
      });
    }
    for (var e = 0; e < events.length; e++) {
      vm.applyInstruction({
        op: 'BindEvent',
        stableId: events[e].stableId,
        eventId: eventId[events[e].event],
        proxyId: events[e].proxyId,
      });
    }

    // Paint derived bindings once from initial state. SSR can't always render a
    // derived value (e.g. a `useMemo` the server-side renderer doesn't evaluate),
    // so compute it on install to make the first paint correct and consistent.
    for (var di = 0; di < derivedFns.length; di++) {
      vm.applyInstruction({
        op: 'SlotSet',
        slotId: derivedFns[di].synthId,
        value: encodeSlotValue(formatSlotValue(derivedFns[di].fn(state))),
      });
    }

    // Reconcile each keyed list once from initial state, adopting the SSR rows
    // into the sink's keyed row map so later changes reconcile minimally.
    for (var lp = 0; lp < listFns.length; lp++) {
      reconcileList(listFns[lp]);
    }

    return { state: state, handlers: handlers };
  }

  // `serve` entry point, classic-script half. This driver is inlined as a
  // classic `<script>` in the body, so it runs DURING parse — before the
  // deferred `runtime.js` module has constructed the one bakabox VM and
  // published `window.__bakabox`. So `boot` cannot install yet; it just queues
  // the payload. `runtime.js`, once it owns the VM, calls `drainReactiveQueue`
  // to install every queued island against it (the same classic-queues /
  // module-drains handshake `TIER_B_INJECT_BOOTSTRAP` uses for `__albedo_inject`).
  function boot(payload) {
    var queue = global.__ALBEDO_REACTIVE_QUEUE || (global.__ALBEDO_REACTIVE_QUEUE = []);
    queue.push(payload);
    // If the module happened to run first (unusual ordering under some
    // bundlers), install immediately rather than waiting for a drain that
    // already fired.
    if (global.__bakabox) drainReactiveQueue(global.__bakabox);
  }

  // Install every queued reactive payload against `vm` (the shared bakabox).
  // Shift-and-install so a redundant drain — e.g. `boot`'s eager path racing
  // the module bootstrap — finds an empty queue and can't double-install an
  // island (which would double-bind its events).
  function drainReactiveQueue(vm) {
    if (!vm) return;
    var queue = global.__ALBEDO_REACTIVE_QUEUE;
    if (!queue || !queue.length) return;
    var doc = global.document;
    var root = doc ? doc.body || doc.documentElement || doc : undefined;
    while (queue.length) {
      installReactiveRuntime({ vm: vm, payload: queue.shift(), root: root });
    }
  }

  var api = {
    installReactiveRuntime: installReactiveRuntime,
    formatSlotValue: formatSlotValue,
    boot: boot,
    drainReactiveQueue: drainReactiveQueue,
  };

  global.__albedoReactive = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);
