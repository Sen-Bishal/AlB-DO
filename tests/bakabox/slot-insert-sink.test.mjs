// SPDX-License-Identifier: MIT
// C2 positioned-insert sink — the VM applies `SlotInsert` to keyed-list DOM.
//
// `SlotDelta` inserts only at the tail and `ReconcileList` re-asserts the whole
// view; `SlotInsert { slotId, before, rows }` is the rung between them — rows
// land ahead of a named `RowKey` (or at the tail for `before: null`) without
// the server having to re-render or re-send the rest of the list.
//
// What's pinned here:
//   1. Position — rows land before the anchor, in `rows` order, and untouched
//      rows keep their DOM node identity (the whole point over a rebuild).
//   2. Fallback — an anchor key the client doesn't hold degrades to a tail
//      append rather than dropping the row. Correctness-via-fallback: a
//      misplaced row is repaired by the next resync, a lost one is not.
//   3. Buffering — an insert arriving before its anchor binds is replayed in
//      arrival order, interleaved with `SlotDelta`, through `pendingListOps`.
//
// Run with: node --test tests/bakabox/slot-insert-sink.test.mjs

import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { Bakabox } from '../../assets/albedo-runtime.js';

// ── Minimal DOM shim ─────────────────────────────────────────────────
//
// Self-contained, like the sibling suites. Beyond `slot-delta-sink`'s shim
// this one implements `insertBefore`, which is the DOM primitive the sink
// under test is built on — including its `null`-reference-node contract
// (insert at the end) and its NotFoundError for a foreign reference node.

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
  get children() {
    return this.childNodes.filter((n) => n.nodeType === 1);
  }
  appendChild(child) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentNode = this;
    this.childNodes.push(child);
    return child;
  }
  /**
   * Per DOM: `reference === null` appends; otherwise the node is placed
   * immediately before `reference`, moving it if it is already in the tree. A
   * `reference` that is not a child throws NotFoundError — the shim keeps that
   * throw so a regression in the sink's `parentNode === anchor` guard fails
   * loudly here instead of silently mis-ordering in a browser.
   */
  insertBefore(child, reference) {
    if (reference === null || reference === undefined) return this.appendChild(child);
    const idx = this.childNodes.indexOf(reference);
    if (idx < 0) {
      throw new Error('NotFoundError: reference node is not a child of this node');
    }
    if (child.parentNode) child.parentNode.removeChild(child);
    // Re-read the index: detaching `child` above may have shifted `reference`.
    this.childNodes.splice(this.childNodes.indexOf(reference), 0, child);
    child.parentNode = this;
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
  get outerHTML() {
    const tag = this.tagName.toLowerCase();
    const attrs = [...this.attributes.entries()]
      .map(([name, value]) => ` ${name}="${value}"`)
      .join('');
    return `<${tag}${attrs}>${this._textContent}</${tag}>`;
  }
  serialize() {
    const tag = this.tagName.toLowerCase();
    return `<${tag} key=${this.getAttribute('data-albedo-key')}>${this._textContent}</${tag}>`;
  }
}

class FakeDocument {
  createElement(tag) {
    return new FakeElement(tag);
  }
  createRange() {
    return {
      createContextualFragment: (html) => {
        const el = parseSingleElement(html);
        return { firstElementChild: el, firstChild: el };
      },
    };
  }
}

/** Parses one `<tag attr="v" ...>text</tag>` into a FakeElement. */
function parseSingleElement(html) {
  const m = /^\s*<([a-zA-Z0-9]+)([^>]*)>([\s\S]*?)<\/\1>\s*$/.exec(html);
  if (!m) return null;
  const el = new FakeElement(m[1]);
  const attrRe = /([a-zA-Z0-9-]+)="([^"]*)"/g;
  let a;
  while ((a = attrRe.exec(m[2]))) el.setAttribute(a[1], a[2]);
  el.textContent = m[3];
  return el;
}

const enc = (s) => new TextEncoder().encode(s);

/** Server-rendered row markup — the payload a `SlotInsert` row carries. */
function rowHtml(key, text) {
  return `<li data-albedo-key="${key}">${text}</li>`;
}

/** Build a keyed-list anchor pre-seeded with SSR rows, register it, return it. */
function mountList(bakabox, doc, slotId, anchorId, rows) {
  const anchor = doc.createElement('ul');
  anchor.setAttribute('data-albedo-id', String(anchorId));
  for (const [key, text] of rows) anchor.appendChild(parseSingleElement(rowHtml(key, text)));
  bakabox.nodes.set(anchorId, anchor);
  bakabox.applyInstruction({ op: 'SetListRef', stableId: anchorId, slotId });
  return anchor;
}

const serializeList = (anchor) => anchor.children.map((c) => c.serialize()).join('');

/** A `SlotInsert` carrying byte payloads, as it arrives off the wire. */
function slotInsert(slotId, before, rows) {
  return {
    op: 'SlotInsert',
    slotId,
    before,
    rows: rows.map(([key, text]) => ({ key, html: enc(rowHtml(key, text)) })),
  };
}

// ── 1. Position ──────────────────────────────────────────────────────

test('inserts before the named anchor row', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);

  bakabox.applyInstruction(slotInsert(5, 'b', [['m', 'mid']]));

  assert.equal(
    serializeList(anchor),
    '<li key=a>alice</li><li key=m>mid</li><li key=b>bob</li>',
    'm lands between a and b',
  );
});

test('head insert — the reverse-chron case this opcode exists for', () => {
  // A `created_at DESC` feed puts every new row at the head. Under v4 that
  // classified as a non-tail insert and shipped a whole-view ReconcileList.
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['b', 'bob'], ['a', 'alice']]);
  const bNode = anchor.children[0];

  bakabox.applyInstruction(slotInsert(5, 'b', [['c', 'carol']]));

  assert.equal(
    serializeList(anchor),
    '<li key=c>carol</li><li key=b>bob</li><li key=a>alice</li>',
  );
  assert.equal(anchor.children[1], bNode, 'the anchor row keeps its DOM node');
});

test('null anchor appends at the tail', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice']]);

  bakabox.applyInstruction(slotInsert(5, null, [['b', 'bob']]));

  assert.equal(serializeList(anchor), '<li key=a>alice</li><li key=b>bob</li>');
});

test('multiple rows land in `rows` order, all before the anchor', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['z', 'zoe']]);

  bakabox.applyInstruction(slotInsert(5, 'z', [['x', 'xander'], ['y', 'yuki']]));

  assert.equal(
    serializeList(anchor),
    '<li key=a>alice</li><li key=x>xander</li><li key=y>yuki</li><li key=z>zoe</li>',
  );
});

test('untouched rows keep their DOM node identity', () => {
  // The reason this isn't just an innerHTML rebuild: an unrelated row's focus,
  // selection, scroll and running animations survive an insert elsewhere.
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);
  const [aNode, bNode] = anchor.children;

  bakabox.applyInstruction(slotInsert(5, 'b', [['m', 'mid']]));

  assert.equal(anchor.children[0], aNode, 'a is the same node');
  assert.equal(anchor.children[2], bNode, 'b is the same node');
});

test('retracts nothing — a row absent from `rows` is left in place', () => {
  // Unlike ReconcileList, this op asserts a position, not a set.
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);

  bakabox.applyInstruction(slotInsert(5, null, [['c', 'carol']]));

  assert.equal(
    serializeList(anchor),
    '<li key=a>alice</li><li key=b>bob</li><li key=c>carol</li>',
  );
});

// ── 2. Fallback and idempotence ──────────────────────────────────────

test('an anchor key the client does not hold degrades to a tail append', () => {
  // Dropping the row would strand it until a navigation; appending misplaces
  // it only until the next resync ReconcileList re-asserts the order.
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice']]);

  bakabox.applyInstruction(slotInsert(5, 'ghost', [['b', 'bob']]));

  assert.equal(serializeList(anchor), '<li key=a>alice</li><li key=b>bob</li>');
});

test('a stale rowsByKey entry for a detached node degrades instead of throwing', () => {
  // `insertBefore` throws NotFoundError for a reference node that is not a
  // child — the sink's `parentNode === anchor` guard is what prevents it.
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);
  const bNode = bakabox.listSlots.get(5).rowsByKey.get('b');
  anchor.removeChild(bNode); // detached, but still mapped

  bakabox.applyInstruction(slotInsert(5, 'b', [['m', 'mid']]));

  assert.equal(serializeList(anchor), '<li key=a>alice</li><li key=m>mid</li>');
});

test('re-inserting an existing key moves that row rather than duplicating it', () => {
  // Makes a redelivered op idempotent — and a duplicate key would break the
  // rowsByKey invariant the delta sink shares.
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob'], ['c', 'carol']]);
  const cNode = anchor.children[2];

  bakabox.applyInstruction(slotInsert(5, 'a', [['c', 'carol']]));

  assert.equal(
    serializeList(anchor),
    '<li key=c>carol</li><li key=a>alice</li><li key=b>bob</li>',
    'c moved to the head, not duplicated',
  );
  assert.equal(anchor.children.length, 3);
  assert.equal(anchor.children[0], cNode, 'unchanged markup reuses the node');
});

test('an existing key with changed markup is re-rendered at the new position', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);

  bakabox.applyInstruction(slotInsert(5, 'a', [['b', 'BOBBY']]));

  assert.equal(serializeList(anchor), '<li key=b>BOBBY</li><li key=a>alice</li>');
  assert.equal(anchor.children.length, 2, 'the old b node is gone, not orphaned');
});

test('an unbound slot is a no-op on an anchorless list, not a throw', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  bakabox.listSlots.set(7, { anchor: null, rowsByKey: new Map() });
  assert.doesNotThrow(() => bakabox.applyInstruction(slotInsert(7, null, [['a', 'alice']])));
});

// ── 3. Pre-binding buffering ─────────────────────────────────────────

test('SlotInsert arriving before SetListRef is buffered, then replayed on bind', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });

  bakabox.applyInstruction(slotInsert(9, null, [['b', 'bob']]));
  assert.equal(bakabox.pendingListOps.get(9)?.length, 1, 'buffered until bound');

  const anchor = mountList(bakabox, doc, 9, 2, [['a', 'alice']]);

  assert.equal(serializeList(anchor), '<li key=a>alice</li><li key=b>bob</li>');
  assert.equal(bakabox.pendingListOps.has(9), false, 'buffer drained');
});

test('buffered ops replay in arrival order, interleaved with SlotDelta', () => {
  // The reason the buffer holds whole ops rather than a flat change list: a
  // SlotInsert names the row it lands ahead of, so replaying it before the
  // delta that created that row would lose the position.
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });

  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: 9,
    changes: [{ weight: 1, key: 'z', payload: enc(rowHtml('z', 'zoe')) }],
  });
  bakabox.applyInstruction(slotInsert(9, 'z', [['m', 'mid']]));
  assert.equal(bakabox.pendingListOps.get(9)?.length, 2, 'two ops, not four changes');

  const anchor = mountList(bakabox, doc, 9, 2, [['a', 'alice']]);

  assert.equal(
    serializeList(anchor),
    '<li key=a>alice</li><li key=m>mid</li><li key=z>zoe</li>',
    'the delta ran first, so the insert found its anchor',
  );
  assert.equal(bakabox.pendingListOps.has(9), false, 'buffer drained');
});
