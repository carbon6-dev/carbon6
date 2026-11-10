'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const { EventEmitter } = require('node:events');
const { diagnostic, exactDiagnostic, navigationObserver } = require('./browser-navigation.cjs');

test('native signature uses the original complete first stack line, not Playwright colon-split message/name', () => {
  const origin = 'http://127.0.0.1:1234', line = diagnostic(origin);
  assert(exactDiagnostic({ name: 'Fetch API cannot load http', message: line.slice(line.indexOf(':') + 1), stack: line + '\nfetch@[native code]' }, origin));
  for (const stack of [undefined, '', 'Error: ' + line, line + ' extra', line.replace('/attention', '/other'),
    line.replace(':1234', ':1235'), 'RuntimeError: unreachable', line.replace('checks.', 'checks')]) {
    assert(!exactDiagnostic({ stack }, origin), String(stack));
  }
  assert(!exactDiagnostic({ message: line, name: line }, origin), 'message alone is never attribution');
});

// Deterministic policy/race controls complement, not substitute for, the native
// browser controls. In particular a fast commit/lost old context is FAIL closed.
async function policy(options = {}) {
  const origin = 'http://127.0.0.1:1234', fatal = [], frame = {}, rows = [];
  let replacement, beforeunload = false, verifying = false;
  const probe = {
    arm(id) { replacement = id; return this.snapshot(); },
    snapshot() {
      if (verifying && options.lostContext) throw new Error('old context destroyed');
      return { token: verifying && options.wrongIdentity ? 'wrong' : 'old', url: origin + '/initial',
        replacement, beforeunload, pagehide: verifying && !!options.pagehide, rows };
    },
  };
  const handle = { async evaluate(fn, arg) { return fn(probe, arg); }, async dispose() {} };
  const page = Object.assign(new EventEmitter(), {
    mainFrame: () => frame, frames: () => [frame],
    async exposeBinding(_name, callback) { this.binding = callback; },
    async addInitScript() {}, async evaluateHandle() { return handle; },
    async evaluate() { return { token: 'new', rows: [] }; },
    async goto(destination) {
      if (!options.missingRequest) this.emit('request', { isNavigationRequest: () => true, frame: () => frame,
        url: () => destination, redirectedFrom: () => null });
      beforeunload = !options.missingBeforeunload;
      if (beforeunload) await this.binding({ frame }, { token: 'old', url: origin + '/initial', replacement, event: 'beforeunload' });
      if (options.domFailure) {
        rows.push({ event: 'window.unhandledrejection', message: diagnostic(origin) });
        await this.binding({ frame }, rows[0]);
      }
      verifying = true;
      if (options.committed) this.emit('framenavigated', frame);
      observer.pageError({ stack: diagnostic(origin), message: 'colon-split message' });
      this.emit('framenavigated', frame);
      return { status: () => 200 };
    },
  });
  frame.url = () => origin + '/destination';
  const observer = await navigationObserver({ page, engine: options.engine || 'webkit', origin,
    baselineAttention: () => !options.customRoutes, fatal: error => fatal.push(String(error)), log: () => {} });
  await observer.navigate(origin + '/destination');
  await new Promise(resolve => setImmediate(resolve));
  return { observer, fatal };
}

test('explicit replacement drains classification before returning and preserves raw error', async () => {
  const { observer, fatal } = await policy();
  assert.deepEqual(fatal, []);
  assert.equal(observer.rawPageErrors.length, 1);
  assert.equal(observer.ledger.filter(row => row.kind === 'classified-native').length, 1);
  assert(observer.ledger.findIndex(row => row.kind === 'classified-native') < observer.ledger.findIndex(row => row.kind === 'replacement-end'));
});

for (const options of [{ engine: 'chromium' }, { missingRequest: true }, { missingBeforeunload: true },
  { wrongIdentity: true }, { lostContext: true }, { pagehide: true }, { committed: true }, { customRoutes: true }, { domFailure: true }]) {
  test(`missing/unsafe attribution stays fatal: ${JSON.stringify(options)}`, async () => {
    const { observer, fatal } = await policy(options);
    assert(fatal.length > 0);
    assert.equal(observer.ledger.filter(row => row.kind === 'classified-native').length, 0);
  });
}
