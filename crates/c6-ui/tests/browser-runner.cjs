#!/usr/bin/env node
'use strict';

// Bounded correctness workers, not benchmark workers. See docs/testing.md.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { performance } = require('node:perf_hooks');
const children = new Set();

const suites = ['workspace-open', 'motion', 'comment-sheet', 'phone-picker', 'phone-bar', 'theme',
  'settings-lists', 'resource-lifecycle', 'markdown-html', 'status-row', 'ring-render',
  'transcript-press', 'transcript-times', 'message-edit', 'message-origin', 'comments-design', 'session-sheet'];
const quick = ['workspace-open', 'motion', 'settings-lists', 'markdown-html', 'transcript-press', 'transcript-times'];
const words = value => value.trim().split(/[\s,]+/).filter(Boolean);
const positive = (name, value) => {
  assert(/^[1-9][0-9]*$/.test(value), `${name} must be a positive integer`);
  assert(Number.isSafeInteger(Number(value)), `${name} is too large`);
  return Number(value);
};
function configuration(env) {
  const tier = env.BROWSER_TIER || 'full';
  assert(['full', 'quick', 'perf'].includes(tier), `Unknown BROWSER_TIER: ${tier}`);
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
  return { tier, selected, engines, jobs: tier === 'perf' ? 1 : jobs,
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
    return ['desktop', 'phone'].flatMap(VIEWPORT => ['light', 'dark'].map(THEME => ({ VIEWPORT, THEME })));
  }
  if (suite === 'phone-picker' && !explicit(['VIEWPORT', 'THEME', 'MOTION_CASE'])) {
    return ['narrow', 'phone', 'wide', 'desktop'].flatMap(VIEWPORT => ['light', 'dark'].map(THEME => ({ VIEWPORT, THEME })));
  }
  if (suite === 'phone-bar' && !explicit(['VIEWPORT', 'THEME', 'CASE'])) {
    return ['portrait-320', 'portrait-390', 'portrait-430', 'landscape-900', 'landscape-430', 'desktop-wide', 'desktop-narrow']
      .flatMap(VIEWPORT => ['light', 'dark'].map(THEME => ({ VIEWPORT, THEME })));
  }
  return [{}];
}
function plan(config, env) {
  const tasks = [];
  for (const engine of config.engines) for (const suite of config.selected) {
    if (suite === 'ring-render' && engine !== 'chromium') continue;
    for (const [index, filters] of shards(suite, env).entries()) tasks.push({ engine, suite, shard: index + 1, filters });
  }
  assert(tasks.length, 'No applicable suite/engine pairs selected (ring-render is Chromium-only)');
  // Start long jobs first so the end of the run isn't one unsharded tail.
  const weight = { 'message-edit': 100, motion: 80, 'session-sheet': 60, theme: 50, 'phone-picker': 40 };
  return tasks.sort((a, b) => (weight[b.suite] || 20) - (weight[a.suite] || 20));
}

async function pool(tasks, jobs, run) {
  const results = [];
  let cursor = 0, failed = false;
  await Promise.all(Array.from({ length: Math.min(tasks.length, jobs) }, async () => {
    while (!failed && cursor < tasks.length) {
      const task = tasks[cursor++];
      const result = await run(task);
      results.push(result);
      if (result.status !== 0) failed = true;
    }
  }));
  return { results, pending: tasks.length - cursor };
}

async function child(task, directory, environment) {
  fs.mkdirSync(directory, { recursive: true });
  const label = `${task.engine}/${task.suite}/${task.shard}`;
  const args = [path.join(__dirname, `${task.suite}.cjs`), ...(task.suite === 'theme' ? ['--browser'] : [])];
  const started = performance.now();
  const log = fs.createWriteStream(path.join(directory, 'run.log'));
  const timeline = fs.createWriteStream(path.join(directory, 'timeline.jsonl'));
  console.log(`RUN ${label} ${JSON.stringify(task.filters)}`);
  let tail = '', launchError;
  const process = spawn(global.process.execPath, args, { env: { ...environment, ...task.filters, ENGINE: task.engine,
    SCREENSHOTS: directory, EVIDENCE: directory }, stdio: ['ignore', 'pipe', 'pipe'] });
  children.add(process);
  const record = channel => chunk => {
    log.write(chunk);
    // Keep timestamps even for phases that predate the common within() helper.
    timeline.write(JSON.stringify({ ms: performance.now() - started, channel, text: chunk.toString() }) + '\n');
    tail = (tail + chunk.toString()).slice(-12000);
  };
  process.stdout.on('data', record('stdout'));
  process.stderr.on('data', record('stderr'));
  process.on('error', error => { launchError = error.message; });
  const [code, signal] = await new Promise(resolve => process.on('close', (...args) => resolve(args)));
  children.delete(process);
  await Promise.all([new Promise(resolve => log.end(resolve)), new Promise(resolve => timeline.end(resolve))]);
  const result = { ...task, seconds: (performance.now() - started) / 1000, status: code ?? 1, signal, launchError, directory };
  console.log(`${result.status === 0 ? 'PASS' : 'FAIL'} ${label} ${result.seconds.toFixed(2)}s`);
  if (result.status !== 0) console.error(tail, `Evidence: ${directory}`);
  return result;
}

async function main(env = process.env) {
  const config = configuration(env), tasks = plan(config, env);
  const output = path.resolve(env.BROWSER_TEST_OUTPUT || 'dist/browser-tests',
    `${config.tier}-${new Date().toISOString().replace(/[:.]/g, '-')}-${process.pid}`);
  fs.mkdirSync(output, { recursive: true });
  const started = performance.now(), report = { config, output, root: path.resolve(env.ROOT || path.join(__dirname, '../dist')), passes: [] };
  const save = () => {
    report.seconds = (performance.now() - started) / 1000;
    fs.writeFileSync(path.join(output, 'summary.json'), JSON.stringify(report, null, 2));
  };
  console.log(`Browser ${config.tier}: ${tasks.length} shards/pass, ${config.jobs} workers; evidence ${output}`);
  const { serve, launchOptions } = require('./motion.cjs');
  const playwright = require(env.PLAYWRIGHT_CORE || 'playwright-core');
  const { server, origin } = await serve(), servers = [];
  const environment = { ...env, BROWSER_TEST_ORIGIN: origin, PERF: config.tier === 'perf' ? '1' : '0', BROWSER_CASE_TIMINGS: '1' };
  let failed = false;
  const interrupt = () => {
    report.interrupted = true;
    for (const child of children) child.kill('SIGTERM');
  };
  process.on('SIGINT', interrupt);
  process.on('SIGTERM', interrupt);
  try {
    if (config.selected.includes('transcript-press')) {
      report.unit = await child({ engine: 'unit', suite: 'follow-press-unit', shard: 1, filters: {} }, path.join(output, 'follow-press-unit'), environment);
      assert.equal(report.unit.status, 0, 'production press helper checks failed; see retained run.log');
    }
    for (const engine of config.engines) {
      if (env.ISOLATE_BROWSER === '1' || !tasks.some(task => task.engine === engine)) continue;
      const browser = await playwright[engine].launchServer(launchOptions(engine));
      servers.push(browser);
      environment[`BROWSER_TEST_${engine.toUpperCase()}_ENDPOINT`] = browser.wsEndpoint();
    }
    for (let pass = 1; pass <= config.passes; pass++) {
      const begin = performance.now();
      const cohort = await pool(tasks, config.jobs, task => child(task,
        path.join(output, `pass-${pass}`, task.engine, task.suite, `shard-${task.shard}`), environment));
      report.passes.push({ pass, seconds: (performance.now() - begin) / 1000, ...cohort });
      save();
      failed = cohort.pending > 0 || cohort.results.some(result => result.status !== 0);
      if (failed) break; // Never retry or hide an earlier failed cohort.
    }
  } finally {
    await Promise.all(servers.map(browser => browser.close()));
    server.closeAllConnections?.();
    await new Promise(resolve => server.close(resolve));
    process.off('SIGINT', interrupt);
    process.off('SIGTERM', interrupt);
    save();
  }
  console.log(`${failed ? 'FAILED' : 'ALL PASS'} ${report.seconds.toFixed(2)}s; ${path.join(output, 'summary.json')}`);
  if (failed) process.exitCode = 1;
}

module.exports = { configuration, shards, plan, pool };
if (require.main === module) main().catch(error => { console.error(error); process.exitCode = 1; });
