#!/usr/bin/env node
'use strict';

// Production WASM; run separately from the unchanged historical message-origin suite:
// ROOT=crates/c6-ui/dist ENGINE=both node crates/c6-ui/tests/transcript-press.cjs
// Native WebKit raw touch has no compatibility click (pinned WPE 2388). Its
// held-touch geometry/cancel test and native mouse activation test are distinct.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const root = path.resolve(process.env.ROOT || path.join(__dirname, '../dist'));
const shots = path.join(process.env.SCREENSHOTS || path.join(require('node:os').homedir(), '.cache/c6-helper/transcript-press'), `${Date.now()}-${process.pid}`);
const initial = 'The agent is writing';
const chunk = ' The assistant continues writing words during the finger press.';
const events = [{ seq: 70, ts: 70, type: 'user', command_id: 'press-target',
  origin: { kind: 'api', token_name: 'press-regression' }, text: 'Inspect the current reply.', prompt: 'Inspect the current reply.' }];
const frame = page => page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));

function observePress() {
  const log = window.__transcriptPress = { events: [] };
  for (const type of ['pointerdown', 'pointerup', 'pointercancel', 'touchstart', 'touchmove', 'touchend', 'touchcancel', 'mousedown', 'mouseup', 'click']) {
    document.addEventListener(type, event => {
      const row = event.target.closest?.('[id^="parent-e"]');
      const button = event.target.closest?.('[aria-label="Message actions"]');
      log.events.push({ type, t: performance.now(), trusted: event.isTrusted, row: row?.id,
        action: !!button, pointerType: event.pointerType, touches: event.touches?.length });
    }, true);
  }
}

async function metric(page, seq = 70) {
  return page.evaluate(seq => {
    const row = document.querySelector(`#parent-e${seq}`), button = row?.querySelector('[aria-label="Message actions"]');
    let box = row;
    while (box && !box.classList.contains('overflow-y-auto')) box = box.parentElement;
    if (!box || !button) return null;
    const rect = button.getBoundingClientRect(), bounds = box.getBoundingClientRect();
    const x = Math.round(rect.x + rect.width / 2), y = Math.round(rect.y + rect.height / 2);
    return { x, y, button: rect.toJSON(), top: box.scrollTop, height: box.scrollHeight, viewport: box.clientHeight,
      gap: box.scrollHeight - box.scrollTop - box.clientHeight,
      visible: rect.width > 0 && rect.top >= Math.max(0, bounds.top) && rect.bottom <= Math.min(innerHeight, bounds.bottom),
      hit: button.contains(document.elementFromPoint(x, y)), expanded: button.getAttribute('aria-expanded') };
  }, seq);
}

async function atEnd(page, seq = 70) {
  await page.waitForFunction(seq => {
    let box = document.querySelector(`#parent-e${seq}`);
    while (box && !box.classList.contains('overflow-y-auto')) box = box.parentElement;
    return box && box.scrollHeight - box.scrollTop - box.clientHeight < 4;
  }, seq);
}

async function nativeTouch(f, engine) {
  // These internals are from the pinned Playwright-core 1.65.0 in-process
  // transport. Fail visibly on an incompatible driver; never use DOM events.
  const session = engine === 'chromium'
    ? (f.touchSession ||= await f.context.newCDPSession(f.page))
    : f.page._connection.toImpl(f.page).delegate._pageProxySession;
  assert(session?.send, 'native WebKit page-proxy input requires pinned in-process Playwright');
  const active = new Map();
  const send = (type, points) => session.send('Input.dispatchTouchEvent', { type, touchPoints: points });
  return {
    async start(point) {
      active.set(point.id, point);
      await send('touchStart', [...active.values()]);
    },
    async move(point) {
      active.set(point.id, point);
      await send('touchMove', [...active.values()]);
    },
    async end() {
      await send('touchEnd', engine === 'webkit' ? [...active.values()] : []);
      active.clear();
    },
    async cancel() {
      if (active.size) await send('touchCancel', engine === 'webkit' ? [...active.values()] : []);
      active.clear();
    },
  };
  // Do not detach the Chromium CDP session: that resets phone touch emulation.
}

async function text(f, value) {
  f.stream({ type: 'text', turn: 100, seq: 9999, text: value });
  await frame(f.page); await frame(f.page);
}

async function resumes(f, previous, seq = 70) {
  await text(f, initial + chunk + chunk);
  await atEnd(f.page, seq);
  const current = await metric(f.page, seq);
  assert(current.height > previous.height, 'a subsequent real stream update changes layout');
  assert(current.gap < 4, 'following resumes at the new actual end');
  return current;
}

async function checkLatest(browser, origin, engine, scenario, motion) {
  const label = `${engine}-${scenario}-${motion}`, seq = 1201, anchor = 'parent-e101';
  const rawTouch = scenario === 'latest-touch-position', mouse = engine === 'webkit' && !rawTouch;
  const evidence = { motion, nativeInput: mouse ? 'mouse' : 'touch', streams: [], pages: [] };
  const sockets = new Set();
  const scrollSnapshot = page => page.evaluate(() => {
    const content = document.querySelector('[data-follow-content]');
    let box = content;
    while (box && !box.classList.contains('overflow-y-auto')) box = box.parentElement;
    if (!box) return null;
    const gap = box.scrollHeight - box.scrollTop - box.clientHeight;
    return { top: box.scrollTop, height: box.scrollHeight, viewport: box.clientHeight, gap,
      farByGeometry: gap > box.clientHeight,
      latestRendered: !!document.querySelector('[title="To the latest (End)"]'), hash: location.hash };
  });
  const scrollSettled = page => page.locator('[data-follow-content]').evaluate(el => new Promise(resolve => {
    let box = el;
    while (box && !box.classList.contains('overflow-y-auto')) box = box.parentElement;
    let previous = box.scrollTop, stable = 0;
    const tick = () => {
      const top = box.scrollTop;
      stable = top === previous ? stable + 1 : 0;
      previous = top;
      if (stable >= 8) resolve(); else requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  }));
  const history = Array.from({ length: seq }, (_, i) => {
    const n = i + 1;
    if (n === 1) return { seq: n, ts: n, type: 'started', connection_id: 'test', model: 'test',
      cwd: '/fixture', system_prompt: '', tools: [], context_window: 128000 };
    if (n === seq) return { ...events[0], seq: n, ts: n };
    if (n % 10 === 1) return { seq: n, ts: n, type: 'user', command_id: `latest-${n}`, text: `Question ${n}` };
    return { seq: n, ts: n, type: 'assistant', turn: n,
      blocks: [{ type: 'text', text: `Reply ${n}. ${'Long readable transcript. '.repeat(12)}` }], stop_reason: 'end_turn' };
  });
  const layout = { tiles: [{ key: 2, columns: [{ key: 3, view: { view: 'session', id: 'parent',
    target: { anchor, at: 101, q: '', open: [] } } }], cursor: 0, active: 3, used: 3,
    back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 4, full: false };
  let f, progress, touch;
  try {
    await within(label, async () => {
      f = await fixture(browser, origin, { width: 320, height: 568 }, 'light',
        { choice: motion, os: 'no-preference', effective: motion }, current => { progress = current; }, {}, {
          layout, path: `/sessions/parent#${anchor}`, instrumentation: false,
          setup: async ({ page, context }) => {
            await context.addInitScript(observePress);
            await context.routeWebSocket('**/sessions/parent/stream?*', socket => {
              sockets.add(socket);
              socket.onClose(() => sockets.delete(socket));
              const q = new URL(socket.url()).searchParams, after = Number(q.get('after'));
              const from = after ? after + 1 : Math.min(seq - 299, Number(q.get('at') || Infinity));
              evidence.streams.push({ after, from, end: seq });
              if (!after) {
                socket.send(JSON.stringify({ type: 'window', from }));
                socket.send(JSON.stringify({ type: 'event', event: history[0] }));
              }
              // Like the real head, at changes the beginning, never the end.
              for (const event of history.filter(e => e.seq >= from)) socket.send(JSON.stringify({ type: 'event', event }));
              socket.send(JSON.stringify({ type: 'synced' }));
            });
            await page.route('**/api/v1/sessions/parent/events?*', route => {
              const q = new URL(route.request().url()).searchParams, before = Number(q.get('before'));
              const from = Math.max(1, Math.min(before - Number(q.get('limit') || 300), Number(q.get('at') || Infinity)));
              evidence.pages.push(Object.fromEntries(q));
              return route.fulfill({ json: history.filter(e => e.seq >= from && e.seq < before) });
            });
          },
        });
      await Promise.race([(async () => {
        const { page } = f;
        const saved = () => page.evaluate(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0]);
        f.phase = 'saved historical window, then real reload';
        await page.locator(`#${anchor}`).waitFor();
        assert.equal((await saved()).columns[0].view.target.anchor, anchor);
        await page.reload({ waitUntil: 'domcontentloaded' });
        await page.locator(`#${anchor}`).waitFor();
        await settle(page);
        assert.equal(await page.locator(`#parent-e${seq}`).count(), 0, 'the real current row is outside the retained historical drawing window');
        assert.equal(evidence.streams.at(-1).from, 101);
        assert.equal(evidence.streams.at(-1).end, seq, 'the complete suffix was delivered despite the historical cutoff');
        const composer = page.locator('textarea').first(), kept = await composer.elementHandle();
        const draft = 'Keep this draft through Latest and native activation';
        await composer.fill(draft);
        const previous = evidence.historical = await saved(), streamCount = evidence.streams.length;
        assert.equal(previous.columns[0].view.target?.anchor, anchor, 'the reloaded historical address is still retained until Latest');
        f.phase = 'native PageDown scrolls within the already historical window';
        const readingAway = evidence.readingAway = { before: await scrollSnapshot(page) };
        await page.locator('[data-follow-content]').evaluate(el => {
          let box = el;
          while (box && !box.classList.contains('overflow-y-auto')) box = box.parentElement;
          const log = window.__latestReadingAway = [];
          for (const type of ['keydown', 'scroll']) box.addEventListener(type, event => {
            log.push({ type, trusted: event.isTrusted, key: event.key, top: box.scrollTop,
              height: box.scrollHeight, viewport: box.clientHeight });
          }, { passive: true });
          box.focus({ preventScroll: true });
        });
        // Historical anchor reveal can start at scrollTop=0 without a scroll
        // event, leaving the floating control's far flag false. A native
        // PageDown within this already-away window delivers that event without
        // paging older history at the top. Never assign offsets or scroll a
        // locator/control into view, and let the native scroll finish.
        await page.keyboard.press('PageDown');
        await scrollSettled(page);
        readingAway.afterPageDown = await scrollSnapshot(page);
        assert(readingAway.afterPageDown.top > readingAway.before.top, 'native PageDown moves within the historical window');
        readingAway.events = await page.evaluate(() => window.__latestReadingAway);
        assert(readingAway.afterPageDown.farByGeometry, 'the historical window retains more than a viewport below, the floating Latest precondition');
        assert(readingAway.events.some(e => e.type === 'keydown' && e.key === 'PageDown' && e.trusted), 'the historical scroll uses trusted native keyboard intent');
        assert(readingAway.events.some(e => e.type === 'scroll' && e.trusted), 'the browser delivers real scroll events');
        const away = await saved();
        assert.deepEqual([away.columns[0].view.target, away.back, away.forward],
          [previous.columns[0].view.target, previous.back, previous.forward], 'native reading-away preserves the retained target and history');
        assert.equal(readingAway.afterPageDown.hash, `#${anchor}`);
        assert.equal(await page.locator(`#parent-e${seq}`).count(), 0, 'reading-away does not bypass Latest or its historical drawing window');
        assert.equal(evidence.streams.length, streamCount, 'reading-away retains the historical stream');
        f.phase = 'floating Latest visibility after verified native reading-away';
        const latest = page.getByTitle('To the latest (End)', { exact: true });
        await latest.waitFor();
        f.phase = 'native Latest hit-test and activation consumes the retained target';
        const latestPoint = evidence.latestPoint = await latest.evaluate(el => {
          const r = el.getBoundingClientRect(), x = Math.round(r.x + r.width / 2), y = Math.round(r.y + r.height / 2);
          return { x, y, visible: r.width > 0 && r.left >= 0 && r.right <= innerWidth && r.top >= 0 && r.bottom <= innerHeight,
            hit: el.contains(document.elementFromPoint(x, y)) };
        });
        assert(latestPoint.visible && latestPoint.hit, 'the real floating Latest control is visible and hit-tested without locator scrolling');
        await page.touchscreen.tap(latestPoint.x, latestPoint.y);
        await page.locator(`#parent-e${seq}`).waitFor();
        await atEnd(page, seq); await settle(page);
        await page.waitForFunction(() => !JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0].columns[0].view.target && !location.hash);
        const current = evidence.latest = await saved();
        assert.deepEqual([current.back, current.forward], [previous.back, previous.forward], 'Latest consumes the address without adding history');
        assert(await composer.evaluate((el, old) => el === old && old.isConnected, kept), 'Latest keeps the mounted composer');
        assert.equal(await composer.inputValue(), draft);
        assert.equal(evidence.streams.length, streamCount, 'Latest retains the session stream');
        assert.equal(await page.locator(`#${anchor}`).count(), 0, 'Latest really trims the old window');
        f.phase = 'post-Latest stream setup and visible initial markdown';
        const transport = { page, stream: message => {
          assert(sockets.size, 'the production transcript stream is connected');
          for (const socket of sockets) socket.send(JSON.stringify({ ...message, turn: 2000 }));
        } };
        await f.setState('working', { last_seq: seq });
        await text(transport, initial);
        await page.locator('.md').filter({ hasText: initial }).last().waitFor();
        f.phase = 'post-Latest initial stream reaches the actual end';
        await atEnd(page, seq); await settle(page);
        evidence.phonePolicy = await page.evaluate(() => ({ coarse: matchMedia('(pointer: coarse)').matches,
          points: navigator.maxTouchPoints, touchEvents: 'ontouchstart' in window }));
        assert(evidence.phonePolicy.coarse && (evidence.phonePolicy.points > 0 || evidence.phonePolicy.touchEvents));
        const before = evidence.before = await metric(page, seq);
        assert(before.gap < 4 && before.visible && before.button.left >= 0 && before.button.right <= 320 && before.hit,
          'after real Latest the API control is at the end, fully visible and hit-tested, without auto-scroll');
        evidence.inputStart = await page.evaluate(() => __transcriptPress.events.length);
        f.phase = 'one real stream chunk during the post-Latest native hold';
        if (mouse) { await page.mouse.move(before.x, before.y); await page.mouse.down(); }
        else { touch = await nativeTouch(f, engine); await touch.start({ id: 1, x: before.x, y: before.y }); }
        await text(transport, initial + chunk);
        const during = evidence.during = await metric(page, seq);
        assert(during.height >= before.height + 22, 'layout actually grows during the native hold after Latest');
        assert(Math.abs(during.button.y - before.button.y) <= 1, 'automatic follow correction must preserve the held target after Latest');
        const held = await page.evaluate(start => __transcriptPress.events.slice(start), evidence.inputStart);
        assert(held.some(e => e.type === (mouse ? 'mousedown' : 'touchstart') && e.trusted && e.action && e.row === `parent-e${seq}`));
        assert(!held.some(e => ['mouseup', 'touchend', 'touchcancel', 'pointercancel'].includes(e.type)), 'the stream grows before the original contact releases');
        const dialog = page.getByRole('dialog', { name: 'Message actions', exact: true });
        assert.equal(await dialog.isVisible(), false, 'holding never activates the action prematurely');
        if (rawTouch) {
          // WebKit raw touch has no compatibility click. Keep this as the
          // post-Latest geometry/cancel/recovery case; ordinary native tap is
          // independently covered by the original fresh-context tap case.
          await touch.cancel(); await settle(page);
          const canceled = await page.evaluate(start => __transcriptPress.events.slice(start), evidence.inputStart);
          assert(canceled.some(e => e.type === 'touchcancel' && e.trusted));
          assert(!canceled.some(e => e.type === 'click'), 'raw WebKit cancellation is not activation');
          evidence.rawTouchMs = canceled.find(e => e.type === 'touchcancel').t - canceled.find(e => e.type === 'touchstart').t;
          assert.equal(await dialog.isVisible(), false);
          evidence.resumed = await resumes(transport, during, seq);
          const target = await metric(page, seq);
          assert(target.visible && target.hit, 'raw-touch recovery leaves the action usable');
          assert.equal(await dialog.isVisible(), false, 'raw-touch cancel/recovery never activates actions');
        } else {
          if (mouse) await page.mouse.up();
          else await touch.end();
          await dialog.waitFor();
          assert.equal(await page.locator(`#parent-e${seq} [aria-label="Message actions"]`).getAttribute('aria-expanded'), 'true');
          const inputs = await page.evaluate(start => __transcriptPress.events.slice(start), evidence.inputStart);
          const down = inputs.find(e => e.type === (mouse ? 'mousedown' : 'touchstart'));
          const up = inputs.find(e => e.type === (mouse ? 'mouseup' : 'touchend'));
          const click = inputs.find(e => e.type === 'click');
          evidence.pressMs = up?.t - down?.t;
          assert(down?.trusted && up?.trusted && click?.trusted && click.action && click.row === `parent-e${seq}`, 'trusted native activation opens the original post-Latest row action');
          evidence.resumed = await resumes(transport, during, seq);
        }
        assert.equal(evidence.streams.length, streamCount);
        assert(!(await saved()).columns[0].view.target);
        assert.equal(await page.locator(`#${anchor}`).count(), 0, 'native input never reinstates the old historical window');
        assert.equal(await composer.inputValue(), draft);
        assert(await page.evaluate(() => matchMedia('(pointer: coarse)').matches &&
          (navigator.maxTouchPoints > 0 || 'ontouchstart' in window)), 'native input preserves phone policy');
        assert.deepEqual(f.errors, []);
        assert.deepEqual(f.requests.filter(request => /^(POST|PUT|PATCH|DELETE) .*\/(messages|side-chats|rewind|interrupt)(\/|$)/.test(request)), []);
      })(), f.crash]);
    });
    console.log(`PASS ${label}`);
    return { label, passed: true, evidence };
  } catch (error) {
    await progress?.page.screenshot({ path: path.join(shots, `${label}-failure.png`) }).catch(() => {});
    console.error(`FAIL ${label}: ${error.message}`);
    return { label, passed: false, phase: progress?.phase, error: error.stack, evidence };
  } finally {
    if (progress) {
      evidence.finalScroll = await scrollSnapshot(progress.page).catch(() => null);
      evidence.readingAwayEvents = await progress.page.evaluate(() => window.__latestReadingAway || []).catch(() => []);
      evidence.events = await progress.page.evaluate(() => __transcriptPress.events).catch(() => []);
      evidence.errors = progress.errors;
      evidence.requests = progress.requests;
    }
    await touch?.cancel().catch(() => {});
    await progress?.context.close();
  }
}

async function check(browser, origin, engine, scenario, motion) {
  const label = `${engine}-${scenario}-${motion}`, evidence = { nativeInput: engine === 'webkit' && ['idle', 'stream'].includes(scenario) ? 'mouse' : 'touch', motion };
  let f, progress, touch;
  try {
    await within(label, async () => {
      f = await fixture(browser, origin, { width: 320, height: 568 }, 'light', { choice: motion, os: 'no-preference', effective: motion },
        current => { progress = current; }, {}, { events, instrumentation: false,
          setup: async ({ context }) => context.addInitScript(observePress) });
      evidence.phonePolicy = await f.page.evaluate(() => ({ coarse: matchMedia('(pointer: coarse)').matches,
        points: navigator.maxTouchPoints, touchEvents: 'ontouchstart' in window }));
      assert(evidence.phonePolicy.coarse && (evidence.phonePolicy.points > 0 || evidence.phonePolicy.touchEvents),
        'native phone fixture retains coarse-pointer/touch policy in both engines');
      f.phase = 'Latest, without historical locator scroll';
      await f.page.locator('#parent-e70').waitFor();
      await settle(f.page); await atEnd(f.page);
      if (!['idle', 'tap'].includes(scenario)) {
        await f.setState('working');
        await text(f, initial);
        await f.page.locator('.md').filter({ hasText: initial }).last().waitFor();
        await atEnd(f.page);
      }
      const before = evidence.before = await metric(f.page);
      evidence.activeGlideBefore = await f.page.evaluate(() => document.getAnimations().some(animation =>
        ['running', 'pending'].includes(animation.playState) && animation.effect?.target?.matches?.('[data-follow-content]')));
      assert(before.gap < 4, 'starts at the actual Latest, not only a follow flag');
      assert(before.visible && before.hit, 'opener is fully visible and hit-tested before native input; no auto-scroll');
      touch = await nativeTouch(f, engine);
      const point = { id: 1, x: before.x, y: before.y };
      const mouse = evidence.nativeInput === 'mouse';
      f.phase = scenario === 'tap' ? 'ordinary native tap control' : 'native press with one stream chunk';
      if (scenario === 'tap') await f.page.touchscreen.tap(point.x, point.y);
      else if (mouse) { await f.page.mouse.move(point.x, point.y); await f.page.mouse.down(); }
      else await touch.start(point);
      if (scenario === 'multi-touch') {
        await touch.start({ id: 2, x: 40, y: point.y });
        assert(await f.page.evaluate(() => __transcriptPress.events.some(e => e.type === 'touchstart' && e.trusted && e.touches === 2)), 'native multi-touch exposes both contacts');
      }
      if (['idle', 'tap'].includes(scenario)) { await frame(f.page); await frame(f.page); }
      else await text(f, initial + chunk);
      evidence.during = await metric(f.page);
      if (!['idle', 'tap'].includes(scenario)) assert(evidence.during.height >= before.height + 22, 'stream layout really grew during the press');

      if (scenario === 'scroll') {
        // Finger down means reading older content. This must remain Away after release.
        await touch.move({ ...point, y: point.y + 50 });
        await frame(f.page);
        await touch.move({ ...point, y: point.y + 120 });
        await frame(f.page);
        await touch.end();
        await settle(f.page);
        const away = await metric(f.page);
        assert(away.gap > 4, 'native press becomes a real reading-away scroll');
        await text(f, initial + chunk + chunk);
        const after = evidence.away = await metric(f.page);
        assert(after.gap > 4, 'a pending follow correction never overrides Away');
        assert.equal(await f.page.getByRole('dialog', { name: 'Message actions', exact: true }).isVisible(), false);
        await f.page.locator('#parent-e70').evaluate(el => {
          while (!el.classList.contains('overflow-y-auto')) el = el.parentElement;
          el.focus({ preventScroll: true });
        });
        await f.page.keyboard.press('End');
        await atEnd(f.page);
        await text(f, initial + chunk + chunk + chunk);
        await atEnd(f.page);
      } else if (scenario === 'unmount') {
        await f.page.getByRole('button', { name: /^Switch sessions and pages,/ }).click();
        const picker = f.page.getByRole('dialog', { name: 'Sessions', exact: true });
        await picker.locator('button[aria-current="page"]').locator('..').getByRole('button', { name: 'Close this session or page', exact: true }).click();
        await f.page.locator('#parent-e70').waitFor({ state: 'detached' });
        await touch.end();
        await settle(f.page);
        assert.equal(await f.page.getByRole('dialog', { name: 'Message actions', exact: true }).isVisible(), false);
      } else if (scenario === 'cancel' || scenario === 'multi-touch' || scenario === 'touch-position') {
        await touch.cancel();
        await settle(f.page);
        assert.equal(await f.page.getByRole('dialog', { name: 'Message actions', exact: true }).isVisible(), false, 'cancel never activates actions');
        evidence.resumed = await resumes(f, evidence.during);
        if (scenario === 'touch-position') {
          assert(Math.abs(evidence.during.button.y - before.button.y) <= 1, 'WebKit native held touch preserves the opener position while layout grows');
        }
        if (engine === 'chromium') {
          const target = await metric(f.page);
          assert(target.visible && target.hit, 'following recovery leaves a usable visible action control');
          await f.page.touchscreen.tap(target.x, target.y);
          await f.page.getByRole('dialog', { name: 'Message actions', exact: true }).waitFor();
        }
      } else {
        if (mouse) await f.page.mouse.up();
        else if (scenario !== 'tap') await touch.end();
        // Child click handlers stop propagation. Require the actual row action,
        // not just a successful protocol call, and then following resumption.
        await f.page.getByRole('dialog', { name: 'Message actions', exact: true }).waitFor();
        assert.equal(await f.page.locator('#parent-e70 [aria-label="Message actions"]').getAttribute('aria-expanded'), 'true');
        const inputs = await f.page.evaluate(() => __transcriptPress.events);
        const down = inputs.find(e => e.type === (mouse ? 'mousedown' : 'touchstart'));
        const up = inputs.find(e => e.type === (mouse ? 'mouseup' : 'touchend'));
        const click = inputs.find(e => e.type === 'click');
        evidence.pressMs = up?.t - down?.t;
        assert(down?.trusted && up?.trusted && click?.trusted && click.action && click.row === 'parent-e70', 'trusted native activation reaches the original row action');
        await f.setState('working');
        evidence.resumed = await resumes(f, ['idle', 'tap'].includes(scenario) ? before : evidence.during);
      }
      assert(await f.page.evaluate(() => matchMedia('(pointer: coarse)').matches &&
        (navigator.maxTouchPoints > 0 || 'ontouchstart' in window)), 'native input never resets phone emulation');
      assert.deepEqual(f.errors, [], 'no app errors or disposed-owner callbacks');
      assert.deepEqual(f.requests.filter(request => /^(POST|PUT|PATCH|DELETE) .*\/(messages|side-chats|rewind|interrupt)(\/|$)/.test(request)), [], 'press lifecycle never sends or rewinds');
    });
    console.log(`PASS ${label}`);
    return { label, passed: true, evidence };
  } catch (error) {
    await progress?.page.screenshot({ path: path.join(shots, `${label}-failure.png`) }).catch(() => {});
    console.error(`FAIL ${label}: ${error.message}`);
    return { label, passed: false, phase: progress?.phase, error: error.stack, evidence };
  } finally {
    if (progress) {
      evidence.events = await progress.page.evaluate(() => __transcriptPress.events).catch(() => []);
      const down = evidence.events.find(event => event.type === (evidence.nativeInput === 'mouse' ? 'mousedown' : 'touchstart'));
      const up = evidence.events.find(event => event.type === (evidence.nativeInput === 'mouse' ? 'mouseup' : 'touchend'));
      if (down && up) evidence.pressMs = up.t - down.t;
      evidence.errors = progress.errors;
      evidence.requests = progress.requests;
    }
    await touch?.cancel().catch(() => {});
    await progress?.context.close();
  }
}

(async () => {
  const engine = process.env.ENGINE || 'both';
  assert(['chromium', 'webkit', 'both'].includes(engine), 'ENGINE=chromium,webkit,both');
  const selected = process.env.PRESS_CASE?.split(',');
  for (const name of selected || []) assert(['idle', 'stream', 'cancel', 'multi-touch', 'scroll', 'unmount', 'touch-position', 'tap', 'latest-stream', 'latest-touch-position'].includes(name), `Unknown PRESS_CASE ${name}`);
  fs.mkdirSync(shots, { recursive: true });
  fs.copyFileSync(__filename, path.join(shots, 'transcript-press.cjs'));
  const sha = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
  const report = { root, testSha256: sha(__filename), wasm: fs.readdirSync(root).filter(file => file.endsWith('.wasm')).map(file => ({ file, sha256: sha(path.join(root, file)) })),
    coverage: { chromium: 'native held touch and automatic compatibility click, including after real Latest; native pan and unmount', webkit: 'native held mouse activation, including after real Latest; separate native raw touch position/cancel and fresh ordinary tap activation, not held-touch compatibility-click or native pan/unmount coverage' }, results: [] };
  const { server, origin } = await serve();
  try {
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      const browser = await launchBrowser(playwright, name);
      try {
        for (const scenario of ['idle', 'stream', 'cancel', 'multi-touch', ...(name === 'chromium' ? ['scroll', 'unmount'] : ['touch-position', 'tap']), 'latest-stream', ...(name === 'webkit' ? ['latest-touch-position'] : [])]) {
          if (selected && !selected.includes(scenario)) continue;
          for (const motion of ['idle', 'stream', 'latest-stream'].includes(scenario) ? ['full', 'reduced', 'off'] : ['off']) {
            const result = await (scenario.startsWith('latest-') ? checkLatest : check)(browser, origin, name, scenario, motion);
            result.browser = browser.version();
            report.results.push(result);
            fs.writeFileSync(path.join(shots, 'results.json'), JSON.stringify(report, null, 2));
          }
        }
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
  assert(report.results.length, 'PRESS_CASE selected no applicable cases');
  const failed = report.results.filter(result => !result.passed).length;
  console.log(`${report.results.length - failed}/${report.results.length} passed; ${failed} failed; no retries. Evidence: ${shots}`);
  if (failed) process.exitCode = 1;
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
