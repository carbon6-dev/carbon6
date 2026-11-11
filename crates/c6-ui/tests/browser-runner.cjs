#!/usr/bin/env node
'use strict';

// Bounded correctness workers, not benchmark workers. See docs/testing.md.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const { spawn } = require('node:child_process');
const { performance } = require('node:perf_hooks');
const { format } = require('node:util');
const children = new Set();
const cancellations = new WeakMap();

function stopChild(child) {
  // Each worker starts its own process group. Browser processes are separately
  // owned by the parent's BrowserServers, including isolated per-worker ones.
  const signal = name => {
    try {
      if (process.platform !== 'win32' && child.pid) process.kill(-child.pid, name);
      else child.kill(name);
    } catch (error) { if (error.code !== 'ESRCH') throw error; }
  };
  signal('SIGTERM');
  const timer = setTimeout(() => signal('SIGKILL'), 5000);
  timer.unref();
  // A worker may leave non-browser descendants. Kill any remaining members
  // before declaring this owned process group drained.
  return () => { clearTimeout(timer); signal('SIGKILL'); };
}

async function supervise(child, deadlineMs, terminate = stopChild) {
  let timedOut = false, interrupted = false, clearKill;
  const stop = () => { clearKill ||= terminate(child); };
  cancellations.set(child, () => { interrupted = true; stop(); });
  const timer = setTimeout(() => { timedOut = true; stop(); }, deadlineMs);
  try {
    const [code, signal] = await new Promise(resolve => child.once('close', (...args) => resolve(args)));
    return { status: timedOut || interrupted ? 1 : code ?? 1, signal, timedOut, interrupted };
  } finally {
    clearTimeout(timer);
    clearKill?.();
    cancellations.delete(child);
  }
}

function acquireLock(env) {
  const directory = env.BROWSER_TEST_LOCK || path.join(os.homedir(), '.cache/carbon6/browser-tests.lock');
  fs.mkdirSync(path.dirname(directory), { recursive: true });
  try { fs.mkdirSync(directory); }
  catch (error) {
    if (error.code !== 'EEXIST') throw error;
    throw new Error(`Another browser runner owns ${directory}. Do not overlap correctness/perf invocations; inspect its owner.json (stale locks require explicit cleanup).`);
  }
  const token = `${process.pid}-${process.hrtime.bigint()}`;
  fs.writeFileSync(path.join(directory, 'owner.json'), JSON.stringify({ token, pid: process.pid, host: os.hostname(), started: new Date().toISOString() }));
  return () => {
    // Never remove a lock someone replaced, and never guess ownership by PID.
    if (JSON.parse(fs.readFileSync(path.join(directory, 'owner.json'))).token === token) fs.rmSync(directory, { recursive: true });
  };
}

const suites = ['workspace-open', 'motion', 'comment-sheet', 'phone-picker', 'phone-bar', 'theme',
  'settings-lists', 'resource-lifecycle', 'markdown-html', 'status-row', 'stale-transcript', 'ring-render',
  'transcript-press', 'transcript-times', 'message-edit', 'message-origin', 'comments-design', 'session-sheet', 'navigation-errors'];
const quick = ['workspace-open', 'motion', 'settings-lists', 'markdown-html', 'transcript-press', 'transcript-times', 'stale-transcript'];
const words = value => value.trim().split(/[\s,]+/).filter(Boolean);
const positive = (name, value) => {
  assert(/^[1-9][0-9]*$/.test(value), `${name} must be a positive integer`);
  assert(Number.isSafeInteger(Number(value)), `${name} is too large`);
  return Number(value);
};
function configuration(env) {
  const tier = env.BROWSER_TIER || 'full';
  assert(['full', 'quick', 'perf'].includes(tier), `Unknown BROWSER_TIER: ${tier}`);
  const matrix = env.BROWSER_MATRIX || 'sample';
  assert(['sample', 'all'].includes(matrix), 'BROWSER_MATRIX must be sample or all');
  const selected = env.BROWSER_SUITES === undefined ? tier === 'quick' ? quick : tier === 'perf' ? ['ring-render'] : suites : words(env.BROWSER_SUITES);
  assert(selected.length, 'BROWSER_SUITES must select at least one suite');
  for (const suite of selected) {
    assert(/^[a-z][a-z0-9-]*$/.test(suite) && fs.existsSync(path.join(__dirname, `${suite}.cjs`)), `Unknown suite: ${suite}`);
  }
  assert.equal(new Set(selected).size, selected.length, 'Duplicate BROWSER_SUITES entry');
  const engines = words(env.BROWSER_ENGINES === undefined ? tier === 'perf' ? 'chromium' : 'chromium webkit' : env.BROWSER_ENGINES);
  assert(engines.length && engines.every(engine => ['chromium', 'webkit'].includes(engine)), 'BROWSER_ENGINES must select chromium and/or webkit');
  assert.equal(new Set(engines).size, engines.length, 'Duplicate BROWSER_ENGINES entry');
  const jobs = positive('BROWSER_JOBS', env.BROWSER_JOBS || '8');
  assert(jobs <= 32, 'BROWSER_JOBS must be at most 32');
  // Performance trials are exclusive, even if the caller exported BROWSER_JOBS.
  assert(tier !== 'perf' || selected.every(suite => suite === 'ring-render'), 'The perf tier runs only ring-render, without correctness neighbours');
  assert(!env.PERF || env.PERF === '0' || tier === 'perf', 'Use make ui-browser-perf for exclusive PERF=1 measurements');
  return { tier, matrix, selected, engines, jobs: tier === 'perf' ? 1 : jobs,
    workerDeadline: positive('BROWSER_WORKER_DEADLINE_SECS', env.BROWSER_WORKER_DEADLINE_SECS || '300'),
    passes: positive('UI_BROWSER_PASSES', env.UI_BROWSER_PASSES || '1') };
}

// A shard only selects existing independent cases. The test still creates a
// fresh context for each case and owns all of its request/response gates.
// Custom/new suites need only a .cjs file (and one suites[] entry for default
// inclusion); no second shell allowlist or mandatory sharder is needed.
function shards(suite, env) {
  const explicit = keys => keys.some(key => env[key] !== undefined);
  if (suite === 'workspace-open' && !explicit(['VIEWPORT', 'PLATFORM', 'ONLY'])) {
    return ['desktop', 'narrow'].flatMap(VIEWPORT => ['Linux x86_64', 'MacIntel'].map(PLATFORM => ({ VIEWPORT, PLATFORM })));
  }
  if (suite === 'motion' && !explicit(['VIEWPORT', 'THEME', 'MOTION_CASE', 'ONLY'])) {
    if (env.BROWSER_MATRIX !== 'all') {
      const interaction = ['desktop', 'phone'].flatMap(VIEWPORT => [
        { VIEWPORT, THEME: 'light', MOTION_CASE: 'full' },
        ...[['light', 'reduced'], ['light', 'off'], ['dark', 'full']].map(([THEME, MOTION_CASE]) => ({ VIEWPORT, THEME, MOTION_CASE,
          ONLY: 'policy,menu,modelPicker,drawer,sheet,sessionsPicker,phone,tiles,tool,sendStop' })),
      ]);
      return [...interaction, ...['system', 'system-reduce', 'full-reduce'].map(MOTION_CASE => ({
        VIEWPORT: 'desktop', THEME: 'light', MOTION_CASE, ONLY: 'policy',
      }))];
    }
    return ['desktop', 'phone'].flatMap(VIEWPORT => ['light', 'dark'].map(THEME => ({ VIEWPORT, THEME })));
  }
  if (suite === 'phone-picker' && !explicit(['VIEWPORT', 'THEME', 'MOTION_CASE'])) {
    if (env.BROWSER_MATRIX !== 'all') return [
      ['narrow', 'light', 'full'], ['narrow', 'dark', 'off'],
      ['phone', 'dark', 'full'], ['phone', 'light', 'reduced'],
      ['wide', 'light', 'full'], ['wide', 'dark', 'reduced'],
      ['desktop', 'dark', 'full'], ['desktop', 'light', 'off'],
    ].map(([VIEWPORT, THEME, MOTION_CASE]) => ({ VIEWPORT, THEME, MOTION_CASE }));
    return ['narrow', 'phone', 'wide', 'desktop'].flatMap(VIEWPORT => ['light', 'dark'].map(THEME => ({ VIEWPORT, THEME })));
  }
  if (suite === 'phone-bar' && !explicit(['VIEWPORT', 'THEME', 'CASE'])) {
    return ['portrait-320', 'portrait-390', 'portrait-430', 'landscape-900', 'landscape-430', 'desktop-wide', 'desktop-narrow']
      .flatMap(VIEWPORT => ['light', 'dark'].filter(THEME => env.BROWSER_MATRIX === 'all' || THEME === 'light'
        || VIEWPORT === 'portrait-320' || VIEWPORT.startsWith('desktop-')).map(THEME => ({ VIEWPORT, THEME })));
  }
  return [{}];
}
function plan(config, env) {
  const tasks = [];
  for (const engine of config.engines) for (const suite of config.selected) {
    if (suite === 'ring-render' && engine !== 'chromium') continue;
    for (const [index, filters] of shards(suite, { ...env, BROWSER_MATRIX: config.matrix }).entries()) tasks.push({ engine, suite, shard: index + 1, filters });
  }
  assert(tasks.length, 'No applicable suite/engine pairs selected (ring-render is Chromium-only)');
  // Start long jobs first so the end of the run isn't one unsharded tail.
  const weight = { 'message-edit': 100, motion: 80, 'session-sheet': 60, theme: 50, 'phone-picker': 40 };
  return tasks.sort((a, b) => (weight[b.suite] || 20) - (weight[a.suite] || 20));
}

async function pool(tasks, jobs, run, cancelled = () => false) {
  const results = [];
  let cursor = 0, failed = false, failure;
  await Promise.all(Array.from({ length: Math.min(tasks.length, jobs) }, async () => {
    while (!failed && !cancelled() && cursor < tasks.length) {
      const task = tasks[cursor++];
      try {
        const result = await run(task);
        results.push(result);
        if (result.status !== 0) failed = true;
      } catch (error) {
        failed = true;
        failure ||= error;
      }
    }
  }));
  if (failure) throw failure; // Other active workers have now saved/closed.
  return { results, pending: tasks.length - cursor };
}

async function child(task, directory, environment, runStarted = performance.now(), deadlineMs = 300000) {
  fs.mkdirSync(directory, { recursive: true });
  const label = `${task.engine}/${task.suite}/${task.shard}`;
  const args = [path.join(__dirname, `${task.suite}.cjs`), ...(task.suite === 'theme' ? ['--browser'] : [])];
  const started = performance.now();
  const log = fs.createWriteStream(path.join(directory, 'run.log'));
  const timeline = fs.createWriteStream(path.join(directory, 'timeline.jsonl'));
  console.log(`RUN ${label} ${JSON.stringify(task.filters)}`);
  let tail = '', launchError, evidenceError;
  const process = spawn(global.process.execPath, args, { env: { ...environment, ...task.filters, ENGINE: task.engine,
    SCREENSHOTS: directory, EVIDENCE: directory }, stdio: ['ignore', 'pipe', 'pipe'], detached: global.process.platform !== 'win32' });
  children.add(process);
  for (const stream of [log, timeline]) stream.on('error', error => {
    evidenceError ||= String(error);
    cancellations.get(process)?.();
  });
  const record = channel => chunk => {
    log.write(chunk);
    // Keep timestamps even for phases that predate the common within() helper.
    timeline.write(JSON.stringify({ ms: performance.now() - started, channel, text: chunk.toString() }) + '\n');
    tail = (tail + chunk.toString()).slice(-12000);
  };
  process.stdout.on('data', record('stdout'));
  process.stderr.on('data', record('stderr'));
  process.on('error', error => { launchError = error.message; });
  const outcome = await supervise(process, deadlineMs);
  children.delete(process);
  await Promise.all([new Promise(resolve => log.end(resolve)), new Promise(resolve => timeline.end(resolve))]);
  const finished = performance.now();
  const result = { ...task, seconds: (finished - started) / 1000, startedSeconds: (started - runStarted) / 1000,
    finishedSeconds: (finished - runStarted) / 1000, ...outcome,
    ...(evidenceError ? { status: 1, evidenceError } : {}), launchError, directory };
  console.log(`${result.status === 0 ? 'PASS' : 'FAIL'} ${label} ${result.seconds.toFixed(2)}s`);
  if (result.status !== 0) console.error(tail, `Evidence: ${directory}`);
  return result;
}

const usesLocalNative = task => task.engine === 'webkit' && task.suite === 'transcript-press';

async function closeOwnedBrowser(browser, kill, deadlineMs) {
  // Client Browser.close has no timeout (and no public kill method). Keep the
  // pinned local server's kill handle, not a remote BrowserServer connection.
  const bounded = async (action, name) => {
    let timer;
    try {
      return await Promise.race([Promise.resolve().then(action), new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${name} deadline (${deadlineMs}ms)`)), deadlineMs);
      })]);
    } finally { clearTimeout(timer); }
  };
  try { await bounded(() => browser.close(), 'local browser close'); }
  catch (error) {
    try {
      assert.equal(typeof kill, 'function', 'pinned local browser force-kill handle unavailable');
      await bounded(kill, 'local browser kill');
    } catch (killError) { throw new AggregateError([error, killError], 'Local browser cleanup failed'); }
    throw error; // Successful escalation still records the cleanup failure.
  }
}

async function localNative(task, directory, environment, runStarted = performance.now(), deadlineMs = 300000, dependencies = {}) {
  const started = performance.now(), label = `${task.engine}/${task.suite}/${task.shard}`;
  const io = dependencies.fs || fs;
  const controller = new AbortController(), cleanupErrors = [];
  let browser, kill, closing, pending, report, error, launchError, evidenceError, tail = '';
  let timedOut = false, interrupted = false, launching = false;
  const close = () => {
    if (browser) closing ||= closeOwnedBrowser(browser, kill, dependencies.cleanupDeadlineMs || 5000)
      .catch(error => { cleanupErrors.push(format(error)); });
    return closing;
  };
  const stop = reason => {
    if (!controller.signal.aborted) controller.abort(reason);
    return close();
  };
  const interrupt = () => {
    interrupted = true;
    stop(dependencies.signal.reason || new Error('Browser invocation interrupted'));
  };
  const record = channel => (...args) => {
    const text = format(...args) + '\n';
    tail = (tail + text).slice(-12000);
    try {
      io.appendFileSync(path.join(directory, 'run.log'), text);
      io.appendFileSync(path.join(directory, 'timeline.jsonl'), JSON.stringify({ ms: performance.now() - started, channel, text }) + '\n');
    } catch (error) {
      evidenceError ||= String(error);
      stop(error); // Event-handler logging must not throw into another worker.
    }
  };
  const log = { log: record('stdout'), error: record('stderr') };
  const timer = setTimeout(() => {
    timedOut = true;
    stop(new Error(`local native worker deadline (${deadlineMs}ms)`));
  }, deadlineMs);
  dependencies.signal?.addEventListener('abort', interrupt, { once: true });
  console.log(`RUN ${label} ${JSON.stringify(task.filters)} parent-local-launch`);
  try {
    io.mkdirSync(directory, { recursive: true });
    io.writeFileSync(path.join(directory, 'run.log'), '');
    io.writeFileSync(path.join(directory, 'timeline.jsonl'), '');
    if (dependencies.signal?.aborted) interrupt();
    controller.signal.throwIfAborted();
    const playwright = dependencies.playwright || require(environment.PLAYWRIGHT_CORE || 'playwright-core');
    const launchOptions = dependencies.launchOptions || require('./motion.cjs').launchOptions;
    const suite = dependencies.suite || require('./transcript-press.cjs').runSuite;
    const options = launchOptions(task.engine);
    launching = true;
    browser = await playwright[task.engine].launch({ ...options, timeout: Math.min(options.timeout || deadlineMs, deadlineMs) });
    launching = false;
    // Capture ownership synchronously, before observing cancellation or running
    // suite code. Remote connections cannot provide this server object.
    const process = browser._connection?.toImpl?.(browser)?.options?.browserProcess;
    if (process?.kill) kill = process.kill.bind(process);
    assert.equal(typeof kill, 'function', 'native WebKit requires pinned local launch with force-kill ownership');
    controller.signal.throwIfAborted();
    pending = Promise.resolve().then(() => {
      controller.signal.throwIfAborted();
      return suite({ browser, engine: task.engine,
        origin: environment.BROWSER_TEST_ORIGIN, root: path.resolve(environment.ROOT || path.join(__dirname, '../dist')),
        evidenceDirectory: directory, log, signal: controller.signal, stop,
        selected: environment.PRESS_CASE?.split(','), timings: true });
    });
    report = await pending;
    assert(Array.isArray(report?.results) && report.results.length, 'native suite returned no controls');
    assert(report.results.every(result => typeof result?.passed === 'boolean'), 'native suite returned invalid controls');
  } catch (caught) {
    error = String(caught.stack || caught);
    if (launching) launchError = error;
    log.error(error);
  } finally {
    clearTimeout(timer);
    await close();
    // Do not release the pool slot, server, or lock with suite callbacks live.
    await pending?.catch(() => {});
    for (const error of cleanupErrors) log.error(error);
    dependencies.signal?.removeEventListener('abort', interrupt);
  }
  const finished = performance.now();
  const result = { ...task, execution: 'parent-local-launch', seconds: (finished - started) / 1000,
    startedSeconds: (started - runStarted) / 1000, finishedSeconds: (finished - runStarted) / 1000,
    status: error || evidenceError || timedOut || interrupted || cleanupErrors.length || report?.results.some(result => !result.passed) ? 1 : 0,
    signal: null, timedOut, interrupted, launchError, error, evidenceError, cleanupErrors, directory };
  console.log(`${result.status === 0 ? 'PASS' : 'FAIL'} ${label} ${result.seconds.toFixed(2)}s parent-local-launch`);
  if (result.status !== 0) console.error(tail, `Evidence: ${directory}`);
  return result;
}

async function run(env, dependencies = {}) {
  const config = configuration(env), tasks = plan(config, env);
  const output = path.resolve(env.BROWSER_TEST_OUTPUT || 'dist/browser-tests',
    `${config.tier}-${new Date().toISOString().replace(/[:.]/g, '-')}-${process.pid}`);
  fs.mkdirSync(output, { recursive: true });
  const started = performance.now(), report = { config, output, root: path.resolve(env.ROOT || path.join(__dirname, '../dist')), passes: [] };
  report.node = process.version;
  const hash = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
  report.assets = fs.readdirSync(report.root, { recursive: true }).filter(file => /\.(wasm|js|css)$/.test(file))
    .map(file => ({ file, sha256: hash(path.join(report.root, file)) }));
  report.tests = [...config.selected, 'motion', 'narrow-fixtures', 'browser-runner', 'browser-runner.test',
    'browser-navigation', 'browser-navigation.test',
    'browser-lifecycle', 'browser-lifecycle.test', 'follow-press-unit'].filter((name, index, names) => names.indexOf(name) === index
    && fs.existsSync(path.join(__dirname, `${name}.cjs`))).map(name => ({ name, sha256: hash(path.join(__dirname, `${name}.cjs`)) }));
  const save = () => {
    report.seconds = (performance.now() - started) / 1000;
    fs.writeFileSync(path.join(output, 'summary.json'), JSON.stringify(report, null, 2));
  };
  console.log(`Browser ${config.tier}: ${tasks.length} shards/pass, ${config.jobs} workers; evidence ${output}`);
  const { serve, launchOptions } = dependencies.fixture || require('./motion.cjs');
  const playwright = dependencies.playwright || require(env.PLAYWRIGHT_CORE || 'playwright-core');
  const runChild = dependencies.child || child;
  const closeBrowser = async browser => {
    try { await browser.close(); }
    catch (error) { await browser.kill(); throw error; }
  };
  const { server, origin } = await serve(), servers = [];
  const nativeController = new AbortController();
  const environment = { ...env, BROWSER_TEST_ORIGIN: origin, BROWSER_MATRIX: config.matrix,
    PERF: config.tier === 'perf' ? '1' : '0', BROWSER_CASE_TIMINGS: '1' };
  let failed = false;
  const interrupt = () => {
    report.interrupted = true;
    nativeController.abort(new Error('Browser invocation interrupted'));
    for (const child of children) cancellations.get(child)?.();
  };
  const checkInterrupted = () => assert(!report.interrupted, 'Browser invocation interrupted; no further workers will start');
  const worker = async (task, directory) => {
    if (usesLocalNative(task)) return localNative(task, directory, environment, started, config.workerDeadline * 1000,
      { playwright, launchOptions, suite: dependencies.nativeSuite, signal: nativeController.signal,
        cleanupDeadlineMs: dependencies.cleanupDeadlineMs });
    if (env.ISOLATE_BROWSER !== '1') return runChild(task, directory, environment, started, config.workerDeadline * 1000);
    // Playwright launches browsers in detached process groups. Never leave
    // their ownership in a child that our watchdog may have to SIGKILL.
    const browser = await playwright[task.engine].launchServer(launchOptions(task.engine));
    try {
      checkInterrupted();
      return await runChild(task, directory, { ...environment, ISOLATE_BROWSER: '0',
        [`BROWSER_TEST_${task.engine.toUpperCase()}_ENDPOINT`]: browser.wsEndpoint() }, started, config.workerDeadline * 1000);
    } finally { await closeBrowser(browser); }
  };
  process.on('SIGINT', interrupt);
  process.on('SIGTERM', interrupt);
  try {
    const units = [
      ...(config.selected.includes('transcript-press') ? ['follow-press-unit'] : []),
      ...(config.selected.some(suite => ['motion', 'navigation-errors'].includes(suite)) ? ['browser-navigation.test'] : []),
      ...(config.selected.includes('session-sheet') ? ['browser-lifecycle.test'] : []),
    ];
    report.units = [];
    for (const suite of units) {
      checkInterrupted();
      const result = await runChild({ engine: 'unit', suite, shard: 1, filters: {} }, path.join(output, suite), environment, started, config.workerDeadline * 1000);
      report.units.push(result);
      assert.equal(result.status, 0, `${suite} checks failed; see retained run.log`);
    }
    for (const engine of config.engines) {
      checkInterrupted();
      if (env.ISOLATE_BROWSER === '1' || !tasks.some(task => task.engine === engine && !usesLocalNative(task))) continue;
      const browser = await playwright[engine].launchServer(launchOptions(engine));
      servers.push(browser);
      checkInterrupted();
      environment[`BROWSER_TEST_${engine.toUpperCase()}_ENDPOINT`] = browser.wsEndpoint();
    }
    for (let pass = 1; pass <= config.passes; pass++) {
      checkInterrupted();
      const begin = performance.now();
      const cohort = await pool(tasks, config.jobs, task => worker(task,
        path.join(output, `pass-${pass}`, task.engine, task.suite, `shard-${task.shard}`)), () => report.interrupted);
      report.passes.push({ pass, seconds: (performance.now() - begin) / 1000, ...cohort });
      save();
      failed = report.interrupted || cohort.pending > 0 || cohort.results.some(result => result.status !== 0);
      if (failed) break; // Never retry or hide an earlier failed cohort.
    }
  } catch (error) {
    report.error = error.stack || String(error);
    throw error;
  } finally {
    const cleanup = await Promise.allSettled(servers.map(closeBrowser));
    try {
      server.closeAllConnections?.();
      await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
    } catch (error) { cleanup.push({ status: 'rejected', reason: error }); }
    finally {
      process.off('SIGINT', interrupt);
      process.off('SIGTERM', interrupt);
      report.cleanupErrors = cleanup.filter(result => result.status === 'rejected').map(result => String(result.reason));
      save();
    }
    if (report.cleanupErrors.length && !report.error) throw new AggregateError(
      cleanup.filter(result => result.status === 'rejected').map(result => result.reason), 'Browser infrastructure cleanup failed');
  }
  failed ||= report.interrupted;
  console.log(`${failed ? 'FAILED' : 'ALL PASS'} ${report.seconds.toFixed(2)}s; ${path.join(output, 'summary.json')}`);
  if (failed) process.exitCode = 1;
}

async function main(env = process.env) {
  configuration(env); // Reject invalid options without creating a lock.
  const release = acquireLock(env);
  try { await run(env); }
  finally { release(); }
}

module.exports = { configuration, shards, plan, pool, acquireLock, supervise, run, child, localNative, usesLocalNative };
if (require.main === module) main().catch(error => { console.error(error); process.exitCode = 1; });
