// SPDX-License-Identifier: MIT
// S2 delta sink — the VM applies a z-set `SlotDelta` to keyed-list DOM.
//
// Two levels:
//   1. `reconcileSlotDelta` — the pure algebra (no DOM): does a batch of signed
//      changes reduce to the right insert/remove/patch plan? This is where the
//      S0 finding is pinned: an update (retract old + insert new, same key) must
//      become a PATCH, never cancel to a no-op.
//   2. `_opSetListRef` + `_opSlotDelta` on the real `Bakabox`, against a DOM
//      shim, including the differential oracle: apply(Δ) must equal a
//      from-scratch full render of the resulting logical list.
//
// Run with: node --test tests/bakabox/slot-delta-sink.test.mjs

import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { Bakabox, reconcileSlotDelta } from '../../assets/albedo-runtime.js';

// ── Minimal DOM shim with single-element HTML parsing ────────────────
//
// The sink instantiates rows from server-rendered HTML via
// `createRange().createContextualFragment`. Rows are simple
// `<li data-albedo-key="k">text</li>` elements, so a one-element parser is
// enough — no jsdom dependency, same discipline as the sibling suites.

class FakeElement {
  constructor(tagName) {
    this.tagName = tagName.toUpperCase();
    this.attributes = new Map();
    this.childNodes = [];
    this.parentNode = null;
    this._textContent = '';
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
  get children() {
    return this.childNodes.filter((n) => n.nodeType === 1);
  }
  appendChild(child) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentNode = this;
    this.childNodes.push(child);
    return child;
  }
  removeChild(child) {
    const idx = this.childNodes.indexOf(child);
    if (idx >= 0) this.childNodes.splice(idx, 1);
    child.parentNode = null;
    return child;
  }
  replaceChild(next, prev) {
    const idx = this.childNodes.indexOf(prev);
    if (idx < 0) return prev;
    if (next.parentNode) next.parentNode.removeChild(next);
    this.childNodes[idx] = next;
    next.parentNode = this;
    prev.parentNode = null;
    return prev;
  }
  set textContent(value) {
    this._textContent = String(value);
    this.childNodes = [];
  }
  get textContent() {
    return this._textContent;
  }
  get nodeType() {
    return 1;
  }
  /**
   * The DOM's own serialization of this element. Real elements have it; the
   * sink reads it when adopting an SSR row so a later resync carrying the same
   * markup is recognised as a no-op. Reproduced here in the SERVER's attribute
   * shape (`data-albedo-key="…"`), which is what a browser round-trips for the
   * simple rows this sink handles.
   */
  get outerHTML() {
    const tag = this.tagName.toLowerCase();
    const attrs = [...this.attributes.entries()]
      .map(([name, value]) => ` ${name}="${value}"`)
      .join('');
    return `<${tag}${attrs}>${this._textContent}</${tag}>`;
  }

  /** Serialize as `<tag key=…>text</tag>` for structural comparison. */
  serialize() {
    const key = this.getAttribute('data-albedo-key');
    return `<${this.tagName.toLowerCase()} key=${key}>${this._textContent}</${this.tagName.toLowerCase()}>`;
  }
}

class FakeDocument {
  createElement(tag) {
    return new FakeElement(tag);
  }
  createRange() {
    return {
      createContextualFragment: (html) => {
        const el = parseSingleElement(html, (t) => new FakeElement(t));
        return { firstElementChild: el, firstChild: el };
      },
    };
  }
}

/** Parses one `<tag attr="v" ...>text</tag>` into a FakeElement. */
function parseSingleElement(html, make) {
  const m = /^\s*<([a-zA-Z0-9]+)([^>]*)>([\s\S]*?)<\/\1>\s*$/.exec(html);
  if (!m) return null;
  const el = make(m[1]);
  const attrRe = /([a-zA-Z0-9-]+)="([^"]*)"/g;
  let a;
  while ((a = attrRe.exec(m[2]))) el.setAttribute(a[1], a[2]);
  el.textContent = m[3];
  return el;
}

const enc = (s) => new TextEncoder().encode(s);

/** Server-rendered row markup, the payload a `+` change carries. */
function rowHtml(key, text) {
  return `<li data-albedo-key="${key}">${text}</li>`;
}

/** Build a keyed-list anchor pre-seeded with SSR rows, register it, return it. */
function mountList(bakabox, doc, slotId, anchorId, rows) {
  const anchor = doc.createElement('ul');
  anchor.setAttribute('data-albedo-id', String(anchorId));
  for (const [key, text] of rows) {
    anchor.appendChild(parseSingleElement(rowHtml(key, text), (t) => new FakeElement(t)));
  }
  bakabox.nodes.set(anchorId, anchor);
  bakabox.applyInstruction({ op: 'SetListRef', stableId: anchorId, slotId });
  return anchor;
}

/** Serialize an anchor's element children — the shape the oracle compares. */
function serializeList(anchor) {
  return anchor.children.map((c) => c.serialize()).join('');
}

/** Full render of an ordered [key,text] list — the oracle's ground truth. */
function fullRender(doc, rows) {
  const anchor = doc.createElement('ul');
  for (const [key, text] of rows) {
    anchor.appendChild(parseSingleElement(rowHtml(key, text), (t) => new FakeElement(t)));
  }
  return serializeList(anchor);
}

// ── 1. Pure reconciliation ───────────────────────────────────────────

test('reconcileSlotDelta: lone insert of an absent key', () => {
  const plan = reconcileSlotDelta([{ weight: 1, key: 'a', payload: 'A' }], () => false);
  assert.deepStrictEqual(plan, [{ action: 'insert', key: 'a', payload: 'A' }]);
});

test('reconcileSlotDelta: lone retract of a present key removes; absent key is a no-op', () => {
  assert.deepStrictEqual(
    reconcileSlotDelta([{ weight: -1, key: 'a', payload: '' }], (k) => k === 'a'),
    [{ action: 'remove', key: 'a' }],
  );
  assert.deepStrictEqual(
    reconcileSlotDelta([{ weight: -1, key: 'ghost', payload: '' }], () => false),
    [],
  );
});

test('reconcileSlotDelta: an UPDATE (retract old + insert new, same key) is a patch, never a no-op', () => {
  // The S0 finding. A weight-summing coalescer would net these to 0 and drop
  // the edit. We must emit a single in-place patch to the new payload.
  const changes = [
    { weight: -1, key: 'a', payload: 'OLD' },
    { weight: 1, key: 'a', payload: 'NEW' },
  ];
  const plan = reconcileSlotDelta(changes, (k) => k === 'a');
  assert.deepStrictEqual(plan, [{ action: 'patch', key: 'a', payload: 'NEW' }]);
});

test('reconcileSlotDelta: keys emit in first-seen (delta/query) order', () => {
  const plan = reconcileSlotDelta(
    [
      { weight: 1, key: 'x', payload: 'X' },
      { weight: 1, key: 'y', payload: 'Y' },
      { weight: 1, key: 'z', payload: 'Z' },
    ],
    () => false,
  );
  assert.deepStrictEqual(plan.map((s) => s.key), ['x', 'y', 'z']);
});

// ── 2. DOM sink ───────────────────────────────────────────────────────

test('SlotDelta inserts a new row and removes an existing one by key', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);

  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: 5,
    changes: [
      { weight: 1, key: 'c', payload: enc(rowHtml('c', 'carol')) },
      { weight: -1, key: 'a', payload: enc('') },
    ],
  });

  assert.equal(
    serializeList(anchor),
    '<li key=b>bob</li><li key=c>carol</li>',
    'a removed, c appended, b untouched',
  );
});

test('SlotDelta patches a row in place, preserving its position', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob'], ['c', 'carol']]);
  const bNodeBefore = anchor.children[1];

  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: 5,
    changes: [
      { weight: -1, key: 'b', payload: enc(rowHtml('b', 'bob')) },
      { weight: 1, key: 'b', payload: enc(rowHtml('b', 'BOBBY')) },
    ],
  });

  assert.equal(
    serializeList(anchor),
    '<li key=a>alice</li><li key=b>BOBBY</li><li key=c>carol</li>',
    'b updated in the middle, a and c unmoved',
  );
  assert.notEqual(anchor.children[1], bNodeBefore, 'patched node is the new payload');
});

test('SlotDelta arriving before SetListRef is buffered, then replayed on bind', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });

  // Delta first — no anchor yet.
  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: 9,
    changes: [{ weight: 1, key: 'a', payload: enc(rowHtml('a', 'alice')) }],
  });
  assert.equal(bakabox.pendingListOps.get(9)?.length, 1, 'buffered until bound');

  // Now the anchor binds — the buffered delta replays.
  const anchor = doc.createElement('ul');
  anchor.setAttribute('data-albedo-id', '2');
  bakabox.nodes.set(2, anchor);
  bakabox.applyInstruction({ op: 'SetListRef', stableId: 2, slotId: 9 });

  assert.equal(serializeList(anchor), '<li key=a>alice</li>');
  assert.equal(bakabox.pendingListOps.has(9), false, 'buffer drained');
});

// ── 3. Differential oracle: apply(Δ) ≡ full_render ────────────────────

test('oracle: apply(Δ) equals a full render of the resulting list (insert + remove + update)', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);

  // One mixed delta: remove a, update b, insert c.
  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: 5,
    changes: [
      { weight: -1, key: 'a', payload: enc('') },
      { weight: -1, key: 'b', payload: enc(rowHtml('b', 'bob')) },
      { weight: 1, key: 'b', payload: enc(rowHtml('b', 'bobby')) },
      { weight: 1, key: 'c', payload: enc(rowHtml('c', 'carol')) },
    ],
  });

  // Resulting logical list, in DOM order (b kept its place, c appended).
  const expected = fullRender(doc, [['b', 'bobby'], ['c', 'carol']]);
  assert.equal(serializeList(anchor), expected, 'incremental apply ≡ full render');
});

// ── Resync (S4 · backpressure recovery) ──────────────────────────────

test('a patch carrying identical markup leaves the row node untouched', () => {
  // The resync a reconnecting client receives re-asserts EVERY row. Rebuilding
  // each one would throw away the DOM identity (focus, selection, scroll) of
  // rows that never changed — turning recovery into a full repaint.
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  mountList(bakabox, doc, 9, 1, [['a', 'alice'], ['b', 'bob']]);
  const before = bakabox.listSlots.get(9).rowsByKey.get('a');

  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: 9,
    changes: [
      { weight: 1, key: 'a', payload: enc(rowHtml('a', 'alice')) },
      { weight: 1, key: 'b', payload: enc(rowHtml('b', 'bob')) },
    ],
  });

  assert.equal(
    bakabox.listSlots.get(9).rowsByKey.get('a'),
    before,
    'an unchanged row must keep its identity across a resync',
  );
});

test('a resync still lands the rows the client actually missed', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 9, 1, [['a', 'alice']]);
  const untouched = bakabox.listSlots.get(9).rowsByKey.get('a');

  // What the server re-asserts: the row this client has, one it never saw,
  // and an edit to one it has.
  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: 9,
    changes: [
      { weight: 1, key: 'a', payload: enc(rowHtml('a', 'alice')) },
      { weight: 1, key: 'b', payload: enc(rowHtml('b', 'bob')) },
    ],
  });

  assert.equal(serializeList(anchor), fullRender(doc, [['a', 'alice'], ['b', 'bob']]));
  assert.equal(
    bakabox.listSlots.get(9).rowsByKey.get('a'),
    untouched,
    'recovery costs only what changed',
  );
});

test('a resync patch with different markup does replace the row', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 9, 1, [['a', 'alice']]);
  const before = bakabox.listSlots.get(9).rowsByKey.get('a');

  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: 9,
    changes: [{ weight: 1, key: 'a', payload: enc(rowHtml('a', 'ALICE')) }],
  });

  assert.notEqual(bakabox.listSlots.get(9).rowsByKey.get('a'), before, 'a real change must land');
  assert.equal(serializeList(anchor), fullRender(doc, [['a', 'ALICE']]));
});
