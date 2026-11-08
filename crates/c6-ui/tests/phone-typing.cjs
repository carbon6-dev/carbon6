#!/usr/bin/env node
'use strict';

// Measurement only: production assets, isolated fake head from motion.cjs.
// On older trees without that helper, set MOTION_HARNESS and PLAYWRIGHT_CORE
// to the helper/packages in a newer checkout. No live services are contacted.
// Single trial: ROOT=... LABEL=... ENGINE=chromium SCENARIO=long-live node ...
// Paired cohort: BASE_ROOT=... FINAL_ROOT=... PAIRS=5 node ...
// Optional PROFILE=1 adds CDP CPU/trace recording (keep out of timing cohorts).
// Quiet repeat reproduction: SCENARIO=long-delete CHILDREN=300
// ATTENTION_MS=1000 CHILD_UPDATES=1 SETUP=desktop CPU=4. Metadata has two
// running children and one shell task; it does NOT mount hundreds of threads.
// SESSION_TILES/HISTORY_TURNS scale retained state; OBSERVED/NONOBSERVED vary
// child kind. Replays intentionally load ALL synthetic history, not the normal
// server-pruned tail. Deletion gaps sample value changes in rAF, before paint;
// keydown EventTiming captures queueing that input EventTiming alone misses.
// CROSS_TILE=1 types in a two-turn, zero-child session beside the retained long
// parent. Each stream receives only its own history; on phones parent is hidden.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const os = require('node:os');
const { spawnSync } = require('node:child_process');
const { serve, fixture, launchBrowser, session } = require(process.env.MOTION_HARNESS || './motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const output = process.env.EVIDENCE || '/tmp/phone-typing';
const root = path.resolve(process.env.ROOT || path.join(__dirname, '../dist'));
const label = process.env.LABEL || 'pilot';
const engine = process.env.ENGINE || 'chromium';
const scenario = process.env.SCENARIO || 'long-live';
const profiling = process.env.PROFILE === '1';
const setup = process.env.SETUP || 'phone';
const phoneSetup = setup === 'phone';
const sessionTiles = Number(process.env.SESSION_TILES || 1);
const historyTurns = Number(process.env.HISTORY_TURNS || 950);
const childCount = Number(process.env.CHILDREN || 0);
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const protocol = { setup, sessionTiles, historyTurns, childCount, singleTile: process.env.SINGLE_TILE === '1',
  crossTile: process.env.CROSS_TILE === '1',
  observedChildren: process.env.OBSERVED === '1', nonObservedChildren: Number(process.env.NONOBSERVED || childCount),
  rowPadding: Number(process.env.ROW_PADDING || 0),
  attentionMs: Number(process.env.ATTENTION_MS || 0), childUpdates: process.env.CHILD_UPDATES === '1',
  viewport: phoneSetup ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
  deviceScaleFactor: phoneSetup ? 3 : 1, isMobile: phoneSetup, hasTouch: phoneSetup,
  cpuSlowdown: Number(process.env.CPU || (engine === 'chromium' && setup !== 'desktop' ? 6 : 1)),
  warmupMs: 15000, characters: 200, keyDelayMs: 40, streamDelayMs: 50,
  deleteDraftCharacters: 4096, deleteRepeats: 450, deleteDelayMs: 40,
  initialReplyParagraphs: 240, measurement: 'EventTiming input-to-next-paint when supported (8ms quantization, unreported <16ms entries conservatively recorded as 16ms); otherwise input-to-next-rAF pre-render proxy only' };

function assetIdentity(root) {
  root = path.resolve(root);
  const files = fs.readdirSync(root).filter(file => file.endsWith('.wasm'));
  assert.equal(files.length, 1, 'serve exactly one immutable UI build per ROOT');
  const wasm = files[0], bytes = fs.readFileSync(path.join(root, wasm));
  return { root, wasm, bytes: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex'),
    indexSha256: crypto.createHash('sha256').update(fs.readFileSync(path.join(root, 'index.html'))).digest('hex') };
}

// Native loopback WebSocket frames avoid one Playwright RPC per replay event.
// The old routed fake stream can OOM the *Node harness* with many long tiles.
function wire(message) {
  const body = Buffer.from(JSON.stringify(message));
  const header = Buffer.alloc(body.length < 126 ? 2 : body.length < 65536 ? 4 : 10);
  header[0] = 0x81;
  if (header.length === 2) header[1] = body.length;
  else if (header.length === 4) { header[1] = 126; header.writeUInt16BE(body.length, 2); }
  else { header[1] = 127; header.writeBigUInt64BE(BigInt(body.length), 2); }
  return Buffer.concat([header, body]);
}

function history(short = scenario.startsWith('short-'), children = childCount) {
  let seq = 66;
  const events = [];
  const add = event => events.push({ seq: seq++, ts: 1700000000000 + seq * 10, ...event });
  const last = short ? 68 : 66 + historyTurns;
  for (let turn = 66; turn < last; turn++) {
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
  for (let i = 0; i < children; i++) add({ type: 'subagent_spawned', call_id: `task-${i}`, child_session_id: `child-${i}`,
    agent: '', description: `Retained child ${i}`, background_task_id: `bg-child-${i}`, connection_id: 'test', model: 'test', effort: '' });
  if (children) add({ type: 'background_task', task_id: 'shell-1', kind: 'shell', description: 'Fixture shell',
    state: 'started', exit_code: 0, elapsed_ms: 0, notify: true, native_type: '', foreground: false });
  return events;
}

function instrument(diagnostic) {
  window.__phone = { keys: [], tasks: [], events: [], fetches: [], phase: null, mutations: 0, timers: [],
    eventTimingSupported: PerformanceObserver.supportedEntryTypes?.includes('event') || false };
  const observe = (type, callback, extra = {}) => {
    if (PerformanceObserver.supportedEntryTypes?.includes(type)) new PerformanceObserver(callback).observe({ type, ...extra });
  };
  observe('longtask', list => {
    __phone.tasks.push(...list.getEntries().map(e => ({ start: e.startTime, duration: e.duration })));
  });
  observe('event', list => {
    __phone.events.push(...list.getEntries().map(e => ({ name: e.name, start: e.startTime, duration: e.duration,
      processingStart: e.processingStart, processingEnd: e.processingEnd, interaction: e.interactionId })));
  }, { durationThreshold: 16 });
  let keydown;
  document.addEventListener('keydown', event => {
    if (__phone.phase && event.target.matches('textarea')) keydown = event.timeStamp;
  }, true);
  document.addEventListener('input', event => {
    if (!__phone.phase || !event.target.matches('textarea')) return;
    const start = event.timeStamp, dispatch = performance.now(), down = keydown;
    requestAnimationFrame(() => {
      const frame = performance.now();
      const key = { start, keydownStart: down, dispatch, frame, rafDuration: frame - start, keydownRafDuration: frame - down };
      __phone.keys.push(key);
      // Diagnostic only: this task can starve behind WebSocket work, long after
      // the frame painted. Never mislabel its delay as paint latency.
      setTimeout(() => { key.afterRafTaskDuration = performance.now() - start; }, 0);
    });
  }, true);
  const fetch = window.fetch;
  window.fetch = (...args) => {
    if (__phone.phase) __phone.fetches.push(new URL(args[0] instanceof Request ? args[0].url : args[0], location.href).pathname);
    return fetch(...args);
  };
  const interval = window.setInterval;
  window.setInterval = (callback, period, ...args) => {
    const timer = { period, count: 0, ms: 0, stack: new Error().stack };
    __phone.timers.push(timer);
    return interval(() => {
      const start = performance.now();
      callback(...args);
      if (__phone.phase) { timer.count++; timer.ms += performance.now() - start; }
    }, period);
  };
  new MutationObserver(records => {
    if (__phone.phase) __phone.mutations += records.filter(r => {
      const target = r.target.nodeType === Node.ELEMENT_NODE ? r.target : r.target.parentElement;
      return target?.closest('.md');
    }).length;
  }).observe(document, { subtree: true, characterData: true, childList: true });
  if (diagnostic) {
    __phone.diagnostic = { innerHTML: [], scrollHeight: [], bounds: [], storage: [], fieldSizing: CSS.supports('field-sizing', 'content') };
    const store = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      const start = performance.now(), result = store.call(this, key, value);
      if (__phone.phase) __phone.diagnostic.storage.push({ start, ms: performance.now() - start, key, bytes: value.length, stack: new Error().stack });
      return result;
    };
    const html = Object.getOwnPropertyDescriptor(Element.prototype, 'innerHTML');
    Object.defineProperty(Element.prototype, 'innerHTML', { ...html, set(value) {
      if (!__phone.phase) return html.set.call(this, value);
      const start = performance.now(), stack = new Error().stack;
      const result = html.set.call(this, value);
      __phone.diagnostic.innerHTML.push({ start, ms: performance.now() - start, bytes: value.length, tag: this.tagName, className: this.className, stack });
      return result;
    } });
    const height = Object.getOwnPropertyDescriptor(Element.prototype, 'scrollHeight');
    Object.defineProperty(Element.prototype, 'scrollHeight', { ...height, get() {
      if (!__phone.phase) return height.get.call(this);
      const start = performance.now(), result = height.get.call(this);
      __phone.diagnostic.scrollHeight.push({ start, ms: performance.now() - start, tag: this.tagName, stack: new Error().stack });
      return result;
    } });
    const bounds = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = function (...args) {
      if (!__phone.phase) return bounds.apply(this, args);
      const start = performance.now(), result = bounds.apply(this, args);
      __phone.diagnostic.bounds.push({ start, ms: performance.now() - start, tag: this.tagName, className: this.className, stack: new Error().stack });
      return result;
    };
  }
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
  assert(['long-quiet', 'long-live', 'short-live', 'long-delete', 'short-delete', 'long-tools'].includes(scenario));
  assert(['phone', 'desktop', 'desktop6'].includes(setup));
  assert(!protocol.crossTile || scenario === 'long-delete', 'cross-tile comparison uses idle held Backspace');
  fs.mkdirSync(output, { recursive: true });
  const events = history(), streaming = scenario.endsWith('-live'), deleting = scenario.endsWith('-delete'), tools = scenario === 'long-tools';
  // Same three-tile fixture as background-work; on phones only session is shown.
  const layout = { tiles: ['session', 'work', 'projects'].map((view, i) => ({ key: 2 + i * 2,
    columns: [{ key: 3 + i * 2, view: view === 'session' ? { view, id: 'parent' } : { view } }],
    cursor: 0, active: 3 + i * 2, used: 3 + i * 2, back: [], forward: [], stack: null, minimized: false })),
  focus: 0, next: 8, full: false };
  if (protocol.singleTile) layout.tiles = layout.tiles.slice(0, 1);
  const companions = Array.from({ length: sessionTiles - 1 }, (_, i) => session(`companion-${i + 1}`));
  if (protocol.crossTile) companions.unshift(session('short'));
  const children = Array.from({ length: childCount }, (_, i) => ({ ...session(`child-${i}`, i < 2 ? 'working' : 'idle'),
    parent_id: 'parent', parent_call_id: `task-${i}`, root_id: 'parent', title: `Retained child ${i}`,
    observed: process.env.OBSERVED === '1' || i >= Number(process.env.NONOBSERVED || childCount),
    auto_labels: protocol.rowPadding ? [`folder:/tmp/${'x'.repeat(protocol.rowPadding)}`] : [],
    end_reason: i < 2 ? null : 'completed' }));
  for (const [i, s] of companions.entries()) layout.tiles.push({ key: 8 + i * 2,
    columns: [{ key: 9 + i * 2, view: { view: 'session', id: s.id } }],
    cursor: 0, active: 9 + i * 2, used: 9 + i * 2, back: [], forward: [], stack: null, minimized: false });
  layout.next = 8 + companions.length * 2;
  if (protocol.crossTile) {
    layout.tiles.unshift(layout.tiles.splice(layout.tiles.findIndex(t => t.key === 8), 1)[0]);
    layout.focus = 1; // Visit the long tile first; cold hidden phone tiles mount lazily.
  }
  const { server, origin } = await serve();
  const sockets = new Set();
  const socketSessions = new Map();
  // Native replay replaces the helper's initial 65 events with our full history.
  const replay = Buffer.concat([...events.map(event => wire({ type: 'event', event })), wire({ type: 'synced' })]);
  const shortEvents = history(true, 0);
  const shortReplay = Buffer.concat([...shortEvents.map(event => wire({ type: 'event', event })), wire({ type: 'synced' })]);
  const allowedStreams = new Set(['parent', ...companions.map(s => s.id)]);
  server.on('upgrade', (request, socket) => {
    const pathname = new URL(request.url, origin).pathname;
    if (!pathname.endsWith('/stream')) return socket.destroy();
    const id = pathname.split('/').at(-2);
    // Companions intentionally stress repeated long histories, but an actual
    // child feed must never silently receive its parent's replay.
    assert(allowedStreams.has(id), `unexpected session stream: ${id}; add an explicit per-session fixture`);
    const accept = crypto.createHash('sha1').update(request.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    sockets.add(socket);
    socketSessions.set(socket, id);
    socket.on('error', () => sockets.delete(socket));
    socket.on('close', () => { sockets.delete(socket); socketSessions.delete(socket); });
    socket.write(id === 'short' ? shortReplay : replay);
  });
  const browser = await launchBrowser(playwright, engine);
  const childResponses = [];
  const liveSockets = new Set();
  const newContext = browser.newContext.bind(browser);
  browser.newContext = async options => {
    const context = await newContext(options), route = context.routeWebSocket.bind(context);
    // Keep the helper's tiny live/snapshot fake; do not intercept native stream.
    context.routeWebSocket = (pattern, handler) => route(pattern === '**/*' ? '**/live*' : pattern, socket => {
      liveSockets.add(socket);
      return handler(socket);
    });
    return context;
  };
  let f, producer, attention, cdp;
  try {
    // upstream/main predates motion policy; do not inject later product code.
    const hasMotion = fs.readFileSync(path.join(root, 'index.html'), 'utf8').includes('motion.js');
    f = await fixture(browser, origin, protocol.viewport, 'dark', { choice: 'full', os: 'no-preference', effective: hasMotion ? 'full' : null },
      progress => { f = progress; }, { deviceScaleFactor: protocol.deviceScaleFactor, isMobile: phoneSetup, hasTouch: phoneSetup }, {
        instrumentation: false, layout, events, sessions: companions,
        path: '/sessions/parent', ready: '#col-3 textarea',
        setup: async ({ page }) => {
          await page.addInitScript(instrument, process.env.DIAGNOSTICS === '1');
          for (const s of [session(), ...companions]) await page.route(`**/api/v1/sessions/${s.id}`, route => route.fulfill({ json: { session: { ...s, last_seq: (s.id === 'short' ? shortEvents : events).at(-1).seq }, worker_name: 'Fixture', worker_online: true, queued: [] } }));
          await page.route('**/api/v1/sessions/short/children*', route => route.fulfill({ json: [] }));
          await page.route('**/api/v1/sessions/parent/children*', route => {
            childResponses.push({ url: route.request().url(), count: children.length, at: Date.now() });
            if (protocol.childUpdates && children.length) { children[0].updated_at = Date.now(); children[0].last_seq++; }
            return route.fulfill({ json: children });
          });
          await page.route('**/attachments/*/*', route => route.fulfill({ contentType: 'image/svg+xml', body: '<svg xmlns="http://www.w3.org/2000/svg" width="320" height="200"><rect width="320" height="200" fill="#2563eb"/></svg>' }));
        },
      });
    f.stream = message => { const frame = wire(message); for (const socket of sockets) if (socketSessions.get(socket) !== 'short') socket.write(frame); };
    f.live = message => { for (const socket of liveSockets) socket.send(JSON.stringify(message)); };
    const { page } = f;
    await page.getByText(`Change ${scenario.startsWith('short-') ? 67 : 65 + historyTurns} checked.`, { exact: false }).first().waitFor({ state: 'attached' });
    if (protocol.crossTile) {
      // Real workspace shortcut, not a reload: retain the expensive pane owner.
      await page.keyboard.press('Control+1');
      await page.locator('#col-9').getByText('Change 67 checked.', { exact: false }).waitFor();
      assert.equal(sockets.size, sessionTiles + 1, 'long and short session feeds must both remain mounted');
    }
    await delay(protocol.warmupMs);
    if (childCount) console.log('CHILD FIXTURE', JSON.stringify({ responses: childResponses.length,
      labels: await page.locator('button[aria-label*="session sheet"]').evaluateAll(bs => bs.map(b => b.getAttribute('aria-label'))) }));
    if (engine === 'chromium') {
      cdp = await f.context.newCDPSession(page);
      // Retain CDP until context close: detaching resets phone touch emulation.
      await cdp.send('Emulation.setCPUThrottlingRate', { rate: protocol.cpuSlowdown });
      await cdp.send('Performance.enable');
    }
    const phone = await page.evaluate(() => ({ coarse: matchMedia('(pointer: coarse)').matches,
      touch: navigator.maxTouchPoints > 0 || 'ontouchstart' in window, dpr: devicePixelRatio,
      width: innerWidth, height: innerHeight }));
    assert.equal(phone.coarse, phoneSetup, JSON.stringify(phone));
    assert.equal(phone.dpr, protocol.deviceScaleFactor, JSON.stringify(phone));
    if (phoneSetup) assert(phone.touch, JSON.stringify(phone));
    let deltas = 0;
    const seq = events.at(-1).seq + 1;
    if (streaming || tools) {
      await f.setState('working', { last_seq: seq });
      f.stream({ type: 'event', event: { seq, ts: Date.now(), type: 'turn_started', turn: 1016 } });
      if (streaming) f.stream({ type: 'text', turn: 1016, seq: seq + 1, text: 'Streaming investigation\n\n' +
        'A **rendered** paragraph with `inline code`, a [local link](/r/home/sessions/parent), and background investigation details.\n\n'.repeat(protocol.initialReplyParagraphs) });
      if (streaming) await page.getByText('Streaming investigation', { exact: false }).first().waitFor();
    }
    const area = page.locator(protocol.crossTile ? '#col-9 textarea' : 'textarea').first();
    await area.focus();
    const draft = 'A long draft paragraph with several words to delete while the session is idle.\n'.repeat(60).slice(0, protocol.deleteDraftCharacters);
    if (deleting) {
      await area.fill(draft);
      await delay(2200);
    }
    if (profiling && cdp) {
      await cdp.send('Profiler.enable');
      await cdp.send('Profiler.start');
      await cdp.send('Tracing.start', { categories: 'devtools.timeline,blink.user_timing,v8.execute,disabled-by-default-devtools.timeline.stack', transferMode: 'ReturnAsStream' });
    }
    const metrics = async () => cdp ? Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map(m => [m.name, m.value])) : {};
    const text = 'Please examine the background activity while I type this draft, preserve every feature, and keep the conversation responsive with several tiles open and a very long history. Show the measured results.  '.slice(0, protocol.characters);
    const before = await metrics();
    const phaseStart = await page.evaluate(deleting => {
      __phone.phase = 'typing'; performance.mark('phone-typing-start');
      if (deleting) {
        const area = document.activeElement;
        if (!area?.matches('textarea')) throw new Error('deletion sampler must follow the focused composer');
        __phone.frames = [];
        let lastLength = area.value.length, lastTime = performance.now();
        const sample = () => {
          if (!__phone.phase) return;
          const time = performance.now(), length = area.value.length;
          if (length !== lastLength) {
            __phone.frames.push({ time, length, deleted: lastLength - length, gap: time - lastTime });
            lastLength = length; lastTime = time;
          }
          requestAnimationFrame(sample);
        };
        requestAnimationFrame(sample);
      }
      return performance.now();
    }, deleting);
    const start = Date.now();
    if (protocol.attentionMs) attention = setInterval(() => {
      f.live({ type: 'attention' });
    }, protocol.attentionMs);
    if (streaming) producer = setInterval(() => {
      deltas++;
      f.stream({ type: 'text', turn: 1016, seq: seq + 1, text: `Another **streamed** result ${deltas}.\n\n` });
    }, protocol.streamDelayMs);
    if (tools) producer = setInterval(() => {
      deltas++;
      const call = `arriving-${deltas}`;
      f.stream({ type: 'event', event: { seq: seq + deltas * 2, ts: Date.now(), type: 'assistant', turn: 1016,
        blocks: [{ type: 'tool_use', id: call, name: 'shell', input_json: '{"command":["echo","arriving"]}' }], stop_reason: 'tool_use' } });
      f.stream({ type: 'event', event: { seq: seq + deltas * 2 + 1, ts: Date.now(), type: 'tool_result',
        call_id: call, tool: 'shell', output: 'One arriving tool result. '.repeat(16), is_error: false, truncated: false, duration_ms: 10, images: 0 } });
    }, 500);
    if (deleting) {
      const pending = [];
      // Independent arrivals, not key→ack→next-key: emulate native held-key
      // repeat. Repeated keyboard.down sets autoRepeat on the native key event.
      await new Promise(resolve => {
        let sent = 0;
        producer = setInterval(() => {
          pending.push(page.keyboard.down('Backspace'));
          if (++sent === protocol.deleteRepeats) { clearInterval(producer); resolve(); }
        }, protocol.deleteDelayMs);
      });
      await Promise.all(pending);
      await page.keyboard.up('Backspace');
    } else await area.pressSequentially(text, { delay: protocol.keyDelayMs });
    await page.waitForFunction(n => __phone.keys.length === n, deleting ? protocol.deleteRepeats : text.length);
    clearInterval(producer);
    clearInterval(attention);
    const wallMs = Date.now() - start;
    const phaseEnd = await page.evaluate(() => { performance.mark('phone-typing-end'); __phone.phase = null; return performance.now(); });
    const after = await metrics();
    if (profiling && cdp) {
      const { profile } = await cdp.send('Profiler.stop');
      fs.writeFileSync(path.join(output, `${label}.cpuprofile`), JSON.stringify(profile));
      await endTrace(cdp, path.join(output, `${label}-trace.json`));
      await page.screenshot({ path: path.join(output, `${label}.png`) });
    }
    // Allow batched EventTiming/long-task delivery, outside the measured window.
    await delay(300);
    const observed = await page.evaluate(() => __phone);
    observed.tasks = observed.tasks.filter(e => e.start >= phaseStart && e.start < phaseEnd);
    observed.events = observed.events.filter(e => e.start >= phaseStart && e.start < phaseEnd);
    for (const key of observed.keys) {
      const event = observed.events.find(e => e.name === 'input' && Math.abs(e.start - key.start) < .1);
      const down = observed.events.find(e => e.name === 'keydown' && Math.abs(e.start - key.keydownStart) < .1);
      key.eventDuration = event?.duration ?? null;
      key.duration = observed.eventTimingSupported ? event?.duration ?? 16 : key.rafDuration;
      key.keydownDuration = observed.eventTimingSupported ? down?.duration ?? 16 : key.keydownRafDuration;
    }
    const quantiles = key => {
      const sorted = observed.keys.map(k => k[key]).sort((a, b) => a - b);
      const q = p => sorted[Math.ceil(sorted.length * p) - 1];
      return { count: sorted.length, p50: q(.5), p95: q(.95), max: sorted.at(-1) };
    };
    const gaps = (observed.frames || []).slice(1).map(f => f.gap).sort((a, b) => a - b);
    const deletionGaps = gaps.length ? { count: gaps.length, p50: gaps[Math.ceil(gaps.length * .5) - 1],
      p95: gaps[Math.ceil(gaps.length * .95) - 1], max: gaps.at(-1), maxBatch: Math.max(...observed.frames.map(f => f.deleted)) } : null;
    const summary = { label, scenario, engine, browser: browser.version(), protocol, profiling, phone,
      build: assetIdentity(root),
      harnessSha256: crypto.createHash('sha256').update(fs.readFileSync(__filename)).digest('hex'),
      helperSha256: crypto.createHash('sha256').update(fs.readFileSync(require.resolve(process.env.MOTION_HARNESS || './motion.cjs'))).digest('hex'),
      historyEvents: events.length, typingHistoryEvents: protocol.crossTile ? shortEvents.length : events.length,
      streamConnections: sockets.size, streamSessions: [...socketSessions.values()], replayPolicy: 'all synthetic history, no server tail pruning', deltas, wallMs, startWallMs: start, phaseStart, phaseEnd,
      childResponses,
      latencyKind: observed.eventTimingSupported ? 'EventTiming input-to-next-paint; <16ms censored' : 'input-to-next-rAF pre-render proxy',
      latency: quantiles('duration'), rafLatency: quantiles('rafDuration'), keydownLatency: quantiles('keydownDuration'),
      censoredKeys: observed.keys.filter(k => k.eventDuration === null).length,
      deletionGaps, firstDeletionGap: observed.frames?.[0]?.gap ?? null,
      taskMs: cdp ? (after.TaskDuration - before.TaskDuration) * 1000 : null,
      scriptMs: cdp ? (after.ScriptDuration - before.ScriptDuration) * 1000 : null,
      layoutMs: cdp ? (after.LayoutDuration - before.LayoutDuration) * 1000 : null,
      longTaskMs: engine === 'chromium' ? observed.tasks.reduce((n, t) => n + t.duration, 0) : null,
      dom: await page.evaluate(() => ({ nodes: document.querySelectorAll('*').length, markdown: document.querySelectorAll('.md').length,
        textareas: [...document.querySelectorAll('textarea')].map(a => ({ visible: a.getClientRects().length > 0, length: a.value.length })) })),
      observed, errors: f.errors };
    fs.writeFileSync(path.join(output, `${label}.json`), JSON.stringify(summary, null, 2));
    assert.equal(await area.inputValue(), deleting ? draft.slice(0, -protocol.deleteRepeats) : text, 'all expected edits retained');
    assert.deepEqual(f.errors, []);
    console.log(JSON.stringify({ label, latency: summary.latency, deletionGaps, longTaskMs: summary.longTaskMs, deltas, wallMs }));
  } finally {
    clearInterval(producer);
    clearInterval(attention);
    await f?.context.close();
    await browser.close();
    for (const socket of sockets) socket.destroy();
    await new Promise(resolve => server.close(resolve));
  }
}

function repeat() {
  const pairs = Number(process.env.PAIRS);
  assert(Number.isInteger(pairs) && pairs >= 5);
  assert(!profiling, 'profile separately from timing cohorts');
  const scenarios = (process.env.SCENARIOS || 'long-quiet,long-live,short-live').split(',');
  const roots = { base: path.resolve(process.env.BASE_ROOT), final: path.resolve(process.env.FINAL_ROOT) };
  fs.mkdirSync(output, { recursive: true });
  const builds = Object.fromEntries(Object.entries(roots).map(([name, root]) => [name, assetIdentity(root)]));
  assert.notEqual(builds.base.sha256, builds.final.sha256, 'A/B artifacts are identical; verify build provenance');
  const manifest = { started: new Date().toISOString(), hostname: os.hostname(), node: process.version, engine,
    harnessSha256: crypto.createHash('sha256').update(fs.readFileSync(__filename)).digest('hex'),
    helperSha256: crypto.createHash('sha256').update(fs.readFileSync(require.resolve(process.env.MOTION_HARNESS || './motion.cjs'))).digest('hex'),
    pairs, scenarios, builds, protocol, trials: [] };
  for (let pair = 1; pair <= pairs; pair++) for (const [index, s] of scenarios.entries()) {
    const order = (pair + index) % 2 ? ['base', 'final'] : ['final', 'base'];
    for (const build of order) {
      const name = `${label}-${s}-p${pair}-${build}`;
      const log = fs.openSync(path.join(output, `${name}.log`), 'w');
      const trial = { pair, scenario: s, build, label: name, started: new Date().toISOString(), load: os.loadavg() };
      console.log(`START ${name}`);
      const result = spawnSync(process.execPath, [__filename], { timeout: 300000,
        env: { ...process.env, PAIRS: '', ROOT: roots[build], LABEL: name, SCENARIO: s }, stdio: ['ignore', log, log] });
      fs.closeSync(log);
      Object.assign(trial, { finished: new Date().toISOString(), status: result.status, signal: result.signal });
      manifest.trials.push(trial);
      fs.writeFileSync(path.join(output, `${label}-manifest.json`), JSON.stringify(manifest, null, 2));
      assert.equal(result.status, 0, `${name} failed; preserve and inspect log`);
      console.log(`PASS ${name}`);
    }
  }
  const spread = values => { const a = [...values].sort((a, b) => a - b); return { median: a[Math.floor(a.length / 2)], min: a[0], max: a.at(-1), values }; };
  const summary = {};
  for (const s of scenarios) {
    const data = Object.fromEntries(['base', 'final'].map(build => [build, manifest.trials.filter(t => t.scenario === s && t.build === build)
      .map(t => JSON.parse(fs.readFileSync(path.join(output, `${t.label}.json`))))]));
    const metrics = { p50: d => d.latency.p50, p95: d => d.latency.p95, max: d => d.latency.max,
      keydownP95: d => d.keydownLatency.p95, keydownMax: d => d.keydownLatency.max,
      gapP95: d => d.deletionGaps?.p95 ?? null, gapMax: d => d.deletionGaps?.max ?? null,
      longTaskMs: d => d.longTaskMs, taskMs: d => d.taskMs, scriptMs: d => d.scriptMs, layoutMs: d => d.layoutMs, wallMs: d => d.wallMs };
    summary[s] = Object.fromEntries(Object.entries(metrics).map(([metric, get]) => {
      const base = data.base.map(get), final = data.final.map(get);
      return [metric, base.some(v => v === null) ? null : { base: spread(base), final: spread(final), pairedDelta: spread(base.map((v, i) => final[i] - v)) }];
    }));
  }
  fs.writeFileSync(path.join(output, `${label}-summary.json`), JSON.stringify(summary, null, 2));
  console.log(JSON.stringify(summary, null, 2));
}

if (process.env.PAIRS) {
  try { repeat(); } catch (error) { console.error(error); process.exitCode = 1; }
} else main().catch(error => { console.error(error); process.exitCode = 1; });
