// SPDX-License-Identifier: MIT
// bakabox / albedo-bincode JS-side conformance gate.
//
// This test loads the same fixture file that the Rust-side test
// `tests/bakabox_wire_conformance.rs` writes and verifies that
// `assets/albedo-bincode.js` decodes it into the structurally-identical
// frame that `src/ir/conformance.rs::canonical_v1_frame` describes.
//
// The Rust source of truth is `src/ir/conformance.rs`. If that file
// changes, regenerate the fixture (`UPDATE_BAKABOX_FIXTURE=1 cargo test
// --test bakabox_wire_conformance`) and update the `EXPECTED_FRAME`
// constant below to match. The variant-count check in
// `canonical_v1_frame_covers_every_instruction_variant` (Rust side)
// catches scope drift; this file catches shape drift.
//
// Run with:  node --test tests/bakabox/conformance.test.mjs

import { strict as assert } from 'node:assert';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { test } from 'node:test';

import {
  BakaboxWireError,
  BincodeReader,
  INSTRUCTION_NAMES,
  LOCKED_WIRE_VERSION,
  decodeFrame,
} from '../../assets/bincode.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURE_PATH = resolve(
  __dirname,
  '..',
  'fixtures',
  'wire',
  'v2_canonical_frame.bin',
);

/**
 * Structural mirror of `canonical_v1_frame()` in `src/ir/conformance.rs`.
 * Keep field order matching the Rust enum/struct declaration order — the
 * comparison helper below is deep-equal, so reordering doesn't matter,
 * but matching the source layout makes drift obvious in review.
 *
 * Byte payloads use `Buffer.from(string)` so we can compare against the
 * decoder's `Uint8Array` subarrays via byte-content equality.
 */
const EXPECTED_FRAME = Object.freeze({
  frameId: 1n,
  componentId: 42n,
  instructions: [
    {
      op: 'InitInternTable',
      table: {
        kind: 'Tag',
        entries: [
          { id: 0, value: 'div' },
          { id: 1, value: 'span' },
        ],
      },
    },
    {
      op: 'PatchInternTable',
      kind: 'Attr',
      ops: [
        { op: 'Set', id: 0, value: 'class' },
        { op: 'Remove', id: 9 },
      ],
    },
    { op: 'Create', tagId: 0, stableId: 1 },
    { op: 'SetAttr', stableId: 1, attrId: 0, value: Buffer.from('root') },
    { op: 'SetText', stableId: 1, text: Buffer.from('hello bakabox') },
    { op: 'Append', parentId: 0, childId: 1 },
    { op: 'Remove', stableId: 99 },
    { op: 'BindEvent', stableId: 1, eventId: 0, proxyId: 7 },
    { op: 'BindSlot', stableId: 1, slotId: 3 },
    { op: 'Placeholder', stableId: 2, suspenseId: 10 },
    { op: 'Patch', suspenseId: 10, range: { start: 0, end: 64 } },
    { op: 'SetTextRef', stableId: 1, slotId: 11 },
    { op: 'SetAttrRef', stableId: 1, attrId: 0, slotId: 12 },
    {
      op: 'SlotSet',
      slotId: 11,
      value: Buffer.from('reactive-value'),
    },
    { op: 'Navigate', url: '/dashboard' },
  ],
});

/**
 * Reads the fixture, returns a `Uint8Array` view into the file bytes.
 * Fails the test with a useful message rather than a generic ENOENT if
 * the Rust side hasn't been run yet.
 */
function loadFixtureBytes() {
  let buffer;
  try {
    buffer = readFileSync(FIXTURE_PATH);
  } catch (cause) {
    throw new Error(
      `failed to load bakabox conformance fixture at ${FIXTURE_PATH}; ` +
        `run \`cargo test --test bakabox_wire_conformance\` on the Rust ` +
        `side to generate it. underlying error: ${cause.message}`,
    );
  }
  return new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength);
}

/**
 * Compares decoded `Vec<u8>` payloads (Uint8Array subarrays) against the
 * `Buffer` literals in `EXPECTED_FRAME`. Both walk every instruction and
 * normalise byte payloads to plain arrays before the deep-equal — this
 * gives Node's assert library a stable shape to diff and keeps error
 * output readable when something drifts.
 */
function normaliseBytePayloads(frame) {
  return {
    frameId: frame.frameId,
    componentId: frame.componentId,
    instructions: frame.instructions.map((instruction) => {
      const copy = { ...instruction };
      for (const field of ['value', 'text']) {
        if (copy[field] instanceof Uint8Array || Buffer.isBuffer(copy[field])) {
          copy[field] = Array.from(copy[field]);
        }
      }
      return copy;
    }),
  };
}

function normaliseExpected(frame) {
  return {
    frameId: frame.frameId,
    componentId: frame.componentId,
    instructions: frame.instructions.map((instruction) => {
      const copy = { ...instruction };
      for (const field of ['value', 'text']) {
        if (copy[field] instanceof Uint8Array || Buffer.isBuffer(copy[field])) {
          copy[field] = Array.from(copy[field]);
        }
      }
      return copy;
    }),
  };
}

test('LOCKED_WIRE_VERSION matches the Rust side', () => {
  // If this fails, either the JS module bumped its version without a
  // matching Rust change or vice versa. Coordinate the release.
  assert.equal(LOCKED_WIRE_VERSION, 2);
});

test('INSTRUCTION_NAMES is exhaustive and well-ordered', () => {
  // Mirrors `canonical_v2_frame_covers_every_instruction_variant` on
  // the Rust side. The expected count is hard-coded so a wire-format
  // addition fails fast here AND in Rust.
  const EXPECTED_VARIANT_COUNT = 15;
  assert.equal(
    INSTRUCTION_NAMES.length,
    EXPECTED_VARIANT_COUNT,
    'INSTRUCTION_NAMES length must match the Rust Instruction enum',
  );
  assert.equal(INSTRUCTION_NAMES[0], 'InitInternTable');
  assert.equal(INSTRUCTION_NAMES[13], 'SlotSet');
  assert.equal(INSTRUCTION_NAMES[14], 'Navigate');
});

test('canonical fixture decodes to the expected frame shape', () => {
  const bytes = loadFixtureBytes();
  const decoded = decodeFrame(bytes);

  assert.equal(
    decoded.consumed,
    bytes.byteLength,
    'decoder must consume every byte of the fixture',
  );

  assert.deepStrictEqual(
    normaliseBytePayloads(decoded),
    normaliseExpected(EXPECTED_FRAME),
  );
});

test('decoder rejects truncated input with a wire error carrying offset', () => {
  const bytes = loadFixtureBytes();
  const truncated = bytes.subarray(0, bytes.byteLength - 1);
  assert.throws(
    () => decodeFrame(truncated),
    (err) => {
      assert.ok(
        err instanceof BakaboxWireError,
        'must surface BakaboxWireError, not a generic Error',
      );
      assert.ok(
        typeof err.offset === 'number' && err.offset >= 0,
        'error must carry a non-negative byte offset',
      );
      return true;
    },
  );
});

test('decoder rejects unknown Instruction variant', () => {
  // Build a tiny frame whose instructions vector contains a single
  // variant index past the table. The bytes:
  //   frame_id varint(0)      = 00
  //   component_id None       = 00
  //   instructions len 1      = 01
  //   variant 99              = 63
  const buf = new Uint8Array([0x00, 0x00, 0x01, 0x63]);
  assert.throws(
    () => decodeFrame(buf),
    (err) => {
      assert.ok(err instanceof BakaboxWireError);
      assert.match(err.message, /unknown Instruction variant 99/);
      return true;
    },
  );
});

test('BincodeReader varint round-trips known marker shapes', () => {
  // Spot-check the varint marker scheme. A bincode bump that touches the
  // varint marker bytes will surface here long before it surfaces as a
  // mis-decoded frame.
  const reader = new BincodeReader(
    new Uint8Array([
      0x00, // 0
      0x7f, // 127
      0xfa, // 250
      0xfb, 0xff, 0x00, // 251-marker → u16(255)
      0xfc, 0x00, 0x01, 0x00, 0x00, // 252-marker → u32(256)
    ]),
  );
  assert.equal(reader.readVarintU32(), 0);
  assert.equal(reader.readVarintU32(), 127);
  assert.equal(reader.readVarintU32(), 250);
  assert.equal(reader.readVarintU32(), 255);
  assert.equal(reader.readVarintU32(), 256);
  assert.equal(reader.remaining(), 0);
});
