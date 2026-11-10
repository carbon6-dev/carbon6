'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
const { configuration, shards, plan, pool, acquireLock, supervise, run, child, localNative } = require('./browser-runner.cjs');

test('correctness and perf invocations cannot overlap or steal each other\'s lock', () => {
  const parent = fs.mkdtempSync(path.join(os.tmpdir(), 'browser-lock-'));
  const env = { BROWSER_TEST_LOCK: path.join(parent, 'lock') };
  try {
    const release = acquireLock(env);
    assert.throws(() => acquireLock(env), /Another browser runner/);
    const owner = JSON.parse(fs.readFileSync(path.join(env.BROWSER_TEST_LOCK, 'owner.json')));
    fs.writeFileSync(path.join(env.BROWSER_TEST_LOCK, 'owner.json'), JSON.stringify({ ...owner, token: 'different-owner' }));
    release();
    assert(fs.existsSync(env.BROWSER_TEST_LOCK), 'a replaced lock is not ours to remove');
    fs.writeFileSync(path.join(env.BROWSER_TEST_LOCK, 'owner.json'), JSON.stringify(owner));
    release();
    assert(!fs.existsSync(env.BROWSER_TEST_LOCK));
    acquireLock(env)();
  } finally { fs.rmSync(parent, { recursive: true }); }
});

test('full selection retains both engines and independent acceptance suites', () => {
  const config = configuration({});
  assert.deepEqual(config.engines, ['chromium', 'webkit']);
  for (const suite of ['message-edit', 'message-origin', 'comments-design', 'session-sheet', 'transcript-press', 'transcript-times', 'navigation-errors', 'stale-transcript'])
    assert(config.selected.includes(suite), suite);
  const tasks = plan(config, {});
  for (const suite of config.selected) for (const engine of config.engines)
    assert.equal(tasks.some(task => task.suite === suite && task.engine === engine), suite !== 'ring-render' || engine === 'chromium');
  assert.equal(shards('workspace-open', {}).length, 4, 'both widths and real platform modifiers');
  assert.equal(shards('motion', { BROWSER_MATRIX: 'all' }).length, 4, 'exhaustive Cartesian mode remains available');
  assert.equal(shards('phone-picker', {}).length, 8);
  assert.equal(shards('phone-bar', { BROWSER_MATRIX: 'all' }).length, 14);
});

test('representative matrices retain the named responsive, policy and theme branches', () => {
  const motion = shards('motion', {});
  assert.equal(motion.length, 11);
  assert.equal(motion.filter(task => task.ONLY === 'policy').length, 3);
  assert.equal(motion.filter(task => !task.ONLY).length, 2, 'all four modelForms paths on desktop and phone');
  for (const VIEWPORT of ['desktop', 'phone']) for (const MOTION_CASE of ['full', 'reduced', 'off'])
    assert(motion.some(task => task.VIEWPORT === VIEWPORT && task.THEME === 'light' && task.MOTION_CASE === MOTION_CASE));
  for (const MOTION_CASE of ['system', 'system-reduce', 'full-reduce']) assert(motion.some(task => task.MOTION_CASE === MOTION_CASE && task.ONLY === 'policy'));
  assert.equal(shards('phone-bar', {}).reduce((count, task) => count + (task.VIEWPORT.startsWith('desktop-') ? 1 : 8), 0), 52);
  assert.deepEqual(shards('phone-picker', {}), [
    ['narrow', 'light', 'full'], ['narrow', 'dark', 'off'], ['phone', 'dark', 'full'], ['phone', 'light', 'reduced'],
    ['wide', 'light', 'full'], ['wide', 'dark', 'reduced'], ['desktop', 'dark', 'full'], ['desktop', 'light', 'off'],
  ].map(([VIEWPORT, THEME, MOTION_CASE]) => ({ VIEWPORT, THEME, MOTION_CASE })));
});

test('diagnostic selectors do not multiply or override caller-selected cases', () => {
  for (const suite of ['motion', 'phone-picker', 'phone-bar', 'workspace-open'])
    assert.deepEqual(shards(suite, { VIEWPORT: 'narrow' }), [{}]);
  assert.deepEqual(shards('stale-transcript', {}), [{}], 'new suites use the generic isolated worker');
});

test('each tier records harness dependencies without a provenance unit gate', async () => {
  for (const tier of ['full', 'quick', 'perf']) {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'browser-dependencies-'));
    let launches = 0, units = 0;
    const engine = { launchServer: async () => {
      assert.equal(units, 0);
      launches++;
      return { wsEndpoint: () => 'owned', close: async () => {} };
    } };
    try {
      await run({ ROOT: directory, BROWSER_TEST_OUTPUT: directory, BROWSER_TIER: tier,
        BROWSER_SUITES: tier === 'perf' ? 'ring-render' : 'settings-lists' }, {
        fixture: { serve: async () => ({ origin: 'http://127.0.0.1:1234', server: { close: done => done() } }), launchOptions: () => ({}) },
        playwright: { chromium: engine, webkit: engine },
        child: async task => {
          if (task.engine === 'unit') {
            units++;
          }
          return { ...task, status: 0 };
        },
      });
      const output = fs.readdirSync(directory).find(file => file.startsWith(`${tier}-`));
      const report = JSON.parse(fs.readFileSync(path.join(directory, output, 'summary.json')));
      assert.deepEqual(report.units, []);
      assert(launches > 0);
      assert(!report.tests.some(row => row.name.startsWith('attention-proof')));
      for (const name of ['browser-navigation', 'browser-navigation.test', 'narrow-fixtures', 'browser-runner.test', 'browser-lifecycle', 'browser-lifecycle.test'])
        assert.match(report.tests.find(row => row.name === name)?.sha256 || '', /^[a-f0-9]{64}$/);
    } finally { fs.rmSync(directory, { recursive: true }); }
  }
});

test('invalid selection and accidental parallel benchmarks fail before launch', () => {
  for (const env of [{ BROWSER_SUITES: '' }, { BROWSER_SUITES: '../motion' }, { BROWSER_SUITES: 'no-such-suite' },
    { BROWSER_ENGINES: '' }, { BROWSER_ENGINES: 'firefox' }, { BROWSER_JOBS: '0' }, { BROWSER_JOBS: '2.5' },
    { BROWSER_JOBS: '33' }, { UI_BROWSER_PASSES: '0' }, { BROWSER_WORKER_DEADLINE_SECS: '0' }, { BROWSER_SUITES: 'motion motion' }, { PERF: '1' },
    { BROWSER_TIER: 'perf', BROWSER_SUITES: 'motion' }, { BROWSER_MATRIX: 'none' }]) assert.throws(() => configuration(env), JSON.stringify(env));
  assert.throws(() => plan(configuration({ BROWSER_SUITES: 'ring-render', BROWSER_ENGINES: 'webkit' }), {}), /Chromium-only/);
  assert.equal(configuration({ BROWSER_TIER: 'perf', BROWSER_JOBS: '8' }).jobs, 1);
  assert.deepEqual(configuration({ BROWSER_TIER: 'perf' }).engines, ['chromium']);
  assert(configuration({ BROWSER_TIER: 'quick' }).selected.length < configuration({}).selected.length);
  assert(configuration({ BROWSER_TIER: 'quick' }).selected.includes('stale-transcript'));
});

test('pool is bounded and runs each task once without retries', async () => {
  let active = 0, peak = 0;
  const seen = [];
  const tasks = Array.from({ length: 13 }, (_, index) => index);
  const result = await pool(tasks, 3, async task => {
    active++; peak = Math.max(peak, active); seen.push(task);
    await new Promise(resolve => setImmediate(resolve));
    active--;
    return { status: 0, task };
  });
  assert.equal(peak, 3);
  assert.deepEqual(seen, tasks);
  assert.equal(result.results.length, tasks.length);
  assert.equal(result.pending, 0);
});

test('first failure prevents new jobs while in-flight jobs keep their evidence', async () => {
  const seen = [];
  const result = await pool([0, 1, 2, 3], 2, async task => {
    seen.push(task);
    await new Promise(resolve => setImmediate(resolve));
    return { status: task === 0 ? 1 : 0, task };
  });
  assert.deepEqual(seen, [0, 1]);
  assert.equal(result.pending, 2);
  assert.equal(result.results.length, 2);
});

test('a worker setup error drains active workers before browser teardown', async () => {
  const finished = [], seen = [];
  await assert.rejects(pool([0, 1, 2], 2, async task => {
    seen.push(task);
    await new Promise(resolve => setImmediate(resolve));
    if (task === 0) throw new Error('cannot create evidence directory');
    finished.push(task);
    return { status: 0 };
  }), /cannot create evidence directory/);
  assert.deepEqual(seen, [0, 1]);
  assert.deepEqual(finished, [1]);
});

test('cancellation stops scheduling even when every active worker exits zero', async () => {
  let cancelled = false;
  const seen = [];
  const result = await pool([0, 1, 2], 2, async task => {
    seen.push(task);
    cancelled = true;
    return { status: 0 };
  }, () => cancelled);
  assert.deepEqual(seen, [0]);
  assert.equal(result.pending, 2);
});

test('worker watchdog terminates a wedged owned child before releasing its lock', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'browser-watchdog-'));
  const env = { BROWSER_TEST_LOCK: path.join(directory, 'lock') };
  const release = acquireLock(env);
  try {
    const worker = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {
      stdio: 'ignore', detached: process.platform !== 'win32',
    });
    const finished = supervise(worker, 20);
    assert.throws(() => acquireLock(env), /Another browser runner/);
    const result = await finished;
    assert.equal(result.status, 1);
    assert.equal(result.timedOut, true);
    assert(worker.signalCode, 'child exit was observed, not just requested');
    assert.throws(() => acquireLock(env), /Another browser runner/);
    release();
    acquireLock(env)();
  } finally { if (fs.existsSync(env.BROWSER_TEST_LOCK)) release(); fs.rmSync(directory, { recursive: true }); }
});

test('interruption during browser startup cannot schedule workers or report success', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'browser-interrupt-'));
  let closed = 0, serverClosed = 0;
  try {
    await assert.rejects(run({ ROOT: directory, BROWSER_TEST_OUTPUT: directory,
      BROWSER_SUITES: 'settings-lists', BROWSER_ENGINES: 'chromium' }, {
      fixture: { serve: async () => ({ origin: 'http://127.0.0.1:1234', server: {
        close: done => { serverClosed++; done(); },
      } }), launchOptions: () => ({}) },
      playwright: { chromium: { launchServer: async () => {
        process.emit('SIGTERM');
        return { close: async () => { closed++; }, wsEndpoint: () => { throw new Error('must not use interrupted server'); } };
      } } },
    }), /Browser invocation interrupted/);
    assert.equal(closed, 1);
    assert.equal(serverClosed, 1);
    const output = fs.readdirSync(directory).find(file => file.startsWith('full-'));
    const report = JSON.parse(fs.readFileSync(path.join(directory, output, 'summary.json')));
    assert.equal(report.interrupted, true);
    assert.equal(report.passes.length, 0);
    assert.match(report.error, /interrupted/);
  } finally { fs.rmSync(directory, { recursive: true }); }
});

test('a rejected server close still drains the other server and HTTP owner', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'browser-cleanup-'));
  const closed = [];
  try {
    const engine = name => ({ launchServer: async () => {
      if (name === 'webkit') process.emit('SIGINT');
      return { wsEndpoint: () => 'unused', close: async () => {
        closed.push(name);
        if (name === 'chromium') throw new Error('close transport failed');
      }, kill: async () => { closed.push(`${name}-kill`); } };
    } });
    await assert.rejects(run({ ROOT: directory, BROWSER_TEST_OUTPUT: directory, BROWSER_SUITES: 'settings-lists' }, {
      fixture: { serve: async () => ({ origin: 'http://127.0.0.1:1234', server: {
        close: done => { closed.push('http'); done(); },
      } }), launchOptions: () => ({}) },
      playwright: { chromium: engine('chromium'), webkit: engine('webkit') },
    }), /Browser invocation interrupted/);
    assert.deepEqual(closed, ['chromium', 'webkit', 'chromium-kill', 'http']);
    const output = fs.readdirSync(directory).find(file => file.startsWith('full-'));
    const report = JSON.parse(fs.readFileSync(path.join(directory, output, 'summary.json')));
    assert.deepEqual(report.cleanupErrors, ['Error: close transport failed']);
  } finally { fs.rmSync(directory, { recursive: true }); }
});

test('isolated workers connect only to fresh parent-owned servers closed after worker failure', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'browser-isolation-'));
  const finished = [], closed = [];
  try {
    const engine = name => ({ launchServer: async () => ({ wsEndpoint: () => `owned-${name}`,
      close: async () => { assert(finished.includes(name)); closed.push(name); } }) });
    await assert.rejects(run({ ROOT: directory, BROWSER_TEST_OUTPUT: directory,
      BROWSER_SUITES: 'settings-lists', ISOLATE_BROWSER: '1' }, {
      fixture: { serve: async () => ({ origin: 'http://127.0.0.1:1234', server: { close: done => done() } }), launchOptions: () => ({}) },
      playwright: { chromium: engine('chromium'), webkit: engine('webkit') },
      child: async (task, _directory, env) => {
        assert.equal(env.ISOLATE_BROWSER, '0');
        assert.equal(env[`BROWSER_TEST_${task.engine.toUpperCase()}_ENDPOINT`], `owned-${task.engine}`);
        finished.push(task.engine);
        throw new Error('simulated worker failure');
      },
    }), /simulated worker failure/);
    assert.deepEqual(closed.sort(), ['chromium', 'webkit']);
  } finally { fs.rmSync(directory, { recursive: true }); }
});

test('asynchronous evidence-stream failure cancels and drains its worker instead of crashing the runner', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'browser-evidence-'));
  fs.mkdirSync(path.join(directory, 'run.log')); // Async stream open will fail with EISDIR.
  try {
    const result = await child({ engine: 'unit', suite: 'follow-press-unit', shard: 1, filters: {} }, directory, process.env);
    assert.equal(result.status, 1);
    assert.match(result.evidenceError, /EISDIR/);
    assert.equal(result.interrupted, true);
    assert(fs.existsSync(path.join(directory, 'timeline.jsonl')), 'other evidence streams were drained');
  } finally { fs.rmSync(directory, { recursive: true }); }
});

test('importing transcript-press neither loads a browser driver nor starts a suite or writes evidence', () => {
  const result = spawnSync(process.execPath, ['-e', `
    const assert = require('node:assert/strict');
    const fs = require('node:fs'), http = require('node:http'), Module = require('node:module');
    const load = Module._load;
    Module._load = function(name, ...args) {
      assert(!name.includes('playwright'), 'driver loaded during import');
      return load.call(this, name, ...args);
    };
    for (const method of ['writeFileSync', 'appendFileSync', 'mkdirSync', 'copyFileSync'])
      fs[method] = () => { throw new Error('import writes evidence'); };
    http.createServer = () => { throw new Error('import starts server'); };
    const before = { exitCode: process.exitCode, log: console.log, error: console.error,
      env: JSON.stringify(process.env), term: process.listenerCount('SIGTERM'), int: process.listenerCount('SIGINT') };
    const suite = require(${JSON.stringify(path.join(__dirname, 'transcript-press.cjs'))});
    assert.equal(typeof suite.runSuite, 'function');
    assert.equal(process.exitCode, before.exitCode);
    assert.equal(console.log, before.log); assert.equal(console.error, before.error);
    assert.equal(JSON.stringify(process.env), before.env);
    assert.equal(process.listenerCount('SIGTERM'), before.term);
    assert.equal(process.listenerCount('SIGINT'), before.int);
  `], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, '');
});

test('WebKit retains all fourteen named native controls and exact raw touch protocol points', async () => {
  const { casesFor, nativeTouch } = require('./transcript-press.cjs');
  assert.deepEqual(casesFor('webkit').map(({ scenario, motion }) => `${scenario}-${motion}`), [
    'idle-full', 'idle-reduced', 'idle-off', 'stream-full', 'stream-reduced', 'stream-off',
    'cancel-off', 'multi-touch-off', 'touch-position-off', 'tap-off',
    'latest-stream-full', 'latest-stream-reduced', 'latest-stream-off', 'latest-touch-position-off',
  ]);
  assert.equal(casesFor('chromium').length, 13);
  const messages = [], page = { _connection: { toImpl: value => {
    assert.equal(value, page);
    return { delegate: { _pageProxySession: { send: async (method, params) => messages.push({ method, ...structuredClone(params) }) } } };
  } } };
  const touch = await nativeTouch({ page }, 'webkit');
  const first = { id: 1, x: 10, y: 20 }, second = { id: 2, x: 40, y: 20 };
  await touch.start(first); await touch.start(second);
  await touch.move({ ...first, y: 30 }); await touch.cancel();
  await touch.start(first); await touch.end(); await touch.cancel();
  assert(messages.every(message => message.method === 'Input.dispatchTouchEvent'));
  assert.deepEqual(messages.map(message => message.type), ['touchStart', 'touchStart', 'touchMove', 'touchCancel', 'touchStart', 'touchEnd']);
  assert.deepEqual(messages[1].touchPoints, [first, second]);
  assert.deepEqual(messages[3].touchPoints, [{ ...first, y: 30 }, second]);
  assert.deepEqual(messages[5].touchPoints, [first], 'WebKit end includes the contact; never substitute a DOM event or tap');
  await assert.rejects(nativeTouch({ page: { _connection: {} } }, 'webkit'), /actual launch, not connect/);
});

const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};

function nativeHarness(t) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'browser-native-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const events = [], closed = deferred();
  const owner = { kill: async () => { events.push('kill'); closed.resolve(); } };
  const browser = { version: () => 'pinned', close: async () => { events.push('close'); closed.resolve(); },
    _connection: { toImpl: value => {
      assert.equal(value, browser); events.push('capture');
      return { options: { browserProcess: owner } };
    } } };
  const task = { engine: 'webkit', suite: 'transcript-press', shard: 1, filters: {} };
  const environment = { ROOT: directory, BROWSER_TEST_ORIGIN: 'http://127.0.0.1:1234' };
  const dependencies = { cleanupDeadlineMs: 10, launchOptions: () => ({ headless: true, timeout: 1000 }),
    playwright: { webkit: { launch: async () => { events.push('launch'); return browser; } } },
    suite: async options => {
      events.push('suite'); options.log.log('PASS native control');
      return { results: [{ passed: true }] };
    } };
  return { directory, events, closed, owner, browser, task, environment, dependencies,
    execute: (overrides = {}, deadline = 1000) => localNative(task, directory, environment, performance.now(), deadline,
      { ...dependencies, ...overrides }) };
}

test('local native success captures ownership, writes shard logs/timeline and closes only its browser', async t => {
  const h = nativeHarness(t), before = { env: { ...process.env }, log: console.log, error: console.error };
  const result = await h.execute();
  assert.equal(result.status, 0);
  assert.equal(result.execution, 'parent-local-launch');
  assert.deepEqual(h.events, ['launch', 'capture', 'suite', 'close']);
  assert.match(fs.readFileSync(path.join(h.directory, 'run.log'), 'utf8'), /PASS native control/);
  const rows = fs.readFileSync(path.join(h.directory, 'timeline.jsonl'), 'utf8').trim().split('\n').map(JSON.parse);
  assert(rows.every(row => row.channel === 'stdout' && row.ms >= 0 && row.text.includes('native control')));
  assert.deepEqual({ ...process.env }, before.env);
  assert.equal(console.log, before.log); assert.equal(console.error, before.error);
});

for (const isolation of ['0', '1']) test(`routing keeps Chromium child/server and no unused WebKit server (isolation=${isolation})`, async t => {
  const h = nativeHarness(t), events = h.events;
  await run({ ROOT: h.directory, BROWSER_TEST_OUTPUT: h.directory, BROWSER_SUITES: 'transcript-press', ISOLATE_BROWSER: isolation }, {
    fixture: { serve: async () => ({ origin: h.environment.BROWSER_TEST_ORIGIN, server: { close: done => { events.push('http-close'); done(); } } }),
      launchOptions: h.dependencies.launchOptions },
    playwright: { chromium: { launchServer: async () => {
      events.push('chromium-server');
      return { wsEndpoint: () => 'shared-chromium', close: async () => { events.push('chromium-close'); } };
    } }, webkit: { ...h.dependencies.playwright.webkit, launchServer: () => { throw new Error('unused WebKit server'); } } },
    child: async (task, _directory, env) => {
      if (task.engine === 'unit') return { ...task, status: 0 };
      assert.equal(task.engine, 'chromium');
      assert.equal(env.BROWSER_TEST_CHROMIUM_ENDPOINT, 'shared-chromium');
      events.push('chromium-child');
      return { ...task, status: 0 };
    },
    nativeSuite: async options => {
      assert.equal(options.engine, 'webkit'); assert.equal(options.origin, h.environment.BROWSER_TEST_ORIGIN);
      assert.equal(options.root, h.directory); assert.equal(options.timings, true);
      options.log.log('native'); events.push('suite');
      return { results: [{ passed: true }] };
    },
  });
  assert(events.includes('chromium-child'));
  assert.deepEqual(events.filter(event => ['launch', 'capture', 'suite', 'close'].includes(event)), ['launch', 'capture', 'suite', 'close']);
  assert(events.indexOf('close') < events.indexOf('http-close'));
  const output = fs.readdirSync(h.directory).find(file => file.startsWith('full-'));
  const report = JSON.parse(fs.readFileSync(path.join(h.directory, output, 'summary.json')));
  assert.equal(report.passes[0].results.find(result => result.engine === 'webkit').execution, 'parent-local-launch');
  assert.equal(report.passes[0].pending, 0);
});

test('native WebKit failure does not close a concurrent WebKit child shared server before it drains', async t => {
  const h = nativeHarness(t), exitCode = process.exitCode;
  t.after(() => { process.exitCode = exitCode; });
  let childFinished = false, serverClosed = false;
  await run({ ROOT: h.directory, BROWSER_TEST_OUTPUT: h.directory,
    BROWSER_SUITES: 'transcript-press settings-lists', BROWSER_ENGINES: 'webkit' }, {
    fixture: { serve: async () => ({ origin: h.environment.BROWSER_TEST_ORIGIN, server: { close: done => done() } }),
      launchOptions: h.dependencies.launchOptions },
    playwright: { webkit: { ...h.dependencies.playwright.webkit, launchServer: async () => ({
      wsEndpoint: () => 'other-webkit-owner', close: async () => { assert(childFinished); serverClosed = true; },
    }) } },
    child: async (task, _directory, env) => {
      if (task.engine === 'unit') return { ...task, status: 0 };
      assert.equal(task.suite, 'settings-lists');
      assert.equal(env.BROWSER_TEST_WEBKIT_ENDPOINT, 'other-webkit-owner');
      await new Promise(resolve => setImmediate(resolve));
      assert.equal(serverClosed, false);
      childFinished = true;
      return { ...task, status: 0 };
    },
    nativeSuite: async () => ({ results: [{ passed: false }] }),
  });
  assert(childFinished && serverClosed);
  const output = fs.readdirSync(h.directory).find(file => file.startsWith('full-'));
  const report = JSON.parse(fs.readFileSync(path.join(h.directory, output, 'summary.json')));
  assert.equal(report.passes[0].results.find(result => result.suite === 'settings-lists').status, 0);
  assert.equal(report.passes[0].results.find(result => result.suite === 'transcript-press').status, 1);
});

test('local launch rejection and unsupported local ownership fail without running the suite', async t => {
  const h = nativeHarness(t);
  let result = await h.execute({ playwright: { webkit: { launch: async () => { throw new Error('launch failed'); } } } });
  assert.equal(result.status, 1); assert.match(result.launchError, /launch failed/);
  assert.deepEqual(h.events, []);
  h.browser._connection = {};
  result = await h.execute();
  assert.equal(result.status, 1); assert.match(result.error, /force-kill ownership/);
  assert.deepEqual(h.events, ['launch', 'close']);
});

test('suite rejection and failed native controls both fail the shard and close its browser', async t => {
  const h = nativeHarness(t);
  let result = await h.execute({ suite: async () => { throw new Error('suite failed'); } });
  assert.equal(result.status, 1); assert.match(result.error, /suite failed/);
  assert.deepEqual(h.events, ['launch', 'capture', 'close']);
  result = await h.execute({ suite: async () => ({ results: [{ passed: false }] }) });
  assert.equal(result.status, 1); assert.equal(h.events.filter(event => event === 'close').length, 2);
});

test('cancellation during launch captures then closes the arriving browser without entering suite code', async t => {
  const h = nativeHarness(t), controller = new AbortController(), launched = deferred();
  const pending = h.execute({ signal: controller.signal, playwright: { webkit: { launch: async options => {
    assert(options.timeout <= 1000); launched.resolve();
    controller.abort(new Error('cancel during launch'));
    return h.browser;
  } } } });
  await launched.promise;
  const result = await pending;
  assert.equal(result.status, 1); assert.equal(result.interrupted, true);
  assert.deepEqual(h.events, ['capture', 'close']);
});

test('already-cancelled native work never launches a browser', async t => {
  const h = nativeHarness(t), controller = new AbortController();
  controller.abort(new Error('cancel before launch'));
  const result = await h.execute({ signal: controller.signal });
  assert.equal(result.status, 1); assert.equal(result.interrupted, true);
  assert.deepEqual(h.events, []);
});

test('deadline during launch still drains and closes the late browser, without running controls', async t => {
  const h = nativeHarness(t);
  const result = await h.execute({ playwright: { webkit: { launch: options => {
    assert.equal(options.timeout, 5);
    return new Promise(resolve => setTimeout(() => resolve(h.browser), 15));
  } } } }, 5);
  assert.equal(result.status, 1); assert.equal(result.timedOut, true);
  assert.deepEqual(h.events, ['capture', 'close']);
});

for (const cancellation of ['deadline', 'signal']) test(`${cancellation} closes and drains native work before releasing a pool slot, without stopping another worker`, async t => {
  const h = nativeHarness(t), controller = new AbortController();
  let drained = false;
  const result = await pool([0, 1], 2, async index => {
    if (index === 1) {
      await new Promise(resolve => setImmediate(resolve));
      h.events.push('other-finished'); return { status: 0 };
    }
    const outcome = await h.execute({ signal: controller.signal, suite: async ({ signal }) => {
      h.events.push('suite');
      if (cancellation === 'signal') controller.abort(new Error('cancel running suite'));
      await h.closed.promise;
      assert.equal(signal.aborted, true);
      await new Promise(resolve => setImmediate(resolve));
      h.events.push('drained'); drained = true;
      return { results: [{ passed: true }] };
    } }, cancellation === 'deadline' ? 10 : 1000);
    assert(drained, 'suite settlement precedes pool result');
    assert.equal(outcome.status, 1);
    assert.equal(outcome.timedOut, cancellation === 'deadline');
    assert.equal(outcome.interrupted, cancellation === 'signal');
    return outcome;
  });
  assert.equal(result.results.length, 2);
  assert(h.events.includes('other-finished'));
  assert(h.events.indexOf('close') < h.events.indexOf('drained'));
});

for (const failure of ['reject', 'hang']) test(`local browser close ${failure} escalates to captured kill and records cleanup failure`, async t => {
  const h = nativeHarness(t);
  h.browser.close = () => {
    h.events.push('close');
    return failure === 'hang' ? new Promise(() => {}) : Promise.reject(new Error('close rejected'));
  };
  const result = await h.execute();
  assert.equal(result.status, 1);
  assert.deepEqual(h.events, ['launch', 'capture', 'suite', 'close', 'kill']);
  assert.equal(result.cleanupErrors.length, 1);
  assert.match(result.cleanupErrors[0], failure === 'hang' ? /close deadline/ : /close rejected/);
});

test('cleanup kill rejection remains an explicit failure even after every control passed', async t => {
  const h = nativeHarness(t);
  h.browser.close = async () => { throw new Error('close failed'); };
  h.owner.kill = async () => { throw new Error('kill failed'); };
  const result = await h.execute();
  assert.equal(result.status, 1);
  assert.match(result.cleanupErrors[0], /Local browser cleanup failed/);
  assert.match(result.cleanupErrors[0], /close failed/);
  assert.match(result.cleanupErrors[0], /kill failed/);
});

test('deadline with a stalled close force-kills and drains the suite before returning', async t => {
  const h = nativeHarness(t);
  let drained = false;
  h.browser.close = () => { h.events.push('close'); return new Promise(() => {}); };
  const result = await h.execute({ suite: async () => {
    h.events.push('suite');
    await h.closed.promise;
    await new Promise(resolve => setImmediate(resolve));
    h.events.push('drained'); drained = true;
    return { results: [{ passed: true }] };
  } }, 5);
  assert.equal(result.status, 1); assert.equal(result.timedOut, true); assert(drained);
  assert.deepEqual(h.events, ['launch', 'capture', 'suite', 'close', 'kill', 'drained']);
});

test('a stalled force-kill has its own bounded failure after a completed suite', async t => {
  const h = nativeHarness(t);
  h.browser.close = () => new Promise(() => {});
  h.owner.kill = () => new Promise(() => {});
  const result = await h.execute();
  assert.equal(result.status, 1);
  assert.match(result.cleanupErrors[0], /close deadline/);
  assert.match(result.cleanupErrors[0], /kill deadline/);
});

test('logging evidence failure aborts and drains native work; initial evidence failure never launches', async t => {
  const h = nativeHarness(t);
  let drained = false;
  const result = await h.execute({ fs: { ...fs, appendFileSync: () => { throw new Error('evidence disk failed'); } },
    suite: async ({ log }) => {
      log.error('native diagnostic');
      await h.closed.promise;
      await new Promise(resolve => setImmediate(resolve));
      drained = true;
      return { results: [{ passed: true }] };
    } });
  assert.equal(result.status, 1); assert.match(result.evidenceError, /evidence disk failed/); assert(drained);
  const before = h.events.length;
  const setup = await h.execute({ fs: { ...fs, mkdirSync: () => { throw new Error('evidence setup failed'); } } });
  assert.equal(setup.status, 1); assert.match(setup.error, /evidence setup failed/);
  assert.equal(h.events.length, before);
});

test('exported suite preserves incremental native14 evidence and explicit case timing options', async t => {
  const h = nativeHarness(t), { runSuite } = require('./transcript-press.cjs');
  let completed = 0;
  const check = async (_browser, _origin, engine, scenario, motion, options) => {
    assert.equal(engine, 'webkit');
    const prior = JSON.parse(fs.readFileSync(path.join(h.directory, 'results.json')));
    assert.equal(prior.results.length, completed);
    await options.runCase(`${engine}-${scenario}-${motion}`, async () => { completed++; });
    return { label: `${engine}-${scenario}-${motion}`, passed: true };
  };
  const report = await runSuite({ browser: h.browser, engine: 'webkit', origin: h.environment.BROWSER_TEST_ORIGIN,
    root: h.directory, evidenceDirectory: h.directory, stop: () => h.browser.close(), log: { log() {}, error() {} } }, {
    check, checkLatest: check, within: async (label, action, options) => {
      assert.deepEqual(options, { evidenceDirectory: h.directory, timings: true });
      const result = await action();
      fs.appendFileSync(path.join(options.evidenceDirectory, 'cases.jsonl'), JSON.stringify({ label, passed: true }) + '\n');
      return result;
    },
  });
  assert.equal(completed, 14); assert.equal(report.results.length, 14);
  assert.equal(JSON.parse(fs.readFileSync(path.join(h.directory, 'results.json'))).results.length, 14);
  assert.equal(fs.readFileSync(path.join(h.directory, 'cases.jsonl'), 'utf8').trim().split('\n').length, 14);
  assert.deepEqual(h.events, [], 'suite borrows browser; owner alone closes it');
});

test('case deadline stops and drains the losing action before suite can start another case', async t => {
  const h = nativeHarness(t), { runSuite } = require('./transcript-press.cjs');
  let entered = 0, drained = false;
  const check = async (_browser, _origin, _engine, _scenario, _motion, options) => {
    entered++;
    try {
      await options.runCase('wedged-case', async () => {
        await h.closed.promise;
        await new Promise(resolve => setImmediate(resolve));
        drained = true;
      });
      assert.fail('deadline should reject');
    } catch (error) { return { label: 'wedged-case', passed: false, error: String(error) }; }
  };
  await assert.rejects(runSuite({ browser: h.browser, engine: 'webkit', origin: h.environment.BROWSER_TEST_ORIGIN,
    root: h.directory, evidenceDirectory: h.directory, stop: () => h.browser.close(), log: { log() {}, error() {} } }, {
    check, within: async (_label, action) => {
      const pending = action();
      await new Promise(resolve => setImmediate(resolve));
      pending.catch(() => {});
      throw new Error('case deadline');
    },
  }), /case deadline/);
  assert(drained); assert.equal(entered, 1);
  assert.deepEqual(h.events, ['close']);
  assert.equal(JSON.parse(fs.readFileSync(path.join(h.directory, 'results.json'))).results.length, 1);
});

test('runner SIGTERM cancels native work, drains another child and saves interruption before HTTP teardown', async t => {
  const h = nativeHarness(t), exitCode = process.exitCode;
  t.after(() => { process.exitCode = exitCode; });
  let nativeDrained = false, otherDrained = false;
  await run({ ROOT: h.directory, BROWSER_TEST_OUTPUT: h.directory, BROWSER_SUITES: 'transcript-press' }, {
    fixture: { serve: async () => ({ origin: h.environment.BROWSER_TEST_ORIGIN, server: {
      close: done => { assert(nativeDrained && otherDrained); done(); },
    } }), launchOptions: h.dependencies.launchOptions },
    playwright: { chromium: { launchServer: async () => ({ wsEndpoint: () => 'chromium-owned', close: async () => {} }) },
      webkit: h.dependencies.playwright.webkit },
    child: async task => {
      if (task.engine !== 'unit') {
        await new Promise(resolve => setImmediate(resolve));
        otherDrained = true;
      }
      return { ...task, status: 0 };
    },
    nativeSuite: async ({ signal }) => {
      process.emit('SIGTERM');
      await h.closed.promise;
      assert(signal.aborted);
      await new Promise(resolve => setImmediate(resolve));
      nativeDrained = true;
      return { results: [{ passed: true }] };
    },
  });
  const output = fs.readdirSync(h.directory).find(file => file.startsWith('full-'));
  const report = JSON.parse(fs.readFileSync(path.join(h.directory, output, 'summary.json')));
  assert.equal(report.interrupted, true);
  assert.equal(report.passes[0].results.find(result => result.engine === 'webkit').interrupted, true);
  assert.equal(process.exitCode, 1);
});

test('case timing evidence failure is fatal and result-write failure still closes the owned browser', async t => {
  const h = nativeHarness(t), { runSuite } = require('./transcript-press.cjs');
  fs.mkdirSync(path.join(h.directory, 'cases.jsonl'));
  const check = async (_browser, _origin, _engine, _scenario, _motion, options) => {
    await options.runCase('timing-write-failure', async () => {});
    return { label: 'unreachable', passed: true };
  };
  let result = await h.execute({ suite: options => runSuite(options, { check }) });
  assert.equal(result.status, 1); assert.match(result.error, /EISDIR/);
  assert.equal(h.events.filter(event => event === 'close').length, 1);
  fs.rmSync(path.join(h.directory, 'cases.jsonl'), { recursive: true });
  result = await h.execute({ suite: options => runSuite(options, { check: async () => {
    fs.rmSync(path.join(h.directory, 'results.json'));
    fs.mkdirSync(path.join(h.directory, 'results.json'));
    return { label: 'completed-before-write-failure', passed: true };
  } }) });
  assert.equal(result.status, 1); assert.match(result.error, /EISDIR/);
  assert.equal(h.events.filter(event => event === 'close').length, 2);
});
