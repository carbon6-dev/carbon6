#!/usr/bin/env node
'use strict';

// Production WASM, private loopback origin and mocked head; no test-only UI.
// BEFORE captures the identical input against a pinned build, not new assertions.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, within, settle, session, launchBrowser } = require('./motion.cjs');
const { browserLifecycle } = require('./browser-lifecycle.cjs');
const before = process.env.BEFORE === '1';
const shots = process.env.SCREENSHOTS || '/tmp/s3-sheet';
const phase = before ? 'before' : 'after';
const sharedTitle = 'Narrow tile layouts, no floor';
const commandTitle = 'Build the production UI';
const nativeIdentity = 'claude:["fixture-scope","native-agent"]';
const historyTotal = 69;
const views = {
  wide: { width: 1600, height: 1000 },
  narrow: { width: 1600, height: 1000 },
  'phone-320': { width: 320, height: 740 },
  'phone-390': { width: 390, height: 844 },
  'phone-430': { width: 430, height: 932 },
};
const now = Date.now() - 120000;
const child = (id, title, offset, state = 'working', extra = {}) => ({
  ...session(id, state), title, parent_id: 'parent', parent_call_id: `spawn-${id}`,
  created_at: now + offset, updated_at: now + offset, ...extra,
});
const children = [
  child('child-a', sharedTitle, 0),
  child('child-b', sharedTitle, 1000), // Same name is not the same identity.
  child('child-failed', 'Agent with a failed latest attempt', 4000, 'failed'),
  child('native-mirror', 'Read-only native mirror', 7900, 'failed', { observed: true }),
];
let seq = 1000;
const task = (id, description, offset, state = 'started', extra = {}) => ({
  seq: seq++, ts: now + offset, type: 'background_task', task_id: id,
  kind: 'shell', description, state, exit_code: 0, elapsed_ms: 5000,
  notify: true, native_type: '', foreground: false, ...extra,
});
const spawn = (id, taskId, offset, description = sharedTitle) => ({
  seq: seq++, ts: now + offset, type: 'subagent_spawned', call_id: `spawn-${id}-${taskId}`,
  child_session_id: id, background_task_id: taskId, description, agent: '',
});
const events = [
  spawn('child-a', 'a-original', 0),
  task('a-original', 'Original human task description', 0, 'started', { kind: 'agent' }),
  task('a-original', 'Original human task description', 500, 'failed', { kind: 'agent', exit_code: 1 }),
  spawn('child-b', 'b-task', 1000),
  task('b-task', sharedTitle, 1000, 'started', { kind: 'agent' }),
  task('build-task', commandTitle, 2000),
  spawn('child-a', 'a-continuation', 3000, 'continue child-a'),
  task('a-continuation', 'continue child-a', 3000, 'started', { kind: 'agent' }),
  spawn('child-failed', 'failed-agent-task', 4000, 'Failed agent'),
  task('failed-agent-task', 'Failed agent', 4000, 'failed', { kind: 'agent', exit_code: 1 }),
  task('failed-command', 'Failed command', 5000, 'finished', { exit_code: 3 }),
  task('interrupted-command', 'Interrupted command', 6000, 'stopped_by_user'),
  task('foreground-only', 'Foreground command must not appear', 7000, 'started',
    { kind: 'claude_code', native_type: 'local_bash', foreground: true }),
  task('claude:["fixture-scope","native-agent","run","run-1"]', 'Native task continuation', 7800, 'failed',
    { kind: 'claude_code', native_type: 'local_agent' }),
  task('claude:["fixture-scope","native-agent","run","run-2"]', 'Native task continuation', 7900, 'finished',
    { kind: 'claude_code', native_type: 'local_agent' }),
  ...Array.from({ length: 65 }, (_, i) => task(`done-${i}`, `Completed command ${i}`, 8000 + i, 'finished')),
];
const stream = text => ({ text, dropped_bytes: 0, note: '' });

async function ids(rows) { return rows.evaluateAll(nodes => nodes.map(n => n.getAttribute('data-background-job'))); }
async function back(sheet) {
  await sheet.getByRole('button', { name: 'Back', exact: true }).click();
  await settle(sheet.page());
}
async function landing(sheet) {
  for (let i = 0; i < 3 && !await sheet.getByRole('heading', { name: 'Session', exact: true }).count(); i++) await back(sheet);
  await heading(sheet, 'Session');
}
async function heading(sheet, name) { await sheet.getByRole('heading', { name, exact: true }).waitFor(); }

// Wide live rows enter the same confirmation drill-in with Stop; ended rows
// retain every action through Inspect. Return the exact path/opener for Back.
async function jobActions(row, sheet) {
  const overflow = row.getByRole('button', { name: /^Actions for / });
  const stop = row.locator('.c6-job-direct').getByRole('button', { name: /^Stop / });
  const inspect = row.locator('.c6-job-direct').getByRole('button', { name: /^Inspect / });
  const direct = await overflow.isVisible() ? overflow : await stop.isVisible() && await stop.isEnabled() ? stop : inspect;
  const opener = await direct.elementHandle();
  await direct.click();
  const depth = direct === inspect ? 2 : 1;
  if (depth === 2) {
    await heading(sheet, 'Inspect');
    await sheet.getByRole('button', { name: 'Actions', exact: true }).click();
  }
  return { opener, depth };
}
async function backFromJobActions(sheet, navigation) {
  for (let i = 0; i < navigation.depth; i++) await back(sheet);
}

async function jobControls(rows, touch) {
  const controls = await rows.evaluateAll(nodes => nodes.map(row => {
    const wide = row.parentElement.getBoundingClientRect().width >= 20 * parseFloat(getComputedStyle(document.documentElement).fontSize);
    const visible = el => getComputedStyle(el).display !== 'none';
    const direct = row.querySelector('.c6-job-direct');
    const overflow = row.querySelector('.c6-job-overflow');
    const buttons = [...(visible(direct) ? direct : overflow).querySelectorAll('button')];
    return { wide, direct: visible(direct), overflow: visible(overflow), buttons: buttons.map(button => {
      const r = button.getBoundingClientRect();
      return { name: button.getAttribute('aria-label'), width: r.width, height: r.height, svg: !!button.querySelector('svg') };
    }) };
  }));
  assert(controls.length > 0);
  for (const control of controls) {
    assert.equal(control.direct, control.wide, 'direct actions follow the job list width');
    assert.equal(control.overflow, !control.wide, 'ellipsis appears only in a narrow list');
    assert(control.buttons.every(button => button.svg), 'every row action preserves a drawn SVG icon');
    if (touch) assert(control.buttons.every(button => button.width >= 43.5 && button.height >= 43.5),
      `touch row actions are at least 44px in both dimensions: ${JSON.stringify(control.buttons)}`);
  }
}

// Diagnostic IO must never replace the first operation failure. Write before
// cleanup as well as afterward: a stuck cleanup must not lose the first state.
function caseDiagnostics(browser, name, details) {
  const lifecycle = browserLifecycle({ browser, label: name });
  let f, firstError, file;
  const writeFailure = () => {
    try {
      fs.writeFileSync(file + '.json', JSON.stringify({ error: firstError, ...details(),
        lifecycle: lifecycle.snapshot() }, null, 2));
    } catch (error) {
      lifecycle.secondary('failure-json', error);
      console.error(`DIAGNOSTIC WRITE FAILURE ${name}:`, error.stack || error);
    }
  };
  const remember = (error, preferred) => {
    lifecycle.firstError(error);
    if (firstError === undefined) firstError = String(error?.stack ?? error);
    if (!file) {
      file = preferred || path.join(shots, `${name}-FAIL`);
      if (fs.existsSync(file + '.json')) file += `-${Date.now()}`;
    }
  };
  return {
    started(progress) { f = progress; lifecycle.observe(progress); },
    phase(value) { lifecycle.phase(value); },
    async failure(error, preferred) {
      remember(error, preferred); // Inventory is captured before any diagnostic/cleanup calls.
      writeFailure();
      lifecycle.phase('failure-screenshot');
      if (f) try { await f.page.screenshot({ path: file + '.png' }); }
      catch (secondary) { lifecycle.secondary('failure-screenshot', secondary); }
      writeFailure();
    },
    async finish() {
      let cleanupError, cleanupFailed = false;
      const hadFailure = firstError !== undefined;
      try {
        lifecycle.phase('cleanup');
        await f?.context.close();
      } catch (error) {
        lifecycle.secondary('context.close', error);
        remember(error);
        cleanupError = error;
        cleanupFailed = true;
      } finally {
        lifecycle.dispose();
        if (file) writeFailure();
        else try {
          fs.appendFileSync(path.join(shots, 'lifecycle.jsonl'), JSON.stringify(lifecycle.snapshot()) + '\n');
        } catch (error) { console.error(`DIAGNOSTIC WRITE FAILURE ${name}:`, error.stack || error); }
      }
      if (cleanupFailed && !hadFailure) throw cleanupError;
    },
  };
}

async function check(browser, origin, engine, theme, view) {
  const name = `${phase}-${engine}-${theme}-${view}`;
  console.log(`START ${name}`);
  const stops = [], snapshots = [], unexpected = [], descendantQueries = [];
  const descendants = children.map(c => ({ ...c }));
  const replay = [...events];
  let rejectStop = null;
  let f;
  const diagnostics = caseDiagnostics(browser, name, () => ({ errors: f?.errors,
    console: f?.consoleMessages, requests: f?.requests, stops, snapshots, unexpected }));
  const emit = event => { replay.push(event); f.send(event); };
  const capture = async suffix => {
    diagnostics.phase(`settle:${suffix}`);
    await settle(f.page);
    diagnostics.phase(`screenshot:${suffix}`);
    await f.page.screenshot({ path: path.join(shots, `${name}-${suffix}.png`) });
    diagnostics.phase('case');
  };
  const finishLive = async () => {
    descendants.filter(c => c.status === 'running').forEach(c => { c.status = 'idle'; c.end_reason = 'completed'; });
    for (const [id, title, kind] of [['a-continuation', 'continue child-a', 'agent'], ['b-task', sharedTitle, 'agent'], ['build-task', commandTitle, 'shell']])
      emit(task(id, title, 95000, 'finished', { kind }));
    await f.setState('idle');
  };
  try {
    f = await fixture(browser, origin, views[view], theme,
      // Playwright's SW-blocking init script reads navigator.serviceWorker in
      // sandboxed output iframes and itself throws SecurityError. Match the
      // existing outputs.cjs fixture; this is still a fresh private origin.
      { choice: 'off', effective: 'off', os: 'reduce' }, progress => { f = progress; diagnostics.started(progress); }, { serviceWorkers: 'allow' }, {
        instrumentation: false, sessions: descendants, events: replay,
        setup: async ({ context }) => {
          await context.route('**/api/v1/sessions/**', async route => {
            const request = route.request(), url = new URL(request.url()), p = url.pathname;
            if (url.origin !== origin) {
              unexpected.push(`Blocked external session API: ${request.url()}`);
              return route.abort();
            }
            if (p.endsWith('/children')) {
              descendantQueries.push(url.search);
              return route.fulfill({ json: descendants });
            }
            if (request.method() === 'POST' && /\/(stop|interrupt)$/.test(p)) {
              stops.push(p);
              if (rejectStop && decodeURIComponent(p).includes(rejectStop))
                return route.fulfill({ status: 503, json: { error: 'Fixture Stop rejected; task is still running' } });
              return route.fulfill({ json: {} });
            }
            if (/\/outputs\/\d+$/.test(p)) return route.fulfill({ contentType: 'text/html',
              body: '<!doctype html><h1>Fixture published output</h1>' });
            const match = p.match(/^\/api\/v1\/sessions\/parent\/tasks\/([^/]+)$/);
            if (match) {
              const id = decodeURIComponent(match[1]); snapshots.push(id);
              const latest = replay.findLast(event => event.type === 'background_task' && event.task_id === id);
              return route.fulfill({ json: { task_id: id, kind: latest?.kind || 'agent',
                command: latest?.description || id, running: latest?.state === 'started',
                exit_code: latest?.state === 'started' ? null : latest?.exit_code ?? null,
                elapsed_ms: 9000, stdin: stream(''), stdout: stream(`Snapshot for ${id}`), stderr: stream('') } });
            }
            const c = descendants.find(c => p === `/api/v1/sessions/${c.id}`);
            if (c) return route.fulfill({ json: { session: c, worker_name: 'Sheet fixture', worker_online: true, queued: [] } });
            // The shared harness owns parent, view settings and git routes.
            // Navigation/read acknowledgements are not job-control requests.
            if (request.method() !== 'GET' && !/\/(view|viewed|seen|ui\/error)$/.test(p)) unexpected.push(`${request.method()} ${p}`);
            return route.fallback();
          });
        },
      });
    diagnostics.phase('case');
    const { page } = f;
    if (view === 'narrow') {
      // Actual tile container width, while the viewport stays desktop-wide.
      await page.addStyleTag({ content: '[data-tile-motion] { width: 338px !important; }' });
      await settle(page);
      assert.equal(await page.evaluate(() => innerWidth), 1600);
      assert(await page.locator('[data-tile-motion]').first().evaluate(el => el.getBoundingClientRect().width < 400));
    }
    const ring = page.getByRole('button', { name: /Open session sheet/ });
    if (before) {
      // Old wide surface is an aside, old popover/sheet has heading Session.
      // Do not ask the old build for new data attributes or drill-in controls.
      if (view !== 'wide') {
        await ring.click();
        await page.getByRole('heading', { name: 'Session', exact: true }).waitFor();
      } else await page.locator('aside:visible').filter({ hasText: 'Runs on' }).waitFor();
      await capture('busy');
      await finishLive();
      await capture('idle');
      assert.deepEqual(f.errors, []);
      console.log(`PASS ${name} (baseline capture only)`);
      return;
    }
    if (view !== 'wide') await ring.click();
    const sheet = page.locator('[data-session-sheet]:visible');
    await sheet.waitFor();
    assert.equal(await sheet.count(), 1, 'one active session overview');
    await sheet.getByText('Running now · 3', { exact: true }).waitFor();
    assert(descendantQueries.includes('?tree=1'), 'seed the production descendants endpoint, not only direct children');
    const rows = sheet.locator('[data-background-job]:visible');
    assert.equal(await rows.count(), 3, 'two different child IDs + one command; continuation is not a fourth job');
    const initialIds = await ids(rows);
    await jobControls(rows, view.startsWith('phone'));
    if (view === 'narrow' || view === 'wide') {
      // Cross both directions without changing the desktop viewport or tile:
      // the list, not either ancestor, owns this breakpoint.
      const list = rows.first().locator('..');
      const originalWidth = await list.evaluate(el => el.style.width);
      for (const width of [280, 360, 280]) {
        await list.evaluate((el, width) => { el.style.width = `${width}px`; }, width);
        await settle(page);
        await jobControls(rows, false);
        assert.equal(await page.evaluate(() => innerWidth), 1600);
      }
      await list.evaluate((el, width) => { el.style.width = width; }, originalWidth);
      await settle(page);
      await jobControls(rows, false);
      assert.deepEqual(stops, [], 'resizing row controls never stops a job');
    }
    assert.equal(new Set(initialIds).size, 3, 'identity is unique, not display title');
    const agentRows = rows.filter({ has: page.getByRole('button', { name: sharedTitle, exact: true }) });
    assert.equal(await agentRows.count(), 2, 'equal names with different IDs both survive');
    assert.match(initialIds[0], /child-b$/, 'stable ascending current-run start order');
    assert.match(initialIds[1], /build-task$/);
    assert.match(initialIds[2], /child-a$/, 'continuation uses this run start, not original child creation');
    assert.equal(await sheet.getByText(/Foreground command must not appear|continue child-a/).count(), 0);
    assert.match(await ring.getAttribute('aria-label'), /3 background/, 'ring count and visible list agree');
    const historyButton = sheet.getByRole('button', { name: 'History · 2 failed · 1 interrupted', exact: true });
    await historyButton.waitFor();
    const paneButtons = async () => {
      for (const label of ['Changes', 'Terminal', 'Files', 'Outputs']) {
        const count = await sheet.getByRole('button', { name: label, exact: true }).count();
        assert.equal(count, view.startsWith('phone') && ['Changes', 'Terminal'].includes(label) ? 1 : 0,
          `${view}: no Files/Outputs or duplicate desktop pane-button row (${label})`);
      }
    };
    await paneButtons();
    if (view.startsWith('phone')) {
      const targets = await sheet.locator('button:visible').evaluateAll(buttons => buttons.map(button => {
        const r = button.getBoundingClientRect();
        return { name: button.getAttribute('aria-label') || button.textContent.trim(), height: r.height };
      }));
      assert(targets.every(target => target.height >= 43.5), `phone landing targets are 44px: ${JSON.stringify(targets)}`);
    }
    if (view === 'wide') {
      await sheet.getByText('Runs on', { exact: true }).waitFor();
      await sheet.getByText('Context', { exact: true }).waitFor();
      await sheet.getByText('Not measured yet', { exact: true }).waitFor();
      assert.equal(await sheet.getByText('0%', { exact: true }).count(), 0, 'missing Usage must not invent a zero-percent context measurement');
      assert.equal(await page.locator('[data-transcript-status] [data-status-tasks]').count(), 1,
        'option A retains the right-hand opener even with the wide glance visible');
      const metadata = await sheet.getByText('Runs on', { exact: true }).elementHandle();
      const runningHeading = await sheet.getByRole('heading', { name: 'Running now · 3', exact: true }).elementHandle();
      assert(await metadata.evaluate((el, next) => !!(el.compareDocumentPosition(next) & Node.DOCUMENT_POSITION_FOLLOWING), runningHeading),
        'wide details expanded before running work');
    } else {
      assert.equal(await sheet.getByText('Runs on', { exact: true }).count(), 0, 'compact landing does not render metadata table');
      await sheet.getByRole('button', { name: /^(Session details|Details)\b/ }).waitFor();
      if (view.startsWith('phone')) {
        assert.equal(await page.getByRole('button', { name: 'Changes', exact: true }).count(), 1);
        assert.equal(await page.getByRole('button', { name: 'Terminal', exact: true }).count(), 1);
      }
    }
    await capture('busy');
    // A repeated started lifecycle record must not reorder an already-running job.
    emit(task('build-task', commandTitle, 90000));
    await settle(page);
    assert.deepEqual(await ids(rows), initialIds, 'live lifecycle refresh retains stable order');

    const command = rows.filter({ has: page.getByRole('button', { name: commandTitle, exact: true }) });
    const inspectIcon = command.getByRole('button', { name: `Inspect ${commandTitle}`, exact: true });
    if (await inspectIcon.isVisible()) {
      await inspectIcon.click();
      await heading(sheet, 'Inspect');
      assert.deepEqual(stops, [], 'direct Inspect is read-only');
      await sheet.getByRole('button', { name: 'Actions', exact: true }).click();
      await heading(sheet, 'Background command');
      await landing(sheet);
    }
    await command.getByRole('button', { name: commandTitle, exact: true }).click();
    await heading(sheet, 'Inspect');
    await sheet.getByText('Snapshot for build-task', { exact: true }).waitFor();
    assert(snapshots.includes('build-task'), 'Inspect requests the selected task, not another child');
    assert.equal(stops.length, 0, 'title is read-only Inspect, never Stop');
    await capture('inspect-command');
    await back(sheet);
    assert.deepEqual(await ids(rows), initialIds);
    await jobActions(command, sheet);
    await heading(sheet, 'Background command');
    assert.deepEqual(stops, [], 'row Stop opens confirmation without sending a request');
    await sheet.getByText('Stop affects only this job. Choose Stop below to confirm, or Back to cancel.', { exact: true }).waitFor();
    await back(sheet);
    assert.deepEqual(stops, [], 'Back cancels Stop confirmation');
    await jobActions(command, sheet);
    await sheet.getByRole('button', { name: 'Inspect', exact: true }).click();
    await heading(sheet, 'Inspect');
    await landing(sheet);
    await jobActions(command, sheet);
    await heading(sheet, 'Background command');
    await Promise.all([
      page.waitForResponse(response => response.request().method() === 'POST'
        && new URL(response.url()).pathname === '/api/v1/sessions/parent/tasks/build-task/stop'),
      sheet.getByRole('button', { name: 'Stop this command', exact: true }).click(),
    ]);
    assert.deepEqual(stops, ['/api/v1/sessions/parent/tasks/build-task/stop']);
    // Back is available if the actions surface remains open after successful Stop.
    if (await sheet.getByRole('button', { name: 'Back', exact: true }).count()) await back(sheet);

    const continued = sheet.locator('[data-background-job]:visible').filter({ has: page.getByRole('button', { name: sharedTitle, exact: true }) }).last();
    await continued.getByRole('button', { name: sharedTitle, exact: true }).click();
    await heading(sheet, 'Inspect');
    await sheet.getByRole('heading', { name: sharedTitle, exact: true }).waitFor();
    assert.equal(stops.length, 1);
    await sheet.getByText('Runs and raw IDs', { exact: true }).click();
    const attempts = await sheet.locator('pre').filter({ hasText: 'a-original' }).textContent();
    assert.match(attempts, /a-original.*failed/, 'earlier failed attempt remains inspectable');
    assert.match(attempts, /a-continuation.*started/, 'current continuation retains its raw task ID');
    assert.doesNotMatch(attempts, /b-task/, 'similarly named sibling is not part of this job');
    await capture('inspect-agent');
    await back(sheet);
    await jobActions(continued, sheet);
    await heading(sheet, 'Background agent');
    await sheet.getByRole('button', { name: 'Inspect', exact: true }).waitFor();
    await sheet.getByRole('button', { name: 'Open thread', exact: true }).waitFor();
    await capture('actions-agent');
    await Promise.all([
      page.waitForResponse(response => response.request().method() === 'POST'
        && ['/api/v1/sessions/parent/tasks/a-continuation/stop', '/api/v1/sessions/child-a/interrupt'].includes(new URL(response.url()).pathname)),
      sheet.getByRole('button', { name: 'Stop this agent', exact: true }).click(),
    ]);
    assert(['/api/v1/sessions/parent/tasks/a-continuation/stop', '/api/v1/sessions/child-a/interrupt'].includes(stops[1]),
      `Stop only current continuation or its child: ${JSON.stringify(stops)}`);
    assert.equal(stops.length, 2, 'one request per Stop, never stop parent or similarly named sibling');
    if (await sheet.getByRole('button', { name: 'Back', exact: true }).count()) await back(sheet);

    await historyButton.click();
    await heading(sheet, 'History');
    const filter = label => sheet.getByRole('button', { name: label, exact: true });
    await filter('Failed').click();
    await settle(page);
    assert.equal(await sheet.locator('[data-background-job]:visible').count(), 2, 'failed child/task mirrors merge, resumed old failure excluded');
    await capture('history-failed');
    await sheet.locator('[data-background-job]:visible').first().getByRole('button').first().click();
    await heading(sheet, 'Inspect');
    await back(sheet);
    await heading(sheet, 'History');
    assert.equal(await filter('Failed').getAttribute('aria-pressed'), 'true', 'Inspect Back retains the selected History filter');
    assert.equal(await sheet.locator('[data-background-job]:visible').count(), 2);
    assert.equal(await sheet.locator('[data-background-job]:visible .c6-job-direct').getByRole('button', { name: /^Stop / }).count(), 0,
      'ended jobs never offer a direct Stop');
    const historyActions = await jobActions(sheet.locator('[data-background-job]:visible').first(), sheet);
    await sheet.getByRole('heading', { name: /^Background (agent|command)$/ }).waitFor();
    await backFromJobActions(sheet, historyActions);
    assert.equal(await filter('Failed').getAttribute('aria-pressed'), 'true', 'Actions Back retains the selected History filter');
    await filter('Interrupted').click();
    await settle(page);
    assert.equal(await sheet.locator('[data-background-job]:visible').count(), 1);
    assert.match((await ids(sheet.locator('[data-background-job]:visible')))[0], /interrupted-command$/);
    await filter('All').click();
    await settle(page);
    const firstPage = await ids(sheet.locator('[data-background-job]:visible'));
    assert(firstPage.length > 2 && firstPage.length < historyTotal, 'history is paginated, not all retained jobs mounted');
    assert.match(firstPage[0], /done-64$/, 'history newest first');
    const more = sheet.getByRole('button', { name: 'Show more', exact: true });
    if (await more.count()) {
      // Progressive pagination is valid too: retain the first page, append
      // another bounded chunk, and never repeat a job inside either chunk.
      await more.click();
      await settle(page);
      const expanded = await ids(sheet.locator('[data-background-job]:visible'));
      assert.deepEqual(expanded.slice(0, firstPage.length), firstPage);
      assert(expanded.length > firstPage.length && expanded.length <= firstPage.length * 2);
      assert(expanded.length < historyTotal, 'second chunk must not eagerly mount the entire history');
      assert.equal(new Set(expanded).size, expanded.length, 'progressive history has no duplicate identities');
      await capture('history-page-2');
      await more.click();
      await settle(page);
      const complete = await ids(sheet.locator('[data-background-job]:visible'));
      assert.equal(complete.length, historyTotal, 'all distinct retained history jobs, including one native identity');
      assert.equal(new Set(complete).size, historyTotal);
      assert.equal(complete.filter(id => id === nativeIdentity).length, 1, 'native scope/task identity ignores run IDs');
      assert.equal(await more.count(), 0, 'pagination ends when all retained history has been loaded');
      const lastHistoryRow = sheet.locator('[data-background-job]:visible').last();
      await lastHistoryRow.scrollIntoViewIfNeeded();
      const scroller = await lastHistoryRow.evaluateHandle(el => {
        for (let p = el.parentElement; p; p = p.parentElement)
          if (p.scrollHeight > p.clientHeight + 2 && /auto|scroll/.test(getComputedStyle(p).overflowY)) return p;
        throw new Error('Paginated history must have a real scroll container');
      });
      const scrollBefore = await scroller.evaluate(el => el.scrollTop);
      assert(scrollBefore > 0, 'exercise an actually scrolled History page');
      const inspectOpener = await lastHistoryRow.getByRole('button').first().elementHandle();
      await lastHistoryRow.getByRole('button').first().click();
      await heading(sheet, 'Inspect');
      await back(sheet);
      await heading(sheet, 'History');
      assert.equal(await filter('All').getAttribute('aria-pressed'), 'true');
      assert.equal(await sheet.locator('[data-background-job]:visible').count(), historyTotal, 'Inspect Back retains loaded History pagination');
      assert(await scroller.evaluate(el => el.isConnected), 'History scroll container remains mounted');
      assert(Math.abs(await scroller.evaluate(el => el.scrollTop) - scrollBefore) < 2, 'Inspect Back restores History scroll position');
      assert(await inspectOpener.evaluate(el => document.activeElement === el), 'Inspect Back restores its exact title opener');
      const actions = await jobActions(sheet.locator('[data-background-job]:visible').last(), sheet);
      await sheet.getByRole('heading', { name: /^Background (agent|command)$/ }).waitFor();
      await backFromJobActions(sheet, actions);
      assert.equal(await sheet.locator('[data-background-job]:visible').count(), historyTotal, 'Actions Back retains loaded History pagination');
      assert(Math.abs(await scroller.evaluate(el => el.scrollTop) - scrollBefore) < 2, 'Actions Back restores History scroll position');
      assert(await actions.opener.evaluate(el => document.activeElement === el), 'Back restores the actual row-action opener without scrolling it away');
    } else {
      await sheet.getByRole('button', { name: /^(Next|Next page)( →)?$/ }).click();
      await settle(page);
      const secondPage = await ids(sheet.locator('[data-background-job]:visible'));
      assert(secondPage.length > 0);
      assert(secondPage.every(id => !firstPage.includes(id)), 'history pages do not repeat identities');
    }
    await back(sheet);
    assert.deepEqual(await ids(rows), initialIds, 'History Back restores live list');
    if (view !== 'wide') {
      await sheet.getByRole('button', { name: /^(Session details|Details)\b/ }).click();
      await sheet.getByText('Runs on', { exact: true }).waitFor();
      await sheet.getByText('Not measured yet', { exact: true }).waitFor();
      assert.equal(await sheet.getByText('0%', { exact: true }).count(), 0, 'compact Details keeps unmeasured context explicit');
      await capture('details');
      await back(sheet);
    }
    // Move all three running identities to history, preserving distinct latest outcomes.
    await finishLive();
    await sheet.getByText('Nothing running now', { exact: true }).waitFor();
    assert.equal(await rows.count(), 0);
    await historyButton.waitFor();
    assert.doesNotMatch(await ring.getAttribute('aria-label'), /[1-9]\d* background/);
    await capture('idle');
    // Correct the latest problem outcomes: quiet history is just History, no zero badges.
    descendants.find(c => c.id === 'child-failed').end_reason = 'completed';
    descendants.find(c => c.id === 'child-failed').status = 'idle';
    for (const id of ['failed-agent-task', 'failed-command', 'interrupted-command'])
      emit(task(id, id, 96000, 'finished', { kind: id === 'failed-agent-task' ? 'agent' : 'shell' }));
    await f.setState('idle');
    await sheet.getByRole('button', { name: 'History', exact: true }).waitFor();
    assert.equal(await sheet.getByRole('button', { name: /History ·/ }).count(), 0);
    await capture('quiet-history');

    // Actual native producer shape: observed child, initially unlinked spawn,
    // then a spawn update for each encoded [scope, task, "run", run_id]. The
    // child title wins; both native attempts belong to its one visible row.
    const linkedTitle = 'Linked native agent with two runs';
    const linkedRun1 = 'claude:["fixture-scope","linked-native","run","one"]';
    const linkedRun2 = 'claude:["fixture-scope","linked-native","run","two"]';
    descendants.push(child('linked-native-mirror', linkedTitle, 97000, 'working', { observed: true, parent_call_id: 'native-tool-call' }));
    const nativeSpawn = (id, offset) => ({ ...spawn('linked-native-mirror', id, offset, linkedTitle), call_id: 'native-tool-call' });
    emit(nativeSpawn('', 97000));
    emit(task(linkedRun1, 'Native supplied description', 97000, 'started', { kind: 'claude_code', native_type: 'local_agent' }));
    emit(nativeSpawn(linkedRun1, 97000));
    emit(task(linkedRun1, 'Native supplied description', 98000, 'failed', { kind: 'claude_code', native_type: 'local_agent' }));
    emit(task(linkedRun2, 'continue native task', 99000, 'started', { kind: 'claude_code', native_type: 'local_agent' }));
    await f.setState('idle');
    await sheet.getByText('Running now · 1', { exact: true }).waitFor();
    await rows.getByRole('button', { name: linkedTitle, exact: true }).waitFor();
    assert.deepEqual(await ids(rows), ['linked-native-mirror'], 'linked native mirror/task/continuation share the producer child identity');
    assert.match(await ring.getAttribute('aria-label'), /1 background/);
    await sheet.getByRole('button', { name: 'History', exact: true }).waitFor();
    await rows.getByRole('button', { name: linkedTitle, exact: true }).click();
    await heading(sheet, 'Inspect');
    await sheet.getByText('Runs and raw IDs', { exact: true }).click();
    const nativeRuns = await sheet.locator('pre').filter({ hasText: linkedRun1 }).textContent();
    assert(nativeRuns.includes(linkedRun1) && nativeRuns.includes(linkedRun2));
    await back(sheet);
    await heading(sheet, 'Session');
    emit(nativeSpawn(linkedRun2, 99000));
    await settle(page);
    await rows.getByRole('button', { name: linkedTitle, exact: true }).waitFor();
    assert.deepEqual(await ids(rows), ['linked-native-mirror'], 'late native spawn link does not change the established child identity');
    await jobActions(rows.filter({ has: page.getByRole('button', { name: linkedTitle, exact: true }) }), sheet);
    await heading(sheet, 'Background agent');
    rejectStop = linkedRun2;
    const stopsBeforeError = stops.length;
    await sheet.getByRole('button', { name: 'Stop this agent', exact: true }).click();
    await sheet.getByRole('alert').filter({ hasText: 'Fixture Stop rejected' }).waitFor();
    assert.equal(stops.length, stopsBeforeError + 1);
    assert.equal(decodeURIComponent(stops.at(-1)), `/api/v1/sessions/parent/tasks/${linkedRun2}/stop`, 'native Stop targets the current producer run only');
    assert(await sheet.getByRole('button', { name: 'Stop this agent', exact: true }).isEnabled(), 'failed Stop remains actionable, not optimistic success');
    await capture('native-stop-error');
    // Ends first, while its menu is open: retain Inspect, remove Stop without
    // another request. The latest native run must control this state.
    rejectStop = null;
    emit(task(linkedRun2, 'continue native task', 100000, 'finished', { kind: 'claude_code', native_type: 'local_agent' }));
    await sheet.getByRole('button', { name: 'Stop this agent', exact: true }).waitFor({ state: 'hidden' });
    await sheet.getByRole('button', { name: 'Inspect', exact: true }).waitFor();
    assert.equal(stops.length, stopsBeforeError + 1);
    await capture('native-ended-first');
    await back(sheet);
    await sheet.getByText('Nothing running now', { exact: true }).waitFor();

    // Details: no invented empty outputs/work/labels summaries, but their
    // editors stay reachable. All requests still go only to the mock head.
    if (view !== 'wide') await sheet.getByRole('button', { name: /^(Session details|Details)\b/ }).click();
    await sheet.getByText('Runs on', { exact: true }).waitFor();
    assert.equal(await sheet.getByText('Newest outputs', { exact: true }).count(), 0);
    assert.equal(await sheet.getByRole('button', { name: 'Show all', exact: true }).count(), 0);
    assert.equal(await sheet.getByText('Labels', { exact: true }).count(), 0);
    assert.equal(await sheet.getByText('Work', { exact: true }).count(), 0);
    await sheet.getByRole('button', { name: 'Assign work', exact: true }).click();
    const popup = page.locator('.c6-pop:visible');
    await popup.locator('select[title="The work this session serves (docs/work.md)"]').waitFor();
    assert.equal(await popup.locator('option[value="new"]').count(), 1, 'empty Work still has an assignment editor');
    await page.keyboard.press('Escape');
    await sheet.waitFor();
    await f.setState('idle', { labels: ['repo:carbon6', 'topic:session-sheet'], auto_labels: [] });
    await sheet.getByText('Labels', { exact: true }).waitFor();
    await sheet.getByText('repo:', { exact: true }).waitFor();
    await sheet.getByText('carbon6', { exact: true }).waitFor();
    await sheet.getByRole('button', { name: 'Edit labels', exact: true }).click();
    await popup.getByRole('button', { name: 'repo:carbon6', exact: true }).waitFor();
    await popup.getByRole('button', { name: 'topic:session-sheet', exact: true }).waitFor();
    assert.equal(await popup.getByRole('button', { name: 'Remove this label', exact: true }).count(), 2, 'full raw labels are editable, not only formatted summaries');
    await page.keyboard.press('Escape');
    for (let i = 1; i <= 4; i++) emit({ seq: seq++, ts: now + 101000 + i, type: 'output',
      name: `sheet-report-${i}.html`, title: `Sheet report ${i}`, mime: 'text/html', size: 42 });
    await sheet.getByText('Newest outputs', { exact: true }).waitFor();
    await sheet.getByRole('button', { name: /Sheet report 4/ }).waitFor();
    const recent = sheet.getByRole('button', { name: /Sheet report \d/ });
    assert.equal(await recent.count(), 3, 'Details shows newest few, not the entire catalog');
    assert.deepEqual(await recent.allTextContents().then(t => t.map(s => s.match(/Sheet report \d/)[0])),
      ['Sheet report 4', 'Sheet report 3', 'Sheet report 2']);
    assert.equal(await sheet.getByRole('button', { name: /Sheet report 1/ }).count(), 0);
    await capture('details-published');
    const retainedDetails = await sheet.getByText('Runs on', { exact: true }).elementHandle();
    await sheet.getByRole('button', { name: 'Show all', exact: true }).click();
    await page.frameLocator('iframe').getByRole('heading', { name: 'Fixture published output', exact: true }).waitFor();
    const outputPane = page.locator('[data-pane-motion]').filter({ has: page.locator('iframe') });
    assert.equal(await outputPane.getByRole('button', { name: /Sheet report \d/ }).count(), 4,
      'Show all opens the real Outputs pane with the complete root replay');
    await capture('published-pane');
    const returnToDetails = async () => {
      if (view.startsWith('phone')) {
        await outputPane.locator('xpath=ancestor::*[@data-tile-surface][1]').locator('[data-tile-bar]')
          .getByRole('button', { name: 'Back to the previous pane', exact: true }).click();
      } else await outputPane.getByRole('button', { name: view === 'wide' ? 'Close this pane' : 'Back to the list', exact: true }).click();
      await sheet.getByText('Runs on', { exact: true }).waitFor();
      if (view !== 'wide') {
        await heading(sheet, 'Session details');
        assert(await retainedDetails.evaluate(el => el.isConnected), 'Back restores the retained Details body, not a new landing view');
      }
      await sheet.getByRole('button', { name: 'Show all', exact: true }).waitFor();
      await sheet.getByText('carbon6', { exact: true }).waitFor();
    };
    await returnToDetails();
    await capture('details-restored');
    // Picking a recent output, not just Show all, must use that actual output
    // and return through the same native pane Back route.
    await sheet.getByRole('button', { name: /Sheet report 2/ }).click();
    await outputPane.getByRole('heading', { name: /^Sheet report 2/ }).waitFor();
    await returnToDetails();
    if (view !== 'wide') {
      await page.keyboard.press('Escape');
      await sheet.waitFor({ state: 'hidden' });
      assert(await ring.evaluate(el => document.activeElement === el), 'dismissal after Outputs Back returns focus to the ring');
    }
    await page.getByRole('button', { name: 'Actions', exact: true }).click();
    const sessionMenu = page.locator('.c6-pop:visible');
    await sessionMenu.getByRole('button', { name: 'Files', exact: true }).waitFor();
    assert(await sessionMenu.getByRole('button', { name: 'Files', exact: true }).isEnabled(), `${view}: Files remains reachable in session Actions`);
    await page.keyboard.press('Escape');
    assert.deepEqual(unexpected, [], 'no other session mutations');
    assert.deepEqual(f.errors, []);
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FIRST FAILURE ${name}:`, error.stack || error);
    await diagnostics.failure(error);
    throw error;
  } finally { await diagnostics.finish(); }
}

async function restrictions(browser, origin, engine, variant) {
  const name = `after-${engine}-light-${variant}`;
  console.log(`START ${name}`);
  const ssh = variant.startsWith('ssh');
  const missing = variant === 'missing-detail';
  const viewport = variant === 'ssh-wide' ? views.wide : views['phone-390'];
  const command = 'Known command during an unavailable session';
  const worker = { id: session().worker_id, name: 'Sheet fixture', hostname: 'fixture', os: 'linux', arch: 'x86_64',
    version: 'test', status: 'offline', enrolled_at: 1, last_seen: 1, connected_since: 1, rtt_us: 0 };
  const stops = [];
  let f, live, disconnected = false;
  const diagnostics = caseDiagnostics(browser, name, () => ({ errors: f?.errors,
    console: f?.consoleMessages, requests: f?.requests, stops }));
  try {
    f = await fixture(browser, origin, viewport, 'light', { choice: 'off', effective: 'off', os: 'reduce' },
      progress => { f = progress; diagnostics.started(progress); }, {}, {
        instrumentation: false, ready: missing ? 'button[aria-label*="Open session sheet"]' : 'textarea',
        events: [task('restricted-task', command, 0)],
        setup: async ({ context }) => {
          if (!ssh && !missing) await context.routeWebSocket('**/live*', socket => {
            live = socket;
            if (disconnected) return; // Keep reconnects silent; don't mask the disconnected state.
            socket.send(JSON.stringify({ type: 'sessions', sessions: [session()] }));
            socket.send(JSON.stringify({ type: 'workers', workers: [worker] }));
            socket.send(JSON.stringify({ type: 'marks', marks: {} }));
          });
          await context.route('**/api/v1/sessions/parent', route => missing
            ? route.fulfill({ status: 503, json: { error: 'Fixture session detail unavailable' } })
            : route.fulfill({ json: { session: { ...session(), ...(ssh ? { ssh: 'demo@remote' } : {}) },
              worker_name: worker.name, worker_online: ssh, queued: [] } }));
          await context.route('**/api/v1/sessions/parent/tasks/*/stop', route => { stops.push(route.request().url()); return route.fulfill({ json: {} }); });
          await context.route('**/api/v1/sessions/parent/tasks/restricted-task', route => route.fulfill({ status: 503,
            json: { error: 'Fixture snapshot unavailable' } }));
        },
      });
    diagnostics.phase('case');
    const { page } = f;
    const ring = page.getByRole('button', { name: /Open session sheet/ });
    if (ssh) {
      await page.getByRole('button', { name: 'Actions', exact: true }).click();
      assert.equal(await page.locator('.c6-pop:visible').getByRole('button', { name: 'Files', exact: true }).count(), 0,
        'SSH does not expose a Files route to the wrong machine');
      await page.keyboard.press('Escape');
      if (variant !== 'ssh-wide') await ring.click();
      assert(await page.getByRole('button', { name: 'Changes', exact: true }).isDisabled(), 'SSH keeps Changes disabled');
      assert(await page.getByRole('button', { name: 'Terminal', exact: true }).isEnabled(), 'SSH does not disable unrelated Terminal control');
    } else {
      await ring.click();
      const sheet = page.locator('[data-session-sheet]:visible');
      const stale = sheet.getByRole('status').filter({ hasText: 'known state may be stale' });
      await stale.waitFor();
      const row = sheet.locator('[data-background-job]').first();
      await row.getByRole('button', { name: command, exact: true }).waitFor();
      const directStop = row.getByRole('button', { name: `Stop ${command}`, exact: true });
      await directStop.waitFor();
      assert(await directStop.isDisabled(), 'offline/missing detail disables the direct row Stop too');
      assert.match(await directStop.getAttribute('title'), /Stop unavailable.*reconnect/);
      await directStop.evaluate(el => el.click());
      await heading(sheet, 'Session');
      assert.deepEqual(stops, [], 'disabled row Stop cannot navigate or send a request');
      await jobActions(row, sheet);
      await heading(sheet, 'Background command');
      await stale.waitFor();
      const stop = sheet.getByRole('button', { name: 'Stop this command', exact: true });
      assert(await stop.isDisabled(), 'offline/missing detail must fail closed, not pretend Stop can work');
      await stop.evaluate(el => el.click());
      assert.deepEqual(stops, [], 'native disabled control cannot submit a Stop');
      await sheet.getByRole('button', { name: 'Inspect', exact: true }).click();
      await heading(sheet, 'Inspect'); await stale.waitFor();
      await sheet.getByRole('alert').filter({ hasText: /snapshot unavailable/i }).waitFor();
      await landing(sheet);
      await sheet.getByRole('button', { name: /^Session details\b/ }).click();
      await heading(sheet, 'Session details'); await stale.waitFor();
      await back(sheet);
      await sheet.getByRole('button', { name: 'History', exact: true }).click();
      await heading(sheet, 'History'); await stale.waitFor();
      await back(sheet);
      if (!missing) {
        worker.status = 'online';
        live.send(JSON.stringify({ type: 'workers', workers: [worker] }));
        await stale.waitFor({ state: 'hidden' });
        assert(await directStop.isEnabled(), 'worker reconnect also enables the direct row Stop');
        await jobActions(row, sheet);
        assert(await stop.isEnabled(), 'worker reconnect restores the actual Stop capability');
        disconnected = true; live.close();
        await stale.waitFor();
        assert(await stop.isDisabled(), 'head/live disconnection also fails closed even with a cached online worker');
        await landing(sheet);
        assert(await directStop.isDisabled(), 'head/live disconnection also disables the direct row Stop');
        assert.deepEqual(stops, []);
      }
    }
    assert.deepEqual(f.errors, []);
    diagnostics.phase('screenshot:restriction');
    await page.screenshot({ path: path.join(shots, `${name}.png`) });
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FIRST FAILURE ${name}:`, error.stack || error);
    await diagnostics.failure(error, path.join(shots, `${name}-FAIL-${Date.now()}`));
    throw error;
  } finally { await diagnostics.finish(); }
}

async function isolation(browser, origin, engine, theme, view) {
  const name = `after-${engine}-${theme}-${view}-isolation`;
  console.log(`START ${name}`);
  const nested = child('nested-child', 'Nested child with its own stream', 0, 'working', { parent_call_id: 'nested-call' });
  const rootEvents = [
    { seq: 1000, ts: now, type: 'assistant', turn: 1000, blocks: [{ type: 'tool_use', id: 'nested-call', name: 'task',
      input_json: JSON.stringify({ description: nested.title, prompt: 'Use the child stream.' }) }], stop_reason: 'tool_use' },
    { ...spawn(nested.id, 'nested-root-task', 0, nested.title), seq: 1001, call_id: 'nested-call' },
    { ...task('nested-root-task', nested.title, 0, 'started', { kind: 'agent' }), seq: 1002 },
    { seq: 1003, ts: now, type: 'output', name: 'focus-report.html', title: 'Focus return report', mime: 'text/html', size: 42 },
  ];
  let f;
  const diagnostics = caseDiagnostics(browser, name, () => ({ errors: f?.errors,
    console: f?.consoleMessages, requests: f?.requests }));
  try {
    f = await fixture(browser, origin, views[view], theme,
      { choice: 'off', effective: 'off', os: 'reduce' }, progress => { f = progress; diagnostics.started(progress); }, { serviceWorkers: 'allow' }, {
        instrumentation: false, sessions: [nested], events: rootEvents,
        layout: { tiles: [
          { key: 2, columns: [{ key: 3, view: { view: 'session', id: 'parent' } }], cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false },
          { key: 5, columns: [{ key: 6, view: { view: 'workers' } }], cursor: 0, active: 6, used: 6, back: [], forward: [], stack: 1, minimized: false },
        ], focus: 0, next: 7, full: true },
        setup: async ({ context }) => {
          await context.addInitScript(() => { if (top === self) localStorage.setItem('c6.sub.nested-child', '1'); });
          await context.routeWebSocket('**/sessions/nested-child/stream*', socket => {
            for (const event of [
              { seq: 1, ts: now, type: 'started', connection_id: 'test', model: 'test', cwd: '/tmp', system_prompt: '', tools: [] },
              { seq: 2, ts: now, type: 'turn_started', turn: 1 },
              { seq: 3, ts: now, type: 'assistant', turn: 1, blocks: [{ type: 'text', text: 'Nested transcript uses its own event stream.' }], stop_reason: 'tool_use' },
              { ...task('child-only-task', 'Child-only command', 0), seq: 4 },
            ]) socket.send(JSON.stringify({ type: 'event', event }));
            socket.send(JSON.stringify({ type: 'synced' }));
          });
          await context.route('**/api/v1/sessions/parent/children*', route => route.fulfill({ json: [nested] }));
          await context.route('**/api/v1/sessions/nested-child', route => route.fulfill({ json: {
            session: nested, worker_name: 'Sheet fixture', worker_online: true, queued: [],
          } }));
          await context.route('**/api/v1/sessions/parent/outputs/*', route => route.fulfill({ contentType: 'text/html',
            body: '<!doctype html><h1>Focus return output</h1>' }));
        },
      });
    diagnostics.phase('case');
    const { page } = f;
    const parentTile = page.locator('[data-tile-motion="2"]');
    if (view === 'narrow') await page.addStyleTag({ content: '[data-tile-motion="2"] { width:600px !important; }' });
    const thread = parentTile.locator('#sub-nested-child');
    await thread.getByText('Nested transcript uses its own event stream.', { exact: true }).waitFor();
    await settle(page);
    assert.equal(await thread.getByRole('button', { name: /^\d+ background tasks?$/, includeHidden: true }).count(), 0,
      'nested Transcript must not inherit its parent SessionChrome/background opener');
    assert.equal(await thread.getByRole('button', { name: /Open session status details$/, includeHidden: true }).count(), 0,
      'nested Transcript must not inherit the root phone status surface either');
    const backgroundLink = parentTile.getByRole('button', { name: /^1 background task$/, includeHidden: true });
    assert.equal(await backgroundLink.count(), 1, 'exactly one root background opener in the retained DOM');
    assert.equal(await backgroundLink.isVisible(), view === 'narrow', 'phones use their status row/ring, not the desktop composer link');
    assert.equal(await parentTile.getByRole('button', { name: '1 background task. Open session status details', exact: true }).count(),
      view === 'phone-390' ? 1 : 0, 'exactly one visible root status opener on phones, no duplicate desktop surface');
    const ring = parentTile.getByRole('button', { name: /Open session sheet/ });
    await ring.click();
    const sheet = page.locator('[data-session-sheet="compact"]:visible');
    await sheet.getByRole('button', { name: /^Session details\b/ }).click();
    await heading(sheet, 'Session details');
    const retained = await sheet.getByText('Runs on', { exact: true }).elementHandle();
    await sheet.getByRole('button', { name: 'Show all', exact: true }).click();
    const outputs = parentTile.locator('[data-pane-motion]').filter({ has: page.locator('iframe') });
    await page.frameLocator('iframe').getByRole('heading', { name: 'Focus return output', exact: true }).waitFor();
    await page.keyboard.press('Control+2');
    await page.waitForFunction(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).focus === 1);
    await settle(page);
    assert(await page.locator('[data-tile-motion="5"]').isVisible());
    assert.equal(await page.locator('[data-session-sheet]:visible').count(), 0, 'hidden origin must not reopen its global sheet/popover over another tile');
    await page.keyboard.press('Control+1');
    await outputs.waitFor();
    assert.equal(await page.locator('[data-session-sheet]:visible').count(), 0, 'returning to Outputs does not prematurely consume the deferred return token');
    if (view.startsWith('phone')) {
      await outputs.locator('xpath=ancestor::*[@data-tile-surface][1]').locator('[data-tile-bar]')
        .getByRole('button', { name: 'Back to the previous pane', exact: true }).click();
    } else await outputs.getByRole('button', { name: 'Back to the list', exact: true }).click();
    await heading(sheet, 'Session details');
    assert(await retained.evaluate(el => el.isConnected), 'origin Details body survives focusing another tile');
    await page.keyboard.press('Escape');
    await sheet.waitFor({ state: 'hidden' });
    assert(await ring.evaluate(el => document.activeElement === el), 'restored sheet dismissal focuses the originating ring');
    assert.deepEqual(f.errors, []);
    diagnostics.phase('screenshot:isolation');
    await page.screenshot({ path: path.join(shots, `${name}.png`) });
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FIRST FAILURE ${name}:`, error.stack || error);
    await diagnostics.failure(error, path.join(shots, `${name}-FAIL-${Date.now()}`));
    throw error;
  } finally { await diagnostics.finish(); }
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const root = path.resolve(process.env.ROOT || path.join(__dirname, '../dist'));
  assert(fs.existsSync(path.join(root, 'index.html')), `Build is not ready: ${root}`);
  fs.writeFileSync(path.join(shots, `${phase}-artifact.json`), JSON.stringify({ root,
    files: fs.readdirSync(root).filter(file => /\.(wasm|js|css)$/.test(file)).map(file => {
      const data = fs.readFileSync(path.join(root, file));
      return { file, bytes: data.length, sha256: crypto.createHash('sha256').update(data).digest('hex') };
    }) }, null, 2));
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const engine = process.env.ENGINE || 'both';
  assert(['chromium', 'webkit', 'both'].includes(engine), `Unknown ENGINE=${engine}`);
  const { server, origin } = await serve();
  let failures = 0, casesRun = 0;
  try {
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      const browser = await launchBrowser(playwright, name);
      try {
        if (process.env.EXTRAS !== 'only') for (const theme of (process.env.THEMES || 'light,dark').split(','))
          for (const view of (process.env.VIEWS || Object.keys(views).join(',')).split(',')) {
            assert(views[view], `Unknown VIEWS entry ${view}`);
            casesRun++;
            // Locator/navigation operations already have the fixture deadline.
            // Do not race the whole case against that same deadline: it can
            // close the browser before the failing operation saves evidence.
            try { await check(browser, origin, name, theme, view); }
            catch (error) { failures++; console.error(`FAIL ${name}/${theme}/${view}`, error.stack || error); }
          }
        if (!before && process.env.EXTRAS !== '0')
          for (const theme of (process.env.THEMES || 'light,dark').split(','))
            for (const view of ['narrow', 'phone-390'].filter(view => !process.env.VIEWS || process.env.VIEWS.split(',').includes(view))) {
              casesRun++;
              try { await isolation(browser, origin, name, theme, view); }
              catch (error) { failures++; console.error(`FAIL ${name}/${theme}/${view}/isolation`, error.stack || error); }
            }
        if (!before && process.env.EXTRAS !== '0')
          for (const variant of ['ssh-wide', 'ssh-phone', 'worker-offline', 'missing-detail']) {
            casesRun++;
            try { await restrictions(browser, origin, name, variant); }
            catch (error) { failures++; console.error(`FAIL ${name}/${variant}`, error.stack || error); }
          }
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
  assert(casesRun > 0, 'No session-sheet cases selected');
  console.log(`COHORT ${casesRun - failures}/${casesRun} passed`);
  assert.equal(failures, 0, `${failures} session-sheet case(s) failed; preserve first errors and screenshots`);
}
main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
