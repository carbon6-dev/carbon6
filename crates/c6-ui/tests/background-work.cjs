#!/usr/bin/env node
'use strict';

// Production WASM and a private fake head, never a live session. Timings are
// evidence rather than test thresholds. ROOT selects an independently built
// baseline; EVIDENCE/LABEL keep before/after traces separate.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser, session } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const output = process.env.EVIDENCE || '/tmp/s15-bg';
const label = process.env.LABEL || 'after';
const streaming = process.env.STREAMING === '1';
const typingOnly = process.env.TYPING_ONLY === '1';
const experimentalPolicies = process.env.EXPERIMENTAL_POLICIES === 'visible-counters';
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));

function history() {
  let seq = 66;
  const events = [];
  const add = event => events.push({ seq: seq++, ts: 1700000000000 + seq * 10, ...event });
  for (let turn = 66; turn < 1016; turn++) {
    add({ type: 'user', command_id: `input-${turn}`, text: `Investigate change ${turn}.`,
      attachments: turn % 10 ? [] : [{ name: 'drawing.svg', mime: 'image/svg+xml', size: 128 }] });
    add({ type: 'turn_started', turn });
    for (let call = 0; call < 6; call++) {
      const id = `call-${turn}-${call}`;
      add({ type: 'assistant', turn, blocks: [{ type: 'tool_use', id, name: 'shell', input_json: '{"command":["echo","fixture"]}' }], stop_reason: 'tool_use' });
      add({ type: 'tool_result', call_id: id, tool: 'shell', output: 'Fixture result. '.repeat(8), is_error: false, truncated: false, duration_ms: 10, images: 0 });
      add({ type: 'usage', model: 'test', input_tokens: 1200, output_tokens: 80, cache_read_tokens: 900, cache_write_tokens: 0, reasoning_tokens: 0 });
    }
    add({ type: 'assistant', turn, blocks: [{ type: 'text', text: `Change ${turn} checked. **Result**: local fixture, with a [link](/r/home/sessions/parent).\n\n` + 'A readable paragraph. '.repeat(12) }], stop_reason: 'end_turn' });
    add({ type: 'turn_ended', turn, reason: 'completed' });
  }
  return events;
}

function instrument() {
  window.__background = { keys: [], tasks: [], events: [], mutations: [], timers: [], fetches: [], phase: null };
  const fetch = window.fetch;
  window.fetch = (...args) => {
    __background.fetches.push(new URL(args[0] instanceof Request ? args[0].url : args[0], location.href).pathname);
    return fetch(...args);
  };
  const interval = window.setInterval, clear = window.clearInterval;
  window.clearInterval = id => {
    const timer = __background.timers.find(timer => timer.id === id);
    if (timer) timer.active = false;
    clear(id);
  };
  window.setInterval = (callback, period, ...args) => {
    const timer = { period, active: true, stack: new Error().stack, phases: {} };
    __background.timers.push(timer);
    timer.id = interval(() => {
      if (!__background.phase) return callback(...args);
      const phase = __background.phase, start = performance.now();
      callback(...args);
      const total = timer.phases[phase] ||= { count: 0, ms: 0 };
      total.count++; total.ms += performance.now() - start;
    }, period);
    return timer.id;
  };
  new PerformanceObserver(list => {
    if (__background.phase) __background.tasks.push(...list.getEntries().map(e => ({ phase: __background.phase, start: e.startTime, duration: e.duration })));
  }).observe({ type: 'longtask', buffered: false });
  new PerformanceObserver(list => {
    if (__background.phase) __background.events.push(...list.getEntries().map(e => ({ name: e.name, start: e.startTime, duration: e.duration,
      processingStart: e.processingStart, processingEnd: e.processingEnd, interaction: e.interactionId })));
  }).observe({ type: 'event', durationThreshold: 16 });
  document.addEventListener('keydown', event => {
    if (__background.phase !== 'typing' || !event.target.matches('textarea')) return;
    const start = event.timeStamp, dispatch = performance.now();
    // A task posted from rAF runs after that rendering opportunity. This is
    // an INP-like estimate, not a claim to measure hardware presentation.
    requestAnimationFrame(() => {
      const frame = performance.now();
      setTimeout(() => __background.keys.push({ start, dispatch, frame, duration: performance.now() - start }), 0);
    });
  }, true);
  new MutationObserver(records => {
    if (!__background.phase) return;
    const transcript = records.filter(r => {
      const target = r.target.nodeType === Node.ELEMENT_NODE ? r.target : r.target.parentElement;
      return target?.closest('.md') || [...r.addedNodes, ...r.removedNodes].some(node =>
        node.nodeType === Node.ELEMENT_NODE && (node.matches('.md') || node.querySelector('.md')));
    });
    if (transcript.length) __background.mutations.push({ phase: __background.phase, count: transcript.length });
  }).observe(document, { subtree: true, characterData: true, childList: true });
}

async function endTrace(cdp, file) {
  const complete = new Promise(resolve => cdp.once('Tracing.tracingComplete', resolve));
  await cdp.send('Tracing.end');
  const { stream } = await complete;
  const fd = fs.openSync(file, 'w');
  for (;;) {
    const chunk = await cdp.send('IO.read', { handle: stream });
    fs.writeSync(fd, chunk.base64Encoded ? Buffer.from(chunk.data, 'base64') : chunk.data);
    if (chunk.eof) break;
  }
  fs.closeSync(fd);
  await cdp.send('IO.close', { handle: stream });
}

async function main() {
  fs.mkdirSync(output, { recursive: true });
  const events = history();
  const layout = { tiles: ['session', 'work', 'projects'].map((view, i) => ({ key: 2 + i * 2,
    columns: [{ key: 3 + i * 2, view: view === 'session' ? { view, id: 'parent' } : { view } }],
    cursor: 0, active: 3 + i * 2, used: 3 + i * 2, back: [], forward: [], stack: null, minimized: false })),
  focus: 0, next: 8, full: false };
  const { server, origin } = await serve();
  const browser = await launchBrowser(playwright, 'chromium');
  let f, producer;
  try {
    f = await fixture(browser, origin, { width: 1800, height: 1000 }, 'dark', { choice: 'full', os: 'no-preference', effective: 'full' }, progress => { f = progress; }, {}, {
      instrumentation: false, layout, events,
      setup: async ({ page }) => {
        await page.addInitScript(instrument);
        await page.route('**/api/v1/sessions/parent', route => route.fulfill({ json: { session: { ...session(), last_seq: events.at(-1).seq }, worker_name: 'Fixture', worker_online: true, queued: [] } }));
        await page.route('**/attachments/*/*', route => route.fulfill({ contentType: 'image/svg+xml', body: '<svg xmlns="http://www.w3.org/2000/svg" width="320" height="200"><rect width="320" height="200" fill="#2563eb"/></svg>' }));
      },
    });
    const { page } = f;
    await page.getByText('Change 1015 checked.', { exact: false }).waitFor();
    console.log('History ready', await page.locator('.md').count());
    // Fixed benchmark warm-up allows the initial 10-second coalesced sidebar
    // invalidations to finish; they are startup work, not quiet steady state.
    await delay(15000);
    const cdp = await f.context.newCDPSession(page);
    await cdp.send('Emulation.setCPUThrottlingRate', { rate: 4 });
    await cdp.send('Performance.enable');
    const requests = [];
    let phase;
    page.on('request', request => { if (phase) requests.push({ phase, method: request.method(), path: new URL(request.url()).pathname }); });
    const metrics = async () => Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map(m => [m.name, m.value]));
    const measure = async (name, action) => {
      phase = name;
      await page.evaluate(name => { __background.phase = name; performance.mark(`s15-${name}-start`); }, name);
      const before = await metrics();
      const start = Date.now();
      await action();
      const after = await metrics();
      await page.evaluate(name => { performance.mark(`s15-${name}-end`); __background.phase = null; }, name);
      phase = null;
      return { wallMs: Date.now() - start, taskMs: (after.TaskDuration - before.TaskDuration) * 1000,
        scriptMs: (after.ScriptDuration - before.ScriptDuration) * 1000, requests: requests.filter(r => r.phase === name) };
    };
    await cdp.send('Profiler.enable');
    let idle = null;
    if (!typingOnly) {
      await cdp.send('Profiler.start');
      // Timed windows are measurements, not sleeps used as completion barriers.
      idle = await measure('idle', () => delay(60000));
      const idleProfile = await cdp.send('Profiler.stop');
      fs.writeFileSync(path.join(output, `${label}-idle.cpuprofile`), JSON.stringify(idleProfile.profile));
    }
    let deltas = 0, produce = () => Promise.resolve();
    if (streaming) {
      const seq = events.at(-1).seq + 1;
      await f.setState('working', { last_seq: seq });
      f.stream({ type: 'event', event: { seq, ts: Date.now(), type: 'turn_started', turn: 1016 } });
      f.stream({ type: 'text', turn: 1016, seq: seq + 1, text: 'Streaming investigation\n\n' +
        'A **rendered** paragraph with `inline code`, a [local link](/r/home/sessions/parent), and background investigation details.\n\n'.repeat(240) });
      await page.getByText('Streaming investigation', { exact: false }).waitFor();
      produce = () => new Promise(resolve => {
        producer = setInterval(() => {
          deltas++;
          f.stream({ type: 'text', turn: 1016, seq: seq + 1, text: `Another **streamed** result ${deltas}.\n\n` });
          if (deltas === 200) { clearInterval(producer); resolve(); }
        }, 50);
      });
    }
    const area = page.locator('textarea').first();
    await area.focus();
    await cdp.send('Profiler.start');
    await cdp.send('Tracing.start', { categories: 'devtools.timeline,blink.user_timing,v8.execute', transferMode: 'ReturnAsStream' });
    const text = 'Please examine the background activity while I type this draft, preserve every feature, and keep the conversation responsive with several tiles open and a very long history. Show the measured results.  '.slice(0, 200);
    assert.equal(text.length, 200);
    const typing = await measure('typing', async () => {
      await Promise.all([area.pressSequentially(text, { delay: 40 }), produce()]);
      if (streaming) await page.getByText('Another streamed result 200.', { exact: false }).waitFor();
    });
    clearInterval(producer);
    await page.waitForFunction(() => __background.keys.length === 200);
    const { profile } = await cdp.send('Profiler.stop');
    fs.writeFileSync(path.join(output, `${label}.cpuprofile`), JSON.stringify(profile));
    await endTrace(cdp, path.join(output, `${label}-trace.json`));
    const observed = await page.evaluate(() => __background);
    const values = observed.keys.map(k => k.duration).sort((a, b) => a - b);
    const quantile = p => values[Math.ceil(values.length * p) - 1];
    const summary = { label, streaming, deltas, browser: browser.version(), cpuSlowdown: 4,
      protocol: { typingOnly, warmupMs: 15000, characters: 200, keyDelayMs: 40, streamChunks: streaming ? 200 : 0, streamDelayMs: 50 },
      history: { events: events.length + 65, turns: 950, images: 95, wireBytes: events.reduce((n, event) => n + Buffer.byteLength(JSON.stringify({ type: 'event', event })), 0) },
      dom: await page.evaluate(() => ({ nodes: document.querySelectorAll('*').length, markdown: document.querySelectorAll('.md').length, images: document.images.length })),
      idle, typing, latency: { count: values.length, p50: quantile(.5), p95: quantile(.95), max: values.at(-1) },
      keys: observed.keys, longTasks: observed.tasks, timers: observed.timers, transcriptMutations: observed.mutations, eventTiming: observed.events, errors: f.errors };
    fs.writeFileSync(path.join(output, `${label}.json`), JSON.stringify(summary, null, 2));
    console.log(JSON.stringify(summary, null, 2));
    assert.deepEqual(f.errors, []);
    if (streaming) assert(observed.mutations.some(m => m.phase === 'typing'), 'live Markdown is a positive control for the mutation observer');
    if (process.env.BASELINE !== '1' && process.env.MEASURE_ONLY !== '1') {
      assert.equal(typing.requests.filter(r => /\/(?:mentions|suggestion|models|messages)(?:\/|$)/.test(r.path)).length, 0,
        'ordinary typing triggers no mention, suggestion, model, or message requests');
      if (!streaming) assert.equal(observed.mutations.filter(m => m.phase === 'typing').length, 0, 'typing does not rewrite transcript markdown');
      assert.equal(await area.inputValue(), text, 'ordinary typing retains the complete draft');
      // No runtime change qualified for release. Keep the rejected policies'
      // behavior checks opt-in for their pinned artifacts, not assertions that
      // would incorrectly require the baseline application to implement them.
      if (!experimentalPolicies) {
        console.log('PASS baseline draft and typing controls; experimental background policies are not retained');
        return;
      }
      // Count fetch *starts* in the browser, not asynchronous route delivery.
      // A title change is an explicit completion barrier for the snapshot.
      await page.evaluate(() => { __background.fetches = []; });
      f.live({ type: 'sessions', sessions: [{ ...session(), title: 'S15 snapshot barrier', last_seq: events.at(-1).seq }] });
      await page.getByText('S15 snapshot barrier', { exact: true }).first().waitFor();
      // Longer than the old broad ten-second coalescer: a short settle alone
      // would miss deferred counter invalidations. Work still uses that path.
      await delay(11000);
      assert.equal(await page.evaluate(() => __background.fetches.filter(p => p.endsWith('/elsewhere') || p.endsWith('/decisions/count')).length), 0,
        'ordinary session snapshots do not invalidate shared realm/decision counters');
      const countResponse = page.waitForResponse(response => new URL(response.url()).pathname.endsWith('/decisions/count'));
      f.live({ type: 'attention' });
      await (await countResponse).finished();
      await settle(page);
      assert.equal(await page.evaluate(() => __background.fetches.filter(p => p.endsWith('/decisions/count')).length), 1,
        'one attention event refreshes one shared decision count, not every drawer');
      const polls = () => page.evaluate(() => __background.timers.filter(t => t.active && [1500, 2000, 3000, 30000, 60000].includes(t.period)).length);
      const visiblePolls = await polls();
      assert(visiblePolls > 0, 'fixture has active fallback polls to exercise');
      await page.evaluate(() => {
        Object.defineProperty(document, 'hidden', { configurable: true, value: true });
        window.dispatchEvent(new Event('visibilitychange'));
      });
      await settle(page);
      assert.equal(await polls(), 0, 'all fallback network intervals are cleared while document is hidden');
      await page.evaluate(() => {
        Object.defineProperty(document, 'hidden', { configurable: true, value: false });
        window.dispatchEvent(new Event('visibilitychange'));
      });
      await settle(page);
      assert.equal(await polls(), visiblePolls, 'visible fallback cadence resumes without adding duplicate intervals');
      assert.equal(await area.inputValue(), text, 'hiding and revealing keeps the draft');

      // Unlike document.hidden above, these are real workspace transitions.
      // Work still has caller-owned fallbacks: the Work-event/shared-query
      // experiment failed its screen and is not in the retained implementation.
      const work = page.locator('[data-tile-motion="4"]');
      await work.getByRole('button', { name: 'Actions', exact: true }).click();
      await page.getByRole('button', { name: 'Minimize', exact: true }).click();
      await page.waitForFunction(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles.find(t => t.key === 4).minimized);
      await settle(page);
      assert.equal(await polls(), visiblePolls - 1, 'minimizing Work pauses its owned fallback interval');
      await page.keyboard.press('Control+2');
      await page.waitForFunction(() => !JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles.find(t => t.key === 4).minimized);
      await settle(page);
      assert.equal(await polls(), visiblePolls, 'revealing Work restores one interval, without remounting the session');

      const parent = page.locator('[data-tile-motion="2"]');
      await parent.getByRole('button', { name: 'Focus', exact: true }).click();
      await settle(page);
      assert.equal(await polls(), visiblePolls - 1, 'focus mode pauses the hidden Work pane');
      await parent.getByRole('button', { name: 'Defocus', exact: true }).click();
      await settle(page);
      assert.equal(await polls(), visiblePolls, 'leaving focus mode restores one Work interval');

      await page.setViewportSize({ width: 390, height: 844 });
      await settle(page);
      assert.equal(await polls(), visiblePolls - 1, 'inactive phone-sized Work pane does not poll');
      await page.keyboard.press('Control+2');
      await settle(page);
      assert.equal(await polls(), visiblePolls, 'activating the phone-sized Work pane resumes its interval');
      await page.keyboard.press('Control+1');
      await settle(page);
      assert.equal(await polls(), visiblePolls - 1, 'leaving the phone-sized Work pane pauses it again');
      assert.equal(await area.inputValue(), text, 'pane transitions keep the session draft');
      await page.setViewportSize({ width: 1800, height: 1000 });
      await settle(page);
      assert.equal(await polls(), visiblePolls, 'wide layout restores the visible Work pane exactly once');
      assert.deepEqual(f.errors, [], 'visibility transitions must remain panic-free');
      console.log('PASS background counter, hidden/resume, minimized/focus/phone panes, draft and typing regressions');
    }
  } finally {
    clearInterval(producer);
    await f?.context.close();
    await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
}

// Independent fresh-browser trials, serial on one machine. Six pairs balance
// which build goes first for each scenario. Idle is measured separately with
// the ordinary full-minute mode, never extrapolated from these typing trials.
function repeat() {
  const { spawnSync } = require('node:child_process');
  const crypto = require('node:crypto'), os = require('node:os');
  const pairs = Number(process.env.PAIRS);
  assert(Number.isInteger(pairs) && pairs >= 5, 'PAIRS must be an integer of at least five');
  const scenarios = (process.env.SCENARIOS || 'quiet,live').split(',');
  assert(scenarios.length && new Set(scenarios).size === scenarios.length && scenarios.every(s => ['quiet', 'live'].includes(s)), 'Invalid SCENARIOS');
  assert(process.env.BASE_ROOT && process.env.FINAL_ROOT, 'BASE_ROOT and FINAL_ROOT must select independently saved production builds');
  fs.mkdirSync(output, { recursive: true });
  const roots = { base: path.resolve(process.env.BASE_ROOT), final: path.resolve(process.env.FINAL_ROOT) };
  const builds = Object.fromEntries(Object.entries(roots).map(([name, root]) => {
    const wasm = fs.readdirSync(root).find(file => file.endsWith('.wasm'));
    return [name, { root, wasm, sha256: crypto.createHash('sha256').update(fs.readFileSync(path.join(root, wasm))).digest('hex') }];
  }));
  const manifest = { started: new Date().toISOString(), hostname: os.hostname(), node: process.version,
    harnessSha256: crypto.createHash('sha256').update(fs.readFileSync(__filename)).digest('hex'),
    pairs, scenarios, builds, protocol: { typingOnly: true, warmupMs: 15000, characters: 200, keyDelayMs: 40, cpuSlowdown: 4 }, trials: [] };
  for (let pair = 1; pair <= pairs; pair++) for (const scenario of scenarios) {
    const live = scenario === 'live';
    const order = (pair + Number(live)) % 2 ? ['base', 'final'] : ['final', 'base'];
    for (const build of order) {
      const name = `${label}-${scenario}-p${pair}-${build}`;
      const log = fs.openSync(path.join(output, `${name}-browser.log`), 'w');
      const trial = { pair, scenario, build, label: name, started: new Date().toISOString(), load: os.loadavg() };
      console.log(`START ${name}`);
      const result = spawnSync(process.execPath, [__filename], { env: { ...process.env, PAIRS: '', ROOT: roots[build],
        LABEL: name, STREAMING: live ? '1' : '0', TYPING_ONLY: '1', MEASURE_ONLY: '1', BASELINE: '' }, stdio: ['ignore', log, log] });
      fs.closeSync(log);
      trial.finished = new Date().toISOString(); trial.status = result.status; trial.signal = result.signal;
      manifest.trials.push(trial);
      fs.writeFileSync(path.join(output, `${label}-manifest.json`), JSON.stringify(manifest, null, 2));
      assert.equal(result.status, 0, `${name} failed; inspect its browser log`);
      console.log(`PASS ${name}`);
    }
  }
  const median = values => {
    const sorted = values.slice().sort((a, b) => a - b), i = sorted.length >> 1;
    return sorted.length % 2 ? sorted[i] : (sorted[i - 1] + sorted[i]) / 2;
  };
  const spread = values => ({ median: median(values), min: Math.min(...values), max: Math.max(...values), values });
  const summary = {};
  for (const scenario of scenarios) {
    const data = Object.fromEntries(['base', 'final'].map(build => [build, manifest.trials.filter(t => t.scenario === scenario && t.build === build)
      .map(t => JSON.parse(fs.readFileSync(path.join(output, `${t.label}.json`))))]));
    const metrics = { p50: d => d.latency.p50, p95: d => d.latency.p95, max: d => d.latency.max,
      taskMs: d => d.typing.taskMs, scriptMs: d => d.typing.scriptMs, wallMs: d => d.typing.wallMs,
      requests: d => d.typing.requests.length, longTasks: d => d.longTasks.filter(t => t.phase === 'typing').length };
    summary[scenario] = Object.fromEntries(Object.entries(metrics).map(([metric, get]) => {
      const base = data.base.map(get), final = data.final.map(get);
      return [metric, { base: spread(base), final: spread(final), pairedDelta: spread(base.map((v, i) => final[i] - v)) }];
    }));
  }
  fs.writeFileSync(path.join(output, `${label}-summary.json`), JSON.stringify(summary, null, 2));
  console.log(JSON.stringify(summary, null, 2));
}
if (process.env.PAIRS) {
  try { repeat(); } catch (error) { console.error(error); process.exitCode = 1; }
} else main().catch(error => { console.error(error); process.exitCode = 1; });
