// SPDX-License-Identifier: MIT
// S3 (local lane) — keyed-list reconciliation on the shared delta sink.
//
// The local (client-satisfiable) list lane recomputes its whole array from
// state on each change, so it drives `ReconcileList { slotId, rows }` (full
// ordered row set) rather than a minimal `SlotDelta`. This proves the reconcile
// is correct for every mutation the retired `.map().join('')` innerHTML rebuild
// used to cover — append, remove, in-place content patch, mid-insert, and
// reorder — while preserving DOM node identity for unchanged rows (the point of
// reconciling instead of rebuilding).
//
// Run with: node --test tests/bakabox/keyed-reconcile.test.mjs

import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { Bakabox } from '../../assets/albedo-runtime.js';

// ── DOM shim (single-element row parsing; same discipline as the sibling tests) ──

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
  removeChild(child) {
    const idx = this.childNodes.indexOf(child);
    if (idx >= 0) this.childNodes.splice(idx, 1);
    child.parentNode = null;
    return child;
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
  serialize() {
    return `<${this.tagName.toLowerCase()} key=${this.getAttribute('data-albedo-key')}>${this._textContent}</${this.tagName.toLowerCase()}>`;
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

const rowHtml = (key, text) => `<li data-albedo-key="${key}">${text}</li>`;
const row = (key, text) => ({ key, html: rowHtml(key, text) });

function mountList(bakabox, doc, slotId, anchorId, ssr) {
  const anchor = doc.createElement('ul');
  anchor.setAttribute('data-albedo-id', String(anchorId));
  for (const [key, text] of ssr) anchor.appendChild(parseSingleElement(rowHtml(key, text)));
  bakabox.nodes.set(anchorId, anchor);
  bakabox.applyInstruction({ op: 'SetListRef', stableId: anchorId, slotId });
  return anchor;
}

const serialize = (anchor) => anchor.children.map((c) => c.serialize()).join('');
const reconcile = (bakabox, slotId, rows) =>
  bakabox.applyInstruction({ op: 'ReconcileList', slotId, rows });

// ── Tests ────────────────────────────────────────────────────────────

test('reconcile appends a new row at the end', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);

  reconcile(bakabox, 5, [row('a', 'alice'), row('b', 'bob'), row('c', 'carol')]);
  assert.equal(serialize(anchor), '<li key=a>alice</li><li key=b>bob</li><li key=c>carol</li>');
});

test('reconcile inserts in the middle / at the front (order honored, not appended)', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['c', 'carol']]);

  reconcile(bakabox, 5, [row('z', 'zoe'), row('a', 'alice'), row('b', 'bob'), row('c', 'carol')]);
  assert.equal(
    serialize(anchor),
    '<li key=z>zoe</li><li key=a>alice</li><li key=b>bob</li><li key=c>carol</li>',
    'a pure append-only delta would have put z and b at the end — reconcile places them by order',
  );
});

test('reconcile removes a row by key', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob'], ['c', 'carol']]);

  reconcile(bakabox, 5, [row('a', 'alice'), row('c', 'carol')]);
  assert.equal(serialize(anchor), '<li key=a>alice</li><li key=c>carol</li>');
});

test('reconcile patches a row whose content changed, keeping its key/position', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);

  reconcile(bakabox, 5, [row('a', 'alice'), row('b', 'BOBBY')]);
  assert.equal(serialize(anchor), '<li key=a>alice</li><li key=b>BOBBY</li>');
});

test('reconcile reorders existing rows, preserving node identity', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob'], ['c', 'carol']]);

  // First reconcile adopts the SSR nodes; capture identities afterwards.
  reconcile(bakabox, 5, [row('a', 'alice'), row('b', 'bob'), row('c', 'carol')]);
  const list = bakabox.listSlots.get(5);
  const nodeA = list.rowsByKey.get('a');
  const nodeC = list.rowsByKey.get('c');

  // Reverse order — same keys, no content change.
  reconcile(bakabox, 5, [row('c', 'carol'), row('b', 'bob'), row('a', 'alice')]);
  assert.equal(serialize(anchor), '<li key=c>carol</li><li key=b>bob</li><li key=a>alice</li>');
  assert.equal(list.rowsByKey.get('a'), nodeA, 'unchanged row a keeps its DOM node across reorder');
  assert.equal(list.rowsByKey.get('c'), nodeC, 'unchanged row c keeps its DOM node across reorder');
});

test('an unchanged reconcile is a no-op that preserves every node', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const anchor = mountList(bakabox, doc, 5, 1, [['a', 'alice'], ['b', 'bob']]);

  reconcile(bakabox, 5, [row('a', 'alice'), row('b', 'bob')]); // adopt
  const before = bakabox.listSlots.get(5).rowsByKey;
  const [nodeA, nodeB] = [before.get('a'), before.get('b')];

  reconcile(bakabox, 5, [row('a', 'alice'), row('b', 'bob')]); // identical
  assert.equal(serialize(anchor), '<li key=a>alice</li><li key=b>bob</li>');
  assert.equal(before.get('a'), nodeA, 'row a untouched');
  assert.equal(before.get('b'), nodeB, 'row b untouched');
});
