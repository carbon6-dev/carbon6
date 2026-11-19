'use strict';

const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');
const { runInNewContext } = require('node:vm');

test('bridge streams at most two downloads and drops disconnected work', async () => {
  const script = path.join(__dirname, '../js/viewers.js');
  const window = {};
  const origin = 'http://127.0.0.1:19000';
  const location = new URL(origin);
  const bodies = new Map();
  const calls = [];
  let observe;
  runInNewContext(readFileSync(script, 'utf8'), {
    window, location, URL, AbortController, DOMException, TextDecoder, Node: { ELEMENT_NODE: 1 },
    document: { addEventListener() {}, body: {} },
    MutationObserver: class { constructor(callback) { observe = callback; } observe() {} },
    fetch(url, { signal }) {
      const name = url.searchParams.get('path');
      calls.push(name);
      const body = new ReadableStream({ start(controller) {
        bodies.set(name, controller);
        signal?.addEventListener('abort', () => controller.error(signal.reason), { once: true });
      } });
      return new Response(body);
    },
  }, { filename: script });
  const owner = () => ({ isConnected: true, nodeType: 1, matches: () => true, querySelectorAll: () => [] });
  const disconnect = el => { el.isConnected = false; observe([{ addedNodes: [], removedNodes: [el] }]); };
  const read = (name, el) => window.c6Viewers.text(`${origin}/api/v1/workers/test/fs/download?path=${name}`, el);
  const turn = () => new Promise(resolve => setImmediate(resolve));
  const finish = (name, text) => {
    bodies.get(name).enqueue(new TextEncoder().encode(text));
    bodies.get(name).close();
  };

  const active = owner();
  const queued = owner();
  const first = read('first', active);
  const second = read('second', owner());
  const third = read('third', queued);
  const fourth = read('fourth', owner());
  const fifth = read('fifth', owner());
  const canceledActive = assert.rejects(first, { name: 'AbortError' });
  const canceledQueued = assert.rejects(third, { name: 'AbortError' });
  await turn();
  assert.deepEqual(calls, ['first', 'second']);

  disconnect(queued);
  await canceledQueued;
  assert.deepEqual(calls, ['first', 'second']);
  disconnect(active);
  await canceledActive;
  await turn();
  assert.deepEqual(calls, ['first', 'second', 'fourth']);

  finish('second', '2');
  assert.equal(await second, '2');
  await turn();
  assert.deepEqual(calls, ['first', 'second', 'fourth', 'fifth']);
  finish('fourth', '4');
  finish('fifth', '5');
  assert.equal(await fourth, '4');
  assert.equal(await fifth, '5');
});
