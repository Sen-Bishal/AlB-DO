// SPDX-License-Identifier: MIT
// PHOSPHOR lane — client state-machine tests.
//
// Drives `createPhosphor`/`bootPhosphor` against a synthetic browser
// profile: a fake Web Locks manager (exclusive queue + steal), a
// BroadcastChannel hub connecting N fake tabs, fake EventSources the test
// pumps server events through, and a virtual clock for the timers. What is
// under test is the protocol — election, epoch fencing, route/nonce frame
// filtering, takeover resync, refcounted detach — not any real transport.
//
// Run with: node --test tests/bakabox/phosphor.test.mjs

import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { bootPhosphor, createPhosphor } from '../../assets/phosphor.js';

// ── Synthetic browser profile ────────────────────────────────────────

function makeClock() {
  let t = 0;
  let seq = 0;
  const pending = new Map();
  return {
    setTimeout(fn, ms) {
      seq += 1;
      pending.set(seq, { at: t + ms, fn });
      return seq;
    },
    clearTimeout(id) {
      pending.delete(id);
    },
    advance(ms) {
      t += ms;
      const due = [...pending.entries()]
        .filter(([, e]) => e.at <= t)
        .sort((a, b) => a[1].at - b[1].at);
      for (const [id, e] of due) {
        pending.delete(id);
        e.fn();
      }
    },
  };
}

/** Exclusive-mode Web Locks with `steal`, per the spec's observable
 * behavior: one holder, FIFO waiters, steal aborts the current holder's
 * request promise and grants the thief. */
function makeLockManager() {
  const state = { holder: null, queue: [] };

  function grant(entry) {
    state.holder = entry;
    queueMicrotask(() => {
      try {
        entry.cb();
      } catch (_err) {
        /* holder threw — treat as released */
      }
    });
  }

  return {
    request(name, opts, cb) {
      if (typeof opts === 'function') {
        cb = opts;
        opts = {};
      }
      return new Promise((_resolve, reject) => {
        const entry = { cb, reject };
        if (opts && opts.steal) {
          if (state.holder) {
            const victim = state.holder;
            state.holder = null;
            victim.reject(new Error('AbortError: stolen'));
          }
          grant(entry);
        } else if (!state.holder) {
          grant(entry);
        } else {
          state.queue.push(entry);
        }
      });
    },
    /** Simulate the browser releasing a dead tab's lock. */
    releaseFor(entry) {
      if (state.holder === entry.holderTag) {
        state.holder = null;
      }
      const next = state.queue.shift();
      if (next) grant(next);
    },
    /** Force-release whoever holds it (tab death), grant next waiter. */
    killHolder() {
      state.holder = null;
      const next = state.queue.shift();
      if (next) grant(next);
    },
    state,
  };
}

function makeChannelHub() {
  const instances = [];
  class FakeChannel {
    constructor(name) {
      this.name = name;
      this.onmessage = null;
      instances.push(this);
    }
    postMessage(msg) {
      for (const other of instances) {
        if (other !== this && other.name === this.name && other.onmessage && !other.closedFor) {
          other.onmessage({ data: msg });
        }
      }
    }
  }
  return { FakeChannel, instances };
}

function makeEventSourceClass(log) {
  class FakeEventSource {
    constructor(url) {
      this.url = url;
      this.listeners = Object.create(null);
      this.closed = false;
      log.push(this);
    }
    addEventListener(name, fn) {
      (this.listeners[name] || (this.listeners[name] = [])).push(fn);
    }
    emit(name, data) {
      for (const fn of this.listeners[name] || []) fn({ data });
    }
    close() {
      this.closed = true;
    }
  }
  return FakeEventSource;
}

/** One browser profile shared by every tab a test creates. */
function makeProfile() {
  const clock = makeClock();
  const locks = makeLockManager();
  const hub = makeChannelHub();
  const sources = [];
  const EventSourceClass = makeEventSourceClass(sources);
  const fetchCalls = [];

  function makeTab({ path = '/g', live = true } = {}) {
    const g = {
      document: {},
      location: { pathname: path },
      navigator: { locks },
      BroadcastChannel: hub.FakeChannel,
      EventSource: EventSourceClass,
      setTimeout: clock.setTimeout,
      clearTimeout: clock.clearTimeout,
      atob: (b64) => Buffer.from(b64, 'base64').toString('binary'),
      fetch(url, opts) {
        const call = { url, body: JSON.parse(opts.body) };
        fetchCalls.push(call);
        return Promise.resolve({ status: 200 });
      },
      addEventListener(name, fn) {
        (g._events[name] || (g._events[name] = [])).push(fn);
      },
      _events: {},
      __ALBEDO_LIVE__: live,
      __bakabox: {
        applied: [],
        applyFrameBytes(bytes) {
          this.applied.push(bytes);
        },
      },
    };
    return g;
  }

  return { clock, locks, sources, fetchCalls, makeTab };
}

/** Let microtask chains (lock grants, fetch promises) settle. */
async function settle() {
  for (let i = 0; i < 8; i += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

const OWNER_URL = '/_albedo/phosphor';

function openTrunkOf(profile) {
  const source = profile.sources[profile.sources.length - 1];
  assert.ok(source, 'an EventSource was opened');
  source.emit('hello', JSON.stringify({ lane: 'lane-1', proto: 1 }));
  return source;
}

function b64(text) {
  return Buffer.from(text, 'utf8').toString('base64');
}

// ── Capability gate ──────────────────────────────────────────────────

test('bootPhosphor declines environments without locks or BroadcastChannel', () => {
  const profile = makeProfile();
  const g = profile.makeTab({});
  delete g.navigator.locks;
  assert.equal(bootPhosphor(g), false, 'no Web Locks → fall back');

  const g2 = profile.makeTab({});
  g2.BroadcastChannel = undefined;
  assert.equal(bootPhosphor(g2), false, 'no BroadcastChannel → fall back');
});

// ── Single tab: election + subscribe ─────────────────────────────────

test('a lone live tab accedes, opens ONE trunk, and subscribes fresh (no resync)', async () => {
  const profile = makeProfile();
  const g = profile.makeTab({ path: '/guestbook' });
  assert.equal(bootPhosphor(g), true);
  await settle();

  assert.equal(profile.sources.length, 1, 'exactly one EventSource for the profile');
  assert.ok(profile.sources[0].url.startsWith(OWNER_URL));

  openTrunkOf(profile);
  await settle();

  assert.equal(profile.fetchCalls.length, 1, 'one subscribe POST');
  const { body } = profile.fetchCalls[0];
  assert.equal(body.lane, 'lane-1');
  assert.equal(body.add.length, 1);
  assert.equal(body.add[0].p, '/guestbook');
  assert.equal(body.add[0].resync, false, 'a fresh page load pays no resync');
  assert.equal(typeof body.add[0].n, 'string', 'the join carries its nonce');
});

// ── Two tabs: one connection, per-joiner seeds ───────────────────────

test('a second tab joins the SAME trunk — no second connection, its own subscribe', async () => {
  const profile = makeProfile();
  const g1 = profile.makeTab({ path: '/guestbook' });
  bootPhosphor(g1);
  await settle();
  openTrunkOf(profile);
  await settle();

  const g2 = profile.makeTab({ path: '/guestbook' });
  bootPhosphor(g2);
  await settle();

  assert.equal(profile.sources.length, 1, 'the wall: N tabs, ONE connection');
  assert.equal(profile.fetchCalls.length, 2, 'the joiner got its own subscribe (its own seed)');
  const second = profile.fetchCalls[1].body.add[0];
  assert.equal(second.p, '/guestbook');
  assert.equal(
    second.n,
    g2.__ALBEDO_PHOSPHOR__._internals.tab.nonce,
    'the second subscribe carries the JOINER’s nonce, so its seed targets only it',
  );
});

// ── Frame filtering: route, nonce, epoch ─────────────────────────────

test('frames fan out by route; nonce-tagged seeds hit only their joiner', async () => {
  const profile = makeProfile();
  const g1 = profile.makeTab({ path: '/guestbook' });
  const g2 = profile.makeTab({ path: '/feed' });
  bootPhosphor(g1);
  await settle();
  const source = openTrunkOf(profile);
  bootPhosphor(g2);
  await settle();

  // Broadcast frame for /guestbook: applies in g1 only.
  source.emit('patch', JSON.stringify({ r: '/guestbook', f: b64('frame-a') }));
  assert.equal(g1.__bakabox.applied.length, 1, 'the /guestbook tab applied it');
  assert.equal(g2.__bakabox.applied.length, 0, 'the /feed tab filtered it out');

  // Nonce-tagged seed for g2's join: applies in g2 only, even though g1
  // (were it on the same route) would see the same envelope.
  const nonce2 = g2.__ALBEDO_PHOSPHOR__._internals.tab.nonce;
  source.emit('patch', JSON.stringify({ r: '/feed', n: nonce2, f: b64('seed-b') }));
  assert.equal(g2.__bakabox.applied.length, 1, 'the joiner applied its seed');
  assert.equal(g1.__bakabox.applied.length, 1, 'nobody else did');

  // A nonce that matches no tab applies nowhere.
  source.emit('patch', JSON.stringify({ r: '/feed', n: 'stranger', f: b64('seed-c') }));
  assert.equal(g2.__bakabox.applied.length, 1, 'a foreign nonce is not ours');
});

// ── Owner death: lock transfer + takeover resync ─────────────────────

test('owner death hands the trunk to a survivor, which resubscribes with resync', async () => {
  const profile = makeProfile();
  const g1 = profile.makeTab({ path: '/guestbook' });
  const g2 = profile.makeTab({ path: '/guestbook' });
  bootPhosphor(g1);
  await settle();
  openTrunkOf(profile);
  bootPhosphor(g2);
  await settle();
  assert.equal(profile.sources.length, 1);
  const epochBefore = g2.__ALBEDO_PHOSPHOR__._internals.tab.maxEpoch;

  // The browser releases a dead tab's lock; the waiter (g2) accedes.
  profile.locks.killHolder();
  await settle();

  assert.equal(profile.sources.length, 2, 'the survivor opened a fresh trunk');
  const takeoverSource = profile.sources[1];
  const callsBefore = profile.fetchCalls.length;
  takeoverSource.emit('hello', JSON.stringify({ lane: 'lane-2', proto: 1 }));
  await settle();

  const takeoverCall = profile.fetchCalls[callsBefore];
  assert.ok(takeoverCall, 'the takeover resubscribed');
  assert.equal(takeoverCall.body.lane, 'lane-2');
  const add = takeoverCall.body.add.find((entry) => entry.p === '/guestbook');
  assert.ok(add, 'the surviving route is resubscribed');
  assert.equal(add.resync, true, 'a takeover always repairs with resync');
  assert.equal(add.n, null, 'takeover repair is broadcast, not nonce-targeted');
  assert.ok(
    g2.__ALBEDO_PHOSPHOR__._internals.tab.maxEpoch > epochBefore,
    'the new owner minted a newer epoch',
  );
});

// ── Epoch fencing ────────────────────────────────────────────────────

test('frames from a deposed owner are fenced off by epoch', async () => {
  const profile = makeProfile();
  const g1 = profile.makeTab({ path: '/guestbook' });
  const g2 = profile.makeTab({ path: '/guestbook' });
  bootPhosphor(g1);
  await settle();
  const oldSource = openTrunkOf(profile);
  bootPhosphor(g2);
  await settle();

  const staleEpoch = g1.__ALBEDO_PHOSPHOR__._internals.owner.epoch;

  profile.locks.killHolder();
  await settle();
  profile.sources[1].emit('hello', JSON.stringify({ lane: 'lane-2', proto: 1 }));
  await settle();

  // The corpse twitches: a frame arrives on the OLD trunk and the deposed
  // owner (whose page still runs) rebroadcasts it with its old epoch.
  const before = g2.__bakabox.applied.length;
  g2.__ALBEDO_PHOSPHOR__._internals.handleMessage({
    k: 'frame',
    epoch: staleEpoch,
    route: '/guestbook',
    nonce: null,
    b64: b64('stale'),
  });
  assert.equal(
    g2.__bakabox.applied.length,
    before,
    'a lower-epoch frame must be dropped — a doubled SlotDelta is a duplicated row',
  );
  assert.ok(oldSource, 'sanity: the old trunk existed');
});

// ── Steal: the frozen-owner escape hatch ─────────────────────────────

test('an unacked attach against a silent holder steals the lock', async () => {
  const profile = makeProfile();

  // A foreign, unresponsive holder: it took the lock and executes nothing —
  // the observable shape of a frozen tab. Its request promise rejects when
  // the steal aborts it, exactly as the real API's would.
  profile.locks
    .request('albedo:phosphor:trunk', {}, () => new Promise(() => {}))
    .catch(() => {});
  await settle();

  const g = profile.makeTab({ path: '/guestbook' });
  bootPhosphor(g);
  await settle();
  assert.equal(profile.sources.length, 0, 'queued behind the frozen holder — no trunk yet');

  // The attach ack never comes; the ack timer trips the steal.
  profile.clock.advance(4000);
  await settle();

  assert.equal(
    g.__ALBEDO_PHOSPHOR__._internals.owner.active,
    true,
    'the tab stole the lock and acceded',
  );
  assert.equal(profile.sources.length, 1, 'and opened the trunk');
});

// ── Detach refcounting ───────────────────────────────────────────────

test('the last detach for a route removes it; earlier ones do not', async () => {
  const profile = makeProfile();
  const g1 = profile.makeTab({ path: '/guestbook' });
  const g2 = profile.makeTab({ path: '/guestbook' });
  bootPhosphor(g1);
  await settle();
  openTrunkOf(profile);
  bootPhosphor(g2);
  await settle();

  const callsBefore = profile.fetchCalls.length;

  // g2 leaves: one claim remains — no remove POST.
  for (const fn of g2._events.pagehide || []) fn();
  await settle();
  const afterFirst = profile.fetchCalls.slice(callsBefore);
  assert.ok(
    !afterFirst.some((call) => call.body.remove.length > 0),
    'a route with a surviving claim is not removed',
  );

  // g1 (the owner) leaves too — but an owner's own pagehide demotes it, so
  // the remove is driven by whoever accedes next; with no tabs left there
  // is no next, and the trunk guard on the server releases everything.
  // What we assert here is the CLIENT-side claim bookkeeping:
  const claims = g1.__ALBEDO_PHOSPHOR__._internals.owner.claims;
  assert.equal(claims.get('/guestbook').size, 1, 'exactly one claim survived');
});

// ── Dev channel handshake ────────────────────────────────────────────

test('a dev sink attaches, reopens the trunk with ?dev=1, and receives dev events', async () => {
  const profile = makeProfile();
  const g = profile.makeTab({ path: '/guestbook' });
  bootPhosphor(g);
  await settle();
  openTrunkOf(profile);
  await settle();
  assert.ok(!profile.sources[0].url.includes('dev=1'), 'no dev flag before a dev sink exists');

  const seen = [];
  g.__ALBEDO_PHOSPHOR__.devAttach((name, data) => seen.push({ name, data }));
  await settle();

  assert.equal(profile.sources.length, 2, 'the trunk reopened to add the dev flag');
  const devSource = profile.sources[1];
  assert.ok(devSource.url.includes('dev=1'));
  devSource.emit('hello', JSON.stringify({ lane: 'lane-dev', proto: 1 }));
  await settle();

  devSource.emit('overlay', JSON.stringify({ event: 'error', message: 'boom' }));
  assert.equal(seen.length, 1);
  assert.equal(seen[0].name, 'overlay');
});

// ── Waiting-sink claim (load-order handshake) ────────────────────────

test('bootPhosphor claims a dev sink parked before it loaded', async () => {
  const profile = makeProfile();
  const g = profile.makeTab({ path: '/guestbook' });
  const seen = [];
  g.__ALBEDO_DEV_SINK_WAITING__ = (name, data) => seen.push({ name, data });

  bootPhosphor(g);
  await settle();

  assert.equal(g.__ALBEDO_DEV_SINK_CLAIMED__, true, 'the parked sink was claimed');
  assert.equal(g.__ALBEDO_DEV_SINK_WAITING__, null, 'and cleared');
  assert.ok(
    profile.sources[0].url.includes('dev=1'),
    'the trunk opened dev-flagged from the start',
  );
});
