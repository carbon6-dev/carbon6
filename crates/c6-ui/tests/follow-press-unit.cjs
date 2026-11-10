'use strict';
// Deterministic ownership/timer checks against the actual selected JS helper.
// This is not browser hit-testing or a model of Rust's follow permission.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const file = process.argv[2] || path.join(__dirname, '../js/motion.js');
const source = fs.readFileSync(file, 'utf8');
const start = source.indexOf('window.c6FollowPress =');
assert(start >= 0, 'press helper exists');
const end = source.indexOf('\n};', start);
assert(end > start);
const code = source.slice(start, end + 3);

function fixture() {
  let now = 0, next = 0;
  const timers = new Map(), changes = [], stops = [];
  class Target {
    constructor(parent, control = false) { this.parentElement = parent; this.control = control; this.listeners = new Map(); this.isConnected = true; }
    addEventListener(type, callback) { if (!this.listeners.has(type)) this.listeners.set(type, new Set()); this.listeners.get(type).add(callback); }
    removeEventListener(type, callback) { this.listeners.get(type)?.delete(callback); }
    contains(target) { for (let el = target; el; el = el.parentElement) if (el === this) return true; return false; }
    closest() { for (let el = this; el; el = el.parentElement) if (el.control) return el; return null; }
    matches() { return !!this.disabled; }
    emit(type, event) { for (const callback of [...(this.listeners.get(type) || [])]) callback(event); }
    count() { return [...this.listeners.values()].reduce((n, set) => n + set.size, 0); }
  }
  const document = new Target(), window = new Target();
  document.hidden = false;
  const element = new Target(document), control = new Target(element, true), plain = new Target(element), outside = new Target(document, true);
  vm.runInNewContext(code, { window, document, Element: Target,
    addEventListener: window.addEventListener.bind(window), removeEventListener: window.removeEventListener.bind(window),
    setTimeout: (callback, delay = 0) => { const id = ++next; timers.set(id, { at: now + delay, callback }); return id; },
    clearTimeout: id => timers.delete(id), c6StopGlide: el => stops.push(el) });
  const dispose = window.c6FollowPress(element, (...args) => changes.push(args));
  const initial = changes.splice(0);
  const baseline = { document: document.count(), window: window.count(), element: element.count() };
  const emit = (type, patch = {}, target = control) => {
    const event = { type, target, pointerId: 1, pointerType: 'mouse', isPrimary: true, button: 0, detail: 1, isTrusted: true, touches: [], ...patch };
    document.emit(type, event);
    if (element.contains(target)) element.emit(type, event);
  };
  const advance = ms => {
    const until = now + ms;
    for (;;) {
      const entry = [...timers].filter(([, timer]) => timer.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
      if (!entry) break;
      const [id, timer] = entry; timers.delete(id); now = timer.at; timer.callback();
    }
    now = until;
  };
  const down = () => emit('pointerdown');
  const up = () => emit('pointerup');
  const touchStart = (ids = [1]) => emit('touchstart', { touches: ids.map(identifier => ({ identifier })) });
  const touchEnd = (ids = []) => emit('touchend', { touches: ids.map(identifier => ({ identifier })) });
  return { document, window, element, control, plain, outside, initial, changes, stops, timers, baseline, emit, advance, down, up, touchStart, touchEnd, dispose };
}

const tests = [];
function test(name, fn) { const f = fixture(); try { fn(f); tests.push({ name, passed: true }); } catch (error) { tests.push({ name, passed: false, error: error.stack }); } finally { f.dispose(); } }
const held = f => f.changes.map(args => args[0]);

test('registration always synchronizes a replacement owner lifecycle', f => {
  assert.deepEqual(f.initial, [[false, true, false]]);
});
test('only enabled controls initiate ownership', f => {
  f.emit('pointerdown', {}, f.plain);
  f.control.disabled = true; f.down(); f.control.disabled = false;
  assert.deepEqual(held(f), []); assert.equal(f.document.count(), f.baseline.document);
  f.down(); assert.deepEqual(held(f), [true]);
});
test('release waits for delayed compatibility click, then completes once', f => {
  f.down(); f.up(); f.advance(300); assert.deepEqual(held(f), [true]);
  f.emit('click'); assert.deepEqual(held(f), [true]); f.advance(0);
  assert.deepEqual(held(f), [true, false]); f.advance(1000); assert.deepEqual(held(f), [true, false]);
  assert.equal(f.document.count(), f.baseline.document); assert.equal(f.window.count(), f.baseline.window);
});
test('unrelated click does not release the owned compatibility interval', f => {
  f.down(); f.up(); f.emit('click', {}, f.outside); f.advance(0);
  assert.deepEqual(held(f), [true]); f.emit('click'); f.advance(0); assert.deepEqual(held(f), [true, false]);
});
test('no-click release terminates; a still-down longpress never expires', f => {
  f.down(); f.advance(10000); assert.deepEqual(held(f), [true]);
  f.up(); f.advance(1000); assert.deepEqual(held(f), [true, false]);
});
test('contextmenu cannot terminate a still-down contact', f => {
  f.down(); f.emit('contextmenu'); f.advance(1000); assert.deepEqual(held(f), [true]);
  f.up(); f.advance(1000); assert.deepEqual(held(f), [true, false]);
});
test('multi-touch retains ownership through the last contact', f => {
  f.emit('pointerdown', { pointerType: 'touch' }); f.touchStart();
  f.emit('pointerdown', { pointerType: 'touch', pointerId: 2 }, f.outside); f.touchStart([1, 2]);
  f.emit('pointerup', { pointerType: 'touch' }); f.touchEnd([2]); f.advance(1000);
  assert.deepEqual(held(f), [true]);
  f.emit('pointerup', { pointerType: 'touch', pointerId: 2 }, f.outside); f.touchEnd(); f.emit('click'); f.advance(0);
  assert.deepEqual(held(f), [true, false]);
});
test('cancel terminates without requiring activation', f => {
  f.down(); f.emit('pointercancel'); f.advance(1000); assert.deepEqual(held(f), [true, false]);
});
test('new contact invalidates the old release timer', f => {
  f.down(); f.up(); f.advance(300); f.down(); f.advance(1000); assert.deepEqual(held(f), [true]);
  f.up(); f.emit('click'); f.advance(0); assert.deepEqual(held(f), [true, false]);
});
test('temporarily detached owner still receives logical completion', f => {
  f.down(); f.element.isConnected = false; f.up(); f.advance(1000);
  assert.deepEqual(held(f), [true, false]); f.element.isConnected = true;
});
test('owner disposal clears timers/listeners and cannot call back later', f => {
  f.down(); f.up(); f.dispose(); f.advance(1000);
  assert.deepEqual(held(f), [true]); assert.equal(f.timers.size, 0);
  assert.equal(f.document.count(), 0); assert.equal(f.window.count(), 0); assert.equal(f.element.count(), 0);
});
test('hidden owner logically completes without authorizing a flush', f => {
  f.down(); f.document.hidden = true; f.emit('visibilitychange', {}, f.document); f.advance(1000);
  assert.deepEqual(held(f), [true, false]); assert.equal(f.changes.at(-1)[1], false); assert.equal(f.changes.at(-1)[2], false);
});
test('pagehide logically completes without authorizing a flush', f => {
  f.down(); f.window.emit('pagehide', { type: 'pagehide' }); f.advance(1000);
  assert.deepEqual(held(f), [true, false]); assert.equal(f.changes.at(-1)[1], false); assert.equal(f.changes.at(-1)[2], false);
});

const report = { source: file, sha256: crypto.createHash('sha256').update(source).digest('hex'), results: tests };
console.log(JSON.stringify(report, null, 2));
if (tests.some(test => !test.passed)) process.exitCode = 1;
