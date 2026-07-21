// SPDX-License-Identifier: MIT
// S4 · SSE patches lane — subscription gate + frame decode.
//
// The lane that carries a broadcast frame to a page with no WebTransport
// (which, on `albedo serve`, is every page). These tests pin the two things
// that make it correct rather than merely present: that a frame arrives at
// bakabox byte-identical to what the WT slot would have delivered, and that
// the page never gets to say which topics it subscribes to.
//
// Run with: node --test tests/bakabox/sse-patches.test.mjs

import { strict as assert } from 'node:assert';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { test } from 'node:test';

import { applyPatchEvent, bootPatchStream } from '../../assets/albedo-wt-bootstrap.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURE_PATH = resolve(__dirname, '..', 'fixtures', 'wire', 'v4_canonical_frame.bin');

function loadFixtureBytes() {
  const buffer = readFileSync(FIXTURE_PATH);
  return new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength);
}

/** Node has no `atob`/`EventSource`; this is the smallest globalThis that boots the lane. */
function makeGlobal({ selectorHit = true } = {}) {
  const opened = [];
  const applied = [];
  return {
    g: {
      // Models the BROWSER's `atob`, which throws `InvalidCharacterError` on
      // anything outside the base64 alphabet. `Buffer.from(x, 'base64')` is
      // lenient and silently skips bad characters — using it here would let a
      // corrupt event look decodable and hide the guard this lane relies on.
      atob: (text) => {
        if (!/^[A-Za-z0-9+/]*={0,2}$/.test(text) || text.length % 4 !== 0) {
          throw new Error('InvalidCharacterError');
        }
        return Buffer.from(text, 'base64').toString('binary');
      },
      location: { pathname: '/todos', search: '?ignored=1' },
      document: {
        querySelector: (selector) => (selectorHit ? { selector } : null),
      },
      EventSource: class FakeEventSource {
        constructor(url) {
          this.url = url;
          this.listeners = new Map();
          opened.push(url);
        }
        addEventListener(name, handler) {
          this.listeners.set(name, handler);
        }
        emit(name, data) {
          const handler = this.listeners.get(name);
          if (handler) handler({ data });
        }
      },
      __bakabox: {
        applyFrameBytes: (bytes) => applied.push(bytes),
      },
    },
    opened,
    applied,
  };
}

test('a patch event reaches bakabox as the exact frame bytes', () => {
  const { g, applied } = makeGlobal();
  const bytes = loadFixtureBytes();

  applyPatchEvent(g, Buffer.from(bytes).toString('base64'));

  assert.equal(applied.length, 1);
  assert.deepStrictEqual(
    Array.from(applied[0]),
    Array.from(bytes),
    'base64 over SSE must round-trip to the same bytes the WT patches slot carries',
  );
});

test('the lane asks for a page path and never for a topic', () => {
  const { g, opened } = makeGlobal();
  const source = bootPatchStream(g);

  assert.ok(source);
  assert.equal(opened.length, 1);
  assert.equal(opened[0], '/_albedo/patches?p=%2Ftodos');
  // The server derives topics from the path. A client that could name topics
  // could subscribe to any collection's stream.
  assert.ok(!opened[0].includes('topic'), 'the client must not name topics');
});

test('booting twice reuses one subscription', () => {
  const { g, opened } = makeGlobal();
  const first = bootPatchStream(g);
  const second = bootPatchStream(g);

  assert.equal(first, second);
  assert.equal(opened.length, 1, 'a duplicate stream would double-apply every frame');
});

test('a page with no live surface opens no stream', () => {
  const { g, opened } = makeGlobal({ selectorHit: false });
  assert.equal(bootPatchStream(g), null);
  assert.equal(opened.length, 0);
});

test('a delivered event drives the sink through the live stream', () => {
  const { g, applied } = makeGlobal();
  const source = bootPatchStream(g);
  const bytes = loadFixtureBytes();

  source.emit('patch', Buffer.from(bytes).toString('base64'));

  assert.equal(applied.length, 1);
  assert.deepStrictEqual(Array.from(applied[0]), Array.from(bytes));
});

test('undecodable or premature events are dropped, not thrown', () => {
  const { g, applied } = makeGlobal();

  applyPatchEvent(g, '!!! not base64 !!!');
  applyPatchEvent(g, '');
  applyPatchEvent(g, undefined);
  assert.equal(applied.length, 0, 'garbage must not reach the VM');

  // Frame arriving before the VM script loaded: dropped, never thrown. The
  // next subscribe re-seeds every slot's value anyway.
  const bare = { atob: g.atob };
  applyPatchEvent(bare, Buffer.from(loadFixtureBytes()).toString('base64'));
});
