// SPDX-License-Identifier: MIT
// S1 unification proof — the REAL bakabox is the one binding-mode VM.
//
// Engine-expansion S1 retired the parallel `makeVm` that used to live in
// `assets/albedo-reactive.js`. Production now drives the real `Bakabox`
// (`assets/albedo-runtime.js`) through `installReactiveRuntime`. This test is
// the counterpart to the QuickJS `tests/reactive_bindings.rs` suite: that side
// proves the Rust payload → driver → handler loop against a contract-faithful
// stub; THIS side proves the shipped bakabox implements that same contract —
// text patch, `SetHtmlRef` innerHTML swap, and a dispatcher that ROUTES
// (local thunk for owned proxies, prior/server dispatcher for the rest).
//
// Node (not QuickJS) because `albedo-runtime.js` is an ES module; QuickJS can't
// `import` it. The classic-only `albedo-reactive.js` IIFE is loaded by evaluating
// its source so `globalThis.__albedoReactive` is defined, exactly as the inline
// `<script>` does in the browser.
//
// Run with: node --test tests/bakabox/reactive-unify.test.mjs

import { strict as assert } from 'node:assert';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { test } from 'node:test';

import { Bakabox, DEFAULT_ANCHOR_ATTRIBUTE } from '../../assets/albedo-runtime.js';

const __dirname = dirname(fileURLToPath(import.meta.url));

// Evaluate the classic driver IIFE so it publishes `globalThis.__albedoReactive`
// — the same side effect the inline `<script>` produces in the browser.
const reactiveSrc = readFileSync(
  resolve(__dirname, '..', '..', 'assets', 'albedo-reactive.js'),
  'utf8',
);
(0, eval)(reactiveSrc);

// ── Minimal DOM shim ─────────────────────────────────────────────────
//
// Only the surface `installReactiveRuntime` + bakabox touch: an anchored-node
// query, text/attr/innerHTML writes, and synchronous event dispatch.

class FakeElement {
  constructor(tagName) {
    this.tagName = tagName.toUpperCase();
    this.attributes = new Map();
    this.children = [];
    this.parentNode = null;
    this.eventListeners = new Map();
    this._textContent = '';
    this.innerHTML = '';
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }
  getAttribute(name) {
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }
  hasAttribute(name) {
    return this.attributes.has(name);
  }
  appendChild(child) {
    child.parentNode = this;
    this.children.push(child);
    return child;
  }
  addEventListener(name, handler) {
    let arr = this.eventListeners.get(name);
    if (!arr) this.eventListeners.set(name, (arr = []));
    arr.push(handler);
  }
  dispatchEventByName(name, event) {
    for (const handler of this.eventListeners.get(name) || []) handler(event);
  }
  get firstChild() {
    return this.children.length ? this.children[0] : null;
  }
  set textContent(value) {
    this._textContent = String(value);
    this.children.length = 0;
  }
  get textContent() {
    // Reflect child text nodes like a real DOM, so an untouched SSR text child
    // and an in-place `firstChild.nodeValue` patch both read back here.
    if (this.children.length) {
      return this.children
        .map((c) => (c.nodeType === 3 ? c.nodeValue : c.textContent))
        .join('');
    }
    return this._textContent;
  }
  walk(visit) {
    visit(this);
    for (const child of this.children) if (child.walk) child.walk(visit);
  }
}

class FakeText {
  constructor(text) {
    this.nodeType = 3;
    this.nodeValue = String(text);
  }
}

class FakeDocument {
  constructor() {
    this.documentElement = new FakeElement('html');
    this.body = new FakeElement('body');
    this.documentElement.appendChild(this.body);
  }
  createElement(tag) {
    return new FakeElement(tag);
  }
  createTextNode(text) {
    return new FakeText(text);
  }
  querySelectorAll(selector) {
    const match = /^\[([^=\]]+)\]$/.exec(selector);
    if (!match) throw new Error(`FakeDocument: unsupported selector ${selector}`);
    const out = [];
    this.documentElement.walk((node) => {
      if (node.hasAttribute(match[1])) out.push(node);
    });
    return out;
  }
}

// A counter-shaped island: a `{n}` text binding on a <span>, a click handler
// on a <button> that increments slot 0, and a derived `{n ? <p>n</p> : ''}`
// conditional exercising the SetHtmlRef → innerHTML path.
function counterPayload() {
  return {
    html: '',
    texts: [{ stableId: 1, slotId: 0 }],
    attrs: [],
    derived: [
      {
        stableId: 3,
        html: true,
        depSlots: [0],
        thunk:
          '(function (__state) { var n = __state[0]; return n ? "<p>" + n + "</p>" : ""; })',
      },
    ],
    events: [{ stableId: 2, event: 'click', proxyId: 7 }],
    handlers: [
      [
        7,
        '(function (__state, __emit) { var n = typeof __state[0] === "number" ? __state[0] : 0; __emit(0, n + 1); })',
      ],
    ],
  };
}

function mountCounter(doc) {
  const span = doc.createElement('span');
  span.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '1');
  span.appendChild(doc.createTextNode('0'));

  const button = doc.createElement('button');
  button.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '2');

  const panel = doc.createElement('div');
  panel.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '3');

  doc.body.appendChild(span);
  doc.body.appendChild(button);
  doc.body.appendChild(panel);
  return { span, button, panel };
}

// ── Tests ────────────────────────────────────────────────────────────

test('installReactiveRuntime drives the real bakabox: local text patch, zero network', () => {
  const doc = new FakeDocument();
  const { span, button } = mountCounter(doc);
  const bakabox = new Bakabox({ document: doc });

  globalThis.__albedoReactive.installReactiveRuntime({
    vm: bakabox,
    payload: counterPayload(),
    root: doc,
  });

  const serverTextNode = span.firstChild;
  assert.equal(span.textContent, '0', 'install leaves the SSR text untouched');

  button.dispatchEventByName('click', { type: 'click', target: button });
  assert.equal(span.textContent, '1', 'local click patched the bound text');
  button.dispatchEventByName('click', { type: 'click', target: button });
  assert.equal(span.textContent, '2', 'slot state persisted across clicks');

  // Fine-grained: the same server text node was mutated in place.
  assert.equal(span.firstChild, serverTextNode);
});

test('SetHtmlRef derived binding toggles innerHTML on the shared VM', () => {
  const doc = new FakeDocument();
  const { button, panel } = mountCounter(doc);
  const bakabox = new Bakabox({ document: doc });

  globalThis.__albedoReactive.installReactiveRuntime({
    vm: bakabox,
    payload: counterPayload(),
    root: doc,
  });

  assert.equal(panel.innerHTML, '', 'install paints the falsy branch (n undefined)');
  button.dispatchEventByName('click', { type: 'click', target: button });
  assert.equal(panel.innerHTML, '<p>1</p>', 'derived recompute swapped in the branch HTML');
});

test('the dispatcher is a router: owned proxy runs local, others fall through', () => {
  const doc = new FakeDocument();
  mountCounter(doc);

  // Prior dispatcher stands in for the server-action POST path bakabox installs
  // in its browser bootstrap. The router must preserve it for unowned proxies.
  const fellThrough = [];
  const bakabox = new Bakabox({
    document: doc,
    eventDispatcher: (proxyId, event) => fellThrough.push({ proxyId, event }),
  });

  globalThis.__albedoReactive.installReactiveRuntime({
    vm: bakabox,
    payload: counterPayload(),
    root: doc,
  });

  // Proxy 7 is owned by the island → handled locally, no fall-through.
  bakabox.eventDispatcher(7, { type: 'click' });
  assert.equal(fellThrough.length, 0, 'owned proxy must not reach the prior dispatcher');

  // Proxy 999 is unknown → routed to the prior (server-action) dispatcher.
  bakabox.eventDispatcher(999, { type: 'click' });
  assert.deepStrictEqual(fellThrough, [{ proxyId: 999, event: { type: 'click' } }]);
});

test('two islands on one VM keep disjoint slot keyspaces', () => {
  const doc = new FakeDocument();

  // Island A: span id 1 / button id 2 (slot 0).
  const aSpan = doc.createElement('span');
  aSpan.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '1');
  aSpan.appendChild(doc.createTextNode('0'));
  const aButton = doc.createElement('button');
  aButton.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '2');
  // Island B: reuses slot 0 but a different node (span id 11 / button id 12).
  const bSpan = doc.createElement('span');
  bSpan.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '11');
  bSpan.appendChild(doc.createTextNode('0'));
  const bButton = doc.createElement('button');
  bButton.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '12');
  for (const el of [aSpan, aButton, bSpan, bButton]) doc.body.appendChild(el);

  const bakabox = new Bakabox({ document: doc });
  const install = globalThis.__albedoReactive.installReactiveRuntime;

  install({
    vm: bakabox,
    payload: {
      html: '', texts: [{ stableId: 1, slotId: 0 }], attrs: [], derived: [],
      events: [{ stableId: 2, event: 'click', proxyId: 7 }],
      handlers: [[7, '(function (s, e) { e(0, (typeof s[0]==="number"?s[0]:0) + 1); })']],
    },
    root: doc,
  });
  install({
    vm: bakabox,
    payload: {
      html: '', texts: [{ stableId: 11, slotId: 0 }], attrs: [], derived: [],
      events: [{ stableId: 12, event: 'click', proxyId: 7 }],
      handlers: [[7, '(function (s, e) { e(0, (typeof s[0]==="number"?s[0]:0) + 1); })']],
    },
    root: doc,
  });

  // Clicking A must not paint B's span, despite both binding "slot 0".
  aButton.dispatchEventByName('click', { type: 'click', target: aButton });
  assert.equal(aSpan.textContent, '1', 'island A patched its own node');
  assert.equal(bSpan.textContent, '0', 'island B is untouched — slot keyspaces are disjoint');
});
