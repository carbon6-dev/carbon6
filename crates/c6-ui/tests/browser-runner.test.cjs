'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const { configuration, shards, plan, pool } = require('./browser-runner.cjs');

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
    { BROWSER_JOBS: '33' }, { UI_BROWSER_PASSES: '0' }, { BROWSER_SUITES: 'motion motion' }, { PERF: '1' },
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
