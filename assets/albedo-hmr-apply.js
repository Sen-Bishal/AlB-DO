// Phase M.2 · slot-preserving HMR client.
//
// Subscribes to `/_albedo/dev/hmr` (SSE) and applies route HTML
// updates in place. Server-side slot state survives because slots
// are keyed by the session cookie and the cookie isn't touched by
// this swap. Scroll position and focused input value are best-effort
// preserved across the swap so an edit-while-typing cycle doesn't
// throw away the operator's draft.

(function installHmrApplyClient(globalScope) {
  'use strict';

  if (!globalScope || !globalScope.document || !globalScope.EventSource) {
    return;
  }

  // ── Connection state ──────────────────────────────────────────
  //
  // The socket lives in `albedo-dev-stream.js`, shared with the error
  // overlay. Reconnect and backoff are owned there; this module only
  // subscribes. See that file for the connection-budget reasoning.

  let lastRevision = 0;

  function connect() {
    const channel = globalScope.__albedoDev;
    if (!channel) {
      return;
    }
    channel.on('hmr', function (data) {
      try {
        const payload = JSON.parse(data);
        handleEvent(payload);
      } catch (_err) {
        // Bad frames are ignored — next clean frame resyncs.
      }
    });
  }

  function handleEvent(payload) {
    if (!payload || typeof payload !== 'object') return;
    // Drop out-of-order revisions — the watcher emits monotonic
    // revisions, but reconnects can replay slightly stale events.
    if (typeof payload.revision === 'number' && payload.revision < lastRevision) {
      return;
    }
    if (typeof payload.revision === 'number') {
      lastRevision = payload.revision;
    }
    if (payload.event === 'apply') {
      applyHmrPatch(payload);
    } else if (payload.event === 'reload') {
      globalScope.location.reload();
    }
  }

  // ── In-place HTML swap ─────────────────────────────────────────

  function applyHmrPatch(payload) {
    if (!payload || typeof payload.html !== 'string') return;
    // Phase M.2 Stage 1: simple replace of the document body's
    // inner HTML. Server-side slot store is keyed by the
    // `albedo-session` cookie (Phase L) and the cookie survives
    // this swap, so useState values are preserved through the
    // round-trip server → next render → next page → new HTML.
    const oldRoot = globalScope.document.body;
    if (!oldRoot) return;

    // Snapshot the ephemeral browser state we can plausibly restore
    // after the swap so a "save while typing" cycle doesn't lose
    // the operator's draft text. Best-effort — only inputs with a
    // stable `name` attribute survive.
    const draft = captureDraftInputs(oldRoot);
    const scroll = { x: globalScope.scrollX, y: globalScope.scrollY };

    // Parse the new body fragment off-document so partial reflow
    // doesn't paint half-installed DOM. Errors in the parsed HTML
    // fall back to a hard reload so the operator sees something.
    let fresh;
    try {
      const doc = new DOMParser().parseFromString(payload.html, 'text/html');
      fresh = doc.body;
    } catch (_err) {
      globalScope.location.reload();
      return;
    }
    if (!fresh) {
      globalScope.location.reload();
      return;
    }

    oldRoot.innerHTML = fresh.innerHTML;

    restoreDraftInputs(oldRoot, draft);
    try {
      globalScope.scrollTo(scroll.x, scroll.y);
    } catch (_e) {
      // Some browsers reject scrollTo with weird args; ignore.
    }
    // Tell userland the swap landed so any custom JS that needs
    // to rebind state can do so. The HMR runtime itself doesn't
    // depend on this event firing successfully.
    try {
      globalScope.dispatchEvent(
        new CustomEvent('albedo:hmr-applied', {
          detail: { route: payload.route || '/', revision: payload.revision || 0 },
        }),
      );
    } catch (_e) {}
  }

  function captureDraftInputs(root) {
    const out = {};
    if (!root || !root.querySelectorAll) return out;
    const fields = root.querySelectorAll('input[name], textarea[name]');
    for (let i = 0; i < fields.length; i++) {
      const el = fields[i];
      const name = el.getAttribute('name');
      if (!name) continue;
      const type = (el.type || '').toLowerCase();
      if (type === 'checkbox' || type === 'radio') {
        out[`${name}:${el.value || ''}:checked`] = !!el.checked;
      } else if (type !== 'password') {
        // Skip password fields — leaking them across a DOM swap
        // is the wrong default even in dev.
        out[name] = el.value;
      }
    }
    return out;
  }

  function restoreDraftInputs(root, draft) {
    if (!draft || !root || !root.querySelectorAll) return;
    const fields = root.querySelectorAll('input[name], textarea[name]');
    for (let i = 0; i < fields.length; i++) {
      const el = fields[i];
      const name = el.getAttribute('name');
      if (!name) continue;
      const type = (el.type || '').toLowerCase();
      if (type === 'checkbox' || type === 'radio') {
        const key = `${name}:${el.value || ''}:checked`;
        if (key in draft) el.checked = !!draft[key];
      } else if (name in draft && type !== 'password') {
        el.value = draft[name];
      }
    }
  }

  connect();
})(typeof window !== 'undefined' ? window : globalThis);
