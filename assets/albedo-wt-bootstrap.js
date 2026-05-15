// SPDX-License-Identifier: MIT
// bakabox / albedo WebTransport bootstrap.
//
// Opens a WebTransport session to the ALBEDO server, reads framed
// messages off each of the 4 stream slots, and dispatches them to the
// right consumer. The wire layout this module implements:
//
//   Slot 0 (Control)  — UTF-8 JSON envelopes: session_init, keep_alive,
//                       route_complete, stream_open. Decoded as text.
//   Slot 1 (Shell)    — UTF-8 HTML; passed verbatim to the shell applier.
//   Slot 2 (Patches)  — Binary bincode `OpcodeFrame`s; fed straight to
//                       `window.__bakabox.applyFrameBytes`. Includes both
//                       the one-shot intern-table bootstrap and the
//                       per-tick patch frames.
//   Slot 3 (Prefetch) — UTF-8 JSON resource hints; injected as <link>.
//
// Each stream's first message is always a `{type:"stream_open", stream_slot}`
// JSON envelope written by the server's `spawn_stream_writer`. The slot
// number it carries is the truth; this client cannot assume stream
// arrival order matches slot order.
//
// The framing is `[u32 BE length] [payload]`, matching
// `write_framed_payload` on the server side.

import { decodeFrame } from './bincode.js';

/** Map<slot, (payload: Uint8Array) => void> dispatchers, keyed by slot index. */
const SLOT_DISPATCHERS = new Map();

/** Path the server exposes the WT session on. */
const DEFAULT_WT_PATH = '/_albedo/wt';

/** Header name carrying the session id back to the streaming handler. */
const SESSION_HEADER = 'x-albedo-wt-session';

/** Hint the streaming handler that this request wants WT over SSE. */
const PREFER_HEADER = 'x-albedo-wt-prefer';

/** Marker used so the streaming handler knows this is a fetch-after-session. */
const BOOTSTRAP_HEADER = 'x-albedo-wt-bootstrap';

/**
 * Stream-slot constants. Mirror the values in
 * `src/runtime/webtransport.rs::WT_STREAM_SLOT_*`. Reordering or
 * renumbering breaks the wire contract.
 */
export const SLOT = Object.freeze({
  CONTROL: 0,
  SHELL: 1,
  PATCHES: 2,
  PREFETCH: 3,
});

const utf8Decoder = new TextDecoder('utf-8');

// ── Entry point ──────────────────────────────────────────────────────

/**
 * Boots the WT session if and only if the environment supports it AND
 * the page contains a Tier-B/C element that wants live streaming. Used
 * as the side-effect at module load time below — exported so tests can
 * drive the boot path with an injected globalThis.
 *
 * @param {object} g  Object that exposes `document`, `WebTransport`, `fetch`, `location`.
 */
export function bootWebTransport(g) {
  const document = g.document;
  if (!document || typeof g.WebTransport !== 'function') return;
  if (!document.querySelector('[data-albedo-tier="b"],[data-albedo-tier="c"]')) {
    return;
  }

  const endpoint = resolveEndpoint(g);
  if (!endpoint) return;

  // The shell applier and bakabox VM are set by other modules. If they
  // aren't installed yet we still register safe fallbacks so the receive
  // loop doesn't throw — the underlying script may load after this one.
  if (typeof g.__ALBEDO_WT_APPLY_SHELL !== 'function') {
    g.__ALBEDO_WT_APPLY_SHELL = function applyShellNoop() {};
  }

  let transport;
  try {
    transport = new g.WebTransport(endpoint);
  } catch {
    return;
  }
  if (transport.closed && transport.closed.catch) {
    transport.closed.catch(() => {});
  }
  if (!transport.ready || !transport.ready.then) return;

  const session = createSessionState(g);
  registerSlotDispatchers(session);

  transport.ready
    .then(() => pumpIncomingStreams(transport, session))
    .catch(() => {});
}

/**
 * Resolves the WT endpoint URL. Honours an explicit
 * `__ALBEDO_WT_ENDPOINT__` global if set; otherwise defaults to
 * `https://<host>/_albedo/wt`. Returns `null` when no usable endpoint
 * can be derived (e.g., the page is served over plain HTTP).
 */
function resolveEndpoint(g) {
  if (typeof g.__ALBEDO_WT_ENDPOINT__ === 'string' && g.__ALBEDO_WT_ENDPOINT__) {
    return g.__ALBEDO_WT_ENDPOINT__;
  }
  if (!g.location || g.location.protocol !== 'https:') return null;
  return `https://${g.location.host}${DEFAULT_WT_PATH}`;
}

/**
 * Per-session mutable state. Held inside the closure of the bootstrap so
 * multiple sessions cannot bleed into each other and so tests can drive
 * a fresh session against a synthetic globalThis.
 */
export function createSessionState(g) {
  return {
    g,
    sessionId: '',
    bootstrapped: false,
  };
}

// ── Stream pump ──────────────────────────────────────────────────────

/**
 * Reads `incomingUnidirectionalStreams` and spawns a per-stream handler
 * for each one. Each handler reads framed messages and dispatches them
 * to the slot the stream's first `stream_open` message declared.
 */
async function pumpIncomingStreams(transport, session) {
  const reader = transport.incomingUnidirectionalStreams?.getReader();
  if (!reader) return;
  while (true) {
    let value;
    try {
      const result = await reader.read();
      if (result.done) return;
      value = result.value;
    } catch {
      return;
    }
    if (value) consumeStream(value, session).catch(() => {});
  }
}

/**
 * Reads framed messages from one WT unidirectional stream. The first
 * message MUST be a JSON `stream_open` envelope that declares the slot;
 * subsequent messages are forwarded to the slot's dispatcher.
 */
async function consumeStream(stream, session) {
  const reader = stream.getReader();
  let buffer = new Uint8Array(0);
  let slot = -1;

  while (true) {
    let value;
    try {
      const result = await reader.read();
      if (result.done) return;
      value = result.value;
    } catch {
      return;
    }
    if (!value || value.length === 0) continue;

    buffer = concatBytes(buffer, value);

    while (true) {
      if (buffer.length < 4) break;
      const length = new DataView(
        buffer.buffer,
        buffer.byteOffset,
        4,
      ).getUint32(0, /* littleEndian */ false);
      if (buffer.length < 4 + length) break;

      const payload = buffer.slice(4, 4 + length);
      buffer = buffer.slice(4 + length);

      if (slot < 0) {
        slot = parseStreamOpen(payload);
        if (slot < 0) {
          // The first message was malformed; we can't route further
          // bytes on this stream without knowing the slot. Drop it.
          return;
        }
        continue;
      }

      const dispatcher = SLOT_DISPATCHERS.get(slot);
      if (dispatcher) {
        try {
          dispatcher(payload, session);
        } catch (err) {
          logDispatchError(slot, err);
        }
      }
    }
  }
}

/**
 * Parses the leading `stream_open` JSON envelope and returns the slot
 * number it declares, or `-1` if the envelope is missing or invalid.
 */
export function parseStreamOpen(bytes) {
  let text;
  try {
    text = utf8Decoder.decode(bytes);
  } catch {
    return -1;
  }
  const json = safeParseJson(text);
  if (json && json.type === 'stream_open' && typeof json.stream_slot === 'number') {
    return json.stream_slot | 0;
  }
  return -1;
}

// ── Slot dispatchers ─────────────────────────────────────────────────
//
// Each dispatcher owns one job and lives in its own function so a future
// reader can scan this section and know exactly which slot does what.

/**
 * Wires the four slot-specific dispatchers into `SLOT_DISPATCHERS`. The
 * session state is captured per-dispatcher so the control-slot handler
 * can call the bootstrap-fetch flow against `session.sessionId` /
 * `session.bootstrapped`.
 */
function registerSlotDispatchers(session) {
  SLOT_DISPATCHERS.set(SLOT.CONTROL, (payload) =>
    dispatchControlEnvelope(payload, session),
  );
  SLOT_DISPATCHERS.set(SLOT.SHELL, (payload) =>
    dispatchShellHtml(payload, session),
  );
  SLOT_DISPATCHERS.set(SLOT.PATCHES, (payload) =>
    dispatchOpcodeFrame(payload, session),
  );
  SLOT_DISPATCHERS.set(SLOT.PREFETCH, (payload) =>
    dispatchPrefetchHints(payload, session),
  );
}

/**
 * Slot 0 — JSON envelope. Triggers the bootstrap fetch the first time we
 * see a `session_init` so the streaming handler can attach to this
 * session and ship the actual content.
 */
export function dispatchControlEnvelope(payload, session) {
  const text = decodeUtf8(payload);
  if (text === null) return;
  const envelope = safeParseJson(text);
  if (!envelope) return;
  if (typeof envelope.session_id === 'string' && !session.sessionId) {
    session.sessionId = envelope.session_id;
    session.g.__ALBEDO_WT_SESSION__ = envelope.session_id;
    triggerBootstrapFetch(session);
  }
  // Other envelope types (keep_alive, route_complete) are observable
  // signals only — we don't need to act on them client-side.
}

/** Slot 1 — UTF-8 HTML for the shell applier. */
export function dispatchShellHtml(payload, session) {
  const html = decodeUtf8(payload);
  if (html === null) return;
  try {
    session.g.__ALBEDO_WT_APPLY_SHELL(html);
  } catch {
    /* shell applier crashed — surface no further; bakabox keeps working. */
  }
}

/**
 * Slot 2 — bincode `OpcodeFrame`. Hands the bytes straight to bakabox.
 * The wire contract (`src/ir/conformance.rs::LOCKED_WIRE_VERSION`) is
 * the only thing that keeps the two sides agreeing here; if decoding
 * throws, the wire is corrupt and we abort the stream.
 */
export function dispatchOpcodeFrame(payload, session) {
  const bakabox = session.g.__bakabox;
  if (!bakabox || typeof bakabox.applyFrameBytes !== 'function') {
    // bakabox VM hasn't loaded yet. Drop the frame; the server will
    // emit subsequent ticks. In practice both scripts ship together so
    // this is only hit during boot-order races.
    return;
  }
  bakabox.applyFrameBytes(payload);
}

/** Slot 3 — JSON resource hints. */
export function dispatchPrefetchHints(payload, session) {
  const text = decodeUtf8(payload);
  if (text === null) return;
  const json = safeParseJson(text);
  if (!json) return;
  if (Array.isArray(json.modules)) {
    for (const href of json.modules) installHintLink(session.g.document, 'modulepreload', href);
  }
  if (Array.isArray(json.assets)) {
    for (const href of json.assets) installHintLink(session.g.document, 'prefetch', href);
  }
}

// ── Bootstrap fetch ──────────────────────────────────────────────────

/**
 * Fires the secondary HTTP request that signals to the streaming
 * handler "this session_id is the one — start shipping the route". Run
 * exactly once per session; the bootstrapped flag guards against
 * duplicate session_init envelopes.
 */
function triggerBootstrapFetch(session) {
  if (session.bootstrapped) return;
  if (!session.g.fetch || !session.g.location) return;
  session.bootstrapped = true;

  const url =
    (session.g.location.pathname || '/') + (session.g.location.search || '');
  session.g
    .fetch(url, {
      method: 'GET',
      headers: {
        [SESSION_HEADER]: session.sessionId,
        [PREFER_HEADER]: 'webtransport',
        [BOOTSTRAP_HEADER]: '1',
      },
      credentials: 'same-origin',
      cache: 'no-store',
    })
    .catch(() => {});
}

// ── Helpers ──────────────────────────────────────────────────────────

function concatBytes(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

function decodeUtf8(bytes) {
  try {
    return utf8Decoder.decode(bytes);
  } catch {
    return null;
  }
}

function safeParseJson(text) {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function installHintLink(document, rel, href) {
  if (!document || !document.head) return;
  if (typeof href !== 'string' || !href) return;

  const marker = `${rel}:${href}`;
  if (document.querySelector(`link[data-albedo-wt="${cssEscape(marker)}"]`)) {
    return;
  }
  const link = document.createElement('link');
  link.rel = rel;
  link.href = href;
  link.setAttribute('data-albedo-wt', marker);
  if (rel === 'prefetch') link.as = 'fetch';
  document.head.appendChild(link);
}

function cssEscape(value) {
  if (typeof CSS === 'object' && typeof CSS.escape === 'function') {
    return CSS.escape(value);
  }
  return String(value).replace(/["\\]/g, '\\$&');
}

function logDispatchError(slot, err) {
  if (typeof console !== 'undefined' && console.warn) {
    console.warn(`[bakabox/wt] slot ${slot} dispatcher threw:`, err);
  }
}

// ── Side-effect: browser boot ────────────────────────────────────────

if (typeof globalThis !== 'undefined' && globalThis.document) {
  bootWebTransport(globalThis);
}
