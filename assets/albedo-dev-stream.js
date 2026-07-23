// Phase M.3 · shared dev SSE channel.
//
// WHY THIS EXISTS — the connection budget.
//
// `albedo dev` used to open three long-lived SSE connections per tab:
// `/_albedo/dev/errors`, `/_albedo/dev/hmr`, and `/_albedo/patches`.
// Browsers cap HTTP/1.1 at SIX connections per origin, so two open
// tabs saturated the pool and every subsequent request — an action
// POST, a reload, an asset — queued forever. The page appeared to
// freeze and reloading could not rescue it, because the reload itself
// could not get a connection.
//
// That was reachable by following the starter's own README, which
// tells you to open a second tab.
//
// This module collapses the two dev-only streams into ONE EventSource
// against `/_albedo/dev/stream`, multiplexed by SSE event name, and
// hands events out to any number of in-page subscribers. Two tabs now
// cost four connections instead of six.
//
// NOTE: merging the server routes alone would not have helped — two
// `new EventSource(url)` calls to the SAME url are still two sockets.
// The single owner has to live here, on the client.
//
// Remaining budget: 2 per tab (this + `/_albedo/patches`), so the wall
// moves from 2 tabs to 3. The real fix is HTTP/2 or moving patches
// onto the WebTransport lane — see `development-plan/TODO.md` § 2d.

(function installDevChannel(globalScope) {
  'use strict';

  if (!globalScope || !globalScope.document || !globalScope.EventSource) {
    return;
  }

  // Idempotent: the first script to load owns the socket.
  if (globalScope.__albedoDev) {
    return;
  }

  const listeners = Object.create(null);
  let backoff = 500;
  let source = null;

  function emit(name, payload) {
    const handlers = listeners[name];
    if (!handlers) return;
    for (let i = 0; i < handlers.length; i += 1) {
      try {
        handlers[i](payload);
      } catch (_err) {
        // A throwing subscriber must not take down the channel or
        // starve the other subscribers on this event.
      }
    }
  }

  function status(state, label) {
    emit('status', { state: state, label: label });
  }

  function connect() {
    status('lost', 'connecting…');
    try {
      source = new EventSource('/_albedo/dev/stream');
    } catch (_err) {
      scheduleReconnect();
      return;
    }

    source.addEventListener('open', function () {
      backoff = 500;
      status('live', 'live');
    });

    // Fan the multiplexed names out to their subscribers. The server
    // keeps the original event names (`overlay`, `hmr`) so the two
    // client modules did not need a new wire contract.
    source.addEventListener('overlay', function (msgEvent) {
      emit('overlay', msgEvent.data);
    });
    source.addEventListener('hmr', function (msgEvent) {
      emit('hmr', msgEvent.data);
    });

    source.addEventListener('error', function () {
      try {
        source.close();
      } catch (_err) {}
      source = null;
      scheduleReconnect();
    });
  }

  function scheduleReconnect() {
    status('lost', 'reconnecting in ' + Math.round(backoff / 1000) + 's');
    globalScope.setTimeout(connect, backoff);
    backoff = Math.min(backoff * 2, 8000);
  }

  globalScope.__albedoDev = {
    // `on('overlay'|'hmr', fn)` receives the raw SSE data string.
    // `on('status', fn)` receives `{ state, label }` for connection UI.
    on: function on(name, handler) {
      if (typeof handler !== 'function') return;
      if (!listeners[name]) listeners[name] = [];
      listeners[name].push(handler);
    },
  };

  connect();
})(typeof window !== 'undefined' ? window : globalThis);
