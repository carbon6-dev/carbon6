#!/usr/bin/env node
'use strict';

// Exercise production caller-owned resources; all traffic stays on the private
// loopback fixture. No live head, resource replacement or cancellation shim.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const output = process.env.EVIDENCE || process.env.SCREENSHOTS || '/tmp/resource-lifecycle';
const choose = (name, values, fallback = values) => {
  const selected = process.env[name] ? process.env[name].split(',') : fallback;
  assert(selected.length && selected.every(value => values.includes(value)), `Invalid ${name}: ${selected}`);
  return selected;
};
const engines = choose('ENGINE', ['chromium', 'webkit', 'both'], ['both']).flatMap(e => e === 'both' ? ['chromium', 'webkit'] : [e]);
const themes = choose('THEME', ['light', 'dark']);
const cases = choose('RESOURCE_CASE', ['chat', 'stale', 'work', 'projects']);
const layout = view => ({ tiles: [{ key: 2, columns: [{ key: 3, view }], cursor: 0,
  active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 4, full: false });
const savedMessage = text => ({ id: 'saved-one', text, created_at: 1,
  meta: { workspace: 'fixture-workspace', channel: 'fixture-channel', channel_name: 'resource-fixture',
    ts: '1.0', user: 'fixture-user', user_name: 'Fixture User' } });
const rpcResponse = (page, method, done) => page.waitForResponse(response => {
  const request = response.request();
  if (new URL(response.url()).pathname !== '/api/v1/memex/rpc' || request.method() !== 'POST') return false;
  const body = request.postDataJSON();
  return body.method === method && (done === undefined || body.params.done === done);
});
async function clickRpc(page, label, method, done) {
  const response = rpcResponse(page, method, done);
  await page.getByRole('button', { name: label, exact: true }).click();
  await (await response).finished();
  await settle(page);
}

const holdNext = state => new Promise(resolve => { state.onHeld = resolve; state.hold = true; });

async function chat(f, state, stale) {
  const { page } = f;
  const step = name => { f.phase = name; console.log(`  ${name}`); };
  if (stale) {
    step('held false -> Completed: observe baseline serial fetch policy');
    // The baseline framework serializes fetches within this owner: the true
    // request starts only after false finishes. Preserve that behavior rather
    // than claiming stronger cancellation/concurrent stale-result guarantees.
    // Wait for the route itself to hold the request: a request event can
    // reach the test before its route handler has run.
    const requested = holdNext(state);
    await page.getByRole('button', { name: 'Saved', exact: true }).click();
    await requested;
    assert(state.held, 'old false response is held by the private route');
    await page.getByRole('button', { name: 'Completed', exact: true }).click();
    await settle(page);
    assert.equal(state.calls.filter(call => call.method === 'memex.chat.saved' && call.params.done).length, 0,
      'baseline serial policy: new dependency is queued behind the pending fetch');
    const oldResponse = rpcResponse(page, 'memex.chat.saved', false);
    const completed = rpcResponse(page, 'memex.chat.saved', true);
    await state.held.fulfill({ status: 500, json: { error: 'STALE response must not replace Completed' } });
    state.held = null;
    await (await oldResponse).finished();
    await (await completed).finished();
    await settle(page);
    assert.equal(await page.getByText('STALE response must not replace Completed', { exact: true }).count(), 0,
      'queued dependency recovers to Completed after the old error; no stronger transient/cancellation claim');
    await page.getByText('Nothing completed yet.', { exact: true }).waitFor();
    step('hold old owner false request across unmount/remount');
    const nextFalse = holdNext(state);
    await page.getByRole('button', { name: 'In progress', exact: true }).click();
    await nextFalse;
    assert(state.held, 'old owner has another held false query');
  } else {
    step('Saved false, action tick refetch, true, error and recovery');
    await clickRpc(page, 'Saved', 'memex.chat.saved', false);
    await page.getByText('Initial saved resource', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'In progress', exact: true }).evaluate(el => { window.__savedTab = el; });
    const refreshed = rpcResponse(page, 'memex.chat.saved', false);
    await clickRpc(page, 'Mark complete', 'memex.chat.save');
    await (await refreshed).finished();
    await page.getByText('Tick-refetched saved resource', { exact: true }).waitFor();
    assert.deepEqual(state.saves, [{ id: 'saved-one', done: true }]);
    assert(await page.evaluate(() => window.__savedTab.isConnected), 'tick refetch does not remount Saved');
    await clickRpc(page, 'Completed', 'memex.chat.saved', true);
    await page.getByText('Nothing completed yet.', { exact: true }).waitFor();
    state.reject = true;
    await clickRpc(page, 'In progress', 'memex.chat.saved', false);
    await page.getByText('Resource lifecycle fixture error', { exact: true }).waitFor();
    await clickRpc(page, 'Completed', 'memex.chat.saved', true);
    await page.getByText('Nothing completed yet.', { exact: true }).waitFor();
    assert.equal(await page.getByText('Resource lifecycle fixture error', { exact: true }).count(), 0);
    await clickRpc(page, 'In progress', 'memex.chat.saved', false);
    await page.getByText('Tick-refetched saved resource', { exact: true }).waitFor();
  }
  // A different pane disposes the Saved owner. A new Saved starts at false,
  // fetches fresh data, and cannot reuse the previous owner's result/error.
  step('remount fetches fresh false independently');
  await page.getByRole('button', { name: 'In progress', exact: true }).evaluate(el => { window.__oldSavedTab = el; });
  await clickRpc(page, 'Unreads', 'memex.chat.unreads');
  await page.getByText("You're all caught up.", { exact: true }).waitFor();
  assert(await page.evaluate(() => !window.__oldSavedTab.isConnected), 'old Saved owner DOM detached');
  state.remount = true;
  const before = state.calls.filter(call => call.method === 'memex.chat.saved').length;
  await clickRpc(page, 'Saved', 'memex.chat.saved', false);
  await page.getByText('Fresh remounted saved resource', { exact: true }).waitFor();
  assert.equal(state.calls.filter(call => call.method === 'memex.chat.saved').length, before + 1);
  if (stale) {
    step('release disposed owner response after fresh owner rendered');
    const disposed = rpcResponse(page, 'memex.chat.saved', false);
    await state.held.fulfill({ status: 500, json: { error: 'Disposed owner must not replace fresh Saved' } });
    state.held = null;
    await (await disposed).finished();
    await settle(page);
    assert.equal(await page.getByText('Disposed owner must not replace fresh Saved', { exact: true }).count(), 0);
    await page.getByText('Fresh remounted saved resource', { exact: true }).waitFor();
  }
  for (const method of ['channels', 'people', 'sections', 'threads'])
    assert(state.calls.some(call => call.method === `memex.chat.${method}`), `startup ${method} was mocked`);
  const saved = state.calls.filter(call => call.method === 'memex.chat.saved');
  assert(saved.some(call => call.params.done === false) && saved.some(call => call.params.done === true));
  for (const call of saved) assert.deepEqual(Object.keys(call.params), ['done']);
}

async function closed(f, state, kind) {
  const { page } = f;
  const label = kind === 'work' ? 'Show closed work' : 'Show done and dropped projects';
  const toggle = page.getByRole('switch', { name: label, exact: true });
  assert.equal(await toggle.getAttribute('aria-checked'), 'false');
  assert.equal(state.closed, 0, 'closed=false does not fetch the closed list');
  const turnOn = async () => {
    const response = page.waitForResponse(response => {
      const url = new URL(response.url());
      return url.pathname === `/api/v1/${kind}` && url.searchParams.get('closed') === 'true';
    });
    await toggle.click();
    await (await response).finished();
    await settle(page);
    assert.equal(await toggle.getAttribute('aria-checked'), 'true');
  };
  await turnOn();
  assert.equal(state.closed, 1);
  await toggle.click();
  await settle(page);
  assert.equal(await toggle.getAttribute('aria-checked'), 'false');
  assert.equal(state.closed, 1, 'turning off clears locally without another closed query');
  await turnOn();
  assert.equal(state.closed, 2, 'turning on again reruns the tracked resource');
  f.phase = 'closed-list cold reload';
  await f.reload({ waitUntil: 'domcontentloaded' });
  await toggle.waitFor();
  await settle(page);
  assert.equal(await toggle.getAttribute('aria-checked'), 'false', 'fresh owner starts closed=false');
  assert.equal(state.closed, 2, 'fresh disabled owner does not fetch closed data');
}

async function check(browser, origin, engine, theme, kind) {
  let f;
  const state = { calls: [], saves: [], closed: 0, reject: false, hold: false, held: null, remount: false };
  const label = `${engine}-${theme}-${kind}`;
  const directory = path.join(output, label);
  fs.mkdirSync(directory, { recursive: true });
  const isChat = kind === 'chat' || kind === 'stale';
  console.log(`START ${label}`);
  try {
    await within(label, async () => {
      f = await fixture(browser, origin, { width: 1440, height: 1000 }, theme,
        { choice: 'off', effective: 'off', os: 'no-preference' }, progress => { f = progress; }, {}, {
          path: isChat ? '/chat' : `/${kind}`, instrumentation: false,
          ready: isChat ? 'input[placeholder="Filter conversations"]' : '[role="switch"]',
          layout: layout(isChat ? { view: 'chat' } : { view: kind, id: '' }),
          setup: async ({ context }) => {
            await context.route('**/api/v1/memex/rpc**', async route => {
              const request = route.request();
              if (new URL(request.url()).origin !== origin) return route.fallback();
              assert.equal(request.method(), 'POST');
              const body = request.postDataJSON();
              if (!body.method.startsWith('memex.chat.')) return route.fallback();
              state.calls.push(body);
              let json = [];
              switch (body.method) {
                case 'memex.chat.channels': case 'memex.chat.people': case 'memex.chat.threads':
                case 'memex.chat.unreads': break;
                case 'memex.chat.sections': json = {}; break;
                case 'memex.chat.save': state.saves.push(body.params); json = {}; break;
                case 'memex.chat.saved':
                  assert.equal(typeof body.params.done, 'boolean');
                  if (state.hold && !body.params.done) { state.hold = false; state.held = route; state.onHeld(); return; }
                  if (state.reject) { state.reject = false; return route.fulfill({ status: 500,
                    json: { error: 'Resource lifecycle fixture error' } }); }
                  json = body.params.done ? [] : [savedMessage(state.remount ? 'Fresh remounted saved resource' :
                    state.saves.length ? 'Tick-refetched saved resource' : 'Initial saved resource')];
                  break;
                default: throw new Error(`Unmocked chat RPC: ${body.method}`);
              }
              await route.fulfill({ json });
            });
            if (!isChat) await context.route(`**/api/v1/${kind}**`, async route => {
              const request = route.request(), url = new URL(request.url());
              if (url.origin !== origin || url.pathname !== `/api/v1/${kind}`) return route.fallback();
              assert.equal(request.method(), 'GET');
              if (url.searchParams.get('closed') === 'true') state.closed++;
              state.calls.push({ method: 'GET', url: url.pathname + url.search });
              await route.fulfill({ json: [] });
            });
          },
        });
      if (isChat) await chat(f, state, kind === 'stale');
      else await closed(f, state, kind);
      assert.deepEqual(f.errors, [], 'no external traffic, UI crashes or runtime errors');
    });
    console.log(`PASS ${label}`);
    return true;
  } catch (error) {
    console.error(`FAIL ${label}: ${error.stack || error}`);
    if (f?.page) await f.page.screenshot({ path: path.join(directory, 'failure.png') }).catch(() => {});
    return false;
  } finally {
    fs.writeFileSync(path.join(directory, 'evidence.json'), JSON.stringify({ calls: state.calls,
      saves: state.saves, closed: state.closed, phase: f?.phase,
      errors: f?.errors, console: f?.consoleMessages, requests: f?.requests,
      navigation: f?.navigation?.ledger, rawPageErrors: f?.navigation?.rawPageErrors }, null, 2));
    await f?.context.close();
  }
}

(async () => {
  const { server, origin } = await serve();
  let passed = 0, failed = 0;
  try {
    for (const engine of engines) {
      const browser = await launchBrowser(playwright, engine);
      try {
        for (const theme of themes) for (const kind of cases) {
          if (await check(browser, origin, engine, theme, kind)) passed++; else failed++;
        }
      } finally { await browser.close(); }
    }
  } finally { server.close(); }
  console.log(`COHORT resource-lifecycle passed=${passed} failed=${failed} total=${passed + failed}; no retries`);
  if (failed) process.exitCode = 1;
})().catch(error => { console.error(error); process.exitCode = 1; });
