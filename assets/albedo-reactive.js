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

  function compileThunk(src) {
    if (typeof src === 'function') return src;
    // `src` is already `(function(__state,__emit){...})`. `new Function`
    // avoids leaking names into the enclosing scope and needs no indirect eval.
    return new Function('return (' + src + ');')();
  }

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
    var events = payload.events || [];
    var handlers = {};
    var sources = normaliseHandlers(payload.handlers);
    for (var pid in sources) {
      if (Object.prototype.hasOwnProperty.call(sources, pid)) {
        handlers[pid] = compileThunk(sources[pid]);
      }
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
      var synthId = 'd' + d;
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

    // The dispatcher IS the round-trip replacement: run the proven-client
    // handler locally, emit each state write as a `SlotSet`, then recompute any
    // derived binding whose dependencies changed. No network.
    vm.eventDispatcher = function (proxyId, event) {
      var thunk = handlers[String(proxyId)];
      if (typeof thunk !== 'function') return;
      var dirty = {};
      thunk(state, function (slotId, value) {
        state[slotId] = value;
        dirty[slotId] = true;
        vm.applyInstruction({
          op: 'SlotSet',
          slotId: slotId,
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
    };

    // Register text + attr bindings before events so the first interaction's
    // SlotSet already has its target sites.
    for (var t = 0; t < texts.length; t++) {
      vm.applyInstruction({ op: 'SetTextRef', stableId: texts[t].stableId, slotId: texts[t].slotId });
    }
    for (var a = 0; a < attrs.length; a++) {
      vm.applyInstruction({
        op: 'SetAttrRef',
        stableId: attrs[a].stableId,
        attr: attrs[a].attr,
        slotId: attrs[a].slotId,
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

    return { state: state, handlers: handlers };
  }

  // A self-contained patcher implementing the subset of the bakabox opcode
  // contract binding mode uses (`InitInternTable` Event / `SetTextRef` /
  // `BindEvent` / `SlotSet`) against a real `document`. This lets `serve` ship
  // ONE inline script with no module resolution and no separate asset to 404 on.
  // It is the same contract the full `albedo-runtime.js` bakabox implements;
  // production can later delegate to the bundled VM unchanged.
  function makeVm(doc) {
    var vm = {
      nodes: {},   // stableId -> element
      events: {},  // eventId -> name
      slots: {},   // slotId -> [stableId]
      eventDispatcher: function () {},
    };
    vm.seedNodesFromDocument = function (root) {
      var scope = root || doc;
      if (scope.querySelectorAll) {
        var matches = scope.querySelectorAll('[data-albedo-id]');
        for (var i = 0; i < matches.length; i++) {
          var el = matches[i];
          vm.nodes[el.getAttribute('data-albedo-id')] = el;
        }
        return;
      }
      var stack = [scope];
      while (stack.length) {
        var n = stack.pop();
        if (n && n.nodeType === 1) {
          var raw = n.getAttribute && n.getAttribute('data-albedo-id');
          if (raw !== null && raw !== undefined) vm.nodes[raw] = n;
          for (var c = n.childNodes.length - 1; c >= 0; c--) stack.push(n.childNodes[c]);
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
        case 'BindEvent':
          var bindEl = vm.nodes[op.stableId];
          var name = vm.events[op.eventId];
          if (bindEl && name && bindEl.addEventListener) {
            bindEl.addEventListener(name, function (ev) { vm.eventDispatcher(op.proxyId, ev); });
          }
          return;
        case 'SlotSet':
          var sites = vm.slots[op.slotId] || [];
          var text = (typeof op.value === 'string')
            ? op.value
            : (typeof TextDecoder !== 'undefined' ? new TextDecoder('utf-8').decode(op.value) : String(op.value));
          for (var s = 0; s < sites.length; s++) {
            var node = vm.nodes[sites[s].stableId];
            if (!node) continue;
            if (sites[s].kind === 'attr') {
              if (node.setAttribute) node.setAttribute(sites[s].attr, text);
            } else if (sites[s].kind === 'html') {
              // Conditional subtree toggle: swap the wrapper's children for the
              // pre-rendered branch HTML. `text` is trusted server-rendered
              // markup (the branch HTML the compiler emitted), not user input.
              node.innerHTML = text;
            } else if (node.firstChild && node.firstChild.nodeType === 3) {
              // In-place text patch — keep the same server-rendered node.
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
  }

  // `serve` entry point: build a patcher over the live document and wire one
  // route's reactive payload against the already-rendered static HTML.
  function boot(payload) {
    global.__albedoBoots = (global.__albedoBoots || 0) + 1;
    if (!global.document) return null;
    var doc = global.document;
    var root = doc.body || doc.documentElement || doc;
    var vm = makeVm(doc);
    var inst = installReactiveRuntime({ vm: vm, payload: payload, root: root });
    global.__albedoDiag = global.__albedoDiag || [];
    global.__albedoDiag.push({ nodes: Object.keys(vm.nodes).length, events: (payload.events || []).length });
    return inst;
  }

  var api = {
    installReactiveRuntime: installReactiveRuntime,
    formatSlotValue: formatSlotValue,
    makeVm: makeVm,
    boot: boot,
  };

  global.__albedoReactive = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);
