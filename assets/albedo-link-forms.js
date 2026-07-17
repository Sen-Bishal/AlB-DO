// Phase L · Link + form + Navigate client interception.
//
// This module extends bakabox with three Phase-L behaviours:
//
//   1. `<a data-albedo-link>` clicks are intercepted: the browser
//      does not perform the default navigation. Instead the runtime
//      requests the new route's shell + Tier-C patches over the
//      existing WebTransport patches stream, then calls
//      `history.pushState(url)` to update the URL bar.
//
//   2. `<form data-albedo-action>` submits are intercepted: the
//      FormData is serialized to a flat JSON object, bincode-encoded
//      as an `ActionEnvelope`, and POSTed to `/_albedo/action`. The
//      server's response is a binary `OpcodeFrame` which bakabox
//      decodes via the existing frame applier.
//
//   3. The `Navigate { url }` opcode (variant 14, wire v2) is
//      dispatched here: `history.pushState(url)` + route refresh.
//      Server-driven navigation (e.g. a successful login emitting
//      `Navigate { url: "/dashboard" }`) lands without a full reload.
//
// Bakabox stays dumb: this module is "more glue", not "more
// reconciliation". No diffing, no virtual DOM. Every observable state
// change still comes from the server as a binary opcode.
//
// Wiring assumption: the existing bakabox runtime exposes
// `globalThis.__ALBEDO_RUNTIME` with the following surface used here:
//
//   * `applyFrameBytes(uint8)`         — decode + dispatch an OpcodeFrame
//   * `encodeActionEnvelope(envelope)` — bincode-encode an envelope
//   * `requestRouteRefresh(path)`      — refetch shell + patches for `path`
//   * `registerInstructionHandler(name, fn)` — install opcode dispatcher
//   * `hashActionName(name)`           — optional FNV-1a-32 helper
//
// If any of those are missing the install steps log and no-op so a
// partial load does not break the page.

(function installAlbedoPhaseL(globalScope) {
  'use strict';

  const ALBEDO = globalScope.__ALBEDO_RUNTIME;
  if (!ALBEDO) {
    if (globalScope.console && globalScope.console.warn) {
      globalScope.console.warn('ALBEDO Phase-L: __ALBEDO_RUNTIME missing, skipping install');
    }
    return;
  }

  // Match the renderer's stamp attribute names. Kept here as
  // constants so a future renderer rename only needs to update one
  // pair of strings on each side.
  const LINK_ATTR = 'data-albedo-link';
  const FORM_ATTR = 'data-albedo-action';
  // The authored sentinel, mirrors `transforms::form::FORM_ACTION_PREFIX`. A
  // client-rendered island keeps `action="action:NAME"` verbatim (no SSR
  // transform), so the interceptor reads the name from either source.
  const FORM_ACTION_PREFIX = 'action:';
  const ACTION_ENDPOINT = '/_albedo/action';

  // The albedo action name for a form, or `''` if it isn't an action form.
  // Prefers the SSR-stamped `data-albedo-action`; falls back to the raw
  // `action="action:NAME"` sentinel a client-rendered island emits.
  function resolveFormActionName(form) {
    const stamped = form.getAttribute(FORM_ATTR);
    if (stamped) {
      return stamped;
    }
    const raw = form.getAttribute('action');
    if (raw && raw.slice(0, FORM_ACTION_PREFIX.length) === FORM_ACTION_PREFIX) {
      return raw.slice(FORM_ACTION_PREFIX.length);
    }
    return '';
  }

  // bakabox ActionEventKind discriminants — mirrors
  // `dom_render_compiler::ir::action::ActionEventKind`.
  const EVENT_KIND_CLICK = 0;
  const EVENT_KIND_INPUT = 1;
  const EVENT_KIND_SUBMIT = 2;
  // EVENT_KIND_OTHER = 3 (unused on this side; reserved for parity)

  // ── 1. Link click interception ────────────────────────────────

  // Capture-phase listener so an inner element's stopPropagation()
  // doesn't lose the Link click before we see it. Match on the
  // closest ancestor carrying `data-albedo-link` so users can wrap
  // arbitrary content (icon + text, etc.) inside the <a> tag.
  document.addEventListener(
    'click',
    function handleAlbedoLinkClick(event) {
      // Honour modifier keys (open in new tab / window) so users
      // never lose normal browser navigation semantics.
      if (
        event.defaultPrevented ||
        event.button !== 0 ||
        event.metaKey ||
        event.ctrlKey ||
        event.shiftKey ||
        event.altKey
      ) {
        return;
      }
      const anchor = event.target && event.target.closest && event.target.closest('a[' + LINK_ATTR + ']');
      if (!anchor) {
        return;
      }
      const href = anchor.getAttribute('href');
      if (!href) {
        return;
      }
      // Only intercept same-origin URLs; external hrefs fall through
      // to the browser's default behaviour.
      let url;
      try {
        url = new URL(href, globalScope.location.href);
      } catch (_err) {
        return;
      }
      if (url.origin !== globalScope.location.origin) {
        return;
      }
      event.preventDefault();
      navigateToRoute(url.pathname + url.search + url.hash);
    },
    true,
  );

  // Drive a route change end-to-end. Asks the runtime to refresh the
  // current route against the new URL, then updates history. The
  // refresh call already issues the WT request that streams the
  // shell and the Tier-C patches.
  function navigateToRoute(pathWithQuery) {
    const refresh = ALBEDO.requestRouteRefresh
      ? ALBEDO.requestRouteRefresh(pathWithQuery)
      : Promise.resolve();

    Promise.resolve(refresh)
      .then(function applyHistoryEntry() {
        try {
          globalScope.history.pushState({}, '', pathWithQuery);
        } catch (err) {
          // pushState can throw on file:// or about: URLs; the runtime
          // already drove the visible DOM change, so log and continue.
          if (globalScope.console && globalScope.console.warn) {
            globalScope.console.warn('ALBEDO pushState failed', err);
          }
        }
      })
      .catch(function reportRefreshFailure(err) {
        if (globalScope.console && globalScope.console.error) {
          globalScope.console.error('ALBEDO route refresh failed', err);
        }
      });
  }

  // Browser back/forward — translate popstate into a route refresh
  // against the new URL so the rendered DOM matches the URL bar.
  globalScope.addEventListener('popstate', function onAlbedoPopState() {
    const loc = globalScope.location;
    const path = loc.pathname + loc.search + loc.hash;
    if (ALBEDO.requestRouteRefresh) {
      ALBEDO.requestRouteRefresh(path);
    }
  });

  // ── 2. Form submit interception ───────────────────────────────

  // Capture-phase, like the click handler, so a child element's
  // preventDefault chain can't make us miss the submit.
  document.addEventListener(
    'submit',
    function handleAlbedoFormSubmit(event) {
      if (event.defaultPrevented) {
        return;
      }
      const form = event.target;
      if (!(form instanceof HTMLFormElement)) {
        return;
      }
      // The action name comes from `data-albedo-action` when the SSR renderer
      // rewrote the form, OR straight from the authored `action="action:NAME"`
      // sentinel when the form was rendered CLIENT-SIDE (a Tier-C island's own
      // render doesn't run the server-side attribute transform). Recognizing the
      // sentinel here makes an authored form action work identically whether it
      // was server- or client-rendered — the FNV-1a-32 id is the same either way.
      const actionName = resolveFormActionName(form);
      if (!actionName) {
        return;
      }
      event.preventDefault();
      submitAlbedoForm(form, actionName);
    },
    true,
  );

  // Serialize a form to a flat JSON object, encode the action
  // envelope, POST it, then hand the response bytes back to the
  // bakabox frame applier so any returned `SetText` / `Navigate` /
  // `SlotSet` opcodes land in the DOM.
  function submitAlbedoForm(form, actionName) {
    const payload = serializeFormToJson(form);
    const actionId = resolveActionId(actionName);

    // `encodeActionEnvelope` (bincode.js) reads camelCase `actionId` /
    // `eventKind` — the same shape runtime.js's own event dispatcher uses. The
    // snake_case keys this once passed made the encoder throw
    // ("actionId … got undefined") synchronously inside the submit listener, so
    // no form action ever reached the wire. `payload` is already UTF-8 JSON
    // bytes from `serializeFormToJson`.
    const envelopeBytes = ALBEDO.encodeActionEnvelope({
      actionId: actionId >>> 0,
      eventKind: EVENT_KIND_SUBMIT,
      payload: payload,
    });

    // A form already carries its `_csrf` token in the serialized payload
    // (the hidden input the renderers stamp), but attach the header too
    // so every action POST — form, click, input — presents the token the
    // same way. The server gate checks the `x-albedo-csrf` header first
    // and falls back to the payload field, so this is belt-and-suspenders,
    // not a second source of truth. Token comes from the global the
    // streaming shell publishes.
    const formHeaders = { 'content-type': 'application/octet-stream' };
    const csrfToken = globalScope.__ALBEDO_CSRF__;
    if (typeof csrfToken === 'string' && csrfToken) {
      formHeaders['x-albedo-csrf'] = csrfToken;
    }

    fetch(ACTION_ENDPOINT, {
      method: 'POST',
      headers: formHeaders,
      body: envelopeBytes,
      credentials: 'same-origin',
    })
      .then(function handleResponse(response) {
        if (!response.ok) {
          // 400 / 403 / 404 / 500: surface but do not throw. The body
          // may still carry useful opcodes for validation failures
          // (those come back on the success path; this branch covers
          // genuine transport errors).
          if (globalScope.console && globalScope.console.warn) {
            globalScope.console.warn('ALBEDO form submit non-ok', response.status);
          }
          return null;
        }
        return response.arrayBuffer();
      })
      .then(function applyFrame(buffer) {
        if (!buffer) {
          return;
        }
        if (ALBEDO.applyFrameBytes) {
          ALBEDO.applyFrameBytes(new Uint8Array(buffer));
        }
      })
      .catch(function reportFormFailure(err) {
        if (globalScope.console && globalScope.console.error) {
          globalScope.console.error('ALBEDO form submit failed', err);
        }
      });
  }

  // Walk a form's elements and produce a JSON `{field: value}` object
  // serialized as UTF-8 bytes. Multi-select and same-name checkbox
  // groups collapse into arrays. File inputs are skipped — they need
  // a multipart path that this Stage 1 envelope can't carry yet.
  function serializeFormToJson(form) {
    const out = Object.create(null);
    const elements = form.elements;
    for (let i = 0; i < elements.length; i++) {
      const el = elements[i];
      const name = el.name;
      if (!name) continue;
      if (el.disabled) continue;
      const tag = (el.tagName || '').toLowerCase();
      const type = (el.type || '').toLowerCase();

      if (tag === 'input' && type === 'file') {
        // Stage 2: file uploads via multipart envelope.
        continue;
      }
      if (tag === 'input' && type === 'submit') {
        // Submit buttons surface as form elements but should not
        // contribute their click label to the payload unless the
        // user explicitly assigned a name with semantic meaning.
        continue;
      }
      if (tag === 'input' && (type === 'checkbox' || type === 'radio')) {
        if (!el.checked) continue;
        // A checkbox with no explicit `value` defaults to "on" in the
        // browser; surface that as a true boolean to match the
        // server-side `FormFieldKind::Boolean` decode.
        const value = el.value === 'on' || el.value === '' ? true : el.value;
        appendField(out, name, value);
        continue;
      }
      if (tag === 'select' && el.multiple) {
        const values = [];
        const opts = el.options;
        for (let j = 0; j < opts.length; j++) {
          if (opts[j].selected) values.push(opts[j].value);
        }
        out[name] = values;
        continue;
      }
      appendField(out, name, el.value);
    }
    const json = JSON.stringify(out);
    return new TextEncoder().encode(json);
  }

  // Append a `(name, value)` pair, coalescing repeated names into
  // arrays so a multi-value form field (multiple same-name
  // checkboxes, for example) doesn't silently overwrite.
  function appendField(target, name, value) {
    if (Object.prototype.hasOwnProperty.call(target, name)) {
      const existing = target[name];
      if (Array.isArray(existing)) {
        existing.push(value);
      } else {
        target[name] = [existing, value];
      }
    } else {
      target[name] = value;
    }
  }

  // Compute the wire `action_id` for a form's action name. Prefer
  // the runtime's helper (if it carries a different hash family in
  // future); fall back to the inline FNV-1a-32 below so forms can
  // submit before bootstrap completes.
  function resolveActionId(actionName) {
    if (typeof ALBEDO.hashActionName === 'function') {
      return ALBEDO.hashActionName(actionName) >>> 0;
    }
    return fnv1a32(actionName) >>> 0;
  }

  // ── 3. Navigate opcode handler ────────────────────────────────

  // Bakabox's frame applier dispatches each decoded opcode through a
  // registered handler map. Register the Navigate handler here so
  // server-driven navigation (e.g. a successful login emitting
  // `Navigate { url: "/dashboard" }`) updates history and triggers a
  // route refresh through the same code path as a Link click.
  if (typeof ALBEDO.registerInstructionHandler === 'function') {
    ALBEDO.registerInstructionHandler('Navigate', function onNavigateOpcode(instr) {
      const url = instr && instr.url;
      if (typeof url !== 'string' || url.length === 0) {
        return;
      }
      navigateToRoute(url);
    });
  }

  // ── Hashing fallback ──────────────────────────────────────────

  // Replicates `runtime::eval::component::fnv1a_32`. Used when the
  // runtime hasn't exposed `hashActionName` yet — same bytes as the
  // server's hash family so action_ids align.
  function fnv1a32(name) {
    let hash = 0x811c9dc5 >>> 0;
    for (let i = 0; i < name.length; i++) {
      hash = (hash ^ name.charCodeAt(i)) >>> 0;
      hash = Math.imul(hash, 0x01000193) >>> 0;
    }
    return hash >>> 0;
  }
})(typeof window !== 'undefined' ? window : globalThis);
