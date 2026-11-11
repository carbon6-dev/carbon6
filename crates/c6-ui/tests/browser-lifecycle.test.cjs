'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const { EventEmitter } = require('node:events');
const { browserLifecycle } = require('./browser-lifecycle.cjs');

// No browser, real clocks, timers, async waits or polling. Event delivery and
// close returns are controlled directly, including unresolved promise identity.
function fixture() {
  let time = 100, usage = 0;
  class Target extends EventEmitter {
    close(...args) { this.received = { receiver: this, args }; return this.result; }
  }
  const page = Object.assign(new Target(), { closed: false, isClosed() { return this.closed; }, url: () => 'http://fixture/' });
  const context = Object.assign(new Target(), { pages: () => [page] });
  const browser = Object.assign(new Target(), { connected: true, contexts: () => [context],
    isConnected() { return this.connected; } });
  const options = { browser, label: 'webkit/light/narrow', clock: () => time,
    utc: () => 10000 + time, cpu: () => ({ user: usage++, system: 0 }), stack: () => 'unit close callsite' };
  return { page, context, browser, options, advance: value => { time += value; } };
}

test('early lifecycle observations carry event identity, phase and deterministic timestamps', () => {
  const f = fixture(), d = browserLifecycle(f.options);
  d.observe(f); d.observe(f); // The fixture's started callback may be reported again.
  d.phase('screenshot:busy');
  f.advance(12);
  f.page.emit('crash'); f.page.closed = true; f.page.emit('close');
  f.context.emit('close'); f.browser.connected = false; f.browser.emit('disconnected');
  const snapshot = d.snapshot();
  assert.deepEqual(snapshot.events.map(row => row.kind), ['observebrowser', 'observecontext', 'observepage',
    'pagecrash', 'pageclose', 'contextclose', 'browserdisconnect']);
  for (const row of snapshot.events.slice(3)) {
    assert.equal(row.ms, 12); assert.equal(row.utcMs, 10112); assert.equal(row.phase, 'screenshot:busy');
  }
  assert.equal(snapshot.events[3].target, snapshot.events[4].target);
  assert.notEqual(snapshot.events[4].target, snapshot.events[5].target);
  assert.equal(snapshot.overhead.observerCpuMicros, snapshot.overhead.observerCalls);
  assert.equal(snapshot.overhead.observerWallMs, 0);
  d.dispose();
});

test('all close wrappers forward exact receiver, argument references and return without adopting promises', () => {
  const f = fixture(), d = browserLifecycle(f.options);
  d.observe(f);
  const promise = new Promise(() => {}), args = [{ reason: 'unit cleanup' }, Symbol('extra')];
  // Detect even an attempt to adopt/chain the original return value.
  Object.defineProperty(promise, 'then', { get() { throw new Error('must not inspect promise'); } });
  for (const target of [f.page, f.context, f.browser]) {
    target.result = promise;
    assert.equal(target.close(...args), promise);
    assert.equal(target.received.receiver, target);
    assert.equal(target.received.args[0], args[0]); assert.equal(target.received.args[1], args[1]);
    const receiver = { result: promise };
    assert.equal(target.close.call(receiver, ...args), promise);
    assert.equal(receiver.received.receiver, receiver);
  }
  const markers = d.snapshot().events.filter(row => row.kind === 'intentionalclose');
  assert.equal(markers.length, 6);
  assert.deepEqual(markers.map(row => row.method), ['page.close', 'page.close', 'context.close', 'context.close', 'browser.close', 'browser.close']);
  assert(markers.every(row => row.argumentCount === 2 && row.callsite === 'unit close callsite'));
  d.dispose();
});

test('intentional marker precedes a synchronous close event and excludes original-close work from observer cost', () => {
  const f = fixture();
  f.context.close = function () { f.advance(500); this.emit('close'); return 19; };
  const original = f.context.close, descriptor = Object.getOwnPropertyDescriptor(f.context, 'close');
  const d = browserLifecycle(f.options); d.observe(f); d.phase('cleanup');
  assert.equal(f.context.close(), 19);
  const snapshot = d.snapshot(), last = snapshot.events.slice(-2);
  assert.deepEqual(last.map(row => [row.kind, row.ms, row.phase]), [['intentionalclose', 0, 'cleanup'], ['contextclose', 500, 'cleanup']]);
  assert.equal(snapshot.overhead.observerWallMs, 0);
  assert.equal(snapshot.durationMs, 500);
  d.dispose();
  assert.equal(f.context.close, original);
  assert.deepEqual(Object.getOwnPropertyDescriptor(f.context, 'close'), descriptor);
});

test('close preserves synchronous exception identity and primitive returns', () => {
  const f = fixture(), error = new Error('close threw');
  f.context.close = () => { throw error; };
  const d = browserLifecycle(f.options); d.observe(f);
  assert.throws(() => f.context.close(), actual => actual === error);
  for (const value of [undefined, null, false, 0, 'return']) {
    f.page.result = value;
    assert.equal(f.page.close(), value);
  }
  d.dispose();
});

test('close markers do not evaluate argument accessors before forwarding them', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  const options = { get reason() { throw new Error('not a passive read'); } };
  f.page.result = 12;
  assert.equal(f.page.close(options), 12);
  assert.equal(f.page.received.args[0], options);
  assert.equal(d.snapshot().events.at(-1).reason, undefined);
  d.dispose();
});

test('a diagnostic stack failure cannot replace the original close return or error', () => {
  const f = fixture(), original = new Error('original close');
  f.options.stack = () => { throw new Error('stack unavailable'); };
  f.context.close = () => { throw original; };
  const d = browserLifecycle(f.options); d.observe(f);
  f.page.result = 29;
  assert.equal(f.page.close(), 29);
  assert.throws(() => f.context.close(), error => error === original);
  assert.deepEqual(d.snapshot().secondaryErrors.map(row => row.stage),
    ['intentionalclose instrumentation', 'intentionalclose instrumentation']);
  d.dispose();
});

test('first-error inventory is fixed before secondary capture and intentional cleanup', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f); d.phase('screenshot:busy');
  const original = new Error('original screenshot'), screenshot = new Error('diagnostic screenshot'), cleanup = new Error('cleanup');
  d.firstError(original);
  const before = d.snapshot();
  f.page.closed = true; f.browser.connected = false;
  d.phase('failure-screenshot'); d.secondary('failure-screenshot', screenshot);
  d.phase('cleanup'); f.context.close(); d.secondary('context.close', cleanup);
  d.firstError(cleanup); d.dispose();
  const after = d.snapshot();
  assert.equal(after.firstError.error.stack, original.stack);
  assert.equal(after.firstError.phase, 'screenshot:busy');
  assert.equal(after.firstError.inventory.connected, true);
  assert.equal(after.firstError.inventory.contexts[0].pages[0].closed, false);
  assert.deepEqual(after.firstError, before.firstError);
  assert.equal(before.events.some(row => row.kind === 'intentionalclose'), false);
  assert.equal(after.events.at(-1).kind, 'intentionalclose');
  assert.deepEqual(after.secondaryErrors.map(row => row.error.stack), [screenshot.stack, cleanup.stack]);
  assert.equal(after.disposed, true);
});

test('unavailable target inventory remains diagnostic rather than masking the original failure', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  f.browser.contexts = () => { throw new Error('transport lost'); };
  f.browser.isConnected = () => { throw new Error('connection query failed'); };
  d.firstError(new Error('first'));
  const first = d.snapshot().firstError;
  assert.equal(first.error.message, 'first');
  assert.equal(first.inventory.liveContexts.unavailable.message, 'transport lost');
  assert.equal(first.inventory.connected.unavailable.message, 'connection query failed');
  assert.equal(first.inventory.contexts.length, 1, 'observed targets survive a live-collection query failure');
  assert.equal(first.inventory.contexts[0].inLiveContexts, null);
  assert.equal(first.inventory.contexts[0].closed, null);
  assert.equal(first.inventory.contexts[0].pages[0].closed, false);
  d.dispose();
});

test('closed page removed from live collection remains in first-error inventory with its ID and URL', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  const id = d.snapshot().events.find(row => row.kind === 'observepage').target;
  f.page.url = () => 'http://fixture/final-document';
  f.page.closed = true; f.page.emit('close');
  f.context.pages = () => [];
  d.firstError(new Error('screenshot target closed'));
  const context = d.snapshot().firstError.inventory.contexts[0], page = context.pages[0];
  assert.deepEqual(context.livePages, []);
  assert.equal(context.closed, false);
  assert.equal(page.id, id); assert.equal(page.observed, true); assert.equal(page.inLivePages, false);
  assert.equal(page.closed, true); assert.equal(page.closedSource, 'pageclose-event');
  assert.equal(page.url, 'http://fixture/final-document'); assert.equal(page.closeObserved, true);
  d.dispose();
});

test('closed context and page removed before first error are retained as immutable plain values', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  const contextId = d.snapshot().events.find(row => row.kind === 'observecontext').target;
  f.page.closed = true; f.page.emit('close'); f.context.emit('close');
  f.context.pages = () => []; f.browser.contexts = () => [];
  d.firstError(new Error('lost context'));
  const before = d.snapshot(), context = before.firstError.inventory.contexts[0];
  assert.deepEqual(before.firstError.inventory.liveContexts, []);
  assert.equal(context.id, contextId); assert.equal(context.inLiveContexts, false);
  assert.equal(context.closed, true); assert.equal(context.closedSource, 'contextclose-event');
  assert.equal(context.pages[0].closed, true); assert.equal(context.pages[0].inLivePages, false);
  assert.equal(context.pages[0].url, 'http://fixture/');
  const first = JSON.stringify(before.firstError);
  assert.deepEqual(before.retainedReferences, { contexts: 1, pages: 1 });
  f.page.closed = false; f.page.url = () => 'http://fixture/changed-after-error';
  d.phase('cleanup'); d.firstError(new Error('secondary')); d.dispose();
  const after = d.snapshot();
  assert.equal(JSON.stringify(after.firstError), first);
  assert.deepEqual(after.retainedReferences, { contexts: 0, pages: 0 });
});

test('removal alone does not invent context closure; public page status is separately attributed', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  f.page.closed = true; f.context.pages = () => []; f.browser.contexts = () => [];
  d.firstError(new Error('removed without close events'));
  const context = d.snapshot().firstError.inventory.contexts[0], page = context.pages[0];
  assert.equal(context.inLiveContexts, false); assert.equal(context.closed, null);
  assert.equal(context.closedSource, 'unknown');
  assert.equal(page.closed, true); assert.equal(page.closedSource, 'page.isClosed');
  assert.equal(page.closeObserved, false);
  assert(!d.snapshot().events.some(row => ['pageclose', 'contextclose'].includes(row.kind)));
  d.dispose();
});

test('closed removed targets retain event proof and last URL if public queries are unavailable', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  f.page.url = () => 'http://fixture/last-known';
  f.page.emit('close'); f.context.emit('close');
  f.browser.contexts = () => [];
  f.context.pages = () => { throw new Error('page list gone'); };
  f.page.isClosed = () => { throw new Error('page state gone'); };
  f.page.url = () => { throw new Error('URL query gone'); };
  d.firstError(new Error('original screenshot'));
  const context = d.snapshot().firstError.inventory.contexts[0], page = context.pages[0];
  assert.equal(context.closed, true); assert.equal(context.livePages.unavailable.message, 'page list gone');
  assert.equal(page.inLivePages, null); assert.equal(page.closed, true); assert.equal(page.closedSource, 'pageclose-event');
  assert.equal(page.url, 'http://fixture/last-known'); assert.equal(page.urlSource, 'last-observed');
  assert.equal(page.closedQueryError.message, 'page state gone'); assert.equal(page.urlQueryError.message, 'URL query gone');
  assert.equal(d.snapshot().firstError.error.message, 'original screenshot');
  d.dispose();
});

test('a dynamically observed popup remains inventoried after removal without classifying a crash as closure', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  const popup = Object.assign(new EventEmitter(), { close() {}, isClosed: () => false, url: () => 'http://fixture/popup' });
  f.context.emit('page', popup); popup.emit('crash');
  d.firstError(new Error('capture failed'));
  const page = d.snapshot().firstError.inventory.contexts[0].pages.find(page => page.url === 'http://fixture/popup');
  assert.equal(page.observed, true); assert.equal(page.inLivePages, false);
  assert.equal(page.crashObserved, true); assert.equal(page.closed, false); assert.equal(page.closeObserved, false);
  assert.equal(page.closedSource, 'page.isClosed');
  assert.deepEqual(d.snapshot().retainedReferences, { contexts: 1, pages: 2 });
  d.dispose();
  assert.deepEqual(d.snapshot().retainedReferences, { contexts: 0, pages: 0 });
});

test('missing targets with unavailable closed query remain unknown without observed close evidence', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  f.browser.contexts = () => []; f.context.pages = () => [];
  f.page.isClosed = () => { throw new Error('unavailable'); };
  d.firstError(new Error('first'));
  const context = d.snapshot().firstError.inventory.contexts[0], page = context.pages[0];
  assert.equal(context.closed, null); assert.equal(page.closed, null);
  assert.equal(page.closedSource, 'unknown'); assert.equal(page.closeObserved, false);
  assert.equal(page.url, 'http://fixture/');
  d.dispose();
});

test('new context pages are observed once and all listeners and inherited wrappers are removed after each case', () => {
  const f = fixture(), originals = [f.browser.close, f.context.close, f.page.close];
  const unrelated = () => {};
  f.browser.on('disconnected', unrelated);
  for (let i = 0; i < 40; i++) {
    const d = browserLifecycle(f.options); d.observe(f);
    const popup = Object.assign(new EventEmitter(), { close() {}, isClosed: () => false, url: () => 'http://fixture/popup' });
    const originalPopupClose = popup.close;
    f.context.emit('page', popup); f.context.emit('page', popup);
    assert.equal(popup.listenerCount('close'), 1); assert.equal(popup.listenerCount('crash'), 1);
    popup.emit('crash');
    const snapshot = d.snapshot();
    assert.equal(snapshot.events.filter(row => row.kind === 'observepage').length, 2);
    d.dispose(); d.dispose();
    assert.equal(popup.close, originalPopupClose);
    assert.equal(popup.listenerCount('close'), 0); assert.equal(popup.listenerCount('crash'), 0);
    for (const target of [f.browser, f.context, f.page]) assert.equal(Object.hasOwn(target, 'close'), false);
    assert.deepEqual([f.browser.close, f.context.close, f.page.close], originals);
    assert.equal(f.context.listenerCount('close'), 0); assert.equal(f.context.listenerCount('page'), 0);
    assert.equal(f.page.listenerCount('close'), 0); assert.equal(f.page.listenerCount('crash'), 0);
    assert.deepEqual(f.browser.listeners('disconnected'), [unrelated]);
    const count = d.snapshot().events.length;
    f.page.emit('close'); f.browser.emit('disconnected'); d.observe(f); d.phase('ignored');
    assert.equal(d.snapshot().events.length, count);
  }
});

test('disposing does not overwrite a later method owner', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  const later = () => 'later'; f.context.close = later;
  d.dispose();
  assert.equal(f.context.close, later);
});

for (const order of [[0, 1], [1, 0]]) test(`nested observers restore methods with disposal order ${order.join(',')}`, () => {
  const f = fixture(), targets = [f.browser, f.context, f.page];
  const originals = targets.map(target => target.close);
  const observers = [browserLifecycle(f.options), browserLifecycle({ ...f.options, label: 'nested' })];
  for (const observer of observers) observer.observe(f);
  const installed = targets.map(target => target.close);
  const promise = new Promise(() => {}), argument = { reason: 'nested' };
  Object.defineProperty(promise, 'then', { get() { throw new Error('must not inspect promise'); } });
  for (const target of targets) {
    target.result = promise;
    assert.equal(target.close(argument), promise);
    assert.equal(target.received.receiver, target);
    assert.equal(target.received.args[0], argument);
  }
  for (const observer of observers) assert.equal(observer.snapshot().events.filter(row => row.kind === 'intentionalclose').length, 3);
  observers[order[0]].dispose();
  assert.deepEqual(targets.map(target => target.close), installed, 'active observer retains the shared wrappers');
  assert.equal(f.browser.listenerCount('disconnected'), 1);
  const stoppedCount = observers[order[0]].snapshot().events.length;
  for (const target of targets) {
    const receiver = { result: promise };
    assert.equal(target.close.call(receiver, argument), promise);
    assert.equal(receiver.received.receiver, receiver);
    assert.equal(receiver.received.args[0], argument);
  }
  f.browser.emit('disconnected'); f.page.emit('close'); f.context.emit('close');
  assert.equal(observers[order[0]].snapshot().events.length, stoppedCount);
  assert.equal(observers[order[1]].snapshot().events.filter(row => row.kind === 'intentionalclose').length, 6);
  observers[order[1]].dispose();
  assert.deepEqual(targets.map(target => target.close), originals);
  for (const target of targets) assert.equal(Object.hasOwn(target, 'close'), false, 'no disposed wrapper is restored');
  assert.equal(f.browser.listenerCount('disconnected'), 0);
  assert.equal(f.context.listenerCount('page'), 0); assert.equal(f.context.listenerCount('close'), 0);
  assert.equal(f.page.listenerCount('close'), 0); assert.equal(f.page.listenerCount('crash'), 0);
});

test('a saved close wrapper becomes passive after disposal', () => {
  const f = fixture(), d = browserLifecycle(f.options); d.observe(f);
  const close = f.page.close;
  d.dispose();
  const count = d.snapshot().events.length;
  f.page.result = 71;
  assert.equal(close.call(f.page, 'late'), 71);
  assert.equal(d.snapshot().events.length, count);
  assert.deepEqual(f.page.received.args, ['late']);
});

test('an own nonconfigurable writable method is restored with its original descriptor', () => {
  const f = fixture();
  Object.defineProperty(f.context, 'close', { configurable: false, enumerable: true,
    writable: true, value() { return 43; } });
  const descriptor = Object.getOwnPropertyDescriptor(f.context, 'close');
  const d = browserLifecycle(f.options); d.observe(f);
  assert.notEqual(f.context.close, descriptor.value);
  assert.equal(f.context.close(), 43);
  d.dispose();
  assert.deepEqual(Object.getOwnPropertyDescriptor(f.context, 'close'), descriptor);
});

test('real synchronous observer work is accounted with a direct fake clock', () => {
  const f = fixture();
  f.options.clock = () => { f.advance(1); return f.options.utc() - 10000; };
  const d = browserLifecycle(f.options); d.observe(f); f.page.emit('close'); d.dispose();
  const snapshot = d.snapshot();
  assert(snapshot.overhead.observerWallMs > 0);
  assert(snapshot.overhead.observerWallMs <= snapshot.durationMs + 10);
  assert.equal(snapshot.overhead.observerCpuMicros, snapshot.overhead.observerCalls);
});
