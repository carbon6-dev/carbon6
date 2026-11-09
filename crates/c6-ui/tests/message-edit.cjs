#!/usr/bin/env node
'use strict';

// Built production WASM; each case owns loopback HTTP/WS and browser storage.
// No real head, model, settings writes, or cancellation timing sleeps.
const assert = require('node:assert/strict');
const { serve, fixture, settle, within, launchBrowser, session } = require('./motion.cjs');
const ownText = 'Edit only this one message, not the later queued message';
const resetError = 'Claude Code history reset failed; input is blocked. Retry rewind before sending another message.';
const deferred = () => {
  let resolve;
  const promise = new Promise(r => { resolve = r; });
  return { promise, resolve };
};

async function check(browser, origin, phone, outcome) {
  let progress;
  const rewindRelease = deferred(), calls = [];
  const rewinding = outcome.startsWith('rewind');
  const replaying = outcome === 'rewind-replay';
  const retrying = outcome.endsWith('error') || replaying;
  let streamSocket, streamSequence = 100, connected = deferred();
  let sends = 0, rewinds = 0, firstRewindRequest;
  const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
    'dark', { choice: 'off', effective: 'off', os: 'reduce' }, p => { progress = p; }, {}, {
      instrumentation: false,
      events: [{ seq: 72, ts: 72, type: 'user', command_id: 'edit-one', text: ownText }],
      setup: async ({ page, context }) => {
        if (replaying) await context.routeWebSocket(/\/sessions\/parent\/stream(?:\?|$)/, socket => {
          const first = !streamSocket;
          streamSocket = socket;
          if (first) {
            socket.send(JSON.stringify({ type: 'event', event: { seq: 1, ts: 1, type: 'started', connection_id: 'test', model: 'test', cwd: '/tmp', system_prompt: '', tools: [], context_window: 128000 } }));
            socket.send(JSON.stringify({ type: 'event', event: { seq: 72, ts: 72, type: 'user', command_id: 'edit-one', text: ownText } }));
            socket.send(JSON.stringify({ type: 'synced' }));
          } else connected.resolve();
        });
        await context.routeWebSocket(/\/live(?:\?|$)/, socket => {
          if (new URL(socket.url()).host !== new URL(origin).host) {
            progress.errors.push(`Blocked external WebSocket: ${socket.url()}`);
            socket.close();
            return;
          }
          socket.send(JSON.stringify({ type: 'sessions', sessions: [session()] }));
          socket.send(JSON.stringify({ type: 'workers', workers: [{
            id: session().worker_id, name: 'Edit fixture', hostname: 'fixture', os: 'linux', arch: 'x86_64',
            version: 'test', status: 'online', enrolled_at: 1, last_seen: 1, connected_since: 1, rtt_us: 0,
            capabilities: ['queue_ops', 'send_now'],
          }] }));
          socket.send(JSON.stringify({ type: 'marks', marks: {} }));
        });
        // Every transcript event is already owned. This is a trap for an
        // unwanted request, never a fictitious pre-ownership cancellation.
        await page.route('**/api/v1/sessions/parent/messages/*/take-back', async route => {
          calls.push('unexpected-take-back');
          await route.fulfill({ status: 409, json: { error: 'The agent has this message' } });
        });
        await page.route('**/api/v1/sessions/parent/messages', async route => {
          calls.push({ send: route.request().postDataJSON().text });
          sends++;
          // An acknowledged error must keep the editor/draft for explicit retry.
          if (outcome === 'correction-send-error' && sends === 1) return route.fulfill({ status: 503, json: { error: 'Fixture send unavailable' } });
          await route.fulfill({ json: { command_id: `sent-${sends}`, text: route.request().postDataJSON().text, created_at: 1, rejected: null } });
        });
        await page.route('**/api/v1/sessions/parent/rewind', async route => {
          calls.push({ rewind: route.request().postDataJSON() });
          const attempt = ++rewinds;
          if (attempt === 1) firstRewindRequest = route.request();
          if (attempt === 1 && ['rewind-early', 'rewind-reset-error', 'rewind-api-error'].includes(outcome)) await rewindRelease.promise;
          if (attempt === 1 && outcome === 'rewind-api-error') {
            return route.fulfill({ status: 503, json: { error: 'Fixture rewind unavailable' } });
          }
          return route.fulfill({ status: outcome === 'rewind' ? 204 : 202 });
        });
        await page.route('**/api/v1/sessions/parent/queue/later-one/pull', route => {
          calls.push('queue-pull');
          return route.fulfill({ json: queued() });
        });
      },
    }).catch(async error => { await progress?.context.close(); throw error; });
  try {
    const { page } = f;
    const phase = label => {
      f.phase = label;
      if (process.env.DEBUG_FIXTURE) console.log(`  EDIT ${label}`);
    };
    const send = event => {
      if (!replaying) return f.send(event);
      const seq = streamSequence++;
      streamSocket.send(JSON.stringify({ type: 'event', event: { seq, ts: seq, ...event } }));
      return seq;
    };
    const disconnect = async () => {
      connected = deferred();
      streamSocket.close();
      await page.getByText('Session stream disconnected. Reconnecting…', { exact: true }).waitFor();
      await connected.promise;
    };
    const original = page.locator('#parent-e72');
    await original.scrollIntoViewIfNeeded();
    const originalNode = await original.elementHandle();
    const originalIcon = await original.locator('svg').first().elementHandle();
    if (phone) {
      await original.getByRole('button', { name: 'Message actions', exact: true }).tap();
      const sheet = page.getByRole('dialog', { name: 'Message actions', exact: true });
      await sheet.getByRole('button', { name: 'Edit', exact: true }).tap();
      await sheet.waitFor({ state: 'hidden' });
    } else {
      await original.hover();
      await original.getByRole('button', { name: 'Edit', exact: true }).click();
    }
    const panel = page.locator('[data-message-edit]');
    await panel.getByText('The agent has this message', { exact: true }).waitFor();
    assert.equal(await panel.locator('textarea').count(), 0, 'transcript Edit never opens a pre-ownership editor');
    assert.deepEqual(await panel.getByRole('button').allTextContents(), ['Send correction', 'Rewind…', 'Close'], 'exact taken choices immediately');
    assert.deepEqual(calls, [], 'transcript Edit never cancels, pulls, sends or rewinds');
    assert(!f.requests.some(r => /\/queue\//.test(r)), 'single Edit never calls an unmocked queue operation');
    send({ type: 'assistant', turn: 74, blocks: [{ type: 'text', text: 'Agent received the original' }], stop_reason: 'end_turn' });
    await page.getByText('Agent received the original', { exact: true }).waitFor();
    assert.equal(await panel.locator('textarea').count(), 0, 'live reply keeps the draft behind explicit taken choices');
    assert.deepEqual(calls, [], 'live delivery does not cancel, send or rewind');
    if (outcome === 'close') {
      await panel.getByRole('button', { name: 'Close', exact: true }).click();
      await panel.waitFor({ state: 'hidden' });
      assert.equal(await original.getByText(ownText, { exact: true }).count(), 1);
      assert.deepEqual(calls, [], 'Close is not cancellation or Send');
      assert.deepEqual(f.errors, []);
      return;
    }
    if (rewinding) {
      phase('rewind confirmation and stale event');
      await panel.getByRole('button', { name: 'Rewind…', exact: true }).click();
      await panel.getByText(/Rewind the conversation to before this message\?/).waitFor();
      assert.deepEqual(calls, [], 'Rewind… opens an explicit confirmation');
      assert.equal(await panel.getByRole('checkbox').isChecked(), false);
      await panel.getByRole('button', { name: 'Cancel', exact: true }).click();
      assert.deepEqual(await panel.getByRole('button').allTextContents(), ['Send correction', 'Rewind…', 'Close'], 'Cancel keeps the draft and taken choices');
      await panel.getByRole('button', { name: 'Rewind…', exact: true }).click();
      // A matching durable result already in the feed is NOT this click's
      // confirmation. It must also survive mounting the panel before click.
      const stale = send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
      await page.locator(`#parent-e${stale}`).waitFor();
      assert.equal(await panel.locator('textarea').count(), 0, 'stale matching Rewound cannot open the editor');
      if (replaying) {
        await disconnect();
        assert.equal(await panel.getByRole('button', { name: 'Rewind', exact: true }).isDisabled(), true, 'no click during disconnected replay');
        const replay = send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
        assert.deepEqual(calls, [], 'replay cannot enqueue a rewind');
        streamSocket.send(JSON.stringify({ type: 'synced' }));
        await page.locator(`#parent-e${replay}`).waitFor();
        assert.equal(await panel.locator('textarea').count(), 0, 'replay before click cannot open the editor');
      }
      phase('rewind POST and worker result');
      const response = page.waitForResponse(r => r.request() === firstRewindRequest);
      await panel.getByRole('button', { name: 'Rewind', exact: true }).click();
      if (outcome === 'rewind-reset-error') {
        const early = send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
        await page.locator(`#parent-e${early}`).waitFor();
        send({ type: 'error', message: resetError });
        await panel.getByText(resetError).waitFor();
        await panel.getByText(/Your draft is kept; nothing was resent/).waitFor();
        assert.equal(await panel.locator('textarea').count(), 0, 'worker Error keeps the draft behind confirmation');
        assert.equal(sends, 0, 'worker Error never resends');
        // Retry even while the previous HTTP response is held. Its eventual
        // acceptance must not overwrite this attempt's pending state.
        const retryResponse = page.waitForResponse(r => r.url().endsWith('/parent/rewind'));
        await panel.getByRole('button', { name: 'Rewind', exact: true }).click();
        await retryResponse;
        await panel.getByText(resetError).waitFor({ state: 'hidden' });
        rewindRelease.resolve();
      } else if (outcome === 'rewind-early' || outcome === 'rewind-api-error') {
        const early = send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
        await page.locator(`#parent-e${early}`).waitFor();
        assert.equal(await panel.locator('textarea').count(), 0, 'early durable event is retained while HTTP is pending');
        rewindRelease.resolve();
      }
      phase('rewind HTTP response');
      // Playwright's routed bodyless 204 need not emit requestfinished.
      // Observe response headers, then UI/event barriers, not finished().
      await response;
      if (outcome === 'rewind-api-error') {
        await panel.getByText('Fixture rewind unavailable').waitFor();
        assert.equal(await panel.locator('textarea').count(), 0, 'API error keeps the draft behind confirmation');
        assert.equal(sends, 0, 'API error never resends');
        const retryResponse = page.waitForResponse(r => r.url().endsWith('/parent/rewind'));
        await panel.getByRole('button', { name: 'Rewind', exact: true }).click();
        await retryResponse;
        await panel.getByText('Fixture rewind unavailable').waitFor({ state: 'hidden' });
      }
      if (replaying) {
        await disconnect();
        await panel.getByText(/Rewind was not confirmed.*Your draft is kept; nothing was resent/).waitFor();
        assert.equal(await panel.getByRole('button', { name: 'Rewind', exact: true }).isDisabled(), true);
        const replay = send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
        streamSocket.send(JSON.stringify({ type: 'synced' }));
        await page.locator(`#parent-e${replay}`).waitFor();
        assert.equal(await panel.locator('textarea').count(), 0, 'matching replay after accepted HTTP cannot confirm an interrupted attempt');
        assert.equal(rewinds, 1, 'reconnect never retries implicitly');
        const retryResponse = page.waitForResponse(r => r.url().endsWith('/parent/rewind'));
        await panel.getByRole('button', { name: 'Rewind', exact: true }).click();
        await retryResponse;
      }
      if (outcome !== 'rewind-early') {
        phase('rewind accepted without confirmation');
        await settle(page);
        assert.equal(await panel.locator('textarea').count(), 0, 'accepted HTTP alone must not open the editor (202 or 204)');
        assert.equal(await panel.getByRole('button', { name: 'Rewind', exact: true }).isDisabled(), true, 'wait for worker confirmation');
        const mismatch = send({ type: 'rewound', to_seq: 71, restored_files: false, restore_error: '' });
        await page.locator(`#parent-e${mismatch}`).waitFor();
        assert.equal(await panel.locator('textarea').count(), 0, 'fresh mismatched Rewound is not success');
        // Replay the stale match after HTTP as well; it still cannot confirm.
        send({ seq: stale, type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
        const barrier = send({ type: 'assistant', turn: 74, blocks: [{ type: 'text', text: 'Stale replay processed' }], stop_reason: 'end_turn' });
        await page.locator(`#parent-e${barrier}`).waitFor();
        assert.equal(await panel.locator('textarea').count(), 0, 'replayed stale matching Rewound is not success');
        assert.equal(sends, 0, 'HTTP acceptance/stale/mismatched events never resend');
        send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
      }
      phase('rewind confirmed editor');
    } else {
      await panel.getByRole('button', { name: 'Send correction', exact: true }).click();
    }
    const editor = panel.getByRole('textbox', { name: 'Edit message', exact: true });
    await editor.waitFor();
    assert(await original.evaluate((el, kept) => el === kept, originalNode), 'Edit/live delivery retains the original row');
    assert(await original.locator('svg').first().evaluate((el, kept) => el === kept, originalIcon), 'Edit/live delivery does not reparse its icon');
    const editorNode = await editor.elementHandle();
    assert.equal(await panel.getByRole('button', { name: 'Add a message', exact: true }).count(), 0, 'single edit cannot add later queue rows');
    assert.equal(await panel.getByTitle('Move up', { exact: true }).count(), 0, 'single edit has no queue reordering controls');
    assert.equal(await panel.getByTitle('Drop this message', { exact: true }).count(), 0, 'single edit cannot drop another message');
    assert.equal(await editor.inputValue(), ownText, 'draft survives response/live delivery/retry/rewind choices');
    await panel.scrollIntoViewIfNeeded();
    const bounds = await panel.boundingBox();
    assert(bounds.x >= 0 && bounds.x + bounds.width <= f.viewport.width, 'editor fits the reading column on desktop and phone');
    if (rewinding) {
      assert.deepEqual(calls, Array.from({ length: retrying ? 2 : 1 }, () =>
        ({ rewind: { to_seq: 72, restore_files: false } })), 'explicit rewind and retry alone do not resend');
      assert.doesNotMatch(await panel.getByRole('status').textContent(), /taken back/i,
        'an explicit rewind must not claim an inbox withdrawal');
    }
    const replacement = `Replacement ${phone ? 'phone' : 'desktop'} ${outcome}`;
    await editor.fill(replacement);
    if (outcome === 'correction-discard') {
      await panel.getByRole('button', { name: 'Discard draft', exact: true }).press('Enter');
      await panel.waitFor({ state: 'hidden' });
      assert.deepEqual(calls, [], 'Enter on Discard never bubbles into Send or cancellation');
      assert.deepEqual(f.errors, []);
      return;
    }
    if (outcome === 'correction') await editor.press('Control+Enter');
    else await panel.getByRole('button', { name: 'Send', exact: true }).click();
    if (outcome === 'correction-send-error') {
      await panel.getByText('Fixture send unavailable', { exact: true }).waitFor();
      assert.equal(await editor.inputValue(), replacement, 'failed ordinary Send keeps the edited draft');
      assert(await editor.evaluate((el, kept) => el === kept, editorNode), 'failed Send retains the same editing textarea');
      await panel.getByRole('button', { name: 'Send', exact: true }).click();
      assert.equal(await page.getByText('Agent received the original', { exact: true }).count(), 1, 'correction keeps the agent reply');
    }
    await panel.waitFor({ state: 'hidden' });
    await settle(page);
    assert.equal(calls.filter(c => c.send).length, outcome === 'correction-send-error' ? 2 : 1);
    assert(calls.filter(c => c.send).every(c => c.send === replacement));
    assert.equal(calls.filter(c => c.rewind).length, rewinding ? retrying ? 2 : 1 : 0, 'ordinary Send never rewinds');
    assert(!f.requests.some(r => /\/queue\//.test(r)), 'single Edit/Send never alters another queued input');
    assert(!calls.includes('unexpected-take-back'), 'no transcript action ever calls take-back');
    if (outcome === 'correction') {
      // The shared send form's original bulk queue mode stays separate.
      f.stream({ type: 'queue', queued: queued() });
      await page.getByText('First later waiting message', { exact: true }).waitFor();
      await page.getByRole('button', { name: 'Edit queue…', exact: true }).first().click();
      const add = page.getByRole('button', { name: 'Add a message', exact: true });
      await add.waitFor();
      const bulk = add.locator('..').locator('..');
      assert.match(await bulk.textContent(), /Edit the queued messages, then send them again/);
      const texts = bulk.getByRole('textbox', { name: 'Queued message', exact: true });
      await texts.nth(1).waitFor();
      f.stream({ type: 'queue', queued: [] });
      assert.equal(await texts.count(), 2);
      assert.equal(await bulk.getByTitle('Move up', { exact: true }).count(), 2);
      assert.equal(await bulk.getByTitle('Move down', { exact: true }).count(), 2);
      const movedArea = await texts.nth(1).elementHandle();
      const movedIcon = await bulk.getByTitle('Move up', { exact: true }).nth(1).locator('svg').elementHandle();
      await bulk.getByTitle('Move up', { exact: true }).nth(1).click();
      await page.waitForFunction(el => el.querySelector('textarea').value === 'Second later waiting message', await bulk.elementHandle());
      assert(await texts.first().evaluate((el, kept) => el === kept, movedArea), 'bulk reorder retains its textarea');
      assert(await bulk.getByTitle('Move up', { exact: true }).first().locator('svg').evaluate((el, kept) => el === kept, movedIcon),
        'shared local action view retains its SVG while rows move');
      await bulk.getByRole('button', { name: 'Add a message', exact: true }).click();
      await texts.nth(2).waitFor();
      await bulk.getByTitle('Drop this message', { exact: true }).nth(2).click();
      await texts.nth(2).waitFor({ state: 'hidden' });
      await bulk.getByRole('button', { name: 'Discard', exact: true }).click();
      await bulk.waitFor({ state: 'hidden' });
      assert.equal(calls.filter(c => c === 'queue-pull').length, 1, 'only deliberate bulk queue Edit pulls its tail');
      assert.equal(calls.filter(c => c.send).length, 1, 'bulk draft manipulation/discard never sends');
    }
    assert(!f.requests.some(r => /^(PUT|POST|DELETE) .*\/settings\//.test(r)), 'no settings writes');
    assert.deepEqual(f.errors, []);
  } catch (error) {
    console.error('EDIT DIAGNOSTIC', JSON.stringify({ phase: f.phase, calls, errors: f.errors,
      console: f.consoleMessages, requests: f.requests.slice(-12) }));
    throw error;
  } finally { rewindRelease.resolve(); await f.context.close(); }
}

function queued() {
  return ['First', 'Second'].map((name, n) => ({ command_id: n ? 'later-two' : 'later-one',
    text: `${name} later waiting message`, created_at: n + 1, rejected: null, with_agent: false }));
}

async function checkQueued(browser, origin, phone, outcome) {
  let progress, cancelRequest, liveSocket;
  const cancelStarted = deferred(), cancelRelease = deferred();
  const mutations = [], feedReads = [], calls = { takeBack: [], send: [], rewind: [] };
  const target = { command_id: 'queued-single', text: 'Original single queued draft; leave later messages alone',
    created_at: 1, rejected: null, with_agent: false };
  const taken = outcome === 'queued-taken-race';
  const unconfirmed = outcome === 'queued-unconfirmed' || outcome === 'queued-pending';
  const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
    'dark', { choice: 'off', effective: 'off', os: 'reduce' }, p => { progress = p; }, {}, {
      instrumentation: false,
      setup: async ({ page, context }) => {
        page.on('request', request => {
          const path = new URL(request.url()).pathname;
          // RPC uses POST even for reads. Validate every body below rather
          // than calling automatic sidebar feed discovery an edit mutation,
          // or blanket-allowing arbitrary memex RPC operations.
          if (request.method() === 'POST' && path === '/api/v1/memex/rpc') {
            feedReads.push(request.postDataJSON());
            return;
          }
          // Session visibility bookkeeping is automatic, not an edit action.
          if (/^(POST|PUT|DELETE)$/.test(request.method()) && path.startsWith('/api/') &&
              !/^\/api\/v1\/sessions\/parent\/(seen|viewed|catchup)$/.test(path)) {
            mutations.push({ method: request.method(), path });
          }
        });
        await context.routeWebSocket(/\/live(?:\?|$)/, socket => {
          if (new URL(socket.url()).host !== new URL(origin).host) {
            progress.errors.push(`Blocked external WebSocket: ${socket.url()}`);
            socket.close();
            return;
          }
          liveSocket = socket;
          socket.send(JSON.stringify({ type: 'sessions', sessions: [session()] }));
          socket.send(JSON.stringify({ type: 'workers', workers: [{
            id: session().worker_id, name: 'Queued edit fixture', hostname: 'fixture', os: 'linux', arch: 'x86_64',
            version: 'test', status: 'online', enrolled_at: 1, last_seen: 1, connected_since: 1, rtt_us: 0,
            capabilities: ['queue_ops', 'send_now'],
          }] }));
          socket.send(JSON.stringify({ type: 'marks', marks: {} }));
        });
        await page.route('**/api/v1/sessions/parent/messages/*/take-back', async route => {
          cancelRequest = route.request();
          calls.takeBack.push(new URL(cancelRequest.url()).pathname);
          cancelStarted.resolve();
          await cancelRelease.promise;
          if (taken) return route.fulfill({ status: 409, json: { error: 'The agent has this message' } });
          if (outcome === 'queued-pending') return route.fulfill({ status: 409, json: { error: 'The agent has this message' } });
          if (unconfirmed) return route.fulfill({ status: 503, json: { error: 'Fixture cancellation unavailable' } });
          return route.fulfill({ status: 204 });
        });
        await page.route('**/api/v1/sessions/parent/messages', route => {
          calls.send.push(route.request().postDataJSON());
          if (unconfirmed && calls.send.length === 1) return route.fulfill({ status: 503, json: { error: 'Fixture queued correction send unavailable' } });
          return route.fulfill({ json: { command_id: `queued-edit-send-${calls.send.length}`,
            text: route.request().postDataJSON().text, created_at: 2, rejected: null } });
        });
        await page.route('**/api/v1/sessions/parent/rewind', route => {
          calls.rewind.push(route.request().postDataJSON());
          return route.fulfill({ status: 202 });
        });
      },
    }).catch(async error => { await progress?.context.close(); throw error; });
  try {
    const { page } = f;
    const laterRemain = async () => {
      for (const row of queued()) assert.equal(await page.getByText(row.text, { exact: true }).count(), 1, 'later queued inputs stay visible and unchanged');
      assert(!mutations.some(r => /\/queue\//.test(r.path)), 'single queued Edit never pulls, deletes, or sends-now any queued tail');
      assert(!f.requests.some(r => /\/queue\//.test(r)), 'single queued Edit never calls an unmocked queue endpoint');
      assert.deepEqual(calls.takeBack, ['/api/v1/sessions/parent/messages/queued-single/take-back'], 'only the selected command is canceled, exactly once');
    };
    f.phase = 'queued single Edit: checking before worker confirmation';
    f.stream({ type: 'queue', queued: [target, ...queued()] });
    const bubble = page.getByText(target.text, { exact: true });
    await bubble.waitFor();
    if (outcome === 'queued-ready') {
      const waiting = page.locator('[data-waiting-messages]');
      liveSocket.send(JSON.stringify({ type: 'sessions', sessions: [session('parent', 'working')] }));
      await settle(page);
      const bounds = await waiting.boundingBox();
      const composerBounds = await page.locator('textarea').first().boundingBox();
      assert(bounds.x <= composerBounds.x && bounds.x + bounds.width <= composerBounds.x + composerBounds.width + 24,
        'waiting input aligns with the reading column and composer, including beside a wide overview');
      assert(await waiting.evaluate(el => {
        for (let p = el.parentElement; p; p = p.parentElement) {
          if (p.dataset.scrollBox !== undefined || (p.scrollHeight > p.clientHeight && getComputedStyle(p).overflowY === 'auto')) return false;
        }
        return true;
      }), 'waiting input is outside the scrolling transcript');
      f.stream({ type: 'text', turn: 100, seq: 9999, text: 'New streamed reply text.\n\n'.repeat(150) });
      await page.locator('.md').filter({ hasText: 'New streamed reply text.' }).last().waitFor();
      await settle(page);
      assert.deepEqual(await waiting.boundingBox(), bounds, 'growing replies cannot move waiting input');
      const last = page.locator('[data-queued-command="later-two"]');
      assert.equal(await last.getByRole('button', { name: 'Edit queue…', exact: true }).count(), 0, 'last input has no tail to edit');
      const actions = page.locator('[data-queued-command="queued-single"] [data-queue-actions]');
      assert.deepEqual((await actions.getByRole('button').allTextContents()).map(s => s.trim()), ['Edit', 'Cancel', 'Edit queue…'], 'one consistent action row, Edit first');
      const editPosition = await actions.getByRole('button', { name: 'Edit', exact: true }).boundingBox();
      const lastEditPosition = await last.getByRole('button', { name: 'Edit', exact: true }).boundingBox();
      assert.equal(editPosition.x, lastEditPosition.x, 'Edit stays in the same horizontal slot with or without a queued tail');
      f.stream({ type: 'queue', queued: [target, ...Array.from({ length: 20 }, (_, i) => ({ ...target, command_id: `many-${i}`, text: `Waiting message ${i}` }))] });
      await page.getByText('Waiting message 19', { exact: true }).waitFor({ state: 'attached' });
      assert(await waiting.evaluate(el => el.scrollHeight > el.clientHeight && el.clientHeight <= innerHeight * .30 + 1), 'many waiting messages have their own bounded scroll');
      f.stream({ type: 'queue', queued: [{ ...target, with_agent: true }, ...queued()] });
      await page.locator('[data-queued-command="queued-single"]').getByRole('button', { name: 'Send now', exact: true }).waitFor();
      assert.deepEqual((await page.locator('[data-queued-command="queued-single"] [data-queue-actions]').getByRole('button').allTextContents()).map(s => s.trim()), ['Send now', 'Edit'], 'with-agent input keeps Edit second and has no cancellable queue tail action');
      assert.equal((await page.locator('[data-queued-command="queued-single"]').getByRole('button', { name: 'Edit', exact: true }).boundingBox()).x,
        editPosition.x, 'Edit keeps its exact horizontal position when the agent takes this input');
      f.stream({ type: 'queue', queued: [target, ...queued()] });
      await page.locator('[data-queued-command="queued-single"]').getByRole('button', { name: 'Cancel', exact: true }).waitFor();
      await bubble.waitFor();
    }
    const row = bubble.locator('..');
    assert.equal(await row.getByRole('button', { name: 'Edit queue…', exact: true }).count(), 1, 'bulk queue Edit remains a separate action');
    const edit = row.getByRole('button', { name: 'Edit', exact: true });
    if (phone) await edit.tap(); else await edit.click();
    await cancelStarted.promise;
    const panel = page.locator('[data-message-edit]');
    await panel.waitFor();
    const panelNode = await panel.elementHandle();
    assert.match(await panel.getByRole('status').textContent(), /Checking/);
    const anotherEdit = page.locator('[data-queued-command="later-one"]').getByRole('button', { name: 'Edit', exact: true });
    assert.equal(await anotherEdit.isDisabled(), true, 'another queued Edit cannot replace a draft during cancellation');
    assert.equal(await panel.locator('textarea').count(), 0, 'no editor before the worker confirms cancellation');
    assert.equal(await panel.getByRole('button', { name: 'Rewind…', exact: true }).count(), 0, 'checking never fabricates a transcript sequence');
    assert.deepEqual(calls.send, []);
    assert.deepEqual(calls.rewind, []);
    let actualSeq;
    if (!unconfirmed) {
      f.stream({ type: 'queue', queued: queued() });
      await bubble.waitFor({ state: 'hidden' });
      if (taken) {
        actualSeq = f.send({ type: 'user', command_id: target.command_id, text: 'Delivered transcript text must not replace the saved queued draft' });
        await page.locator(`#parent-e${actualSeq}`).waitFor();
      }
      assert(await panel.evaluate((el, kept) => el === kept, panelNode), 'removing the queued bubble / mounting its User event retains the Transcript-owned panel');
      assert.equal(await panel.locator('textarea').count(), 0, 'live queue removal/User delivery is not confirmed cancellation');
    } else {
      const otherSeq = f.send({ type: 'user', command_id: 'different-command', text: 'An unrelated delivered message is not this queued draft' });
      await page.locator(`#parent-e${otherSeq}`).waitFor();
    }
    await laterRemain();
    const response = page.waitForResponse(r => r.request() === cancelRequest);
    cancelRelease.resolve();
    assert.equal((await response).status(), taken || outcome === 'queued-pending' ? 409 : unconfirmed ? 503 : 204);
    if (taken) {
      f.phase = 'queued cancellation lost race: explicit actual-sequence rewind';
      await panel.getByText('The agent has this message', { exact: true }).waitFor();
      assert.deepEqual(await panel.getByRole('button').allTextContents(), ['Send correction', 'Rewind…', 'Close']);
      assert.equal(await panel.locator('textarea').count(), 0, '409 offers explicit choices, never an implicit editor');
      await panel.getByRole('button', { name: 'Rewind…', exact: true }).click();
      await panel.getByText(/Rewind the conversation to before this message\?/).waitFor();
      assert.equal(await panel.getByRole('checkbox').isChecked(), false);
      assert.deepEqual(calls.rewind, [], 'choosing Rewind… still requires separate confirmation');
      const accepted = page.waitForResponse(r => r.url().endsWith('/parent/rewind'));
      await panel.getByRole('button', { name: 'Rewind', exact: true }).click();
      await accepted;
      await settle(page);
      assert.deepEqual(calls.rewind, [{ to_seq: actualSeq, restore_files: false }], 'rewind uses only the User event matching this command ID');
      assert.equal(await panel.locator('textarea').count(), 0, 'accepted rewind HTTP is not durable confirmation');
      assert.deepEqual(calls.send, [], 'losing cancellation and explicit rewind never resend automatically');
      f.send({ type: 'rewound', to_seq: actualSeq, restored_files: false, restore_error: '' });
    } else if (unconfirmed) {
      f.phase = 'queued cancellation unconfirmed: correction before transcript';
      await panel.getByText(/Could not confirm withdrawal/).waitFor();
      assert.deepEqual(await panel.getByRole('button').allTextContents(), ['Send correction', 'Close'], 'uncertainty offers correction/close, not only Retry withdrawal or a fabricated rewind');
      assert.match(await panel.textContent(), /waiting.*transcript.*rewind|rewind.*transcript/i, 'explain why rewind waits for the matching transcript event');
      assert.equal(await panel.locator('textarea').count(), 0);
      await panel.getByRole('button', { name: 'Send correction', exact: true }).click();
    }
    const editor = panel.getByRole('textbox', { name: 'Edit message', exact: true });
    await editor.waitFor();
    assert.equal(await editor.inputValue(), target.text, 'single queued draft survives cancellation, queue removal, and matching transcript resolution');
    assert.equal(await panel.getByRole('button', { name: 'Add a message', exact: true }).count(), 0);
    assert.equal(await panel.getByTitle('Move up', { exact: true }).count(), 0);
    assert.equal(await panel.getByTitle('Drop this message', { exact: true }).count(), 0);
    await laterRemain();
    if (taken) {
      await panel.getByRole('button', { name: 'Discard draft', exact: true }).click();
    } else {
      const replacement = `Queued ${phone ? 'phone' : 'desktop'} ${outcome} edited text`;
      await editor.fill(replacement);
      const editorNode = await editor.elementHandle();
      assert.equal(await anotherEdit.isDisabled(), true, 'another queued Edit cannot discard a typed draft or start another cancellation');
      if (unconfirmed) {
        f.phase = 'matching User arrives while correction editor is open';
        f.stream({ type: 'queue', queued: queued() });
        actualSeq = f.send({ type: 'user', command_id: target.command_id, text: 'Late delivered original; preserve the correction being typed' });
        await page.locator(`#parent-e${actualSeq}`).waitFor();
        assert(await panel.evaluate((el, kept) => el === kept, panelNode), 'late delivery retains the edit panel');
        assert(await editor.evaluate((el, kept) => el === kept, editorNode), 'resolving the matching transcript seq does not remount the correction textarea');
        assert.equal(await editor.inputValue(), replacement, 'late User event never overwrites a typed correction');
        assert.deepEqual(calls.rewind, []);
      }
      await panel.getByRole('button', { name: 'Send', exact: true }).click();
      if (unconfirmed) {
        await panel.getByText('Fixture queued correction send unavailable', { exact: true }).waitFor();
        assert.equal(await editor.inputValue(), replacement, 'failed correction Send keeps the draft');
        assert(await editor.evaluate((el, kept) => el === kept, editorNode), 'failed Send retains the same textarea');
        await laterRemain();
        await panel.getByRole('button', { name: 'Send', exact: true }).click();
      }
      await panel.waitFor({ state: 'hidden' });
      assert(calls.send.every(body => body.text === replacement), 'plain Send uses only the edited single draft');
    }
    await panel.waitFor({ state: 'hidden' });
    assert.equal(await anotherEdit.isEnabled(), true, 'closing the single draft releases other queued Edit actions');
    await laterRemain();
    assert.equal(calls.send.length, taken ? 0 : unconfirmed ? 2 : 1);
    assert.equal(calls.rewind.length, taken ? 1 : 0);
    for (const rpc of feedReads) assert.deepEqual(rpc, { method: 'memex.feeds', params: {} },
      'only automatic read-only feed discovery is allowed outside the explicit edit actions');
    assert.deepEqual(mutations, [
      { method: 'POST', path: '/api/v1/sessions/parent/messages/queued-single/take-back' },
      ...Array.from({ length: taken ? 1 : unconfirmed ? 2 : 1 }, () =>
        ({ method: 'POST', path: `/api/v1/sessions/parent/${taken ? 'rewind' : 'messages'}` })),
    ], 'only explicit single cancellation, confirmed rewind or ordinary Send mutate anything; never bulk pull, later-row cancel, or settings writes');
    assert.deepEqual(f.errors, []);
  } catch (error) {
    console.error('QUEUED EDIT DIAGNOSTIC', JSON.stringify({ phase: f.phase, calls, mutations, feedReads, errors: f.errors,
      console: f.consoleMessages, requests: f.requests.slice(-12) }));
    throw error;
  } finally { cancelRelease.resolve(); await f.context.close(); }
}

// Unlike seeded transcript and cancellation-race cases, this starts with a
// real composer Send while a turn is still running. Actions belong to the
// durable User event, not the queue receipt, a reply, or a finished turn.
async function checkSentDuringTurn(browser, origin, phone, outcome) {
  const provenance = outcome.slice('sent-during-turn-'.length);
  const text = `Composer message sent during a working turn (${provenance})`;
  const receipt = { command_id: 'composer-during-turn', text, created_at: 1, rejected: null, with_agent: false };
  const calls = { send: [], rewind: [] }, mutations = [];
  let progress;
  const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
    'dark', { choice: 'off', effective: 'off', os: 'reduce' }, p => { progress = p; }, {}, {
      instrumentation: false,
      setup: async ({ page }) => {
        page.on('request', request => {
          const path = new URL(request.url()).pathname;
          if (/^(POST|PUT|DELETE)$/.test(request.method()) && path.startsWith('/api/v1/sessions/parent/') &&
              !/\/(seen|viewed|catchup)$/.test(path)) mutations.push({ method: request.method(), path });
        });
        await page.route('**/api/v1/sessions/parent/messages', route => {
          calls.send.push(route.request().postDataJSON());
          return route.fulfill({ json: receipt });
        });
        await page.route('**/api/v1/sessions/parent/rewind', route => {
          calls.rewind.push(route.request().postDataJSON());
          return route.fulfill({ status: 202 });
        });
      },
    }).catch(async error => { await progress?.context.close(); throw error; });
  try {
    const { page } = f;
    f.phase = 'composer Send during a running turn';
    await f.setState('working');
    const turn = f.send({ type: 'turn_started', turn: 100 });
    await page.locator(`#parent-e${turn}`).waitFor({ state: 'attached' });
    const slot = page.locator('.c6-send-slot').first();
    await slot.getByRole('button', { name: /^Stop/ }).waitFor();
    const composer = page.locator('textarea').first();
    await composer.fill(text);
    const sent = page.waitForResponse(r => r.url().endsWith('/parent/messages'));
    await slot.getByRole('button', { name: /^Send/ }).click();
    assert.equal((await sent).status(), 200);
    assert.equal(await composer.inputValue(), '', 'real composer Send clears the accepted draft');
    assert.equal(calls.send.length, 1);
    assert.equal(calls.send[0].text, text);
    await slot.getByRole('button', { name: /^Stop/ }).waitFor();
    f.stream({ type: 'queue', queued: [receipt] });
    const waiting = page.locator(`[data-queued-command="${receipt.command_id}"]`);
    await waiting.waitFor();
    assert.equal(await page.locator('.c6-transcript-row').filter({ hasText: text }).count(), 0,
      'HTTP receipt is queued, not a fabricated durable transcript event');
    assert.deepEqual(calls.rewind, []);

    f.phase = 'queue becomes taken, then the durable User arrives without ending the turn';
    f.stream({ type: 'queue', queued: [{ ...receipt, with_agent: true }] });
    await waiting.getByRole('button', { name: 'Send now', exact: true }).waitFor();
    // Unrelated live events make the durable seq differ from the turn and any
    // queue position. Neither is a valid substitute for this message's seq.
    f.send({ type: 'assistant', turn: 100, blocks: [{ type: 'text', text: 'Still working before delivery' }], stop_reason: '' });
    const actualSeq = f.send({ type: 'user', command_id: receipt.command_id, text,
      ...(provenance === 'legacy' ? {} : { origin: { kind: provenance } }) });
    const original = page.locator(`#parent-e${actualSeq}`);
    f.phase = `waiting for durable User ${actualSeq}`;
    await original.waitFor();
    assert.equal(await slot.getByRole('button', { name: /^Stop/ }).isVisible(), true,
      'the agent is still working when the durable User offers actions');
    // A durable User must suffice even before the next queue snapshot arrives.
    const sheet = page.getByRole('dialog', { name: 'Message actions', exact: true });
    const action = async name => {
      f.phase = `${name} on the delivered User while the turn is still running`;
      await original.scrollIntoViewIfNeeded();
      if (phone) {
        await original.getByRole('button', { name: 'Message actions', exact: true }).tap();
        await sheet.waitFor();
        for (const label of ['Edit', 'Ask in a side chat', 'Rewind…']) {
          const button = sheet.getByRole('button', { name: label, exact: true });
          assert.equal(await button.isEnabled(), true,
            `${label} is available on the durable User while working`);
          assert.equal(await button.locator('svg').count(), 1, `${label} retains its drawn icon`);
        }
        assert.equal(await sheet.getByText(provenance === 'unknown' ? 'Message' : 'You', { exact: true }).count(), 1);
        const label = name === 'Ask' ? 'Ask in a side chat' : name === 'Rewind' ? 'Rewind…' : name;
        await sheet.getByRole('button', { name: label, exact: true }).tap();
        await sheet.waitFor({ state: 'hidden' });
      } else {
        await original.hover();
        for (const label of ['Edit', 'Rewind', 'Ask']) {
          const button = original.getByRole('button', { name: label, exact: true });
          assert.equal(await button.isEnabled(), true,
            `${label} is available on the durable User while working`);
          assert.equal(await button.locator('svg').count(), 1, `${label} retains its drawn icon`);
        }
        await original.getByRole('button', { name, exact: true }).click();
      }
    };
    f.phase = 'checking durable User provenance';
    if (provenance === 'unknown') await original.getByText('Message · origin unknown', { exact: true }).waitFor();
    await action('Edit');
    const edit = page.locator('[data-message-edit]');
    await edit.getByText('The agent has this message', { exact: true }).waitFor();
    assert.equal(await edit.locator('textarea').count(), 0, 'taken Edit never pretends it can withdraw the message');
    assert.deepEqual(calls.rewind, [], 'opening Edit is not a rewind');
    await edit.getByRole('button', { name: 'Close', exact: true }).click();
    await edit.waitFor({ state: 'hidden' });
    f.stream({ type: 'queue', queued: [] });
    await waiting.waitFor({ state: 'hidden' });
    await action('Ask');
    const question = page.getByPlaceholder(/^Ask about this in a side chat/);
    await question.waitFor();
    await question.locator('..').getByRole('button', { name: 'Cancel', exact: true }).click();
    await question.waitFor({ state: 'hidden' });

    f.phase = 'explicit direct Rewind targets the actual delivered User sequence';
    await action('Rewind');
    const warning = page.getByText('Rewind the conversation to before this message? The messages after it stop counting (they stay visible, folded).', { exact: true });
    await warning.waitFor();
    const confirmation = warning.locator('..');
    assert.equal(await confirmation.getByRole('checkbox').isChecked(), false);
    assert.deepEqual(calls.rewind, [], 'Rewind entry still needs a separate confirmation');
    assert.equal(await confirmation.getByRole('button', { name: 'Rewind', exact: true }).isEnabled(), true,
      'a running turn is not a rewind blocker');
    const accepted = page.waitForResponse(r => r.url().endsWith('/parent/rewind'));
    await confirmation.getByRole('button', { name: 'Rewind', exact: true }).click();
    assert.equal((await accepted).status(), 202);
    assert.deepEqual(calls.rewind, [{ to_seq: actualSeq, restore_files: false }],
      'the request uses the durable User seq, not a queue id, index, or turn seq');
    assert.equal(await warning.isVisible(), true, 'HTTP enqueue alone cannot confirm rewind');
    f.send({ type: 'rewound', to_seq: actualSeq, restored_files: false, restore_error: '' });
    await warning.waitFor({ state: 'hidden' });
    assert.equal(calls.send.length, 1, 'Edit, Ask cancellation, and confirmed rewind never resend');
    assert.deepEqual(mutations, [
      { method: 'POST', path: '/api/v1/sessions/parent/messages' },
      { method: 'POST', path: '/api/v1/sessions/parent/rewind' },
    ], 'no queue cancellation/pull, interrupt, side-chat creation, or implicit send');
    assert(!f.requests.some(r => /^(PUT|POST|DELETE) .*\/settings\//.test(r)), 'no settings writes');
    assert.deepEqual(f.errors, []);
  } catch (error) {
    console.error('SENT DURING TURN DIAGNOSTIC', JSON.stringify({ phase: f.phase, calls, mutations,
      errors: f.errors, console: f.consoleMessages, requests: f.requests.slice(-12) }));
    throw error;
  } finally { await f.context.close(); }
}

async function main() {
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const { server, origin } = await serve();
  try {
    const engine = process.env.ENGINE || 'chromium';
    assert(['chromium', 'webkit', 'both'].includes(engine), 'ENGINE=chromium,webkit,both');
    let checked = 0;
    const failures = [];
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      for (const phone of [false, true]) for (const outcome of [
        'correction', 'correction-send-error', 'correction-discard', 'close',
        'rewind', 'rewind-early', 'rewind-reset-error', 'rewind-api-error', 'rewind-replay',
        'queued-ready', 'queued-taken-race', 'queued-unconfirmed', 'queued-pending',
        'sent-during-turn-human', 'sent-during-turn-unknown', 'sent-during-turn-legacy',
      ]) {
        const label = `${name}/${phone ? 'phone' : 'desktop'}/${outcome}`;
        if (process.env.CASE_EXACT && label !== process.env.CASE_EXACT) continue;
        if (process.env.CASE && !label.includes(process.env.CASE)) continue;
        const browser = await launchBrowser(playwright, name);
        try {
          console.log(`RUN ${label}`);
          await within(label, () => outcome.startsWith('sent-during-turn-') ? checkSentDuringTurn(browser, origin, phone, outcome)
            : outcome.startsWith('queued-') ? checkQueued(browser, origin, phone, outcome) : check(browser, origin, phone, outcome));
          checked++;
          console.log(`PASS ${label}`);
        } catch (error) {
          failures.push(label);
          console.error(`FAIL ${label}`, error.stack || error);
        } finally { await browser.close(); }
      }
    }
    assert(checked + failures.length, 'CASE/CASE_EXACT must select at least one case');
    console.log(`SUMMARY ${checked} passed; ${failures.length} failed`);
    assert.deepEqual(failures, [], 'all selected cases must pass; isolated reruns do not erase cohort failures');
  } finally { await new Promise(resolve => server.close(resolve)); }
}
main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
