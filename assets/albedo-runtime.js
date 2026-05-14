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

import { BakaboxWireError, decodeFrame } from './bincode.js';

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

/**
 * No-op event dispatcher used until the hook system (Phase E) provides
 * a real one. Bound events fall through to this when an opcode-declared
 * proxy fires; logging keeps the no-op visible during development.
 *
 * @param {number} proxyId
 * @param {Event} event
 */
function defaultEventDispatcher(proxyId, event) {
  if (typeof console !== 'undefined' && console.debug) {
    console.debug('[bakabox] event proxy not yet wired', { proxyId, event });
  }
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
    eventDispatcher = defaultEventDispatcher,
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
  }

  _opSetAttrRef({ stableId, attrId, slotId }) {
    this._requireNode(stableId, 'SetAttrRef');
    this._ensureSlot(slotId).push({ kind: 'attr', stableId, attrId });
  }

  _opSlotSet({ slotId, value }) {
    const sites = this.slots.get(slotId);
    if (!sites || sites.length === 0) {
      return; // No bindings yet — server may emit SlotSet ahead of BindSlot during HMR.
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

  // ── Internal helpers ───────────────────────────────────────────────

  _ensureSlot(slotId) {
    let sites = this.slots.get(slotId);
    if (!sites) {
      sites = [];
      this.slots.set(slotId, sites);
    }
    return sites;
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
  globalScope.__bakabox = bakabox;
  installLegacyHtmlInjector(globalScope, globalScope.document);
}
