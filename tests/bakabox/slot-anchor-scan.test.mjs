// SPDX-License-Identifier: MIT
// B4 · client boot-scan of scalar shared-slot reads.
//
// The transpile pass stamps `<span>{sharedSlot}</span>` with
// `data-albedo-slot="topic"`. This is the client half: bakabox scans for those
// elements and registers each as a TEXT binding on the topic's broadcast slot
// (`fnv1a_32("broadcast::{topic}")`), holding the element itself.
//
// It exists because the SSR span carries no `data-albedo-id`, so Phase K's
// `SetTextRef` never fires for it and no binding site was ever registered — a
// `broadcast()` write arrived with the right value and stranded in
// `pendingSlotValues` forever. A reload showed the new value (SSR re-reads the
// topic), which is exactly what made the bug read as "broadcast is broken"
// rather than "the paint site is missing". Lists were given this seam in B3;
// scalars were not.
//
// Run with: node --test tests/bakabox/slot-anchor-scan.test.mjs

import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { Bakabox, topicSlotId, slotAnchorText, slotValueText } from '../../assets/albedo-runtime.js';

// ── DOM shim ─────────────────────────────────────────────────────────

class FakeText {
  constructor(value) { this.nodeValue = String(value); }
  get nodeType() { return 3; }
}

class FakeElement {
  constructor(tagName) {
    this.tagName = tagName.toUpperCase();
    this.attributes = new Map();
    this.childNodes = [];
    this.parentNode = null;
  }
  setAttribute(name, value) { this.attributes.set(name, String(value)); }
  getAttribute(name) { return this.attributes.has(name) ? this.attributes.get(name) : null; }
  hasAttribute(name) { return this.attributes.has(name); }
  appendChild(child) { child.parentNode = this; this.childNodes.push(child); return child; }
  get firstChild() { return this.childNodes[0] || null; }
  get children() { return this.childNodes.filter((n) => n.nodeType === 1); }
  set textContent(v) { this.childNodes = [new FakeText(v)]; }
  get textContent() { return this.childNodes.map((n) => n.nodeValue ?? '').join(''); }
  get nodeType() { return 1; }
  walk(visit) { visit(this); for (const c of this.childNodes) if (c.walk) c.walk(visit); }
}

class FakeDocument {
  constructor() {
    this.documentElement = new FakeElement('html');
    this.body = new FakeElement('body');
    this.documentElement.appendChild(this.body);
  }
  createElement(tag) { return new FakeElement(tag); }
  querySelectorAll(selector) {
    const m = /^\[([^=\]]+)\]$/.exec(selector);
    if (!m) throw new Error(`unsupported selector ${selector}`);
    const out = [];
    this.documentElement.walk((n) => { if (n.hasAttribute && n.hasAttribute(m[1])) out.push(n); });
    return out;
  }
}

const enc = (s) => new TextEncoder().encode(s);

/** The SSR shape: a stamped holder whose lone text node is the rendered value. */
function mountScalar(doc, topic, ssrValue, path) {
  const span = doc.createElement('span');
  span.setAttribute('data-albedo-slot', topic);
  if (path) span.setAttribute('data-albedo-slot-path', path);
  span.textContent = ssrValue;
  doc.body.appendChild(span);
  return span;
}

// ── Tests ────────────────────────────────────────────────────────────

test('scanSlotAnchors registers a text site holding the ELEMENT, not a stableId', () => {
  const doc = new FakeDocument();
  const span = mountScalar(doc, 'lobby:counter', '0');
  const bakabox = new Bakabox({ document: doc });

  bakabox.scanSlotAnchors();

  const sites = bakabox.slots.get(topicSlotId('lobby:counter'));
  assert.ok(sites && sites.length === 1, 'one site registered under the topic slot');
  assert.equal(sites[0].kind, 'text');
  assert.equal(sites[0].element, span, 'the site holds the span (no data-albedo-id needed)');
  assert.equal(sites[0].stableId, undefined, 'and binds by element, not by node id');
});

test('a SlotSet on the topic paints the scanned span — the bug this fixes', () => {
  const doc = new FakeDocument();
  const span = mountScalar(doc, 'lobby:counter', '0');
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanSlotAnchors();

  bakabox.applyInstruction({
    op: 'SlotSet',
    slotId: topicSlotId('lobby:counter'),
    value: enc('7'),
  });

  assert.equal(span.textContent, '7', 'the broadcast value painted with no reload');
});

test('a SlotSet that beats the scan is buffered, then replayed on registration', () => {
  const doc = new FakeDocument();
  const span = mountScalar(doc, 'lobby:counter', '0');
  const bakabox = new Bakabox({ document: doc });

  // Broadcast wins the race against the boot scan. This is the ordinary case:
  // `render_entry_with_broadcast` prepends auto-subscribe SlotSets.
  bakabox.applyInstruction({
    op: 'SlotSet',
    slotId: topicSlotId('lobby:counter'),
    value: enc('42'),
  });
  assert.equal(span.textContent, '0', 'nothing painted yet — no binding exists');
  assert.ok(bakabox.pendingSlotValues.has(topicSlotId('lobby:counter')), 'value buffered');

  bakabox.scanSlotAnchors();

  assert.equal(span.textContent, '42', 'registration replayed the buffered value');
  assert.equal(
    bakabox.pendingSlotValues.has(topicSlotId('lobby:counter')),
    false,
    'buffer drained',
  );
});

test('the paint mutates the existing text node rather than replacing the subtree', () => {
  const doc = new FakeDocument();
  const span = mountScalar(doc, 't', 'before');
  const original = span.firstChild;
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanSlotAnchors();

  bakabox.applyInstruction({ op: 'SlotSet', slotId: topicSlotId('t'), value: enc('after') });

  assert.equal(span.firstChild, original, 'same text node, mutated in place');
  assert.equal(span.textContent, 'after');
});

test('scanSlotAnchors is idempotent — a re-scan does not stack a second site', () => {
  const doc = new FakeDocument();
  const span = mountScalar(doc, 't', '0');
  const bakabox = new Bakabox({ document: doc });

  bakabox.scanSlotAnchors();
  bakabox.scanSlotAnchors(); // e.g. after a Tier-B inject elsewhere on the page

  assert.equal(bakabox.slots.get(topicSlotId('t')).length, 1, 'still one site');
  bakabox.applyInstruction({ op: 'SlotSet', slotId: topicSlotId('t'), value: enc('9') });
  assert.equal(span.textContent, '9');
});

test('two spans on the same topic both paint from one SlotSet', () => {
  const doc = new FakeDocument();
  const a = mountScalar(doc, 't', '0');
  const b = mountScalar(doc, 't', '0');
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanSlotAnchors();

  bakabox.applyInstruction({ op: 'SlotSet', slotId: topicSlotId('t'), value: enc('5') });

  assert.equal(a.textContent, '5');
  assert.equal(b.textContent, '5', 'a topic can have more than one holder on a page');
});

// ── The paint rule ───────────────────────────────────────────────────
//
// A topic's wire value is its JSON encoding, and SSR rendered this holder by
// parsing that same JSON — so the two only agree if the client decodes before
// it formats. It did not, and every non-number disagreed: a string painted
// with its quotes, and an object arrived as JSON over an SSR pass that had
// written `[object Object]`. The table below is the same one
// `__albedo_slot_text` applies in the engine; if they drift, a page changes
// shape on its first update without the data changing.

test('the paint rule matches the SSR table, value for value', () => {
  for (const [json, expected] of [
    ['"green"', 'green'],
    ['4242', '4242'],
    ['true', 'true'],
    ['null', ''],
    ['{"state":"ok"}', '{"state":"ok"}'],
    ['[1,2,3]', '[1,2,3]'],
  ]) {
    assert.equal(slotAnchorText(json, undefined), expected, `for ${json}`);
  }
});

test('a JSON string paints unquoted — SSR never showed the quotes', () => {
  const doc = new FakeDocument();
  const span = mountScalar(doc, 'ops', 'green');
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanSlotAnchors();

  bakabox.applyInstruction({ op: 'SlotSet', slotId: topicSlotId('ops'), value: enc('"amber"') });

  assert.equal(span.textContent, 'amber', 'not `"amber"`');
});

test('a path projects the topic value — the read that used to be static', () => {
  const doc = new FakeDocument();
  const span = mountScalar(doc, 'ops', 'ok', 'state');
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanSlotAnchors();

  bakabox.applyInstruction({
    op: 'SlotSet',
    slotId: topicSlotId('ops'),
    value: enc('{"state":"degraded","depth":3}'),
  });

  assert.equal(span.textContent, 'degraded', 'only the member this holder shows');
});

test('a nested path walks every segment', () => {
  const doc = new FakeDocument();
  const span = mountScalar(doc, 'ops', 'abc', 'build.commit');
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanSlotAnchors();

  bakabox.applyInstruction({
    op: 'SlotSet',
    slotId: topicSlotId('ops'),
    value: enc('{"build":{"commit":"def"}}'),
  });

  assert.equal(span.textContent, 'def');
});

test('a projection that fails against parsed JSON blanks, exactly as SSR does', () => {
  const doc = new FakeDocument();
  const span = mountScalar(doc, 'ops', 'ok', 'state');
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanSlotAnchors();

  bakabox.applyInstruction({ op: 'SlotSet', slotId: topicSlotId('ops'), value: enc('7') });

  assert.equal(span.textContent, '', 'the server would render nothing for this shape too');
});

// The initial-paint case, and the reason the two failure modes are not the
// same: an unwritten topic's value is EMPTY BYTES, which are not JSON. A
// projecting holder must keep what the server rendered rather than blank
// itself while waiting for the first real value.
test('non-JSON bytes leave a projecting holder alone, and paint a bare one verbatim', () => {
  const doc = new FakeDocument();
  const projecting = mountScalar(doc, 'a', 'ok', 'state');
  const bare = mountScalar(doc, 'b', 'ok');
  const bakabox = new Bakabox({ document: doc });
  bakabox.scanSlotAnchors();

  bakabox.applyInstruction({ op: 'SlotSet', slotId: topicSlotId('a'), value: enc('') });
  bakabox.applyInstruction({ op: 'SlotSet', slotId: topicSlotId('b'), value: enc('raw') });

  assert.equal(projecting.textContent, 'ok', 'server text survives an unwritten topic');
  assert.equal(bare.textContent, 'raw', 'a non-JSON producer still paints verbatim');
});

// Phase-K text sites are fed by the action lane (JSON) *and* by the local
// reactive driver (already-formatted text), with nothing at the site to tell
// them apart — so only the anchor lane, whose producer is fixed by the slot id
// its `data-albedo-slot` hashes to, may decode.
test('a Phase-K text site is untouched by the rule', () => {
  const doc = new FakeDocument();
  const bakabox = new Bakabox({ document: doc });
  const el = doc.createElement('span');
  bakabox.nodes.set(1, el);
  bakabox.applyInstruction({ op: 'SetTextRef', stableId: 1, slotId: 99 });

  bakabox.applyInstruction({ op: 'SlotSet', slotId: 99, value: enc('"quoted"') });

  assert.equal(el.textContent, '"quoted"', 'verbatim, as before');
});

test('scanSlotAnchors scoped to a subtree only adopts that subtree', () => {
  const doc = new FakeDocument();
  const outside = mountScalar(doc, 'a', '0');
  const island = doc.createElement('div');
  doc.body.appendChild(island);
  const inside = doc.createElement('span');
  inside.setAttribute('data-albedo-slot', 'b');
  inside.textContent = '0';
  island.appendChild(inside);

  const bakabox = new Bakabox({ document: doc });
  // Mirror the post-injection call: scope is the injected parent.
  island.querySelectorAll = (sel) => {
    const out = [];
    island.walk((n) => { if (n.hasAttribute && n.hasAttribute('data-albedo-slot')) out.push(n); });
    return out;
  };
  bakabox.scanSlotAnchors(island);

  assert.ok(bakabox.slots.has(topicSlotId('b')), 'the injected span registered');
  assert.equal(bakabox.slots.has(topicSlotId('a')), false, 'the outside span was not touched');
  assert.equal(outside.textContent, '0');
});
