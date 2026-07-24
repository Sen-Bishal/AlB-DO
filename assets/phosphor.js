// SPDX-License-Identifier: MIT
// PHOSPHOR · the lane — client half. Design: development-plan/PHOSPHOR.md.
//
// One physical connection per BROWSER PROFILE (the trunk); zero per tab.
// Tabs hold route-scoped subscriptions (circuits) that ride the trunk. The
// tab that owns the trunk is elected with the Web Locks API:
//
//   * exactly one holder per origin per profile — the single-owner property
//     a SharedWorker would give, without a second implementation (and
//     SharedWorker does not exist on Chrome for Android, so the "fallback"
//     would have to be first-class anyway — at which point it is the design);
//   * the browser hands the lock to the next waiter when the holder dies —
//     tab close, crash, navigation — with no timeout heuristics;
//   * `steal: true` covers the one case death detection can't: a holder
//     frozen by the browser while background (detected by an attach that
//     goes un-acked, or heartbeat silence past the throttled-timer horizon).
//
// Every takeover, reconnect, and backpressure kill heals through the same
// path: reopen the trunk, re-subscribe every route with `resync`, let the
// server's `ReconcileList` repair the rows. One recovery policy.
//
// Epoch fencing: each owner mints an epoch at accession and stamps every
// message. Tabs track the highest epoch seen and drop messages from any
// lower one, so a transiently-live stolen-from owner cannot double-deliver
// (a doubled `SlotDelta` insert would be a duplicated row).
//
// The server decides the topics, not the client — unchanged from the
// per-tab lane. A tab announces the ROUTE it is on; the subscribe POST
// resolves it through the same router + manifest the render used.

const LOCK_NAME = 'albedo:phosphor:trunk';
const CHANNEL_NAME = 'albedo:phosphor:v1';
const TRUNK_PATH = '/_albedo/phosphor';
const SUBSCRIBE_PATH = '/_albedo/phosphor/routes';

/** How long an attach may go un-acked before the tab suspects a frozen
 * owner and steals the lock. Long enough for a slow subscribe round-trip,
 * short enough that a stranded tab self-heals within a breath. */
const ACK_TIMEOUT_MS = 4000;

/** Owner heartbeat cadence — background timers throttle to ~1/min, so the
 * follower-side silence horizon is several multiples of the throttled
 * worst case, not of this nominal value. */
const HEARTBEAT_MS = 20000;
const SILENCE_HORIZON_MS = 150000;

/** Reconnect backoff for the trunk EventSource. */
const BACKOFF_MIN_MS = 500;
const BACKOFF_MAX_MS = 8000;

/** Census cadence: owner asks every tab to re-claim its route so refcounts
 * leaked by crashed tabs (no pagehide) are released. */
const CENSUS_MS = 300000;
const CENSUS_GRACE_MS = 10000;

/**
 * Boot the lane for this tab. Returns `true` when PHOSPHOR is viable in
 * this environment (the caller must then NOT open the legacy per-tab
 * patches lane); `false` hands the page to the fallback untouched.
 *
 * Viability is capability-only: the handle installs even on a page that is
 * not live, because the dev channel may attach later (`devAttach`) and the
 * trunk only opens when something actually needs it — the same
 * no-socket-unless-needed gating `__ALBEDO_LIVE__` gives the legacy lane.
 *
 * @param {object} g  window-like: document, location, navigator.locks,
 *                    BroadcastChannel, EventSource, fetch, atob, timers.
 * @returns {boolean}
 */
export function bootPhosphor(g) {
  if (g.__ALBEDO_PHOSPHOR__) return true;
  if (
    !g.document ||
    typeof g.EventSource !== 'function' ||
    typeof g.BroadcastChannel !== 'function' ||
    !g.navigator ||
    !g.navigator.locks ||
    typeof g.navigator.locks.request !== 'function'
  ) {
    return false;
  }

  const phosphor = createPhosphor(g);
  g.__ALBEDO_PHOSPHOR__ = phosphor;

  // A dev-channel module that loaded before us left its sink waiting —
  // claim it now (see albedo-dev-stream.js for the other half of the
  // handshake; load order between an async module and a deferred script is
  // nondeterministic, so both directions must work).
  if (typeof g.__ALBEDO_DEV_SINK_WAITING__ === 'function') {
    phosphor.devAttach(g.__ALBEDO_DEV_SINK_WAITING__);
    g.__ALBEDO_DEV_SINK_WAITING__ = null;
  }

  if (g.__ALBEDO_LIVE__ === true) {
    phosphor.attachRoute();
  }
  return true;
}

/**
 * Build the per-tab lane handle: election participant, channel endpoint,
 * frame filter, and (when elected) the trunk owner. Exported for tests,
 * which drive it against synthetic locks/channel/EventSource.
 */
export function createPhosphor(g) {
  const setTimeout_ = (fn, ms) => g.setTimeout(fn, ms);
  const clearTimeout_ = (id) => g.clearTimeout(id);

  const tab = {
    id: randomId(),
    route: null,
    nonce: randomId(),
    /** True until the first acked attach: the SSR HTML in this tab IS the
     * current state, so the join needs a seed, not a resync. Every
     * re-attach after that (takeover, census) is `fresh:false`, which is
     * what tells the owner to subscribe with `resync`. */
    fresh: true,
    devSink: null,
    maxEpoch: 0,
    lastOwnerSeen: 0,
    ackTimer: null,
    watchdogTimer: null,
    stealing: false,
  };

  const owner = {
    active: false,
    epoch: 0,
    laneId: null,
    source: null,
    backoff: BACKOFF_MIN_MS,
    /** route → Map<tabId, {nonce, fresh}> — the client-side claim mirror.
     * The server holds the authoritative refcount; this decides when to
     * POST add (every claim: each joiner needs its own seed) and remove
     * (last claim gone). */
    claims: new Map(),
    devWanted: false,
    reopenTimer: null,
    hbTimer: null,
    censusTimer: null,
    censusPending: null,
  };

  const channel = new g.BroadcastChannel(CHANNEL_NAME);
  channel.onmessage = (event) => handleMessage(event && event.data);

  /** Owner-side send: BroadcastChannel does not loop back to the poster,
   * and the owner is also a tab — deliver locally first, then broadcast. */
  function publish(msg) {
    handleMessage(msg);
    try {
      channel.postMessage(msg);
    } catch (_err) {
      /* a detached channel must not take down the lane */
    }
  }

  function handleMessage(msg) {
    if (!msg || typeof msg.k !== 'string') return;

    // Epoch fencing for owner-originated messages: accept equal-or-newer,
    // drop stale. A live owner seeing a NEWER epoch than its own has been
    // stolen from (or raced) — it must demote before the old trunk
    // double-delivers.
    if (typeof msg.epoch === 'number') {
      if (msg.epoch < tab.maxEpoch) return;
      tab.maxEpoch = msg.epoch;
      tab.lastOwnerSeen = now();
      if (owner.active && msg.epoch > owner.epoch) demote();
    }

    switch (msg.k) {
      case 'attach':
        if (owner.active) ownerOnAttach(msg);
        break;
      case 'detach':
        if (owner.active) ownerOnDetach(msg);
        break;
      case 'owner':
        // New owner's accession — re-announce so it can rebuild claims.
        reattach();
        break;
      case 'census':
        reattach();
        break;
      case 'attached':
        if (msg.route === tab.route && msg.nonce === tab.nonce) {
          tab.fresh = false;
          clearAckTimer();
        }
        break;
      case 'frame':
        tabOnFrame(msg);
        break;
      case 'dev':
        if (tab.devSink) {
          try {
            tab.devSink(msg.name, msg.data);
          } catch (_err) {
            /* a throwing sink must not break frame delivery */
          }
        }
        break;
      case 'hb':
        break; // lastOwnerSeen already updated by the epoch stamp
      default:
        break;
    }
  }

  // ── Tab role ───────────────────────────────────────────────────────

  function attachRoute() {
    tab.route = (g.location && g.location.pathname) || '/';
    joinElection();
    sendAttach();
    armWatchdog();
  }

  function devAttach(sink) {
    tab.devSink = sink;
    g.__ALBEDO_DEV_SINK_CLAIMED__ = true;
    joinElection();
    sendAttach();
  }

  function sendAttach() {
    publish({
      k: 'attach',
      tabId: tab.id,
      route: tab.route,
      nonce: tab.nonce,
      fresh: tab.fresh,
      dev: !!tab.devSink,
    });
    armAckTimer();
  }

  /** Re-announce to a new owner or a census. Not "fresh": the tab has been
   * live, so its rows may be stale — the owner subscribes with resync. */
  function reattach() {
    if (tab.route === null && !tab.devSink) return;
    if (owner.active) return; // the owner's claims include itself already
    sendAttach();
  }

  function tabOnFrame(msg) {
    if (msg.route !== tab.route) return;
    if (msg.nonce != null && msg.nonce !== tab.nonce) return;
    clearAckTimer(); // frames for us prove the owner heard our attach
    tab.fresh = false;
    const bytes = decodeBase64(g, msg.b64);
    if (!bytes) return;
    const bakabox = g.__bakabox;
    if (!bakabox || typeof bakabox.applyFrameBytes !== 'function') return;
    try {
      bakabox.applyFrameBytes(bytes);
    } catch (_err) {
      /* a bad frame must not stop the lane */
    }
  }

  // Ack-timeout steal — the frozen-owner escape hatch. A dead owner never
  // needs this (the lock transfers on its own); a FROZEN one keeps the
  // lock while executing nothing, so the tab that notices takes it.
  function armAckTimer() {
    clearAckTimer();
    tab.ackTimer = setTimeout_(() => {
      tab.ackTimer = null;
      if (owner.active) return;
      if (now() - tab.lastOwnerSeen < ACK_TIMEOUT_MS) {
        // Something owner-shaped is alive; re-ask instead of stealing.
        sendAttach();
        return;
      }
      steal();
    }, ACK_TIMEOUT_MS);
  }

  function clearAckTimer() {
    if (tab.ackTimer !== null) {
      clearTimeout_(tab.ackTimer);
      tab.ackTimer = null;
    }
  }

  function armWatchdog() {
    if (tab.watchdogTimer !== null) return;
    const tick = () => {
      tab.watchdogTimer = setTimeout_(tick, 30000);
      if (owner.active) return;
      if (tab.lastOwnerSeen !== 0 && now() - tab.lastOwnerSeen > SILENCE_HORIZON_MS) {
        steal();
      }
    };
    tab.watchdogTimer = setTimeout_(tick, 30000);
  }

  let electionJoined = false;
  function joinElection() {
    if (electionJoined) return;
    electionJoined = true;
    requestLock({});
  }

  function steal() {
    if (tab.stealing || owner.active) return;
    tab.stealing = true;
    requestLock({ steal: true });
  }

  function requestLock(options) {
    // The holder callback's returned promise is held open for the life of
    // the ownership; the browser releases the lock when the tab dies or a
    // steal aborts it (the request promise then rejects — demote and
    // rejoin the queue).
    g.navigator.locks
      .request(LOCK_NAME, options, () => {
        tab.stealing = false;
        return new Promise((_resolve) => {
          accede();
          // Never resolved: ownership ends only by death or steal.
        });
      })
      .catch(() => {
        // Stolen from (AbortError) or refused: demote and queue up again
        // unless the page is going away.
        if (owner.active) demote();
        electionJoined = false;
        joinElection();
      });
  }

  // ── Owner role ─────────────────────────────────────────────────────

  function accede() {
    owner.active = true;
    // Having ever seen an owner-stamped epoch means a predecessor existed —
    // this accession is a TAKEOVER, and every route must resubscribe with
    // resync (any tab may have missed frames in the gap). A truly fresh
    // profile (first tab, first owner) has seen nothing and pays none.
    owner.tookOver = tab.maxEpoch > 0;
    owner.epoch = mintEpoch(tab.maxEpoch);
    owner.claims = new Map();
    // Announce; every OTHER tab re-attaches (a follower's `reattach` is a
    // no-op in the owner tab), which rebuilds the claim table and drives
    // the subscribe POSTs…
    publish({ k: 'owner', epoch: owner.epoch });
    // …and the owner is also a tab, so it claims its own route directly.
    if (tab.route !== null || tab.devSink) {
      ownerOnAttach({
        k: 'attach',
        tabId: tab.id,
        route: tab.route,
        nonce: tab.nonce,
        fresh: tab.fresh,
        dev: !!tab.devSink,
      });
    }
    armHeartbeat();
    armCensus();
  }

  // Cancel-safe recurring timers: each tick checks ownership and re-arms
  // itself, so demotion only has to clear the currently-pending id (a
  // returned-id-from-setInterval scheme would orphan the re-armed timer).
  function armHeartbeat() {
    if (!owner.active) return;
    owner.hbTimer = setTimeout_(() => {
      if (!owner.active) return;
      publish({ k: 'hb', epoch: owner.epoch });
      armHeartbeat();
    }, HEARTBEAT_MS);
  }

  function armCensus() {
    if (!owner.active) return;
    owner.censusTimer = setTimeout_(() => {
      if (!owner.active) return;
      runCensus();
      armCensus();
    }, CENSUS_MS);
  }

  function demote() {
    owner.active = false;
    if (owner.source) {
      try {
        owner.source.close();
      } catch (_err) {}
      owner.source = null;
    }
    owner.laneId = null;
    for (const timer of [owner.reopenTimer, owner.hbTimer, owner.censusTimer]) {
      if (timer !== null && timer !== undefined) clearTimeout_(timer);
    }
    owner.reopenTimer = owner.hbTimer = owner.censusTimer = null;
    owner.censusPending = null;
  }

  function ownerOnAttach(msg) {
    if (msg.dev) owner.devWanted = true;
    if (msg.route != null) {
      let tabs = owner.claims.get(msg.route);
      if (!tabs) {
        tabs = new Map();
        owner.claims.set(msg.route, tabs);
      }
      tabs.set(msg.tabId, { nonce: msg.nonce, fresh: !!msg.fresh });
      if (owner.censusPending) {
        const pending = owner.censusPending.get(msg.route);
        if (pending) pending.delete(msg.tabId);
      }
    }
    ensureTrunk();
    if (msg.route != null && owner.laneId) {
      postSubscribe(
        [{ p: msg.route, n: msg.nonce, resync: !msg.fresh }],
        [],
      );
      publish({ k: 'attached', epoch: owner.epoch, route: msg.route, nonce: msg.nonce });
    }
    // No lane yet: the hello handler flushes every claim, and acks then.
  }

  function ownerOnDetach(msg) {
    const tabs = owner.claims.get(msg.route);
    if (!tabs) return;
    tabs.delete(msg.tabId);
    if (tabs.size === 0) {
      owner.claims.delete(msg.route);
      if (owner.laneId) postSubscribe([], [msg.route]);
    }
  }

  function ensureTrunk() {
    const needsDev = owner.devWanted;
    if (owner.source) {
      // A trunk opened before the first dev tab attached lacks the dev
      // events — reopen with the flag; the resubscribe path repairs state.
      if (needsDev && !owner.source.__albedoDev) reopenTrunk();
      return;
    }
    openTrunk();
  }

  function openTrunk() {
    if (!owner.active) return;
    let source;
    const url = TRUNK_PATH + (owner.devWanted ? '?dev=1' : '');
    try {
      source = new g.EventSource(url);
    } catch (_err) {
      scheduleReopen();
      return;
    }
    source.__albedoDev = owner.devWanted;
    owner.source = source;

    source.addEventListener('hello', (event) => {
      owner.backoff = BACKOFF_MIN_MS;
      const hello = safeJson(event && event.data);
      if (!hello || typeof hello.lane !== 'string') return;
      const isReopen =
        owner.tookOver === true || owner.laneId !== null || owner.everHadLane === true;
      owner.laneId = hello.lane;
      owner.everHadLane = true;
      flushClaims(isReopen);
    });
    source.addEventListener('patch', (event) => {
      const envelope = safeJson(event && event.data);
      if (!envelope || typeof envelope.r !== 'string') return;
      publish({
        k: 'frame',
        epoch: owner.epoch,
        route: envelope.r,
        nonce: typeof envelope.n === 'string' ? envelope.n : null,
        b64: envelope.f,
      });
    });
    source.addEventListener('overlay', (event) => {
      publish({ k: 'dev', epoch: owner.epoch, name: 'overlay', data: event && event.data });
    });
    source.addEventListener('hmr', (event) => {
      publish({ k: 'dev', epoch: owner.epoch, name: 'hmr', data: event && event.data });
    });
    source.addEventListener('error', () => {
      try {
        source.close();
      } catch (_err) {}
      if (owner.source === source) {
        owner.source = null;
        owner.laneId = null;
        scheduleReopen();
      }
    });
  }

  function reopenTrunk() {
    if (owner.source) {
      try {
        owner.source.close();
      } catch (_err) {}
      owner.source = null;
    }
    owner.laneId = null;
    openTrunk();
  }

  function scheduleReopen() {
    if (!owner.active || owner.reopenTimer) return;
    owner.reopenTimer = setTimeout_(() => {
      owner.reopenTimer = null;
      openTrunk();
    }, owner.backoff);
    owner.backoff = Math.min(owner.backoff * 2, BACKOFF_MAX_MS);
  }

  /**
   * Push the whole claim table to a fresh lane. On a REOPEN (takeover,
   * reconnect, backpressure kill) every route subscribes with `resync` and
   * no nonce: any tab may have missed frames, so the seed + ReconcileList
   * broadcast to the whole route. On the FIRST lane of a fresh owner, each
   * claim keeps its own nonce/freshness — a fresh single-tab boot pays no
   * resync (its rows are the SSR HTML it just rendered).
   */
  function flushClaims(isReopen) {
    const adds = [];
    for (const [route, tabs] of owner.claims) {
      if (isReopen) {
        adds.push({ p: route, n: null, resync: true });
      } else {
        for (const [, claim] of tabs) {
          adds.push({ p: route, n: claim.nonce, resync: !claim.fresh });
        }
      }
    }
    if (adds.length > 0) postSubscribe(adds, []);
    // Ack every claim now that the lane exists.
    for (const [route, tabs] of owner.claims) {
      for (const [, claim] of tabs) {
        publish({ k: 'attached', epoch: owner.epoch, route, nonce: claim.nonce });
      }
    }
  }

  function postSubscribe(add, remove) {
    if (!owner.laneId || typeof g.fetch !== 'function') return;
    g.fetch(SUBSCRIBE_PATH, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      credentials: 'same-origin',
      // `keepalive` lets the final detach POST of a closing tab survive the
      // page's death instead of being cancelled mid-flight.
      keepalive: true,
      body: JSON.stringify({ lane: owner.laneId, add, remove }),
    })
      .then((response) => {
        if (response && response.status === 404) {
          // The lane died server-side; the trunk error handler usually
          // notices first, but a lost SSE close can leave a zombie — treat
          // the 404 as the close signal.
          reopenTrunk();
        }
      })
      .catch(() => {
        /* transient network failure — the trunk's own error path recovers */
      });
  }

  function runCensus() {
    // Snapshot current claims; anything not re-claimed within the grace
    // window belonged to a tab that died without pagehide. The owner's own
    // claim is excluded — a follower's `reattach` is a no-op in this tab,
    // so it could never re-claim itself and would evict its own route.
    const pending = new Map();
    for (const [route, tabs] of owner.claims) {
      const ghosts = new Set(tabs.keys());
      ghosts.delete(tab.id);
      if (ghosts.size > 0) pending.set(route, ghosts);
    }
    if (pending.size === 0) return;
    owner.censusPending = pending;
    publish({ k: 'census', epoch: owner.epoch });
    setTimeout_(() => {
      if (!owner.active || owner.censusPending !== pending) return;
      for (const [route, ghosts] of pending) {
        for (const tabId of ghosts) {
          ownerOnDetach({ k: 'detach', tabId, route });
        }
      }
      owner.censusPending = null;
    }, CENSUS_GRACE_MS);
  }

  // ── Lifecycle ──────────────────────────────────────────────────────

  if (g.addEventListener) {
    g.addEventListener('pagehide', () => {
      if (tab.route !== null) {
        publish({ k: 'detach', tabId: tab.id, route: tab.route });
      }
      if (owner.active) demote();
    });
  }

  const api = {
    attachRoute,
    devAttach,
    /** Test/debug surface — no production caller. */
    _internals: { tab, owner, publish, handleMessage },
  };
  return api;
}

// ── Helpers ──────────────────────────────────────────────────────────

function now() {
  return Date.now();
}

/** Accession epochs must beat everything already seen — a stealer's clock
 * can lag the victim's, so base on the observed maximum, not the clock. */
function mintEpoch(seen) {
  return Math.max(Date.now(), seen + 1);
}

function randomId() {
  return Math.random().toString(36).slice(2, 10) + Math.random().toString(36).slice(2, 10);
}

function safeJson(text) {
  if (typeof text !== 'string') return null;
  try {
    return JSON.parse(text);
  } catch (_err) {
    return null;
  }
}

/** base64 → Uint8Array; mirrors the legacy lane's decoder. */
function decodeBase64(g, text) {
  if (typeof text !== 'string' || !text) return null;
  const atob = typeof g.atob === 'function' ? g.atob : null;
  if (!atob) return null;
  let binary;
  try {
    binary = atob(text);
  } catch (_err) {
    return null;
  }
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index) & 0xff;
  }
  return bytes;
}
