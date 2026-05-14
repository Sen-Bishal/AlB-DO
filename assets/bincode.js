// SPDX-License-Identifier: MIT
// bakabox / albedo wire decoder — pure-JS bincode v2 reader.
//
// This file is the **client-side mirror** of the codec configuration locked
// in `src/ir/wire.rs::config`. The Rust runtime encodes opcode frames with
//
//   bincode::config::standard()
//       .with_little_endian()
//       .with_variable_int_encoding()
//       .with_no_limit()
//
// and this module decodes the resulting bytes back into JS objects. Any
// drift between the two sides is a wire-format break — the conformance
// fixture `tests/fixtures/wire/v1_canonical_frame.bin` is the gate that
// catches it. See `LOCKED_WIRE_VERSION` in `src/ir/conformance.rs` and the
// twin Rust+JS tests that consume the same fixture bytes.
//
// Design notes:
//
// - Hand-rolled, no dependencies. Total surface is one `BincodeReader`,
//   one `BakaboxWireError`, and the variant-keyed `INSTRUCTION_READERS`
//   table. The whole file is meant to read top-to-bottom as a wire spec.
// - u64 fields (`frame_id`, `component_id`, `suspense_id`-shaped values
//   that round up) decode to BigInt. u16/u32 IDs decode to Number — those
//   widths never exceed `Number.MAX_SAFE_INTEGER`.
// - `Vec<u8>` payloads decode to **slices of the input buffer** (zero-copy
//   subarrays). This matters: bakabox's apply loop hands them straight to
//   DOM APIs (`TextDecoder.decode`, `setAttribute` after base64 etc.)
//   without an intermediate copy.
// - Decoder errors throw `BakaboxWireError` with the byte offset at which
//   the read failed. That lets the WT bootstrap log a usable trail when a
//   producer/consumer drift the wire.

/**
 * Wire-format version this decoder speaks. Must match
 * `LOCKED_WIRE_VERSION` in `src/ir/conformance.rs`. A bump on either
 * side without a matching update on the other is a coordinated-release
 * break — bakabox refuses to decode frames it can't structurally trust.
 */
export const LOCKED_WIRE_VERSION = 1;

/**
 * Symbolic names for each `InternTableKind`, indexed by the wire's
 * variant order (matches the `#[repr(u8)]` enum in `src/ir/opcode.rs`).
 */
export const INTERN_TABLE_KIND = Object.freeze(['Tag', 'Attr', 'Event']);

/**
 * Symbolic names for each `Instruction` variant, in declaration order.
 * **Position is the wire contract** — reordering this array is a wire
 * break. Used for debug strings and for the variant-dispatch table below.
 */
export const INSTRUCTION_NAMES = Object.freeze([
  'InitInternTable',    // 0
  'PatchInternTable',   // 1
  'Create',             // 2
  'SetAttr',            // 3
  'SetText',            // 4
  'Append',             // 5
  'Remove',             // 6
  'BindEvent',          // 7
  'BindSlot',           // 8
  'Placeholder',        // 9
  'Patch',              // 10
  'SetTextRef',         // 11
  'SetAttrRef',         // 12
  'SlotSet',            // 13
]);

/**
 * Typed error thrown by every decode failure. Carries the byte offset at
 * which the read went wrong so the WT bootstrap can surface a useful
 * "wire diverged here" log line instead of a generic JS exception.
 */
export class BakaboxWireError extends Error {
  /**
   * @param {string} message
   * @param {number} offset Byte offset within the frame where the read failed.
   */
  constructor(message, offset) {
    super(`bakabox wire error @${offset}: ${message}`);
    this.name = 'BakaboxWireError';
    this.offset = offset;
  }
}

// Varint marker bytes. See `bincode v2` standard config encoding:
//   0..=250  → one-byte literal
//   251      → next 2 bytes are u16 (little-endian)
//   252      → next 4 bytes are u32 (little-endian)
//   253      → next 8 bytes are u64 (little-endian)
//   254      → next 16 bytes are u128 (out of scope here)
const VARINT_U16_MARKER = 251;
const VARINT_U32_MARKER = 252;
const VARINT_U64_MARKER = 253;
const VARINT_U128_MARKER = 254;

/**
 * Stateful cursor over a `Uint8Array`. One reader is constructed per
 * frame decode; it owns no resources beyond a numeric offset.
 *
 * All `read*` methods advance the cursor and throw `BakaboxWireError`
 * when there isn't enough input. The reader is intentionally
 * boring — variant dispatch and opcode-specific reads live in the
 * `INSTRUCTION_READERS` table below, not here.
 */
export class BincodeReader {
  /**
   * @param {Uint8Array} bytes
   */
  constructor(bytes) {
    if (!(bytes instanceof Uint8Array)) {
      throw new TypeError(
        'BincodeReader requires a Uint8Array (got ' +
          Object.prototype.toString.call(bytes) +
          ')',
      );
    }
    /** @type {Uint8Array} */
    this.bytes = bytes;
    /** @type {DataView} */
    this.view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    /** @type {number} */
    this.offset = 0;
    /** @type {TextDecoder} */
    this._stringDecoder = new TextDecoder('utf-8', { fatal: true });
  }

  /** Number of bytes still unread. */
  remaining() {
    return this.view.byteLength - this.offset;
  }

  /** Total bytes consumed so far. Matches the Rust side's `consumed` return. */
  consumed() {
    return this.offset;
  }

  _ensure(numBytes) {
    if (this.offset + numBytes > this.view.byteLength) {
      throw new BakaboxWireError(
        `tried to read ${numBytes} bytes, only ${this.remaining()} remain`,
        this.offset,
      );
    }
  }

  /** Reads one raw byte. */
  readU8() {
    this._ensure(1);
    const value = this.view.getUint8(this.offset);
    this.offset += 1;
    return value;
  }

  /**
   * Reads a `bool`. `bincode` encodes booleans as a single byte `0` or
   * `1`; anything else is a corrupt frame.
   */
  readBool() {
    const byte = this.readU8();
    if (byte === 0) return false;
    if (byte === 1) return true;
    throw new BakaboxWireError(`invalid bool tag ${byte}`, this.offset - 1);
  }

  /**
   * Reads a variable-length unsigned integer that fits in `Number`.
   * Use [`readVarintU64`] when the field could overflow 2^53.
   */
  readVarintU32() {
    const value = this._readVarintAsBigInt();
    if (value > 0xffffffffn) {
      throw new BakaboxWireError(
        `varint ${value} exceeds u32 range`,
        this.offset,
      );
    }
    return Number(value);
  }

  /**
   * Reads a variable-length unsigned integer in `u16` range.
   * Returns a `Number`.
   */
  readVarintU16() {
    const value = this._readVarintAsBigInt();
    if (value > 0xffffn) {
      throw new BakaboxWireError(
        `varint ${value} exceeds u16 range`,
        this.offset,
      );
    }
    return Number(value);
  }

  /**
   * Reads a variable-length unsigned integer in `u64` range.
   * Returns a `BigInt` — `Number` would silently round for values above
   * 2^53. Use [`readVarintU32`] / [`readVarintU16`] for narrower fields.
   */
  readVarintU64() {
    return this._readVarintAsBigInt();
  }

  /** Internal — decodes the bincode v2 varint marker scheme. */
  _readVarintAsBigInt() {
    const marker = this.readU8();
    if (marker < VARINT_U16_MARKER) {
      return BigInt(marker);
    }
    if (marker === VARINT_U16_MARKER) {
      this._ensure(2);
      const value = this.view.getUint16(this.offset, /* littleEndian */ true);
      this.offset += 2;
      return BigInt(value);
    }
    if (marker === VARINT_U32_MARKER) {
      this._ensure(4);
      const value = this.view.getUint32(this.offset, true);
      this.offset += 4;
      return BigInt(value);
    }
    if (marker === VARINT_U64_MARKER) {
      this._ensure(8);
      const value = this.view.getBigUint64(this.offset, true);
      this.offset += 8;
      return value;
    }
    if (marker === VARINT_U128_MARKER) {
      // u128 doesn't occur in our wire today. Refuse rather than silently
      // truncate.
      throw new BakaboxWireError(
        'u128 varints are not supported',
        this.offset - 1,
      );
    }
    throw new BakaboxWireError(
      `unknown varint marker ${marker}`,
      this.offset - 1,
    );
  }

  /**
   * Reads an `Option<T>` — `0u8` for None, `1u8` followed by `T` for Some.
   * @template T
   * @param {(reader: BincodeReader) => T} readInner
   * @returns {T | null}
   */
  readOption(readInner) {
    const tag = this.readU8();
    if (tag === 0) return null;
    if (tag === 1) return readInner(this);
    throw new BakaboxWireError(`invalid Option tag ${tag}`, this.offset - 1);
  }

  /**
   * Reads a `Vec<T>` — varint length, then `length` invocations of `readInner`.
   * @template T
   * @param {(reader: BincodeReader) => T} readInner
   * @returns {T[]}
   */
  readVec(readInner) {
    const lengthBig = this._readVarintAsBigInt();
    // The Rust side runs `with_no_limit()`, but JS arrays must fit in a
    // 32-bit length. Anything bigger is a corrupt or malicious frame and
    // we refuse it explicitly rather than OOM'ing the tab.
    if (lengthBig > 0xffffffffn) {
      throw new BakaboxWireError(
        `Vec length ${lengthBig} exceeds JS array bound`,
        this.offset,
      );
    }
    const length = Number(lengthBig);
    const out = new Array(length);
    for (let i = 0; i < length; i += 1) {
      out[i] = readInner(this);
    }
    return out;
  }

  /**
   * Reads a length-prefixed byte slice and returns a **subarray view** of
   * the underlying buffer — no copy. The returned `Uint8Array` aliases
   * the frame's bytes; do not mutate it.
   */
  readByteSlice() {
    const lengthBig = this._readVarintAsBigInt();
    if (lengthBig > BigInt(this.remaining())) {
      throw new BakaboxWireError(
        `byte slice length ${lengthBig} exceeds remaining ${this.remaining()}`,
        this.offset,
      );
    }
    const length = Number(lengthBig);
    const slice = this.bytes.subarray(this.offset, this.offset + length);
    this.offset += length;
    return slice;
  }

  /**
   * Reads a `String` — same wire shape as a byte slice, decoded as UTF-8.
   * Throws `BakaboxWireError` if the bytes are not valid UTF-8 (the
   * `TextDecoder` is constructed with `{ fatal: true }`).
   */
  readString() {
    const slice = this.readByteSlice();
    try {
      return this._stringDecoder.decode(slice);
    } catch (cause) {
      throw new BakaboxWireError(
        `invalid UTF-8 string (${cause.message || cause})`,
        this.offset - slice.byteLength,
      );
    }
  }
}

// ── Opcode wire readers ──────────────────────────────────────────────────
//
// Each helper reads one wire-level shape and returns a plain JS object.
// The output shapes are documented in `assets/albedo-runtime.js` — the
// VM's `apply(op)` switch is the single consumer of everything below.

/** Reads an `InternTableKind` (variant-only enum). */
function readInternTableKind(r) {
  const idx = r.readVarintU32();
  if (idx >= INTERN_TABLE_KIND.length) {
    throw new BakaboxWireError(
      `unknown InternTableKind variant ${idx}`,
      r.offset - 1,
    );
  }
  return INTERN_TABLE_KIND[idx];
}

/** Reads one `InternEntry { id: u16, value: String }`. */
function readInternEntry(r) {
  const id = r.readVarintU16();
  const value = r.readString();
  return { id, value };
}

/** Reads an `InternTable { kind, entries }`. */
function readInternTable(r) {
  const kind = readInternTableKind(r);
  const entries = r.readVec(readInternEntry);
  return { kind, entries };
}

/** Reads one `InternPatchOp::{Set,Remove}`. */
function readInternPatchOp(r) {
  const variant = r.readVarintU32();
  switch (variant) {
    case 0:
      return { op: 'Set', id: r.readVarintU16(), value: r.readString() };
    case 1:
      return { op: 'Remove', id: r.readVarintU16() };
    default:
      throw new BakaboxWireError(
        `unknown InternPatchOp variant ${variant}`,
        r.offset - 1,
      );
  }
}

/** Reads an `InstructionRange { start: u32, end: u32 }`. */
function readInstructionRange(r) {
  const start = r.readVarintU32();
  const end = r.readVarintU32();
  if (start > end) {
    // Mirrors `RangeError::StartAfterEnd` on the Rust side. The Rust
    // constructor refuses this shape, so receiving it on the wire means
    // a producer bypassed `try_new` — refuse to apply.
    throw new BakaboxWireError(
      `InstructionRange start ${start} > end ${end}`,
      r.offset,
    );
  }
  return { start, end };
}

/**
 * Variant-keyed dispatch table. **Order is the wire contract** — position
 * `i` in this array must decode bincode variant `i` and produce the
 * shape `apply(op)` in `assets/albedo-runtime.js` expects.
 *
 * @type {ReadonlyArray<(r: BincodeReader) => object>}
 */
const INSTRUCTION_READERS = Object.freeze([
  // 0: InitInternTable { table: InternTable }
  (r) => ({ op: 'InitInternTable', table: readInternTable(r) }),

  // 1: PatchInternTable { kind: InternTableKind, ops: Vec<InternPatchOp> }
  (r) => ({
    op: 'PatchInternTable',
    kind: readInternTableKind(r),
    ops: r.readVec(readInternPatchOp),
  }),

  // 2: Create { tag_id: TagId(u16), stable_id: StableId(u32) }
  (r) => ({
    op: 'Create',
    tagId: r.readVarintU16(),
    stableId: r.readVarintU32(),
  }),

  // 3: SetAttr { stable_id, attr_id, value: Vec<u8> }
  (r) => ({
    op: 'SetAttr',
    stableId: r.readVarintU32(),
    attrId: r.readVarintU16(),
    value: r.readByteSlice(),
  }),

  // 4: SetText { stable_id, text: Vec<u8> }
  (r) => ({
    op: 'SetText',
    stableId: r.readVarintU32(),
    text: r.readByteSlice(),
  }),

  // 5: Append { parent_id, child_id }
  (r) => ({
    op: 'Append',
    parentId: r.readVarintU32(),
    childId: r.readVarintU32(),
  }),

  // 6: Remove { stable_id }
  (r) => ({ op: 'Remove', stableId: r.readVarintU32() }),

  // 7: BindEvent { stable_id, event_id, proxy_id }
  (r) => ({
    op: 'BindEvent',
    stableId: r.readVarintU32(),
    eventId: r.readVarintU16(),
    proxyId: r.readVarintU32(),
  }),

  // 8: BindSlot { stable_id, slot_id }
  (r) => ({
    op: 'BindSlot',
    stableId: r.readVarintU32(),
    slotId: r.readVarintU32(),
  }),

  // 9: Placeholder { stable_id, suspense_id }
  (r) => ({
    op: 'Placeholder',
    stableId: r.readVarintU32(),
    suspenseId: r.readVarintU32(),
  }),

  // 10: Patch { suspense_id, range: InstructionRange }
  (r) => ({
    op: 'Patch',
    suspenseId: r.readVarintU32(),
    range: readInstructionRange(r),
  }),

  // 11: SetTextRef { stable_id, slot_id }
  (r) => ({
    op: 'SetTextRef',
    stableId: r.readVarintU32(),
    slotId: r.readVarintU32(),
  }),

  // 12: SetAttrRef { stable_id, attr_id, slot_id }
  (r) => ({
    op: 'SetAttrRef',
    stableId: r.readVarintU32(),
    attrId: r.readVarintU16(),
    slotId: r.readVarintU32(),
  }),

  // 13: SlotSet { slot_id, value: Vec<u8> }
  (r) => ({
    op: 'SlotSet',
    slotId: r.readVarintU32(),
    value: r.readByteSlice(),
  }),
]);

/**
 * Decodes a single `Instruction` from a reader positioned at its variant
 * byte. Returns a plain object `{ op: <string>, ...fields }`. The
 * variant-keyed entry decides the field layout — adding a new opcode on
 * the Rust side requires adding a row here in the same position.
 *
 * @param {BincodeReader} r
 */
export function readInstruction(r) {
  const variant = r.readVarintU32();
  const reader = INSTRUCTION_READERS[variant];
  if (!reader) {
    throw new BakaboxWireError(
      `unknown Instruction variant ${variant}`,
      r.offset - 1,
    );
  }
  return reader(r);
}

/**
 * Decodes one `OpcodeFrame` from the given bytes.
 *
 * Caller must have already reassembled all WebTransport STREAM messages
 * sharing the same `frame_id` (see `albedo-wt-bootstrap.js`). The
 * decoder is single-pass and consumes the buffer linearly.
 *
 * The returned `consumed` field exposes how many bytes were read. When
 * the frame is shipped exactly once per chunk this equals `bytes.length`;
 * the field exists so callers that splice multiple frames into one
 * buffer (Phase D `Patch` reassembly) can advance correctly.
 *
 * @param {Uint8Array} bytes
 * @returns {{
 *   frameId: bigint,
 *   componentId: bigint | null,
 *   instructions: Array<object>,
 *   consumed: number,
 * }}
 */
export function decodeFrame(bytes) {
  const reader = new BincodeReader(bytes);
  const frameId = reader.readVarintU64();
  const componentId = reader.readOption((r) => r.readVarintU64());
  const instructions = reader.readVec(readInstruction);
  return {
    frameId,
    componentId,
    instructions,
    consumed: reader.consumed(),
  };
}
