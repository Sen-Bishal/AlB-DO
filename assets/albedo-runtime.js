// SPDX-License-Identifier: MIT
// bakabox — the dumb-client opcode VM for ALBEDO.
//
// This module owns nothing the app could care about. Every byte of state
// is server-authoritative: bakabox holds four lookup maps (nodes, intern
// mirrors, slot bindings, pending placeholders) and a `switch(op)` over
// the decoded `Instruction` variants. There is no diffing, no virtual
// DOM, no reconciliation here — only "apply".
//
// The wire format is locked by `LOCKED_WIRE_VERSION` in `bincode.js`; this
// module never inspects bytes directly. It consumes the decoder's plain
// JS objects and writes to the DOM. Adding an opcode on the Rust side
// requires a matching `case` here in the same variant position; the
// `INSTRUCTION_NAMES` array in `bincode.js` is the cross-check.
//
// Browser bootstrap is at the bottom of this file: if `globalThis.document`
// is present, a Bakabox singleton is constructed and exposed as
// `window.__bakabox` for the WT bootstrap to feed binary frames into. The
// legacy `window.__albedo_inject` HTML-injection queue is preserved as
// `installLegacyHtmlInjector` for as long as the Tier-B JSON path coexists
// with opcodes (subsumed in Phase E).

import {
  ACTION_EVENT_KIND,
  BakaboxWireError,
  decodeFrame,
  encodeActionEnvelope,
  encodeFormDataPayload,
} from './bincode.js';

/**
 * Default attribute the shell renderer stamps on every element bakabox
 * may target. The VM reads this on boot to seed the `nodes` map; without
 * it, no opcode can address an existing DOM element by `StableId`.
 *
 * Mirrors the literal used by the shell renderer in
 * `crates/albedo-server/src/renderer_runtime.rs` (Phase C step 3).
 */
export const DEFAULT_ANCHOR_ATTRIBUTE = 'data-albedo-id';

/** Default attribute used to mark placeholder spans for async islands. */
export const DEFAULT_SUSPENSE_ATTRIBUTE = 'data-albedo-suspense';

/** Default HTTP endpoint the action dispatcher POSTs to. */
export const DEFAULT_ACTION_ENDPOINT = '/_albedo/action';

/**
 * Pre-installation no-op dispatcher. The `Bakabox` constructor uses
 * this as a default so the VM is usable before
 * [`createActionDispatcher`] has been wired (the bootstrap block at
 * the bottom of this file replaces it once a `Bakabox` instance
 * exists). Logging keeps the unwired state visible in dev.
 *
 * @param {number} proxyId
 * @param {Event} event
 */
function noopEventDispatcher(proxyId, event) {
  if (typeof console !== 'undefined' && console.debug) {
    console.debug('[bakabox] action dispatcher not yet installed', { proxyId, type: event?.type });
  }
}

/**
 * Classifies a DOM event into the wire-level `event_kind` byte that
 * `ActionEnvelope` carries. Mirrors the discriminants in
 * `src/ir/action.rs::ActionEventKind`. Adding a kind here requires the
 * matching variant on the Rust side.
 *
 * @param {Event} event
 * @returns {{ kind: number, payload: Uint8Array }}
 */
function classifyEvent(event) {
  const type = event?.type ?? 'other';
  if (type === 'click' || type === 'dblclick' || type === 'mousedown' || type === 'mouseup') {
    return { kind: ACTION_EVENT_KIND.Click, payload: new Uint8Array(0) };
  }
  if (type === 'input' || type === 'change') {
    const value = event?.target?.value;
    if (typeof value === 'string') {
      return {
        kind: ACTION_EVENT_KIND.Input,
        payload: new TextEncoder().encode(value),
      };
    }
    return { kind: ACTION_EVENT_KIND.Input, payload: new Uint8Array(0) };
  }
  if (type === 'submit') {
    // Phase-I: serialize the originating form's `FormData` as the
    // payload. The submit handler is responsible for stopping the
    // browser's default navigation; we extract entries from the form
    // BEFORE that has had a chance to navigate away. File inputs are
    // skipped by the encoder (Phase-I MVP is text-only).
    const form = event?.target;
    if (form && typeof FormData === 'function' && form instanceof HTMLFormElement) {
      try {
        return {
          kind: ACTION_EVENT_KIND.Submit,
          payload: encodeFormDataPayload(new FormData(form)),
        };
      } catch {
        // Fall through to the empty-payload submit shape on encoding errors.
      }
    }
    return { kind: ACTION_EVENT_KIND.Submit, payload: new Uint8Array(0) };
  }
  return { kind: ACTION_EVENT_KIND.Other, payload: new Uint8Array(0) };
}

/**
 * Builds an event dispatcher bound to a specific bakabox instance and
 * endpoint. The returned function is what the VM hands to
 * `addEventListener` for every `BindEvent`-wired element: on fire, it
 * serializes an `ActionEnvelope`, POSTs to the server, and feeds the
 * binary response back through `bakabox.applyFrameBytes` so the
 * resulting patches mutate the DOM in place.
 *
 * Network failures are logged but never thrown — a stuck server must
 * not deadlock the page. Non-200 responses are dropped after a single
 * `console.warn` so the client stays observable.
 *
 * @param {object} options
 * @param {Bakabox} options.bakabox
 * @param {string} [options.endpoint]  Action POST URL. Defaults to `/_albedo/action`.
 * @param {typeof fetch} [options.fetch]  Override for tests.
 */
export function createActionDispatcher({ bakabox, endpoint = DEFAULT_ACTION_ENDPOINT, fetch: fetchImpl } = {}) {
  if (!bakabox) {
    throw new TypeError('createActionDispatcher requires a bakabox instance');
  }
  const resolvedFetch =
    fetchImpl ||
    (typeof globalThis !== 'undefined' && typeof globalThis.fetch === 'function'
      ? globalThis.fetch.bind(globalThis)
      : null);

  return async function dispatchEvent(proxyId, event) {
    if (!resolvedFetch) {
      if (typeof console !== 'undefined' && console.warn) {
        console.warn('[bakabox] no fetch available; action dropped', { proxyId });
      }
      return;
    }
    // Phase-I — when a form is server-action-bound, stop the browser
    // from navigating to the form's `action` URL before we ship the
    // envelope. The server-side Navigate opcode (if any) drives any
    // redirect that follows submission.
    if (event?.type === 'submit' && typeof event.preventDefault === 'function') {
      event.preventDefault();
    }
    const { kind, payload } = classifyEvent(event);
    const bytes = encodeActionEnvelope({
      actionId: proxyId,
      eventKind: kind,
      payload,
    });

    // Phase L · attach the per-session CSRF token the streaming shell
    // published as `globalThis.__ALBEDO_CSRF__`. A click/input envelope's
    // bincode payload has no field to carry a token, so the server's
    // action gate reads it from this header (`x-albedo-csrf`, mirrored in
    // `crates/albedo-server/src/handlers/action.rs`). Without it every
    // non-form action 403s.
    const headers = { 'content-type': 'application/octet-stream' };
    const csrfToken = globalThis.__ALBEDO_CSRF__;
    if (typeof csrfToken === 'string' && csrfToken) {
      headers['x-albedo-csrf'] = csrfToken;
    }

    let response;
    try {
      response = await resolvedFetch(endpoint, {
        method: 'POST',
        headers: headers,
        credentials: 'same-origin',
        cache: 'no-store',
        body: bytes,
      });
    } catch (err) {
      if (typeof console !== 'undefined' && console.warn) {
        console.warn('[bakabox] action fetch failed', err);
      }
      return;
    }
    if (!response.ok) {
      if (typeof console !== 'undefined' && console.warn) {
        console.warn('[bakabox] action returned non-200', response.status);
      }
      return;
    }
    const buffer = await response.arrayBuffer();
    if (buffer.byteLength === 0) return;
    try {
      bakabox.applyFrameBytes(new Uint8Array(buffer));
    } catch (err) {
      if (typeof console !== 'undefined' && console.warn) {
        console.warn('[bakabox] action response failed to apply', err);
      }
    }
  };
}

/**
 * One binding site for a server-side reactive slot. Bakabox stores an
 * array of these per `SlotId`; a `SlotSet` opcode walks the array and
 * re-applies the new value to every site.
 *
 * @typedef {object} BindingSite
 * @property {'text'|'attr'|'html'|'sentinel'} kind  How to apply a new value at this site.
 * @property {number} stableId      Element id; bakabox looks up the DOM node via `nodes`.
 * @property {number} [attrId]      Attribute id (intern table); the byte-wire attr binding.
 * @property {string} [attrName]    Literal attribute name; the local binding-mode attr
 *   binding (`SetAttrRef` carrying `attr` instead of an interned `attrId`). Preferred over
 *   `attrId` when present so the reactive driver needn't intern names it already knows.
 */

/**
 * Reduces a z-set delta to a minimal DOM plan, keyed by `RowKey`.
 *
 * This is the algebra's apply side, kept pure (no DOM) so it is unit-testable
 * on its own. The crux — the S0 finding — is that it does **not** sum weights:
 * an update arrives as a retract of the old record plus an insert of the new
 * one (two changes sharing a key, different payloads); summing their weights
 * to zero would silently drop the edit. Instead the decision is made per key
 * from *current DOM presence* and *whether an insert payload arrived*:
 *
 *   - insert payload present, key not in DOM      → `insert`
 *   - insert payload present, key already in DOM  → `patch` (in place)
 *   - only a retract, key in DOM                   → `remove`
 *   - only a retract, key absent                   → no-op
 *
 * Multiple changes to one key within the batch collapse to a single step; keys
 * are emitted in first-seen order so inserts append in the delta's order (query
 * order, when the emitter walks the view in order). Sorted/positional insert is
 * a later rung — this landing appends.
 *
 * @param {Array<{ weight: number, key: string, payload: (Uint8Array|string) }>} changes
 * @param {(key: string) => boolean} hasKey  Is the key currently materialized?
 * @returns {Array<{ action: 'insert'|'remove'|'patch', key: string, payload?: (Uint8Array|string) }>}
 */
export function reconcileSlotDelta(changes, hasKey) {
  const order = [];
  const byKey = new Map(); // key -> { insert: payload|null, retract: bool }
  for (const change of changes) {
    let entry = byKey.get(change.key);
    if (!entry) {
      entry = { insert: null, retract: false };
      byKey.set(change.key, entry);
      order.push(change.key);
    }
    if (change.weight > 0) entry.insert = change.payload;
    else if (change.weight < 0) entry.retract = true;
  }

  const plan = [];
  for (const key of order) {
    const entry = byKey.get(key);
    if (entry.insert !== null) {
      plan.push({
        action: hasKey(key) ? 'patch' : 'insert',
        key,
        payload: entry.insert,
      });
    } else if (entry.retract && hasKey(key)) {
      plan.push({ action: 'remove', key });
    }
  }
  return plan;
}

/**
 * FNV-1a-32 over the UTF-8 bytes of `str`. Byte-exact mirror of the Rust
 * `dom_render_compiler::runtime::eval::component::fnv1a_32` (offset
 * `0x811c9dc5`, prime `0x01000193`) — `Math.imul` gives the 32-bit wrapping
 * multiply. Used to derive a broadcast topic's wire slot id client-side so it
 * never has to travel on the wire.
 *
 * @param {string} str
 * @returns {number} unsigned 32-bit hash
 */
export function fnv1a32Utf8(str) {
  const bytes = new TextEncoder().encode(str);
  let hash = 0x811c9dc5;
  for (let i = 0; i < bytes.length; i += 1) {
    hash ^= bytes[i];
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash >>> 0;
}

/**
 * Wire slot id for a broadcast `topic` — the client mirror of the Rust
 * `broadcast_slot_id`: `fnv1a_32("broadcast::{topic}")`. A `useSharedSlot`
 * list's `data-albedo-list-slot` carries the topic; this turns it into the slot
 * a `SlotDelta` frame targets.
 *
 * @param {string} topic
 * @returns {number}
 */
export function topicSlotId(topic) {
  return fnv1a32Utf8('broadcast::' + topic);
}

/**
 * The opcode VM. One instance per document. All public methods are
 * synchronous; the only async surface is the WT message pump that
 * `applyFrameBytes` feeds.
 */
export class Bakabox {
  /**
   * @param {object} options
   * @param {Document} options.document  The DOM to apply opcodes against.
   * @param {string} [options.anchorAttribute]    Attribute that carries `StableId`. Default `data-albedo-id`.
   * @param {string} [options.suspenseAttribute]  Attribute set on placeholder spans. Default `data-albedo-suspense`.
   * @param {(proxyId: number, event: Event) => void} [options.eventDispatcher]
   *   Called when a server-bound event fires. Replaced by the hook system in Phase E.
   */
  constructor({
    document,
    anchorAttribute = DEFAULT_ANCHOR_ATTRIBUTE,
    suspenseAttribute = DEFAULT_SUSPENSE_ATTRIBUTE,
    eventDispatcher = noopEventDispatcher,
  }) {
    if (!document) {
      throw new TypeError('Bakabox requires a Document');
    }
    /** @type {Document} */
    this.document = document;
    /** @type {string} */
    this.anchorAttribute = anchorAttribute;
    /** @type {string} */
    this.suspenseAttribute = suspenseAttribute;
    /** @type {(proxyId: number, event: Event) => void} */
    this.eventDispatcher = eventDispatcher;

    /** Map<StableId.0, Element> */
    this.nodes = new Map();
    /** Map<TagId, string> — intern mirror for element tag names. */
    this.tags = new Map();
    /** Map<AttrId, string> — intern mirror for attribute names. */
    this.attrs = new Map();
    /** Map<EventId, string> — intern mirror for event names. */
    this.events = new Map();
    /** Map<SlotId, BindingSite[]> — reactive-slot bindings (Phase E hook surface). */
    this.slots = new Map();
    /** Map<SuspenseId, Element> — placeholder elements awaiting Patch (Phase D). */
    this.pending = new Map();
    /**
     * Map<SlotId, Uint8Array> — last `SlotSet.value` for slots that
     * arrived BEFORE any `BindSlot` / `SetTextRef` / `SetAttrRef` for
     * them. The renderer's `render_entry_with_broadcast` prepends
     * auto-subscribe SlotSets so shared-slot state seeds first; bakabox
     * sees that value before the binding instruction registers a target.
     * We buffer the bytes here and replay them as soon as the binding
     * lands so the initial broadcast paints without waiting for the WT
     * patches lane (which dev mode doesn't have at all).
     */
    this.pendingSlotValues = new Map();

    /**
     * Map<SlotId, { anchor: Element, rowsByKey: Map<RowKey, Element> }> —
     * keyed-list bindings. A `SetListRef` marks an element as the anchor whose
     * children are the rows of a list slot; `SlotDelta` reconciles those rows
     * by `RowKey`. Distinct from `slots` (scalar text/attr/html sites).
     */
    this.listSlots = new Map();
    /**
     * Map<SlotId, SlotChange[]> — `SlotDelta` changes that arrived before the
     * slot's `SetListRef` bound its anchor. Replayed by `_opSetListRef`, the
     * z-set analogue of `pendingSlotValues` for scalar `SlotSet`.
     */
    this.pendingSlotDeltas = new Map();

    /** UTF-8 decoder reused for every SetText/SetAttr payload. */
    this._textDecoder = new TextDecoder('utf-8');
  }

  // ── Bootstrap surface ──────────────────────────────────────────────

  /**
   * Walks the document and registers every element carrying
   * `anchorAttribute` into the `nodes` map. Call once after the shell
   * HTML lands; subsequent `Create` opcodes register themselves.
   *
   * Returns the number of nodes seeded so the caller can detect "no
   * anchors were stamped" misconfigurations early.
   *
   * @param {ParentNode} [root]  Defaults to `document`.
   * @returns {number}
   */
  seedNodesFromDocument(root) {
    const scope = root || this.document;
    const selector = `[${this.anchorAttribute}]`;
    const matches = scope.querySelectorAll(selector);
    let seeded = 0;
    for (const el of matches) {
      const raw = el.getAttribute(this.anchorAttribute);
      const id = Number.parseInt(raw, 10);
      if (Number.isFinite(id) && id >= 0) {
        this.nodes.set(id, el);
        seeded += 1;
      }
    }
    return seeded;
  }

  // ── Frame surface ──────────────────────────────────────────────────

  /**
   * Decodes a wire-encoded `OpcodeFrame` and applies every instruction
   * in order. Returns the decoded frame metadata for logging /
   * observability — the application work has already happened by the
   * time this returns.
   *
   * @param {Uint8Array} bytes
   */
  applyFrameBytes(bytes) {
    const frame = decodeFrame(bytes);
    for (const instruction of frame.instructions) {
      this.applyInstruction(instruction);
    }
    return frame;
  }

  /**
   * Applies one decoded instruction. Dispatches on `op` to one of the
   * single-purpose handlers below. Unknown opcodes throw — bakabox
   * refuses to silently skip; a stale client speaking an old wire
   * version must fail loudly so the WT bootstrap can disconnect.
   *
   * @param {object} op
   */
  applyInstruction(op) {
    switch (op.op) {
      case 'InitInternTable':
        return this._opInitInternTable(op);
      case 'PatchInternTable':
        return this._opPatchInternTable(op);
      case 'Create':
        return this._opCreate(op);
      case 'SetAttr':
        return this._opSetAttr(op);
      case 'SetText':
        return this._opSetText(op);
      case 'Append':
        return this._opAppend(op);
      case 'Remove':
        return this._opRemove(op);
      case 'BindEvent':
        return this._opBindEvent(op);
      case 'BindSlot':
        return this._opBindSlot(op);
      case 'Placeholder':
        return this._opPlaceholder(op);
      case 'Patch':
        return this._opPatch(op);
      case 'SetTextRef':
        return this._opSetTextRef(op);
      case 'SetAttrRef':
        return this._opSetAttrRef(op);
      case 'SetHtmlRef':
        return this._opSetHtmlRef(op);
      case 'SetListRef':
        return this._opSetListRef(op);
      case 'SlotSet':
        return this._opSlotSet(op);
      case 'SlotDelta':
        return this._opSlotDelta(op);
      case 'ReconcileList':
        return this._opReconcileList(op);
      case 'Navigate':
        return this._opNavigate(op);
      default:
        throw new BakaboxWireError(
          `bakabox saw unknown opcode '${op.op}'`,
          -1,
        );
    }
  }

  // ── Intern table handlers ──────────────────────────────────────────

  _opInitInternTable({ table }) {
    const target = this._internMapFor(table.kind);
    target.clear();
    for (const entry of table.entries) {
      target.set(entry.id, entry.value);
    }
  }

  _opPatchInternTable({ kind, ops }) {
    const target = this._internMapFor(kind);
    for (const patch of ops) {
      if (patch.op === 'Set') {
        target.set(patch.id, patch.value);
      } else if (patch.op === 'Remove') {
        target.delete(patch.id);
      } else {
        throw new BakaboxWireError(`unknown InternPatchOp ${patch.op}`, -1);
      }
    }
  }

  _internMapFor(kind) {
    switch (kind) {
      case 'Tag':
        return this.tags;
      case 'Attr':
        return this.attrs;
      case 'Event':
        return this.events;
      default:
        throw new BakaboxWireError(`unknown InternTableKind ${kind}`, -1);
    }
  }

  // ── DOM mutation handlers ──────────────────────────────────────────

  _opCreate({ tagId, stableId }) {
    const tagName = this._lookupIntern(this.tags, tagId, 'tag');
    const element = this.document.createElement(tagName);
    element.setAttribute(this.anchorAttribute, String(stableId));
    this.nodes.set(stableId, element);
  }

  _opSetAttr({ stableId, attrId, value }) {
    const element = this._requireNode(stableId, 'SetAttr');
    const attrName = this._lookupIntern(this.attrs, attrId, 'attr');
    element.setAttribute(attrName, this._decodeBytes(value));
  }

  _opSetText({ stableId, text }) {
    const element = this._requireNode(stableId, 'SetText');
    element.textContent = this._decodeBytes(text);
  }

  _opAppend({ parentId, childId }) {
    const parent = this._requireNode(parentId, 'Append:parent');
    const child = this._requireNode(childId, 'Append:child');
    parent.appendChild(child);
  }

  _opRemove({ stableId }) {
    const element = this.nodes.get(stableId);
    if (element && element.parentNode) {
      element.parentNode.removeChild(element);
    }
    this.nodes.delete(stableId);
    // Phase D — if a placeholder for an async island is removed before
    // its Patch arrives, cancellation is already handled server-side
    // (`pending_placeholders` lookup). We just drop the local entry.
    for (const [suspenseId, pendingEl] of this.pending) {
      if (pendingEl === element) {
        this.pending.delete(suspenseId);
        break;
      }
    }
  }

  _opBindEvent({ stableId, eventId, proxyId }) {
    const element = this._requireNode(stableId, 'BindEvent');
    const eventName = this._lookupIntern(this.events, eventId, 'event');
    const dispatcher = this.eventDispatcher;
    element.addEventListener(eventName, (event) => dispatcher(proxyId, event));
  }

  _opBindSlot({ stableId, slotId }) {
    // BindSlot is the wire's neutral "register this element as caring
    // about slotId" marker. The concrete binding kind (text or attr) is
    // expressed by SetTextRef / SetAttrRef; this opcode is recorded only
    // so the server can audit that the client acknowledged the binding.
    this._ensureSlot(slotId).push({ kind: 'sentinel', stableId });
    this._replayPendingSlotValue(slotId);
  }

  // ── Suspense handlers ──────────────────────────────────────────────

  _opPlaceholder({ stableId, suspenseId }) {
    const span = this.document.createElement('span');
    span.setAttribute(this.suspenseAttribute, String(suspenseId));
    span.setAttribute(this.anchorAttribute, String(stableId));
    this.nodes.set(stableId, span);
    this.pending.set(suspenseId, span);
  }

  _opPatch({ suspenseId }) {
    // Phase C: the Patch opcode just acknowledges that an async island
    // resolved server-side. The replacement instructions that fill the
    // placeholder ship as opcodes that follow the Patch within the same
    // frame (per the Phase D wire amendment in `src/ir/opcode.rs`).
    // Those subsequent `Create`/`SetText`/`Append` ops target the same
    // stable_id; bakabox is already structured to handle them without
    // any Patch-specific replacement logic.
    //
    // We clear `pending` so that a stale Patch for an already-resolved
    // suspense id doesn't keep a phantom reference. The placeholder
    // element itself is recycled (`nodes` still holds it under its
    // `stable_id`) until a subsequent `Remove` or `Append` mutates it.
    this.pending.delete(suspenseId);
  }

  // ── Reactive-slot handlers (Alt-D wire surface) ────────────────────

  _opSetTextRef({ stableId, slotId }) {
    this._requireNode(stableId, 'SetTextRef');
    this._ensureSlot(slotId).push({ kind: 'text', stableId });
    this._replayPendingSlotValue(slotId);
  }

  _opSetAttrRef({ stableId, attrId, attr, slotId }) {
    this._requireNode(stableId, 'SetAttrRef');
    // Two callers, one binding kind: the byte-wire lane carries an interned
    // `attrId`; the local binding-mode driver carries the literal `attr` name.
    // Keep whichever arrived — `_opSlotSet` prefers the name when present.
    this._ensureSlot(slotId).push({ kind: 'attr', stableId, attrId, attrName: attr });
    this._replayPendingSlotValue(slotId);
  }

  _opSetHtmlRef({ stableId, slotId }) {
    // Binding-mode only (no byte-wire opcode): a conditional/list subtree whose
    // rendered branch HTML the driver swaps in wholesale. `value` is trusted
    // server-rendered markup the compiler emitted, never user input — the same
    // contract the retired `makeVm` honoured. The delta wire (S2) supersedes
    // this innerHTML swap with a keyed `+`/`−`/`patch` `SlotDelta`.
    this._requireNode(stableId, 'SetHtmlRef');
    this._ensureSlot(slotId).push({ kind: 'html', stableId });
    this._replayPendingSlotValue(slotId);
  }

  _opSlotSet({ slotId, value }) {
    const sites = this.slots.get(slotId);
    if (!sites || sites.length === 0) {
      // No bindings yet — common during initial paint where
      // `render_entry_with_broadcast` prepends auto-subscribe SlotSets
      // BEFORE the Phase K SetTextRef/SetAttrRef that target them.
      // Cache the bytes so `_replayPendingSlotValue` can apply them as
      // soon as the matching binding registers. Without this, the chat
      // route's initial broadcast value never paints.
      this.pendingSlotValues.set(slotId, value);
      return;
    }
    const decoded = this._decodeBytes(value);
    for (const site of sites) {
      if (site.kind === 'text') {
        const element = this.nodes.get(site.stableId);
        if (!element) continue;
        // Fine-grained patch: mutate the existing server-rendered text node in
        // place rather than replacing the subtree, so the DOM node identity the
        // page was painted with survives the update. Falls back to `textContent`
        // when the element isn't holding a lone text node.
        const first = element.firstChild;
        if (first && first.nodeType === 3) {
          first.nodeValue = decoded;
        } else {
          element.textContent = decoded;
        }
      } else if (site.kind === 'attr') {
        const element = this.nodes.get(site.stableId);
        const attrName = site.attrName || this.attrs.get(site.attrId);
        if (element && attrName) element.setAttribute(attrName, decoded);
      } else if (site.kind === 'html') {
        const element = this.nodes.get(site.stableId);
        if (element) element.innerHTML = decoded;
      }
      // 'sentinel' sites from bare BindSlot are intentionally skipped.
    }
  }

  // ── Keyed-list handlers (z-set delta sink) ─────────────────────────

  /**
   * Marks `stableId`'s element as the anchor of a keyed list bound to
   * `slotId`. Client-only (no byte-wire opcode) — the render path emits it
   * inline, like `SetHtmlRef`. Seeds `rowsByKey` from any server-rendered
   * rows already under the anchor (keyed by `data-albedo-key`) so a delta
   * reconciles against SSR output instead of rebuilding it, then replays any
   * `SlotDelta` that arrived before this binding.
   */
  _opSetListRef({ stableId, slotId }) {
    this._registerListAnchor(slotId, this._requireNode(stableId, 'SetListRef'));
  }

  /**
   * Register an element as the keyed-list anchor for `slotId`: seed `rowsByKey`
   * from its `data-albedo-key` rows (so a delta reconciles against SSR output
   * rather than rebuilding it) and replay any `SlotDelta` that arrived first.
   * The single register point for both the local `SetListRef` lane and the B3
   * broadcast boot-scan; it stores the anchor *element* directly, since a
   * broadcast anchor (Tier-B `<ul>`) carries no `data-albedo-id`.
   */
  _registerListAnchor(slotId, anchor) {
    this.listSlots.set(slotId, { anchor, rowsByKey: this._seedRowsByKey(anchor) });
    const pending = this.pendingSlotDeltas.get(slotId);
    if (pending) {
      this.pendingSlotDeltas.delete(slotId);
      this._opSlotDelta({ slotId, changes: pending });
    }
  }

  /** Build a `RowKey → element` map from an anchor's `data-albedo-key` rows. */
  _seedRowsByKey(anchor) {
    const rowsByKey = new Map();
    const children = anchor.children || [];
    for (const child of children) {
      const key = child.getAttribute && child.getAttribute('data-albedo-key');
      if (key === null || key === undefined) continue;
      // Remember the markup this row was painted from, so a later patch
      // carrying identical markup can be recognised as a no-op. Seeding it
      // from `outerHTML` is what makes a RESYNC after a dropped connection
      // cheap: the server re-asserts every row, and only the ones that really
      // changed touch the DOM.
      if (child.__albedoRowHtml === undefined && typeof child.outerHTML === 'string') {
        child.__albedoRowHtml = child.outerHTML;
      }
      rowsByKey.set(key, child);
    }
    return rowsByKey;
  }

  /**
   * B3 · adopt every server-rendered shared-slot list under `scope` (default
   * `document`). A `data-albedo-list-slot="topic"` container (stamped by the B2
   * transpile pass) is registered as a keyed-list anchor bound to the topic's
   * broadcast slot — the seam a FORGE write's `SlotDelta` fans into. Idempotent:
   * an already-registered slot is skipped so a re-scan (e.g. after a Tier-B
   * `__albedo_inject`) never resets applied rows. Tier-B islands inject their
   * markup after boot, so this runs at boot AND after each injection.
   */
  scanListAnchors(scope) {
    const root = scope || this.document;
    if (!root || typeof root.querySelectorAll !== 'function') return;
    const anchors = root.querySelectorAll('[data-albedo-list-slot]');
    for (const anchor of anchors) {
      const topic = anchor.getAttribute('data-albedo-list-slot');
      if (topic === null || topic === undefined) continue;
      const slotId = topicSlotId(topic);
      if (this.listSlots.has(slotId)) continue;
      this._registerListAnchor(slotId, anchor);
    }
  }

  /**
   * Applies a z-set delta to a keyed list: `+` inserts a row, `−` removes it,
   * a same-key `−`/`+` patches in place. Buffers when the slot's anchor isn't
   * bound yet (mirrors `pendingSlotValues`). The insert/remove/patch decision
   * is made by the pure `reconcileSlotDelta` — it keys on DOM presence and
   * payload, never on summed weights, so an update never cancels to a no-op.
   */
  _opSlotDelta({ slotId, changes }) {
    const list = this.listSlots.get(slotId);
    if (!list) {
      const buffered = this.pendingSlotDeltas.get(slotId) || [];
      for (const change of changes) buffered.push(change);
      this.pendingSlotDeltas.set(slotId, buffered);
      return;
    }
    const anchor = list.anchor;
    if (!anchor) return;

    const plan = reconcileSlotDelta(changes, (key) => list.rowsByKey.has(key));
    for (const step of plan) {
      if (step.action === 'insert') {
        const node = this._instantiateRow(step.payload);
        if (node) {
          node.__albedoRowHtml = this._rowHtml(step.payload);
          anchor.appendChild(node);
          list.rowsByKey.set(step.key, node);
        }
      } else if (step.action === 'remove') {
        const node = list.rowsByKey.get(step.key);
        if (node && node.parentNode) node.parentNode.removeChild(node);
        list.rowsByKey.delete(step.key);
      } else if (step.action === 'patch') {
        const existing = list.rowsByKey.get(step.key);
        const html = this._rowHtml(step.payload);
        // A patch whose markup matches what the row already holds is a no-op,
        // and doing it anyway would swap in a fresh node — losing the DOM
        // identity (focus, selection, scroll, running animation) of a row that
        // did not change. That matters most on a RESYNC, where the server
        // re-asserts every row: without this, a reconnect would rebuild the
        // whole list to restore at most a couple of rows.
        if (existing && existing.__albedoRowHtml === html) continue;
        const node = this._instantiateRow(step.payload);
        if (existing && node && existing.parentNode) {
          node.__albedoRowHtml = html;
          existing.parentNode.replaceChild(node, existing);
          list.rowsByKey.set(step.key, node);
        }
      }
    }
  }

  /**
   * Keyed-list reconciliation from the FULL desired row set — the local
   * (client-satisfiable) list lane's entry point. Where `SlotDelta` carries
   * only what changed (the append/broadcast lane, minimal by construction), a
   * local list recomputes its whole array from state each change, so the driver
   * hands the complete ordered `rows` and this brings the DOM to match: remove
   * gone keys, create/patch by key, and move nodes into the target order. It
   * shares the same `listSlots`/`rowsByKey`/anchor as `SlotDelta` — one sink,
   * two apply modes — and preserves node identity for unchanged rows (so it
   * handles reorder and mid-insert, which a full innerHTML rebuild lost the
   * point of and an append-only delta couldn't express).
   *
   * `rows` is `[{ key, html }]` in desired order. Each row remembers its source
   * HTML on the node (`__albedoRowHtml`) so an unchanged row is left untouched.
   */
  _opReconcileList({ slotId, rows }) {
    const list = this.listSlots.get(slotId);
    if (!list) return;
    const anchor = list.anchor;
    if (!anchor) return;
    const rowsByKey = list.rowsByKey;

    // 1. Drop rows whose key vanished from the desired set.
    const nextKeys = new Set(rows.map((r) => r.key));
    for (const [key, node] of Array.from(rowsByKey)) {
      if (!nextKeys.has(key)) {
        if (node.parentNode) node.parentNode.removeChild(node);
        rowsByKey.delete(key);
      }
    }

    // 2. Upsert in desired order. Appending an existing child moves it, so
    //    walking `rows` in order and appending each leaves the anchor's
    //    children in exactly that order — reorder falls out for free.
    for (const row of rows) {
      // `html` is a string from the local reactive driver but a Uint8Array off
      // the byte wire; normalise so the identity comparison and the source-of-
      // truth stamp are the same shape either way.
      const html = this._rowHtml(row.html);
      let node = rowsByKey.get(row.key);
      if (!node) {
        node = this._instantiateRow(html);
        if (!node) continue;
      } else if (node.__albedoRowHtml !== html) {
        const fresh = this._instantiateRow(html);
        if (fresh) {
          if (node.parentNode) node.parentNode.removeChild(node);
          node = fresh;
        }
      }
      node.__albedoRowHtml = html;
      rowsByKey.set(row.key, node);
      anchor.appendChild(node);
    }
  }

  /**
   * The markup a row payload carries, as a string. `SlotDelta` payloads arrive
   * as bytes off the wire and as strings from the local driver; both are the
   * same row, and comparing them to `__albedoRowHtml` is how an unchanged row
   * is recognised without touching the DOM.
   *
   * @param {Uint8Array|string} payload
   */
  _rowHtml(payload) {
    return typeof payload === 'string' ? payload : this._decodeBytes(payload);
  }

  /**
   * Instantiates a single row element from its server-rendered HTML payload.
   * Uses `createContextualFragment` (real DOM parse) when available, falling
   * back to an off-document container's `innerHTML`. Returns the row's root
   * element (or first node), or `null` for empty markup.
   *
   * @param {Uint8Array|string} payload
   */
  _instantiateRow(payload) {
    const html = typeof payload === 'string' ? payload : this._decodeBytes(payload);
    const doc = this.document;
    if (typeof doc.createRange === 'function') {
      const fragment = doc.createRange().createContextualFragment(html);
      return fragment.firstElementChild || fragment.firstChild || null;
    }
    const container = doc.createElement('div');
    container.innerHTML = html;
    return container.firstElementChild || container.firstChild || null;
  }

  /**
   * Phase-I — applies a server-driven navigation. Drives the browser's
   * top-level URL change via `location.assign`. The `navigator` field
   * on the document's owning window is preferred over `globalThis`
   * directly so SSR / test contexts that provide a stub window can
   * intercept without monkey-patching the global scope.
   *
   * No-op in environments that don't expose a `location` object — the
   * Node DOM shim used by tests injects one explicitly when it wants
   * to assert against the navigation target.
   */
  _opNavigate({ url }) {
    const ownerWindow =
      this.document?.defaultView ||
      (typeof globalThis !== 'undefined' ? globalThis : undefined);
    const loc = ownerWindow?.location;
    if (loc && typeof loc.assign === 'function') {
      loc.assign(url);
    }
  }

  // ── Internal helpers ───────────────────────────────────────────────

  _ensureSlot(slotId) {
    let sites = this.slots.get(slotId);
    if (!sites) {
      sites = [];
      this.slots.set(slotId, sites);
    }
    return sites;
  }

  /**
   * If a SlotSet for this slot arrived before any binding was
   * registered, replay it now via `_opSlotSet`. Called from
   * `_opBindSlot` / `_opSetTextRef` / `_opSetAttrRef` so the cached
   * value paints as soon as the binding target exists. No-op when
   * nothing was buffered for the slot.
   */
  _replayPendingSlotValue(slotId) {
    const pending = this.pendingSlotValues.get(slotId);
    if (pending === undefined) return;
    this.pendingSlotValues.delete(slotId);
    this._opSlotSet({ slotId, value: pending });
  }

  _requireNode(stableId, opName) {
    const element = this.nodes.get(stableId);
    if (!element) {
      throw new BakaboxWireError(
        `${opName} targets unknown stable_id ${stableId}; ` +
          `is the shell missing a [${this.anchorAttribute}] anchor?`,
        -1,
      );
    }
    return element;
  }

  _lookupIntern(map, id, kindLabel) {
    const value = map.get(id);
    if (value === undefined) {
      throw new BakaboxWireError(
        `bakabox saw unknown ${kindLabel} intern id ${id}; ` +
          `did the control stream skip InitInternTable?`,
        -1,
      );
    }
    return value;
  }

  _decodeBytes(bytes) {
    return this._textDecoder.decode(bytes);
  }
}

/**
 * Convenience factory.
 *
 * @param {ConstructorParameters<typeof Bakabox>[0]} options
 */
export function createBakabox(options) {
  return new Bakabox(options);
}

// ── Legacy `__albedo_inject` HTML injection queue ───────────────────────
//
// Tier-B JSON-over-WT still ships HTML strings and expects
// `window.__albedo_inject(id, html, status)` to be available. This survives
// in parallel with the opcode wire until Phase E subsumes Tier-B with
// opcodes. The two paths do not share state.

/**
 * Installs the legacy `__albedo_inject` / `__albedo_hydrate` globals on
 * the supplied window-like object. Returns the queue object for tests.
 * Idempotent — safe to call after the WT bootstrap re-mounts.
 *
 * @param {object} target  Object that gets the globals (usually `window`).
 * @param {Document} document
 */
export function installLegacyHtmlInjector(target, document) {
  const queue = Object.create(null);

  function flush() {
    for (const id in queue) {
      const el = document.getElementById(id);
      if (!el) continue;
      const pending = queue[id];
      delete queue[id];
      apply(el, pending.html, pending.status);
    }
  }

  function apply(el, html, status) {
    if (html === null) {
      el.setAttribute('data-albedo-error', status || 'error');
      return;
    }
    const parent = el.parentNode;
    el.outerHTML = html;
    flush();
    // Register the freshly-injected `[data-albedo-id]` nodes with bakabox. A
    // Tier-B chunk lands by replacing a placeholder's outerHTML with
    // server-rendered markup bakabox never saw at boot, so its `nodes` map has
    // no entry for those anchors. Any opcode that later targets one — notably
    // the P6 form-error `SetText` that clears a field's error span on submit —
    // then throws in `_requireNode` and takes the entire action-response frame
    // down with it (the "guestbook needs a reload to show the row it wrote"
    // bug). Re-seeding the injected subtree is the register step the boot-time
    // `seedNodesFromDocument` could not do because the markup wasn't there yet.
    const bakabox = target.__bakabox;
    if (parent && bakabox && typeof bakabox.seedNodesFromDocument === 'function') {
      bakabox.seedNodesFromDocument(parent);
      // B3 · a Tier-B island (e.g. the guestbook) injects its keyed-list markup
      // here, after boot — adopt any `data-albedo-list-slot` anchor it brought so
      // a broadcast `SlotDelta` on that topic reconciles its rows.
      if (typeof bakabox.scanListAnchors === 'function') {
        bakabox.scanListAnchors(parent);
      }
    }
  }

  target.__albedo_inject = function inject(id, html, status) {
    const el = document.getElementById(id);
    if (!el) {
      queue[id] = { html, status };
      return;
    }
    apply(el, html, status);
  };

  target.__albedo_hydrate = function hydrate(componentId, placeholderId, props) {
    import('/_albedo/hydration.js').then((rt) => {
      if (rt && typeof rt.hydrate === 'function') {
        rt.hydrate(componentId, placeholderId, props);
      }
    });
  };

  if (typeof MutationObserver !== 'undefined') {
    new MutationObserver(flush).observe(document.documentElement, {
      childList: true,
      subtree: true,
    });
  }

  // Drain calls buffered by the head-level classic stub
  // (`TIER_B_INJECT_BOOTSTRAP`) that ran before this deferred module installed
  // the real handlers. Replay them through the now-real functions, in order.
  const pendingInject = target.__ALBEDO_INJECT_QUEUE;
  if (pendingInject) {
    target.__ALBEDO_INJECT_QUEUE = null;
    for (let i = 0; i < pendingInject.length; i++) {
      target.__albedo_inject.apply(null, pendingInject[i]);
    }
  }
  const pendingHydrate = target.__ALBEDO_HYDRATE_QUEUE;
  if (pendingHydrate) {
    target.__ALBEDO_HYDRATE_QUEUE = null;
    for (let i = 0; i < pendingHydrate.length; i++) {
      target.__albedo_hydrate.apply(null, pendingHydrate[i]);
    }
  }

  return queue;
}

// ── Browser bootstrap ───────────────────────────────────────────────────
//
// Side-effect block executed once at module load time when running in a
// browser. The Node test suite imports the named exports above and skips
// this entirely (no `document`, no `window`).

const globalScope =
  typeof globalThis !== 'undefined' ? globalThis : undefined;

if (globalScope && globalScope.document) {
  const bakabox = createBakabox({ document: globalScope.document });
  bakabox.seedNodesFromDocument();
  // B3 · adopt any inline (non-injected) shared-slot list anchors present at boot.
  bakabox.scanListAnchors();

  // Phase-G: install the real action dispatcher so `BindEvent`-wired
  // listeners actually POST to the server when their DOM events fire.
  // The endpoint can be overridden by setting
  // `globalThis.__ALBEDO_ACTION_ENDPOINT__` before this module loads.
  const endpoint =
    typeof globalScope.__ALBEDO_ACTION_ENDPOINT__ === 'string' &&
    globalScope.__ALBEDO_ACTION_ENDPOINT__
      ? globalScope.__ALBEDO_ACTION_ENDPOINT__
      : DEFAULT_ACTION_ENDPOINT;
  bakabox.eventDispatcher = createActionDispatcher({ bakabox, endpoint });

  globalScope.__bakabox = bakabox;
  // Phase P · post-P wire-through — publish `__ALBEDO_RUNTIME` as
  // the public API surface that other client modules (Phase L's
  // link-forms.js, future debug overlays, userland integrations)
  // call into. The name + shape matches `Window.__ALBEDO_RUNTIME`
  // declared in `scaffold/src/albedo-env.d.ts`. Without this,
  // link-forms.js's submit interceptor silently skipped install and
  // form submits fell through to native POST → 405.
  globalScope.__ALBEDO_RUNTIME = {
    applyFrameBytes: function applyFrameBytes(bytes) {
      return bakabox.applyFrameBytes(bytes);
    },
    encodeActionEnvelope: function encodeActionEnvelopePublic(envelope) {
      return encodeActionEnvelope(envelope);
    },
    hashActionName: function hashActionName(name) {
      // FNV-1a-32, same family Phase L / Stream C use for action_id.
      // Mirrors `dom_render_compiler::transforms::form::allocate_form_action_id`.
      var hash = 0x811c9dc5 >>> 0;
      for (var i = 0; i < name.length; i++) {
        hash ^= name.charCodeAt(i) & 0xff;
        hash = (hash + ((hash << 1) + (hash << 4) + (hash << 7) + (hash << 8) + (hash << 24))) >>> 0;
      }
      return hash;
    },
    requestRouteRefresh: function requestRouteRefresh(path) {
      // Fetch the route HTML + re-apply any inline opcode frame.
      // Best-effort; the dev HMR client owns the in-place swap, this
      // is the userland hook for after-action revalidation.
      return fetch(path, { credentials: 'same-origin', cache: 'no-store' })
        .then(function (response) { return response.text(); })
        .then(function (html) {
          var doc = new DOMParser().parseFromString(html, 'text/html');
          if (doc && doc.body) {
            globalScope.document.body.innerHTML = doc.body.innerHTML;
            bakabox.seedNodesFromDocument();
            bakabox.scanListAnchors();
            applyInlineOpcodeFrames(globalScope.document, bakabox);
          }
        });
    },
    registerInstructionHandler: function registerInstructionHandler(name, fn) {
      if (typeof name === 'string' && typeof fn === 'function') {
        if (!bakabox._customHandlers) bakabox._customHandlers = {};
        bakabox._customHandlers[name] = fn;
      }
    },
  };

  // Phase P · post-P wire-through — apply any inline opcode frames
  // the renderer baked into the page. The dev `render_all_routes`
  // and the production manifest builder emit
  // `<script type="application/x-albedo-frame" data-base64="...">`
  // tags carrying the bincode-encoded `OpcodeFrame` so BindEvent /
  // SetTextRef bindings activate even without a WT patches lane
  // (which dev mode doesn't have at all).
  applyInlineOpcodeFrames(globalScope.document, bakabox);

  installLegacyHtmlInjector(globalScope, globalScope.document);

  // Binding-mode (Tier-C reactive) islands ship a classic inline driver plus
  // one `boot(payload)` call each. Those ran during parse — before this
  // deferred module executed and published `__bakabox` — so `boot` only
  // *queued* its payloads. Drain them now against this single VM: the reactive
  // driver's `installReactiveRuntime` binds each island's text/attr/html slots
  // and routes its `on*` handlers locally, no second VM. Guarded so non-reactive
  // routes (no driver present) are a clean no-op.
  const reactive = globalScope.__albedoReactive;
  if (reactive && typeof reactive.drainReactiveQueue === 'function') {
    reactive.drainReactiveQueue(bakabox);
  }
}

function applyInlineOpcodeFrames(document, bakabox) {
  var scripts = document.querySelectorAll(
    'script[type="application/x-albedo-frame"]',
  );
  for (var i = 0; i < scripts.length; i++) {
    var script = scripts[i];
    if (script.dataset && script.dataset.albedoApplied === '1') continue;
    var b64 = script.getAttribute('data-base64');
    if (!b64) continue;
    try {
      var binary = atob(b64);
      var bytes = new Uint8Array(binary.length);
      for (var j = 0; j < binary.length; j++) bytes[j] = binary.charCodeAt(j);
      bakabox.applyFrameBytes(bytes);
      if (script.dataset) script.dataset.albedoApplied = '1';
    } catch (err) {
      if (typeof console !== 'undefined' && console.warn) {
        console.warn('ALBEDO bootstrap: inline opcode frame decode failed', err);
      }
    }
  }
}
