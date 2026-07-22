// SPDX-License-Identifier: MIT
// S3 Half-B · B3 — client boot-scan of shared-slot list anchors.
//
// The B2 transpile pass stamps a `useSharedSlot` list's container with
// `data-albedo-list-slot="topic"`. B3 is the client side: bakabox scans for
// those anchors and registers each as a keyed-list anchor bound to the topic's
// broadcast slot (`fnv1a_32("broadcast::{topic}")`), seeding `rowsByKey` from
// the SSR `data-albedo-key` rows. A `SlotDelta` on that slot then reconciles —
// which is exactly what a FORGE topic write fans out in S4.
//
// Run with: node --test tests/bakabox/list-anchor-scan.test.mjs

import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { Bakabox, topicSlotId, fnv1a32Utf8 } from '../../assets/albedo-runtime.js';

// ── DOM shim (attr query + single-element row parsing) ───────────────

class FakeElement {
  constructor(tagName) {
    this.tagName = tagName.toUpperCase();
    this.attributes = new Map();
    this.childNodes = [];
    this.parentNode = null;
    this._textContent = '';
  }
  setAttribute(name, value) { this.attributes.set(name, String(value)); }
  getAttribute(name) { return this.attributes.has(name) ? this.attributes.get(name) : null; }
  hasAttribute(name) { return this.attributes.has(name); }
  get children() { return this.childNodes.filter((n) => n.nodeType === 1); }
  appendChild(child) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentNode = this;
    this.childNodes.push(child);
    return child;
  }
  removeChild(child) {
    const i = this.childNodes.indexOf(child);
    if (i >= 0) this.childNodes.splice(i, 1);
    child.parentNode = null;
    return child;
  }
  set textContent(v) { this._textContent = String(v); this.childNodes = []; }
  get textContent() { return this._textContent; }
  get nodeType() { return 1; }
  walk(visit) { visit(this); for (const c of this.childNodes) if (c.walk) c.walk(visit); }
  serialize() {
    return `<${this.tagName.toLowerCase()} key=${this.getAttribute('data-albedo-key')}>${this._textContent}</${this.tagName.toLowerCase()}>`;
  }
}

class FakeDocument {
  constructor() {
    this.documentElement = new FakeElement('html');
    this.body = new FakeElement('body');
    this.documentElement.appendChild(this.body);
  }
  createElement(tag) { return new FakeElement(tag); }
  createRange() {
    return { createContextualFragment: (html) => {
      const el = parseRow(html);
      return { firstElementChild: el, firstChild: el };
    } };
  }
  querySelectorAll(selector) {
    const m = /^\[([^=\]]+)\]$/.exec(selector);
    if (!m) throw new Error(`unsupported selector ${selector}`);
    const out = [];
    this.documentElement.walk((n) => { if (n.hasAttribute(m[1])) out.push(n); });
    return out;
  }
}

function parseRow(html) {
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
const enc = (s) => new TextEncoder().encode(s);

/** Build a `<ul data-albedo-list-slot="topic">` with keyed SSR rows in `doc.body`. */
function mountSharedList(doc, topic, ssr) {
  const ul = doc.createElement('ul');
  ul.setAttribute('data-albedo-list-slot', topic);
  for (const [key, text] of ssr) ul.appendChild(parseRow(rowHtml(key, text)));
  doc.body.appendChild(ul);
  return ul;
}

const serialize = (ul) => ul.children.map((c) => c.serialize()).join('');

// ── Tests ────────────────────────────────────────────────────────────

test('topicSlotId matches the Rust broadcast_slot_id literal (cross-language lock)', () => {
  // Mirror of `broadcast_slot_id_matches_the_js_mirror` in src/runtime/broadcast.rs.
  assert.equal(fnv1a32Utf8('broadcast::guestbook'), 3800127029);
  assert.equal(topicSlotId('guestbook'), 3800127029);
  assert.equal(topicSlotId('chat'), 2183019110);
});

test('scanListAnchors registers a shared-slot list under its topic slot, seeded from SSR rows', () => {
  const doc = new FakeDocument();
  const ul = mountSharedList(doc, 'guestbook', [['1', 'ada'], ['2', 'alan']]);
  const bakabox = new Bakabox({ document: doc });

  bakabox.scanListAnchors();

  const slot = topicSlotId('guestbook');
  const list = bakabox.listSlots.get(slot);
  assert.ok(list, 'anchor registered under the topic slot');
  assert.equal(list.anchor, ul, 'the anchor is the <ul> element itself (no data-albedo-id needed)');
  assert.deepStrictEqual([...list.rowsByKey.keys()], ['1', '2'], 'rowsByKey seeded from SSR keys');
});

test('a SlotDelta on the topic slot reconciles the scanned list (the S4 path)', () => {
  const doc = new FakeDocument();
  const ul = mountSharedList(doc, 'guestbook', [['1', 'ada'], ['2', 'alan']]);
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanListAnchors();

  // FORGE write → broadcast SlotDelta: append row 3.
  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: topicSlotId('guestbook'),
    changes: [{ weight: 1, key: '3', payload: enc(rowHtml('3', 'grace')) }],
  });

  assert.equal(
    serialize(ul),
    '<li key=1>ada</li><li key=2>alan</li><li key=3>grace</li>',
    'the broadcast row appended with no reload',
  );
});

test('scanListAnchors is idempotent — a re-scan never resets applied rows', () => {
  const doc = new FakeDocument();
  const ul = mountSharedList(doc, 'guestbook', [['1', 'ada']]);
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanListAnchors();
  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: topicSlotId('guestbook'),
    changes: [{ weight: 1, key: '2', payload: enc(rowHtml('2', 'alan')) }],
  });

  bakabox.scanListAnchors(); // re-scan (e.g. after another inject)

  assert.equal(serialize(ul), '<li key=1>ada</li><li key=2>alan</li>', 'rows survive the re-scan');
  assert.equal(bakabox.listSlots.get(topicSlotId('guestbook')).rowsByKey.size, 2);
});

test('a SlotDelta arriving before the scan is buffered, then replayed on registration', () => {
  const doc = new FakeDocument();
  const ul = mountSharedList(doc, 'guestbook', [['1', 'ada']]);
  const bakabox = new Bakabox({ document: doc });

  // Broadcast beats the boot-scan.
  bakabox.applyInstruction({
    op: 'SlotDelta',
    slotId: topicSlotId('guestbook'),
    changes: [{ weight: 1, key: '2', payload: enc(rowHtml('2', 'alan')) }],
  });
  assert.equal(bakabox.pendingListOps.get(topicSlotId('guestbook'))?.length, 1);

  bakabox.scanListAnchors(); // registration replays the buffered delta

  assert.equal(serialize(ul), '<li key=1>ada</li><li key=2>alan</li>');
  assert.equal(bakabox.pendingListOps.has(topicSlotId('guestbook')), false, 'buffer drained');
});
