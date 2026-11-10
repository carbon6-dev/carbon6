'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { configuration, shards, plan, pool, acquireLock, supervise, run, child } = require('./browser-runner.cjs');

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
  for (const suite of ['message-edit', 'message-origin', 'comments-design', 'session-sheet', 'transcript-press', 'transcript-times'])
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

test('invalid selection and accidental parallel benchmarks fail before launch', () => {
  for (const env of [{ BROWSER_SUITES: '' }, { BROWSER_SUITES: '../motion' }, { BROWSER_SUITES: 'no-such-suite' },
    { BROWSER_ENGINES: '' }, { BROWSER_ENGINES: 'firefox' }, { BROWSER_JOBS: '0' }, { BROWSER_JOBS: '2.5' },
    { BROWSER_JOBS: '33' }, { UI_BROWSER_PASSES: '0' }, { BROWSER_WORKER_DEADLINE_SECS: '0' }, { BROWSER_SUITES: 'motion motion' }, { PERF: '1' },
    { BROWSER_TIER: 'perf', BROWSER_SUITES: 'motion' }, { BROWSER_MATRIX: 'none' }]) assert.throws(() => configuration(env), JSON.stringify(env));
  assert.throws(() => plan(configuration({ BROWSER_SUITES: 'ring-render', BROWSER_ENGINES: 'webkit' }), {}), /Chromium-only/);
  assert.equal(configuration({ BROWSER_TIER: 'perf', BROWSER_JOBS: '8' }).jobs, 1);
  assert.deepEqual(configuration({ BROWSER_TIER: 'perf' }).engines, ['chromium']);
  assert(configuration({ BROWSER_TIER: 'quick' }).selected.length < configuration({}).selected.length);
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
