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

    let response;
    try {
      response = await resolvedFetch(endpoint, {
        method: 'POST',
        headers: { 'content-type': 'application/octet-stream' },
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
 * @property {'text'|'attr'} kind  How to apply a new value at this site.
 * @property {number} stableId      Element id; bakabox looks up the DOM node via `nodes`.
 * @property {number} [attrId]      Attribute id (intern table); required when `kind === 'attr'`.
 */

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
      case 'SlotSet':
        return this._opSlotSet(op);
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

  _opSetAttrRef({ stableId, attrId, slotId }) {
    this._requireNode(stableId, 'SetAttrRef');
    this._ensureSlot(slotId).push({ kind: 'attr', stableId, attrId });
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
        if (element) element.textContent = decoded;
      } else if (site.kind === 'attr') {
        const element = this.nodes.get(site.stableId);
        const attrName = this.attrs.get(site.attrId);
        if (element && attrName) element.setAttribute(attrName, decoded);
      }
      // 'sentinel' sites from bare BindSlot are intentionally skipped.
    }
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
    el.outerHTML = html;
    flush();
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
