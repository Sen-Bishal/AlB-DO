// SPDX-License-Identifier: MIT
// bakabox action dispatcher tests.
//
// Exercises `createActionDispatcher` against a stub fetch + stub
// bakabox. The dispatcher's job is to (a) classify the event, (b)
// build an `ActionEnvelope` with `encodeActionEnvelope`, (c) POST it,
// and (d) hand the binary response back to the bakabox VM. Each
// behaviour is asserted in isolation here so a regression surfaces
// against the exact step that broke.
//
// Run with: node --test tests/bakabox/action-dispatcher.test.mjs

import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import {
  DEFAULT_ACTION_ENDPOINT,
  createActionDispatcher,
} from '../../assets/albedo-runtime.js';

/** Builds a recording fake fetch that returns the supplied bytes as a 200 response. */
function fakeFetchReturning(responseBytes) {
  const calls = [];
  const fetchImpl = async (url, init) => {
    calls.push({ url, init });
    return {
      ok: true,
      status: 200,
      arrayBuffer: async () => responseBytes.buffer.slice(
        responseBytes.byteOffset,
        responseBytes.byteOffset + responseBytes.byteLength,
      ),
    };
  };
  return { fetchImpl, calls };
}

/** Minimal bakabox stand-in: records every `applyFrameBytes` call. */
function fakeBakabox() {
  const applied = [];
  return {
    applied,
    applyFrameBytes(bytes) {
      applied.push(bytes);
    },
  };
}

test('dispatcher POSTs to the default endpoint with bincode body', async () => {
  const { fetchImpl, calls } = fakeFetchReturning(new Uint8Array(0));
  const bakabox = fakeBakabox();
  const dispatch = createActionDispatcher({ bakabox, fetch: fetchImpl });

  await dispatch(42, { type: 'click' });

  assert.equal(calls.length, 1);
  const call = calls[0];
  assert.equal(call.url, DEFAULT_ACTION_ENDPOINT);
  assert.equal(call.init.method, 'POST');
  assert.equal(call.init.headers['content-type'], 'application/octet-stream');

  // Body = encodeActionEnvelope({ actionId: 42, eventKind: Click(0), payload: [] })
  // = [42, 0, 0]
  assert.deepStrictEqual(Array.from(call.init.body), [42, 0, 0]);
});

test('dispatcher classifies input events and carries the value as payload bytes', async () => {
  const { fetchImpl, calls } = fakeFetchReturning(new Uint8Array(0));
  const bakabox = fakeBakabox();
  const dispatch = createActionDispatcher({ bakabox, fetch: fetchImpl });

  await dispatch(7, { type: 'input', target: { value: 'hi' } });

  // [actionId=7, eventKind=Input(1), len=2, 'h', 'i']
  assert.deepStrictEqual(
    Array.from(calls[0].init.body),
    [7, 1, 2, 0x68, 0x69],
  );
});

test('dispatcher feeds the binary response into bakabox.applyFrameBytes', async () => {
  const responseBytes = new Uint8Array([0x01, 0x02, 0x03, 0x04]);
  const { fetchImpl } = fakeFetchReturning(responseBytes);
  const bakabox = fakeBakabox();
  const dispatch = createActionDispatcher({ bakabox, fetch: fetchImpl });

  await dispatch(1, { type: 'click' });

  assert.equal(bakabox.applied.length, 1);
  assert.deepStrictEqual(
    Array.from(bakabox.applied[0]),
    Array.from(responseBytes),
    'bakabox must receive exactly the response bytes',
  );
});

test('dispatcher skips applyFrameBytes when the server returns an empty body', async () => {
  const { fetchImpl } = fakeFetchReturning(new Uint8Array(0));
  const bakabox = fakeBakabox();
  const dispatch = createActionDispatcher({ bakabox, fetch: fetchImpl });

  await dispatch(1, { type: 'click' });

  assert.equal(
    bakabox.applied.length,
    0,
    'empty action responses are valid and must not invoke the VM',
  );
});

test('dispatcher drops non-200 responses without throwing', async () => {
  const fetchImpl = async () => ({
    ok: false,
    status: 500,
    arrayBuffer: async () => new ArrayBuffer(0),
  });
  const bakabox = fakeBakabox();
  const dispatch = createActionDispatcher({ bakabox, fetch: fetchImpl });

  // Must resolve without throwing.
  await dispatch(1, { type: 'click' });
  assert.equal(bakabox.applied.length, 0);
});

test('dispatcher honours an endpoint override', async () => {
  const { fetchImpl, calls } = fakeFetchReturning(new Uint8Array(0));
  const dispatch = createActionDispatcher({
    bakabox: fakeBakabox(),
    fetch: fetchImpl,
    endpoint: '/custom/actions',
  });

  await dispatch(1, { type: 'click' });
  assert.equal(calls[0].url, '/custom/actions');
});

test('createActionDispatcher refuses construction without a bakabox', () => {
  assert.throws(
    () => createActionDispatcher({}),
    /requires a bakabox instance/,
  );
});
