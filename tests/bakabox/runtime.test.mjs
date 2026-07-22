// SPDX-License-Identifier: MIT
// bakabox VM end-to-end test — applies the canonical conformance frame
// against a minimal DOM shim and asserts the DOM mutations bakabox
// produces.
//
// This is the JS-side counterpart of
// `crates/albedo-server/tests/server_wire_integration.rs`: the Rust side
// proves bytes flow out of the pipeline; this side proves they land
// correctly in the DOM. Together they close the round trip.
//
// The DOM shim is intentionally tiny — bakabox only needs Element,
// createElement, getElementById, querySelectorAll, setAttribute /
// getAttribute / removeAttribute, textContent, appendChild,
// removeChild, addEventListener, and parentNode. Anything more is
// added on demand, not speculatively.
//
// Run with: node --test tests/bakabox/runtime.test.mjs

import { strict as assert } from 'node:assert';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { test } from 'node:test';

import {
  Bakabox,
  DEFAULT_ANCHOR_ATTRIBUTE,
  DEFAULT_SUSPENSE_ATTRIBUTE,
  createBakabox,
} from '../../assets/albedo-runtime.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURE_PATH = resolve(
  __dirname,
  '..',
  'fixtures',
  'wire',
  'v5_canonical_frame.bin',
);

// ── Minimal DOM shim ─────────────────────────────────────────────────
//
// Implements only the surface bakabox touches. Kept inline (no test
// dependency on jsdom / linkedom) so this test runs against vanilla
// Node — the same way CI will run it.

class FakeElement {
  constructor(tagName) {
    this.tagName = tagName.toUpperCase();
    this.attributes = new Map();
    this.children = [];
    this.parentNode = null;
    this.eventListeners = new Map();
    this._textContent = '';
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  getAttribute(name) {
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }

  removeAttribute(name) {
    this.attributes.delete(name);
  }

  hasAttribute(name) {
    return this.attributes.has(name);
  }

  appendChild(child) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentNode = this;
    this.children.push(child);
    return child;
  }

  removeChild(child) {
    const idx = this.children.indexOf(child);
    if (idx >= 0) this.children.splice(idx, 1);
    child.parentNode = null;
    return child;
  }

  addEventListener(name, handler) {
    let arr = this.eventListeners.get(name);
    if (!arr) {
      arr = [];
      this.eventListeners.set(name, arr);
    }
    arr.push(handler);
  }

  dispatchEventByName(name, event) {
    const arr = this.eventListeners.get(name) || [];
    for (const handler of arr) handler(event);
  }

  get textContent() {
    return this._textContent;
  }

  set textContent(value) {
    this._textContent = String(value);
    this.children.length = 0;
  }

  // Recursive descent for the seed/query helpers below.
  walk(visit) {
    visit(this);
    for (const child of this.children) child.walk(visit);
  }
}

class FakeDocument {
  constructor() {
    this.documentElement = new FakeElement('html');
    this.body = new FakeElement('body');
    this.documentElement.appendChild(this.body);
    this._byId = new Map();
  }

  createElement(tagName) {
    return new FakeElement(tagName);
  }

  getElementById(id) {
    return this._byId.get(id) || null;
  }

  registerId(id, element) {
    this._byId.set(id, element);
  }

  querySelectorAll(selector) {
    // bakabox only ever passes `[<attr>]` — implement that one form.
    const match = /^\[([^=\]]+)\]$/.exec(selector);
    if (!match) {
      throw new Error(`FakeDocument: unsupported selector ${selector}`);
    }
    const attr = match[1];
    const out = [];
    this.documentElement.walk((node) => {
      if (node.hasAttribute(attr)) out.push(node);
    });
    return out;
  }
}

// ── Tests ────────────────────────────────────────────────────────────

function loadFixtureBytes() {
  const buffer = readFileSync(FIXTURE_PATH);
  return new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength);
}

test('Bakabox refuses to construct without a document', () => {
  assert.throws(() => new Bakabox({}), TypeError);
});

test('seedNodesFromDocument registers every anchored element', () => {
  const doc = new FakeDocument();
  const anchored = doc.createElement('div');
  anchored.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '7');
  const orphan = doc.createElement('span');
  doc.body.appendChild(anchored);
  doc.body.appendChild(orphan);

  const bakabox = createBakabox({ document: doc });
  const seeded = bakabox.seedNodesFromDocument();

  assert.equal(seeded, 1, 'only anchored elements seed into the map');
  assert.equal(bakabox.nodes.get(7), anchored);
  assert.equal(bakabox.nodes.size, 1);
});

test('applyFrameBytes processes the canonical fixture end-to-end', () => {
  const doc = new FakeDocument();
  const bakabox = createBakabox({ document: doc });

  // Pre-seed the parent node for the Append{parent:0, child:1} op and the
  // target for Remove{stable_id:99}. In production the shell renderer
  // emits these as `data-albedo-id` anchors in the initial HTML.
  const root = doc.createElement('root');
  root.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '0');
  doc.body.appendChild(root);
  bakabox.seedNodesFromDocument();

  const doomed = doc.createElement('div');
  doomed.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '99');
  doc.body.appendChild(doomed);
  bakabox.nodes.set(99, doomed);

  // Pre-seed the Event intern table. The canonical fixture exercises
  // every Instruction variant exactly once; InitInternTable only fires
  // once (for Tag), so Event interns must arrive via a separate bootstrap
  // chunk in production. The Rust-side bootstrap (`drain_bootstrap_intern_chunk`)
  // ships all three intern kinds — the JS test mirrors that here.
  bakabox.events.set(0, 'click');

  const frame = bakabox.applyFrameBytes(loadFixtureBytes());

  assert.equal(frame.frameId, 1n);
  assert.equal(frame.componentId, 42n);
  // 18 in wire v5 — S2 added SlotDelta at index 15, insert-position work added
  // ReconcileList at index 16 and SlotInsert at index 17. The three list ops do
  // no DOM work here: slot 11 has no list anchor bound, so the keyed ones
  // buffer into `pendingListOps` and ReconcileList no-ops.
  assert.equal(frame.instructions.length, 18);

  // Intern tables populated.
  assert.equal(bakabox.tags.get(0), 'div');
  assert.equal(bakabox.tags.get(1), 'span');
  assert.equal(bakabox.attrs.get(0), 'class');
  // attr id 9 was Set then Removed by the fixture's PatchInternTable.
  assert.equal(bakabox.attrs.has(9), false);

  // Create{tag_id:0 ("div"), stable_id:1} produced an element.
  const created = bakabox.nodes.get(1);
  assert.ok(created, 'Create must register stable_id 1');
  assert.equal(created.tagName, 'DIV');
  assert.equal(created.getAttribute(DEFAULT_ANCHOR_ATTRIBUTE), '1');

  // SetAttr{attr:"class", value:"root"} held the attribute through the
  // tail of the frame — no SlotSet pushed to slot 12 (the attr slot).
  assert.equal(created.getAttribute('class'), 'root');

  // SetText{text:"hello bakabox"} ran first, but a later
  // SlotSet{slot_id:11, value:"reactive-value"} re-applied the text-ref
  // binding registered by SetTextRef. Final value reflects the slot push.
  assert.equal(created.textContent, 'reactive-value');

  // Append{parent:0, child:1} reparented.
  assert.equal(created.parentNode, root);

  // Remove{stable_id:99} dropped the element from both map and DOM.
  assert.equal(bakabox.nodes.has(99), false);
  assert.equal(doomed.parentNode, null);

  // Slot 11 has two bindings registered by SetTextRef + the
  // bare-sentinel BindSlot for slot 3 was separate. Slot 12 holds the
  // attr-ref binding but had no matching SlotSet — the attribute stays
  // at its static value.
  assert.equal(bakabox.slots.get(11)?.length ?? 0, 1);
  assert.equal(bakabox.slots.get(12)?.length ?? 0, 1);
  assert.equal(bakabox.slots.get(3)?.length ?? 0, 1); // BindSlot sentinel
});

test('BindEvent without intern table seed surfaces a usable error', () => {
  const doc = new FakeDocument();
  const bakabox = createBakabox({ document: doc });
  const el = doc.createElement('button');
  el.setAttribute(DEFAULT_ANCHOR_ATTRIBUTE, '1');
  bakabox.nodes.set(1, el);

  assert.throws(
    () =>
      bakabox.applyInstruction({
        op: 'BindEvent',
        stableId: 1,
        eventId: 7,
        proxyId: 99,
      }),
    /unknown event intern id 7/,
  );
});

test('BindEvent + SlotSet wires server-driven values to the DOM', () => {
  const doc = new FakeDocument();
  const dispatchedEvents = [];
  const bakabox = createBakabox({
    document: doc,
    eventDispatcher: (proxyId, event) =>
      dispatchedEvents.push({ proxyId, event }),
  });

  // Seed intern tables manually (the WT bootstrap does this from the
  // bootstrap intern chunk in production).
  bakabox.tags.set(0, 'button');
  bakabox.attrs.set(0, 'data-label');
  bakabox.events.set(0, 'click');

  bakabox.applyInstruction({ op: 'Create', tagId: 0, stableId: 1 });
  const button = bakabox.nodes.get(1);
  doc.body.appendChild(button);

  bakabox.applyInstruction({
    op: 'BindEvent',
    stableId: 1,
    eventId: 0,
    proxyId: 42,
  });
  bakabox.applyInstruction({ op: 'SetTextRef', stableId: 1, slotId: 5 });
  bakabox.applyInstruction({
    op: 'SetAttrRef',
    stableId: 1,
    attrId: 0,
    slotId: 5,
  });

  // Fire the bound event — must call the injected dispatcher with proxyId=42.
  const fakeEvent = { type: 'click' };
  button.dispatchEventByName('click', fakeEvent);
  assert.equal(dispatchedEvents.length, 1);
  assert.deepStrictEqual(dispatchedEvents[0], { proxyId: 42, event: fakeEvent });

  // SlotSet — both bindings (text + attr) must reflect the same new value.
  bakabox.applyInstruction({
    op: 'SlotSet',
    slotId: 5,
    value: new TextEncoder().encode('Click me'),
  });
  assert.equal(button.textContent, 'Click me');
  assert.equal(button.getAttribute('data-label'), 'Click me');
});

test('Placeholder registers in pending and is cleared by Patch', () => {
  const doc = new FakeDocument();
  const bakabox = createBakabox({ document: doc });

  bakabox.applyInstruction({
    op: 'Placeholder',
    stableId: 50,
    suspenseId: 99,
  });

  const placeholder = bakabox.nodes.get(50);
  assert.ok(placeholder, 'Placeholder registers the stable id');
  assert.equal(placeholder.tagName, 'SPAN');
  assert.equal(placeholder.getAttribute(DEFAULT_SUSPENSE_ATTRIBUTE), '99');
  assert.equal(bakabox.pending.get(99), placeholder);

  bakabox.applyInstruction({ op: 'Patch', suspenseId: 99, range: { start: 0, end: 0 } });
  assert.equal(bakabox.pending.has(99), false, 'Patch must clear pending');
});

test('Remove cleans up pending entries for the same element', () => {
  const doc = new FakeDocument();
  const bakabox = createBakabox({ document: doc });

  bakabox.applyInstruction({
    op: 'Placeholder',
    stableId: 50,
    suspenseId: 99,
  });
  doc.body.appendChild(bakabox.nodes.get(50));

  bakabox.applyInstruction({ op: 'Remove', stableId: 50 });

  assert.equal(bakabox.nodes.has(50), false);
  assert.equal(bakabox.pending.has(99), false);
});

test('unknown opcode shape throws BakaboxWireError', () => {
  const doc = new FakeDocument();
  const bakabox = createBakabox({ document: doc });
  assert.throws(
    () => bakabox.applyInstruction({ op: 'NotAnOpcode' }),
    /unknown opcode 'NotAnOpcode'/,
  );
});

test('Navigate opcode drives the document window location', () => {
  // Inject a fake window via `document.defaultView` — bakabox prefers
  // it over `globalThis.location` so SSR / test harnesses can
  // intercept without monkey-patching the global scope.
  const navigated = [];
  const doc = new FakeDocument();
  doc.defaultView = {
    location: {
      assign(url) {
        navigated.push(url);
      },
    },
  };
  const bakabox = createBakabox({ document: doc });
  bakabox.applyInstruction({ op: 'Navigate', url: '/dashboard?ack=1' });
  assert.deepStrictEqual(navigated, ['/dashboard?ack=1']);
});

test('Navigate opcode is a no-op when no window is reachable', () => {
  const doc = new FakeDocument();
  // No `defaultView` set; the FakeDocument has no `location` either.
  const bakabox = createBakabox({ document: doc });
  // Must not throw.
  bakabox.applyInstruction({ op: 'Navigate', url: '/nope' });
});
