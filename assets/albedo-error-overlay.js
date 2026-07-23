// Phase M.1 · floating error overlay client.
//
// Subscribes to `/_albedo/dev/errors` (SSE), renders an overlay div
// over the running page when an `Error` event arrives, removes it
// on `Dismiss { id }` or `Clear`. ESC dismisses the currently
// focused error.
//
// The overlay is a single fixed-position element with shadow DOM
// scoping so it never picks up the host page's styles. No external
// dependencies — the IIFE is self-contained so dev mode works even
// when the rest of the runtime hasn't loaded.

(function installErrorOverlay(globalScope) {
  'use strict';

  if (!globalScope || !globalScope.document || !globalScope.EventSource) {
    return;
  }

  // ── Mount ──────────────────────────────────────────────────────

  const HOST_ATTR = 'data-albedo-error-overlay';
  // Reuse an existing host if the script gets injected twice (HMR
  // can re-evaluate the same module).
  let host = globalScope.document.querySelector(`[${HOST_ATTR}]`);
  if (host) {
    return;
  }
  host = globalScope.document.createElement('div');
  host.setAttribute(HOST_ATTR, '1');
  host.style.cssText =
    'position:fixed;top:0;left:0;right:0;bottom:0;z-index:2147483647;' +
    'pointer-events:none;font:13px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;';
  globalScope.document.documentElement.appendChild(host);
  const shadow = host.attachShadow({ mode: 'closed' });

  shadow.innerHTML = `
    <style>
      :host { color: #f5f5f7; }
      .stack {
        position: fixed; top: 0; left: 0; right: 0;
        max-height: 60vh; overflow: auto;
        pointer-events: auto;
        background: rgba(11, 13, 18, 0.96);
        backdrop-filter: blur(8px);
        border-bottom: 1px solid rgba(232, 84, 62, 0.6);
        box-shadow: 0 12px 32px rgba(0, 0, 0, 0.5);
        display: none;
      }
      .stack.visible { display: block; }
      .entry {
        padding: 14px 20px;
        border-bottom: 1px solid rgba(255, 255, 255, 0.05);
        display: grid;
        grid-template-columns: auto 1fr auto;
        gap: 10px;
        align-items: start;
      }
      .entry:last-child { border-bottom: 0; }
      .kind {
        display: inline-block;
        padding: 2px 8px;
        border-radius: 4px;
        font-size: 11px;
        font-weight: 700;
        letter-spacing: 0.06em;
        text-transform: uppercase;
        background: #e8543e;
        color: #0b0d12;
      }
      .kind.compile { background: #f0934f; }
      .kind.render  { background: #e8543e; }
      .kind.action  { background: #c95cd1; }
      .kind.runtime { background: #ffcf5c; color: #0b0d12; }
      .body {
        min-width: 0;
        word-break: break-word;
      }
      .body .head {
        font-weight: 600;
        color: #fff;
        margin-bottom: 2px;
      }
      .body .meta {
        font-size: 11px;
        color: #a8aebf;
      }
      .body pre {
        margin: 6px 0 0 0;
        padding: 8px 10px;
        background: rgba(0,0,0,0.4);
        border: 1px solid rgba(255,255,255,0.05);
        border-radius: 6px;
        font: 12px/1.4 ui-monospace, "SF Mono", Menlo, Consolas, monospace;
        color: #e6e8ee;
        max-height: 240px;
        overflow: auto;
      }
      .actions {
        display: flex;
        gap: 6px;
      }
      .actions button {
        background: transparent;
        border: 1px solid rgba(255, 255, 255, 0.18);
        color: #f4f4f5;
        font: inherit;
        padding: 4px 10px;
        border-radius: 6px;
        cursor: pointer;
      }
      .actions button:hover { border-color: #fff; }
      .conn {
        position: fixed;
        bottom: 12px; right: 12px;
        font-size: 11px;
        color: #6e7385;
        pointer-events: none;
        font-family: ui-monospace, Menlo, Consolas, monospace;
      }
      .conn.lost { color: #e8543e; }
    </style>
    <div class="stack" id="stack" role="alert" aria-live="polite"></div>
    <div class="conn" id="conn" aria-hidden="true">albedo dev · waiting</div>
  `;

  const stackEl = shadow.getElementById('stack');
  const connEl = shadow.getElementById('conn');

  /** id → DOM entry node */
  const live = new Map();

  function renderEntry(err) {
    const entry = globalScope.document.createElement('div');
    entry.className = 'entry';
    entry.dataset.id = String(err.id);

    const kind = globalScope.document.createElement('span');
    kind.className = `kind ${err.kind || 'render'}`;
    kind.textContent = err.kind || 'error';

    const body = globalScope.document.createElement('div');
    body.className = 'body';
    const head = globalScope.document.createElement('div');
    head.className = 'head';
    head.textContent = firstLine(err.message);
    body.appendChild(head);

    const meta = globalScope.document.createElement('div');
    meta.className = 'meta';
    const metaBits = [];
    if (err.route) metaBits.push(err.route);
    if (err.file) {
      metaBits.push(
        err.line ? `${err.file}:${err.line}${err.column ? ':' + err.column : ''}` : err.file,
      );
    }
    if (metaBits.length) {
      meta.textContent = metaBits.join(' · ');
      body.appendChild(meta);
    }

    const detail = remainingLines(err.message);
    if (detail) {
      const pre = globalScope.document.createElement('pre');
      pre.textContent = detail;
      body.appendChild(pre);
    }

    const actions = globalScope.document.createElement('div');
    actions.className = 'actions';
    const dismissBtn = globalScope.document.createElement('button');
    dismissBtn.type = 'button';
    dismissBtn.textContent = 'dismiss';
    dismissBtn.addEventListener('click', function dismiss() {
      removeEntry(err.id);
    });
    actions.appendChild(dismissBtn);

    entry.appendChild(kind);
    entry.appendChild(body);
    entry.appendChild(actions);
    return entry;
  }

  function firstLine(message) {
    if (typeof message !== 'string') return String(message);
    const newline = message.indexOf('\n');
    return newline === -1 ? message : message.slice(0, newline);
  }
  function remainingLines(message) {
    if (typeof message !== 'string') return '';
    const newline = message.indexOf('\n');
    return newline === -1 ? '' : message.slice(newline + 1);
  }

  function addEntry(err) {
    if (!err || typeof err.id !== 'number') return;
    if (live.has(err.id)) {
      removeEntry(err.id);
    }
    const entry = renderEntry(err);
    live.set(err.id, entry);
    stackEl.insertBefore(entry, stackEl.firstChild);
    stackEl.classList.add('visible');
  }

  function removeEntry(id) {
    const entry = live.get(id);
    if (entry) {
      entry.remove();
      live.delete(id);
    }
    if (live.size === 0) {
      stackEl.classList.remove('visible');
    }
  }

  function clearAll() {
    live.clear();
    stackEl.innerHTML = '';
    stackEl.classList.remove('visible');
  }

  // ── ESC dismisses the top error ────────────────────────────────

  globalScope.addEventListener('keydown', function onEsc(event) {
    if (event.key !== 'Escape') return;
    if (live.size === 0) return;
    const first = live.keys().next().value;
    if (typeof first === 'number') {
      removeEntry(first);
    }
  });

  // ── SSE wiring ─────────────────────────────────────────────────
  //
  // The socket is NOT owned here. `albedo-dev-stream.js` holds one
  // EventSource for every dev consumer and fans events out by name —
  // see that file for why (browsers cap HTTP/1.1 at six connections
  // per origin, and dev used to spend three of them per tab).

  function setConn(state, label) {
    connEl.className = `conn ${state}`;
    connEl.textContent = `albedo dev · ${label}`;
  }

  function connect() {
    const channel = globalScope.__albedoDev;
    if (!channel) {
      // The shared channel is injected immediately before this script
      // and both are `defer`, so document order guarantees it. If it
      // is missing, say so rather than silently showing nothing.
      setConn('lost', 'dev channel unavailable');
      return;
    }

    channel.on('status', function (status) {
      setConn(status.state, status.label);
    });

    channel.on('overlay', function (data) {
      try {
        handleEvent(JSON.parse(data));
      } catch (_err) {
        // Ignore malformed events — the overlay isn't trying to be
        // a strict decoder.
      }
    });
  }

  function handleEvent(payload) {
    if (!payload || typeof payload !== 'object') return;
    if (payload.event === 'error') {
      addEntry(payload);
    } else if (payload.event === 'dismiss' && typeof payload.id === 'number') {
      removeEntry(payload.id);
    } else if (payload.event === 'clear') {
      clearAll();
    }
  }

  connect();
})(typeof window !== 'undefined' ? window : globalThis);
