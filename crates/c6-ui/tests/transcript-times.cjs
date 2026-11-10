#!/usr/bin/env node
'use strict';

// Production WASM/CSS; loopback fake-head transport only. No application/DOM
// mutation, retry, live head, build, or dependency change. See companion README.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser, session } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const root = path.resolve(process.env.ROOT || path.join(__dirname, '../dist'));
const shots = path.join(process.env.SCREENSHOTS || path.join(os.homedir(),
  '.cache/c6-helper/transcript-times'), `${Date.now()}-${process.pid}`);
const viewport = { width: 320, height: 568 };
const stamp = 1794240000000;
const event = (seq, kind) => ({ seq, ts: stamp, ...kind });
const started = { type: 'started', connection_id: 'test', model: 'test', cwd: '/fixture',
  system_prompt: 'Timestamp fixture prompt', tools: [], context_window: 128000 };
const notice = title => ({ type: 'notice', title, message: title, priority: 'normal' });
const ended = (turn, reason = 'completed') => ({ type: 'turn_ended', turn, reason });
const task = state => ({ type: 'background_task', task_id: `task-${state}`, kind: 'shell',
  description: `Timestamp task ${state}`, state, exit_code: 0, elapsed_ms: 0 });
const seed = [
  event(40, { type: 'turn_started', turn: 900 }), // summary before the visible window
  event(100, started), event(101, notice('Gap before bookkeeping')),
  event(102, { type: 'turn_started', turn: 701 }),
  event(103, { type: 'tool_result', call_id: 'orphan-tool', tool: 'bash', output: 'Result only',
    is_error: false, truncated: false, duration_ms: 0 }),
  event(104, { type: 'usage', model: 'test', input_tokens: 0, output_tokens: 0,
    cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0 }),
  event(105, { type: 'checkpoint', error: '' }), event(106, task('started')),
  event(107, { type: 'subagent_model_set' }),
  event(108, { type: 'notice_queued', command_id: 'queued', text: 'Queued only' }),
  event(109, ended(700)), event(110, ended(702)), event(111, { type: 'checkpoint' }),
  event(112, notice('Gap after bookkeeping')),
  event(113, { type: 'turn_started', turn: 703 }), event(114, ended(703)),
  event(115, ended(900)), event(116, ended(704, 'interrupted')),
  event(117, ended(705, 'worker_restarted')),
  event(118, { type: 'subagent_spawned', call_id: 'spawn', child_session_id: 'timestamp-child',
    description: 'Spawn bookkeeping' }),
  event(119, task('finished')), event(120, task('future-state')),
  event(121, { type: 'user', command_id: 'fold', text: 'Timestamp folded question' }),
  event(122, { type: 'assistant', turn: 706,
    blocks: [{ type: 'text', text: 'Timestamp folded reply' }], stop_reason: 'end_turn' }),
  event(123, { type: 'rewound', to_seq: 121, restored_files: false }),
  event(124, { type: 'assistant', turn: 707,
    blocks: [{ type: 'thinking', text: 'Timestamp reasoning only' }], stop_reason: 'end_turn' }),
  event(125, notice('Timestamp tail')),
];
const hoverSelector = ':scope > span.pointer-events-none.absolute';
const captionSelector = ':scope > p[title]';

async function empty(row, label) {
  await row.waitFor({ state: 'attached' });
  assert.deepEqual(await row.evaluate(el => ({ empty: el.matches(':empty'),
    display: getComputedStyle(el).display, height: el.getBoundingClientRect().height,
    rects: el.getClientRects().length, elements: el.children.length })),
  { empty: true, display: 'none', height: 0, rects: 0, elements: 0 }, label);
}
async function times(row, caption, hover, label) {
  assert.equal(await row.locator(captionSelector).count(), Number(caption), `${label}: caption`);
  assert.equal(await row.locator(hoverSelector).count(), Number(hover), `${label}: hover fallback`);
  for (const selector of [caption && captionSelector, hover && hoverSelector].filter(Boolean)) {
    assert((await row.locator(selector).textContent()).trim(), `${label}: nonempty timestamp`);
  }
}
async function retained(handle, row, label) {
  assert(await row.evaluate((el, old) => el === old && old.isConnected, handle), label);
}

async function check(browser, origin, engine, mode, side = false) {
  const label = `${engine}-${mode}${side ? '-side' : ''}`, evidence = [];
  let progress;
  const history = [], waiters = [], sockets = new Set();
  const nextHistory = () => history.length ? Promise.resolve(history.shift()) : new Promise(resolve => waiters.push(resolve));
  const showThinking = mode === 'all';
  try {
    await within(label, async () => {
      const f = await fixture(browser, origin, viewport, 'light',
        { choice: 'off', os: 'reduce', effective: 'off' }, current => { progress = current; }, {}, {
          instrumentation: false,
          setup: async ({ context }) => {
            await context.route('**/api/v1/settings/transcript', route => route.fulfill({ json: {
              times: mode, show_thinking: showThinking,
            } }));
            if (side) await context.route('**/api/v1/sessions/parent', route => route.fulfill({ json: {
              session: { ...session(), fork_seq: 1 }, worker_name: 'Motion fixture', worker_online: true, queued: [],
            } }));
            // Override just the stream, not motion.cjs's auth/live/API fixture.
            await context.routeWebSocket(/\/api\/v1\/sessions\/parent\/stream\?/, socket => {
              sockets.add(socket);
              socket.onClose(() => sockets.delete(socket));
              for (const e of seed) socket.send(JSON.stringify({ type: 'event', event: e }));
              socket.send(JSON.stringify({ type: 'window', from: 100 }));
              socket.send(JSON.stringify({ type: 'synced' }));
            });
            // Held until assertions explicitly release them: scrolling to the
            // top may naturally request an older page before the button tap.
            await context.route('**/api/v1/sessions/parent/events?*', route => new Promise(resolve => {
              const request = { route, resolve, before: Number(new URL(route.request().url()).searchParams.get('before')) };
              if (waiters.length) waiters.shift()(request); else history.push(request);
            }));
          },
        });
      await Promise.race([(async () => {
        const { page } = f;
        const row = seq => page.locator(`#parent-e${seq}`);
        await row(125).waitFor({ state: 'visible' });
        // Hydration can draw the default Exchanges before the settings GET
        // settles. Observe its actual DOM effect, not just request completion.
        await page.waitForFunction(mode => {
          const caption = seq => document.getElementById(`parent-e${seq}`)?.querySelector(':scope > p[title]');
          return Boolean(caption(101)) === (mode === 'all') && Boolean(caption(119)) === (mode !== 'off') &&
            document.getElementById('parent-e124')?.classList.contains('hidden') === (mode !== 'all');
        }, mode);
        assert.equal(sockets.size, 1, 'one overridden production transcript stream');
        if (side) {
          f.phase = 'side Started suppression';
          await empty(row(100), 'side Started has no body, hover, height or flex item');
          await times(row(101), false, true, 'visible side notice still has a hover timestamp');
        } else {
          f.phase = 'empty bookkeeping and real flex gap';
          for (let seq = 102; seq <= 111; seq++) await empty(row(seq), `bookkeeping ${seq}/${mode}`);
          await empty(row(113), 'zero-usage matching TurnStarted is itself empty');
          await row(101).scrollIntoViewIfNeeded();
          await settle(page);
          const geometry = await row(101).evaluate(a => {
            const b = document.getElementById('parent-e112'), parent = a.parentElement;
            return { sameParent: parent === b.parentElement, display: getComputedStyle(parent).display,
              gap: parseFloat(getComputedStyle(parent).rowGap),
              actual: b.getBoundingClientRect().top - a.getBoundingClientRect().bottom };
          });
          assert(geometry.sameParent && geometry.display === 'flex', 'gap anchors share actual flex transcript');
          assert(Number.isFinite(geometry.gap) && geometry.gap > 0, 'production transcript has a measurable row gap');
          assert(Math.abs(geometry.actual - geometry.gap) < 1,
            `one ordinary gap, not extra 16px bookkeeping gaps: ${JSON.stringify(geometry)}`);
          evidence.push({ phase: f.phase, geometry });

          f.phase = 'visible setup, rewind and end notes';
          for (const [seq, text] of [[100, 'Session started'], [116, 'Interrupted'],
            [117, 'Stopped: the worker restarted during this turn'], [123, 'Rewound to before']]) {
            await row(seq).scrollIntoViewIfNeeded();
            assert(await row(seq).isVisible(), `body ${seq} visible`);
            assert((await row(seq).textContent()).includes(text), `body ${seq}: ${text}`);
            await times(row(seq), false, true, `visible special row ${seq}/${mode}`);
          }
          assert.match(await row(114).textContent(), /Turn 703 ·/,
            'matching TurnStarted with no Usage still renders its zero-usage summary');
          assert.doesNotMatch(await row(114).textContent(), /model call|context /,
            'this matching turn did not acquire Usage from unrelated turn 701');
          await times(row(114), false, true, 'zero-usage matching turn');
          await times(row(118), mode === 'all', false, 'SubagentSpawned never gets fallback');
          if (mode !== 'all') await empty(row(118), 'uncaptioned SubagentSpawned stays empty');
          for (const seq of [119, 120]) {
            assert((await row(seq).textContent()).includes(`Timestamp task ${seq === 119 ? 'finished' : 'future-state'}`));
            await times(row(seq), mode !== 'off', mode === 'off', `non-started task ${seq}`);
          }
          await times(row(101), mode === 'all', mode !== 'all', 'ordinary visible notice');

          f.phase = 'folded rows and thinking visibility';
          const originalFold = await row(121).elementHandle();
          for (const seq of [121, 122]) {
            assert(await row(seq).evaluate(el => el.classList.contains('hidden') && el.classList.contains('opacity-50') && !el.getClientRects().length),
              `folded row ${seq} cannot leave a timestamp/gap`);
          }
          const show = row(123).getByRole('button', { name: 'show 1 message and its reply', exact: true });
          await show.tap();
          await row(123).getByRole('button', { name: 'hide 1 message and its reply', exact: true }).waitFor();
          for (const seq of [121, 122]) assert(await row(seq).isVisible(), `expanded row ${seq}`);
          await retained(originalFold, row(121), 'rewind expansion retains keyed message node');
          await times(row(121), mode !== 'off', mode === 'off', 'expanded user');
          await times(row(122), mode === 'all', mode !== 'all', 'expanded reply');
          await row(123).getByRole('button', { name: 'hide 1 message and its reply', exact: true }).tap();
          await row(123).getByRole('button', { name: 'show 1 message and its reply', exact: true }).waitFor();
          assert.equal(await row(124).evaluate(el => el.classList.contains('hidden')), !showThinking);
          await times(row(124), showThinking && mode === 'all', showThinking && mode !== 'all', 'reasoning-only row');
          if (showThinking) {
            await row(124).scrollIntoViewIfNeeded();
            assert(await row(124).getByText('Timestamp reasoning only', { exact: true }).isVisible());
          } else assert.equal(await row(124).evaluate(el => el.getClientRects().length), 0);

          // Deliberately synthetic history substitutions exercise both memo
          // directions through Feed::older, without mutating a WASM signal.
          // Real immutable server history must not move/omit a TurnStarted.
          f.phase = 'same-row reactive usage membership';
          const originalEnd = await row(115).elementHandle();
          assert.match(await row(115).textContent(), /Turn 900 ·/);
          await times(row(115), false, true, 'summary membership initially present');
          const load = async (before, events) => {
            const requested = nextHistory();
            await page.getByRole('button', { name: /^(Earlier messages|Loading earlier messages…)$/ }).tap();
            const request = await requested;
            assert.equal(request.before, before, 'history boundary');
            evidence.push({ phase: f.phase, before, events });
            await request.route.fulfill({ json: events }); request.resolve();
            await row(events[0].seq).waitFor({ state: 'attached' });
          };
          await load(100, [event(30, notice('Synthetic page without summary start'))]);
          await page.waitForFunction(() => document.getElementById('parent-e115')?.matches(':empty'));
          await empty(row(115), 'removing only matching membership removes summary AND fallback');
          await retained(originalEnd, row(115), 'membership loss does not remount TurnEnded');
          await load(30, [event(1, started), event(10, { type: 'turn_started', turn: 900 })]);
          await page.waitForFunction(() => document.getElementById('parent-e115')?.textContent.includes('Turn 900 ·'));
          await times(row(115), false, true, 'membership gain restores summary AND fallback');
          await retained(originalEnd, row(115), 'membership gain does not remount TurnEnded');
          await empty(row(110), 'unrelated turn membership never grants another TurnEnded a fallback');

          f.phase = 'live append invalidates usage without remount';
          const live = event(1000, { type: 'turn_started', turn: 901 });
          for (const socket of sockets) socket.send(JSON.stringify({ type: 'event', event: live }));
          await row(1000).waitFor({ state: 'attached' });
          await empty(row(1000), 'live bookkeeping remains empty');
          await retained(originalEnd, row(115), 'unrelated live start preserves existing end node');
          await times(row(115), false, true, 'unrelated live start preserves matching fallback');
        }
        await settle(page);
        assert.deepEqual(f.errors, [], 'no browser errors, bad stream messages or external requests');
        evidence.push({ phase: 'complete', showThinking });
      })(), f.crash]);
    });
    console.log(`PASS ${label}`);
    return { label, passed: true, evidence };
  } catch (error) {
    await progress?.page.screenshot({ path: path.join(shots, `${label}-failure.png`) }).catch(() => {});
    const result = { label, passed: false, phase: progress?.phase, error: error.stack,
      errors: progress?.errors, requests: progress?.requests, console: progress?.consoleMessages, evidence };
    fs.writeFileSync(path.join(shots, `${label}-failure.json`), JSON.stringify(result, null, 2));
    console.error(`FAIL ${label}: ${error.stack}`);
    return result;
  } finally { await progress?.context.close(); }
}

(async () => {
  const engine = process.env.ENGINE || 'both';
  assert(['chromium', 'webkit', 'both'].includes(engine), 'ENGINE=chromium,webkit,both');
  fs.mkdirSync(shots, { recursive: true });
  fs.copyFileSync(__filename, path.join(shots, 'transcript-times.cjs'));
  const hash = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
  const report = { root, viewport, theme: 'light', retries: 0,
    testSha256: hash(__filename), fixtureSha256: hash(path.join(__dirname, 'motion.cjs')),
    wasm: fs.readdirSync(root, { recursive: true }).filter(file => file.endsWith('.wasm'))
      .map(file => ({ file, sha256: hash(path.join(root, file)) })), results: [] };
  const record = result => {
    report.results.push(result);
    fs.writeFileSync(path.join(shots, 'results.json'), JSON.stringify(report, null, 2));
  };
  const { server, origin } = await serve();
  try {
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      let browser;
      try { browser = await launchBrowser(playwright, name); }
      catch (error) {
        for (const mode of ['off', 'exchanges', 'all', 'off-side']) {
          record({ label: `${name}-${mode}`, passed: false, phase: 'browser launch', error: error.stack });
        }
        continue;
      }
      try {
        for (const mode of ['off', 'exchanges', 'all']) record(await check(browser, origin, name, mode));
        record(await check(browser, origin, name, 'off', true));
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
  const failures = report.results.filter(result => !result.passed).length;
  console.log(`${report.results.length - failures}/${report.results.length} passed; ${failures} failed (no retries). Evidence: ${shots}`);
  if (failures) process.exitCode = 1;
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
