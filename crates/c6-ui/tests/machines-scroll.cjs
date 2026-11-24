#!/usr/bin/env node
'use strict';

// Production WASM, private fixture traffic, and manually delivered real polling
// callbacks. No production hooks and no wall-clock sleeps or animation disabling.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const EVIDENCE = process.env.EVIDENCE || process.env.SCREENSHOTS;

const layout = { tiles: [{ key: 2, columns: [{ key: 3, view: { view: 'workers' } }],
  cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false }],
focus: 0, next: 4, full: false };

function polling() {
  const interval = window.setInterval.bind(window), clear = window.clearInterval.bind(window);
  const now = Date.now.bind(Date);
  let offset = 0, next = -1;
  const timers = new Map();
  Date.now = () => now() + offset;
  window.setInterval = (callback, delay, ...args) => {
    if (Number(delay) !== 15000) return interval(callback, delay, ...args);
    const id = next--;
    timers.set(id, () => callback(...args));
    return id;
  };
  window.clearInterval = id => timers.delete(id) || clear(id);
  window.__machinesPoll = {
    ids: () => [...timers.keys()],
    fire: ids => { offset += 15000; for (const id of ids) timers.get(id)?.(); },
  };
}

function sample(at, generation) {
  return { at_ms: at, cpu_percent: generation ? 72 : 12, cpu_count: 8,
    load: generation ? [2, 1, 1] : [1, 1, 1], memory_used: generation ? 700 : 300,
    memory_available: generation ? 300 : 700, memory_total: 1000,
    swap_used: generation ? 200 : 100, swap_total: 1000, oom_kills: 0,
    pressure: {}, disks: [{ mount: '/', filesystem: 'fixturefs', total: 1000,
      used: generation ? 650 : 250, available: generation ? 350 : 750,
      inodes_used: 50 }], sessions: [] };
}

async function check(browser, origin, engine, viewport) {
  const calls = [], pending = [], arrivals = [];
  let generation = 0, hold = false;
  const waitHeld = history => pending.some(call => call.history === history)
    ? Promise.resolve() : new Promise(resolve => arrivals.push({ history, resolve }));
  const f = await fixture(browser, origin, viewport, 'light',
    { choice: 'off', effective: 'off', os: 'no-preference' }, () => {}, {}, {
      path: '/machines', ready: '[data-machine-card="head"]', layout, instrumentation: false,
      setup: async ({ page, context }) => {
        await page.addInitScript(polling);
        await context.route('**/api/v1/machines**', async route => {
          const url = new URL(route.request().url());
          if (!/^\/api\/v1\/machines(?:\/[^/]+\/samples)?$/.test(url.pathname)) return route.fallback();
          const history = url.pathname.endsWith('/samples');
          const call = { path: url.pathname, history, generation };
          calls.push(call);
          const at = Date.now() + generation * 15000;
          const json = history
            ? Array.from({ length: 8 }, (_, i) => sample(at - (7 - i) * 300000, generation))
            : Array.from({ length: 10 }, (_, i) => ({ id: i ? `worker-${i}` : 'head',
              name: i ? `Worker ${i}` : 'Head fixture', role: i ? 'worker' : 'head',
              hostname: `machine-${i}.fixture`, version: `fixture-v${generation + 1}`,
              online: true, sample_secs: 15, last_sample: sample(at, generation),
              warnings: [], worker: null, head: null }));
          if (hold) await new Promise(resolve => {
            pending.push({ history, resolve });
            for (let i = arrivals.length - 1; i >= 0; i--)
              if (arrivals[i].history === history) arrivals.splice(i, 1)[0].resolve();
          });
          await route.fulfill({ json });
        });
      },
    });
  const { page } = f;
  const label = `${engine}-${viewport.width}x${viewport.height}`;
  const phases = [];
  const snapshot = () => page.evaluate(() => {
    const s = window.__machineSnapshot;
    if (!s) return null;
    const tops = s.observations.map(sample => sample.top), heights = s.observations.map(sample => sample.height);
    return { top: s.top, height: s.height, currentTop: s.scroller.scrollTop, currentHeight: s.scroller.scrollHeight,
      container: { tag: s.scroller.tagName, id: s.scroller.id, classes: s.scroller.className,
        clientHeight: s.scroller.clientHeight, overflowY: getComputedStyle(s.scroller).overflowY },
      range: { minTop: Math.min(...tops), maxTop: Math.max(...tops), minHeight: Math.min(...heights), maxHeight: Math.max(...heights) },
      same: document.querySelector('[data-machine-card="head"]') === s.card
        && document.querySelector('[data-machine-panel]') === s.panel,
      observations: s.observations };
  });
  const logGeometry = (phase, state) => {
    if (!state) return console.log(`GEOMETRY ${label} ${phase}: snapshot not initialized`);
    console.log(`GEOMETRY ${label} ${phase}: top=${state.top}->${state.currentTop}`
      + ` height=${state.height}->${state.currentHeight} clientHeight=${state.container.clientHeight}`
      + ` topRange=${state.range.minTop}..${state.range.maxTop} heightRange=${state.range.minHeight}..${state.range.maxHeight}`
      + ` container=${state.container.tag}#${state.container.id} classes=${JSON.stringify(state.container.classes)}`);
  };
  const evidence = async (passed, error, state) => {
    if (!EVIDENCE) return;
    fs.mkdirSync(EVIDENCE, { recursive: true });
    const screenshot = path.join(EVIDENCE, `${label}.png`);
    const screenshotError = await page.screenshot({ path: screenshot, timeout: 5000 }).then(() => null, failure => failure.message);
    const file = path.join(EVIDENCE, `${label}.json`);
    fs.writeFileSync(file, JSON.stringify({ label, passed, viewport, phase: f.phase, error: error?.stack,
      phases, geometry: state, calls, errors: f.errors, consoleMessages: f.consoleMessages,
      screenshot, screenshotError }, null, 2) + '\n');
    console.log(`MACHINES EVIDENCE ${file}${screenshotError ? ` (screenshot: ${screenshotError})` : ` ${screenshot}`}`);
  };
  const countHistory = () => calls.filter(call => call.history).length;
  const release = history => {
    for (let i = pending.length - 1; i >= 0; i--) if (pending[i].history === history) pending.splice(i, 1)[0].resolve();
  };
  try {
    f.phase = 'initial card and chart data';
    await page.waitForFunction(() => document.querySelectorAll('[data-machine-card]').length === 10
      && document.querySelectorAll('[data-machine-panel] figure path').length >= 5
      && [...document.querySelectorAll('[data-machine-card] svg path, [data-machine-panel] figure path')]
        .every(path => path.getAttribute('d')));
    await settle(page);
    assert.equal(countHistory(), 11, 'one history request per card plus the selected machine detail');
    const timers = await page.evaluate(() => __machinesPoll.ids());
    assert(timers.length >= 2, 'inventory and selected-detail polling timers installed');
    // Machines installs its inventory interval before mounting any children.
    const inventoryTimer = timers[0];
    await page.evaluate(() => {
      const panel = document.querySelector('[data-machine-panel]');
      let scroller = panel.parentElement;
      while (scroller && !(scroller.scrollHeight > scroller.clientHeight + 100
        && /auto|scroll/.test(getComputedStyle(scroller).overflowY))) scroller = scroller.parentElement;
      scroller ||= document.scrollingElement;
      const target = Math.min(scroller.scrollHeight - scroller.clientHeight - 80,
        panel.getBoundingClientRect().top - scroller.getBoundingClientRect().top + scroller.scrollTop
          + panel.getBoundingClientRect().height * 0.55);
      scroller.scrollTop = target;
      const card = document.querySelector('[data-machine-card="head"]');
      const paths = [...document.querySelectorAll('[data-machine-card] svg path, [data-machine-panel] figure path')];
      const svgs = [...document.querySelectorAll('[data-machine-card] svg, [data-machine-panel] figure svg')];
      window.__machineSnapshot = { scroller, panel, card, paths, svgs,
        cardPath: card.querySelector('path').getAttribute('d'),
        detailPath: panel.querySelector('figure path').getAttribute('d'),
        top: scroller.scrollTop, height: scroller.scrollHeight, observations: [], watching: true };
      const record = () => {
        const s = __machineSnapshot;
        if (!s.watching) return;
        s.observations.push({ at: performance.now(), top: scroller.scrollTop, height: scroller.scrollHeight,
          retained: [s.card, s.panel, ...paths, ...svgs].every(el => el.isConnected),
          nonempty: paths.every(path => !!path.getAttribute('d')) });
      };
      const observer = new MutationObserver(record);
      observer.observe(panel.parentElement, { subtree: true, childList: true, attributes: true, characterData: true });
      sFrame();
      function sFrame() { record(); if (__machineSnapshot.watching) requestAnimationFrame(sFrame); else observer.disconnect(); }
      record();
    });
    assert(await page.evaluate(() => __machineSnapshot.top > 100), 'the actual overflowing container is scrolled');

    const assertStable = async phase => {
      f.phase = phase;
      await settle(page);
      const state = await snapshot();
      phases.push({ phase, ...state, observations: undefined });
      logGeometry(phase, state);
      assert(state.same, `${phase}: original card and detail panel retained`);
      assert(state.observations.length > 2, `${phase}: sampled real layout while pending`);
      for (const observation of state.observations) {
        assert(observation.retained, `${phase}: card, detail SVG and path nodes never detached`);
        assert(observation.nonempty, `${phase}: chart paths never cleared`);
        assert(Math.abs(observation.top - state.top) <= 2,
          `${phase}: scrollTop changed ${state.top} -> ${observation.top}`);
        assert(Math.abs(observation.height - state.height) <= 2,
          `${phase}: scrollHeight changed ${state.height} -> ${observation.height}`);
      }
    };

    hold = true;
    generation = 1;
    const initialHistory = countHistory();
    f.phase = 'waiting for held inventory route';
    await page.evaluate(id => __machinesPoll.fire([id]), inventoryTimer);
    // Route arrival, not elapsed time, gates every pending-response assertion.
    await within('held inventory route', () => waitHeld(false));
    await assertStable('inventory response held');
    release(false);
    f.phase = 'waiting for refreshed inventory metadata';
    await page.locator('[data-machine-card="head"]').getByText('online · fixture-v2', { exact: true }).waitFor();
    await assertStable('inventory response applied');
    assert.equal(countHistory(), initialHistory, 'inventory refresh does not remount cards or refetch per-card history');
    assert.match(await page.locator('[data-machine-card="head"]').innerText(), /72%/,
      'retained card consumes new inventory metrics, not its initial snapshot');

    f.phase = 'waiting for held detail history route';
    await page.evaluate(ids => __machinesPoll.fire(ids), timers.slice(1));
    await within('held detail history route', () => waitHeld(true));
    await assertStable('history response held');
    hold = false;
    release(false);
    release(true);
    f.phase = 'waiting for refreshed history metrics';
    await page.waitForFunction(() => {
      const s = __machineSnapshot;
      return document.querySelector('[data-machine-panel] figure path')?.getAttribute('d') !== s.detailPath;
    });
    await page.locator('[data-machine-panel] figure').first().getByText('72.0', { exact: true }).waitFor();
    await assertStable('history response applied');
    assert(await page.evaluate(() => __machineSnapshot.card.querySelector('path').getAttribute('d') !== __machineSnapshot.cardPath),
      'retained card sparkline genuinely updates');
    assert.deepEqual(f.errors, []);
    const final = await snapshot();
    logGeometry('PASS', final);
    await evidence(true, null, final);
    console.log(`PASS ${engine} ${viewport.width}x${viewport.height}: held polling, scroll geometry, retained DOM, live metadata and chart paths`);
  } catch (error) {
    const failed = await snapshot().catch(() => null);
    logGeometry(`FAIL during ${f.phase}`, failed);
    await evidence(false, error, failed);
    throw error;
  } finally {
    hold = false;
    release(false);
    release(true);
    await page.evaluate(() => { if (window.__machineSnapshot) __machineSnapshot.watching = false; }).catch(() => {});
    await f.context.close();
  }
}

if (require.main === module) (async () => {
  const engines = !process.env.ENGINE ? ['chromium'] : process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : process.env.ENGINE.split(',');
  assert(engines.every(engine => ['chromium', 'webkit'].includes(engine)), 'ENGINE must select chromium or webkit');
  const { server, origin } = await serve();
  try {
    for (const engine of engines) {
      const browser = await launchBrowser(playwright, engine);
      try {
        for (const width of [1280, 390]) await within(`${engine} machines ${width}`, () => check(browser, origin, engine, { width, height: 700 }));
      } finally { await browser.close(); }
    }
  } finally { server.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
