#!/usr/bin/env node
'use strict';

// Built production WASM/CSS, with a private loopback origin and fake head.
// No npm install, real head, credentials, fixed ports, or test-only UI required.
// See docs/testing.md, "Motion: production browser regression".
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');

const ROOT = path.resolve(process.env.ROOT || path.join(__dirname, '../dist'));
const DEADLINE = Number(process.env.C6_TEST_DEADLINE_SECS || 300) * 1000;
const SHOTS = process.env.SCREENSHOTS || '/tmp/m1-screenshots';
const VIEWPORTS = [
  ['desktop', { width: 1440, height: 1000 }],
  ['phone', { width: 390, height: 844 }],
  ['narrow', { width: 320, height: 568 }],
  ['landscape', { width: 900, height: 480 }],
];
const phoneViewport = viewport => viewport.width < 768 || viewport.height < 512;
const CASES = [
  { name: 'system', choice: null, os: 'no-preference', effective: 'full' },
  { name: 'system-reduce', choice: null, os: 'reduce', effective: 'reduced' },
  { name: 'full', choice: 'full', os: 'no-preference', effective: 'full' },
  { name: 'full-reduce', choice: 'full', os: 'reduce', effective: 'full' },
  { name: 'reduced', choice: 'reduced', os: 'no-preference', effective: 'reduced' },
  { name: 'off', choice: 'off', os: 'reduce', effective: 'off' },
];
const selected = (env, value) => !process.env[env] || process.env[env].split(',').includes(value);
async function within(label, action) {
  let timer;
  try {
    return await Promise.race([action(), new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label}: hang deadline (${DEADLINE}ms)`)), DEADLINE);
    })]);
  } finally { clearTimeout(timer); }
}
const worker = {
  id: '00000000-0000-0000-0000-000000000000', name: 'Motion fixture',
  hostname: 'fixture', os: 'linux', arch: 'x86_64', version: 'test', status: 'online',
  enrolled_at: 1, last_seen: 1, connected_since: 1, rtt_us: 0,
};
const session = (id = 'parent', state = 'idle') => ({
  id, realm: 'home', worker_id: worker.id, title: `Motion fixture ${id}`, cwd: '/tmp',
  connection_id: 'test', model: 'test', status: state === 'working' ? 'running' : 'idle',
  error: null, effort: null, created_at: 1, updated_at: 1, last_seq: 70,
  awaiting_approval: state === 'waiting', parent_id: null, parent_call_id: null,
  end_reason: state === 'failed' ? 'error' : null,
});
const layout = {
  tiles: [{ key: 2, columns: [{ key: 3, view: { view: 'session', id: 'parent' } }],
    cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false }],
  focus: 0, next: 4, full: false,
};

function initialEvents() {
  const events = [{ seq: 1, ts: 1, type: 'started', connection_id: 'test', model: 'test',
    cwd: '/tmp', system_prompt: '', tools: [], context_window: 128000 }];
  for (let seq = 2; seq < 65; seq++) events.push({ seq, ts: seq, type: 'assistant', turn: seq,
    blocks: [{ type: 'text', text: `Reply ${seq}. ${'Long readable transcript. '.repeat(12)}` }], stop_reason: 'end_turn' });
  events.push({ seq: 65, ts: 65, type: 'turn_ended', turn: 64, reason: 'completed' });
  return events;
}

async function serve() {
  assert(fs.existsSync(path.join(ROOT, 'index.html')),
    `No built UI at ${ROOT}. Ask the build owner to run env -u NO_COLOR make ui, or set ROOT.`);
  const server = http.createServer((req, res) => {
    const pathname = decodeURIComponent(new URL(req.url, 'http://127.0.0.1').pathname);
    if (pathname.startsWith('/api/')) { res.writeHead(500); res.end('Unmocked API'); return; }
    const file = path.resolve(ROOT, pathname === '/' || pathname.startsWith('/r/') ? 'index.html' : `.${pathname}`);
    if (!file.startsWith(ROOT + path.sep) || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
      res.writeHead(404); res.end('Not found'); return;
    }
    const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css',
      '.wasm': 'application/wasm', '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png' };
    res.writeHead(200, { 'content-type': mime[path.extname(file)] || 'application/octet-stream', 'cache-control': 'no-store' });
    fs.createReadStream(file).pipe(res);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return { server, origin: `http://127.0.0.1:${server.address().port}` };
}

// Installed before production code: observe actual effects, without creating
// animations, changing the DOM, or replacing application motion helpers.
function instrumentation() {
  if (typeof Notification === 'undefined') window.Notification = class { static permission = 'denied'; };
  const test = window.__motionTest = { effects: [], reads: [], frame: 0, watching: false, ignored: false };
  const seen = new WeakSet();
  const capture = animation => {
    if (seen.has(animation) || !animation.effect) return;
    seen.add(animation);
    const effect = animation.effect;
    test.effects.push({ tag: effect.target?.tagName, classes: String(effect.target?.className?.baseVal ?? effect.target?.className ?? ''),
      workspace: !!effect.target?.matches?.('[data-tile-motion], [data-pane-motion], [data-tile-reveal], [inert][aria-hidden="true"]'),
      tool: !!effect.target?.closest?.('.c6-tool-details'), pseudo: effect.pseudoElement || null,
      name: animation.animationName || animation.transitionProperty || 'web-animation',
      duration: effect.getTiming().duration, iterations: effect.getTiming().iterations,
      frames: effect.getKeyframes() });
  };
  const animate = Element.prototype.animate;
  Element.prototype.animate = function (...args) {
    const animation = animate.apply(this, args);
    capture(animation);
    return animation;
  };
  const sample = () => {
    test.frame++;
    for (const animation of document.getAnimations()) capture(animation);
    requestAnimationFrame(sample);
  };
  requestAnimationFrame(sample);
  // Count production geometry reads by animation frame, not by elapsed time.
  // FLIP may read before and after a change; it must not read on every frame.
  const record = element => {
    if (test.watching && !test.ignored && element.matches('[data-tile-motion], [data-pane-motion]')) {
      test.reads.push(test.frame);
    }
  };
  const bounds = Element.prototype.getBoundingClientRect;
  Element.prototype.getBoundingClientRect = function (...args) { record(this); return bounds.apply(this, args); };
  for (const name of ['offsetWidth', 'offsetHeight', 'offsetTop', 'offsetLeft']) {
    const descriptor = Object.getOwnPropertyDescriptor(HTMLElement.prototype, name);
    if (descriptor?.get) Object.defineProperty(HTMLElement.prototype, name,
      { ...descriptor, get() { record(this); return descriptor.get.call(this); } });
  }
  window.__keyboard = (height, top = 0) => {
    const viewport = visualViewport;
    Object.defineProperty(viewport, 'height', { configurable: true, value: height });
    Object.defineProperty(viewport, 'offsetTop', { configurable: true, value: top });
    viewport.dispatchEvent(new Event('resize'));
    viewport.dispatchEvent(new Event('scroll'));
  };
}

async function fixture(browser, origin, viewport, theme, motion, started, contextOptions = {}, seed = {}) {
  const debug = phase => { if (process.env.DEBUG_FIXTURE) console.log(`  FIXTURE ${phase}`); };
  debug('new context');
  const context = await browser.newContext({ viewport, hasTouch: phoneViewport(viewport),
    isMobile: phoneViewport(viewport), reducedMotion: motion.os, colorScheme: theme, serviceWorkers: 'block', ...contextOptions });
  debug('new page');
  const page = await context.newPage();
  debug('configure');
  page.setDefaultTimeout(DEADLINE);
  page.setDefaultNavigationTimeout(DEADLINE);
  const errors = [], requests = [], consoleMessages = [], streams = new Set(), lives = new Set();
  let fail;
  const crash = new Promise((_, reject) => { fail = reject; });
  crash.catch(() => {}); // The fixture may still be wiring its awaited signals.
  const progress = { page, context, errors, requests, consoleMessages, motion, theme, viewport, crash, phase: 'configure' };
  started(progress);
  let state = 'idle', sequence = 100, suggestion = null, marks = seed.marks || {};
  let sessionView = { catchup: true, fold: false }, deferredView;
  page.on('pageerror', error => {
    errors.push(String(error.stack || error)); console.error('PAGEERROR', error.stack || error);
    fail(new Error(`browser crash during ${progress.phase}: ${error.stack || error}`));
  });
  page.on('console', message => {
    if (['error', 'warning'].includes(message.type())) {
      consoleMessages.push({ type: message.type(), text: message.text() });
      if (consoleMessages.length > 50) consoleMessages.shift();
    }
    if (/bad stream message|unreachable|panicked at|ResizeObserver loop/.test(message.text())) errors.push(message.text());
  });
  await page.addInitScript(instrumentation);
  // Never send a request to another origin, even if production code regresses.
  await context.route('**/*', route => {
    const url = route.request().url();
    if (url.startsWith(origin + '/') || /^(blob|data):/.test(url)) return route.continue();
    errors.push(`Blocked external request: ${url}`);
    return route.abort();
  });
  await context.routeWebSocket('**/*', socket => {
    const url = new URL(socket.url());
    if (url.host !== new URL(origin).host) { errors.push(`Blocked external WebSocket: ${url}`); socket.close(); return; }
    if (url.pathname.endsWith('/live')) {
      lives.add(socket);
      socket.onClose(() => lives.delete(socket));
      socket.send(JSON.stringify({ type: 'sessions', sessions: [session('parent', state)] }));
      socket.send(JSON.stringify({ type: 'workers', workers: [worker] }));
      socket.send(JSON.stringify({ type: 'marks', marks }));
    } else if (url.pathname.endsWith('/stream')) {
      streams.add(socket);
      for (const event of [...initialEvents(), ...(seed.events || [])]) socket.send(JSON.stringify({ type: 'event', event }));
      socket.send(JSON.stringify({ type: 'synced' }));
      socket.onClose(() => streams.delete(socket));
    } else { errors.push(`Unmocked WebSocket: ${url}`); socket.close(); }
  });
  await context.route('**/api/**', async route => {
    const request = route.request(), p = new URL(request.url()).pathname;
    if (!request.url().startsWith(origin + '/')) { errors.push(`Blocked external API: ${request.url()}`); await route.abort(); return; }
    requests.push(`${request.method()} ${p}`);
    let json = [];
    if (p.endsWith('/auth/status')) json = { authenticated: true, required: false, passkeys_here: false, has_passkeys: false };
    else if (p.endsWith('/elsewhere')) json = [{ id: 'home', name: 'Home', waiting: 0 }, { id: 'other', name: 'Other', waiting: 0 }];
    else if (p.endsWith('/version')) json = { version: 'motion-test' };
    else if (p.endsWith('/connections')) json = [{ id: 'test', name: 'Test', kind: 'openai_compat',
      base_url: 'http://127.0.0.1', default_model: 'test', context_window: null, supports_effort: true, has_api_key: true, created_at: 1 }];
    else if (p.endsWith('/connections/test/models')) json = ['test', ...Array.from({ length: 32 }, (_, i) => `fixture-model-${i + 1}`)];
    else if (p.endsWith('/settings/transcript')) json = { show_thinking: false };
    else if (p.endsWith('/settings/session_bar')) json = { colour: 'tinted' };
    else if (p.endsWith('/settings/facets')) json = {};
    else if (p.endsWith('/attention')) json = { waiting: [], worth: [], elsewhere: [] };
    else if (p.endsWith('/sessions/parent')) json = { session: session('parent', state), worker_name: worker.name, worker_online: true, queued: [] };
    else if (p.endsWith('/sessions/parent/suggestion')) {
      if (suggestion === null) return route.fulfill({ status: 204 });
      json = suggestion;
    }
    else if (p.endsWith('/sessions/parent/catchup')) return route.fulfill({ status: 204 });
    else if (p.endsWith('/sessions/parent/view')) {
      if (request.method() === 'PUT') {
        const value = request.postDataJSON(), pending = deferredView;
        deferredView = undefined;
        pending?.received(value);
        await pending?.reply;
        sessionView = value;
      }
      json = sessionView;
    }
    else if (p.endsWith('/messages')) json = { command_id: 'motion-message', text: request.postDataJSON().text, created_at: 1, rejected: null };
    else if (p.endsWith('/interrupt')) json = {};
    else if (p.endsWith('/git/status')) json = { branch: 'fixture', files: [], clean: true };
    else if (p.endsWith('/git/diff')) json = { diff: '' };
    else if (p.endsWith('/ui/error')) { errors.push(`UI crash report: ${request.postData()}`); json = {}; }
    await route.fulfill({ json });
  });
  await page.route('**/__seed', route => route.fulfill({ contentType: 'text/html', body: '<!doctype html><title>fixture seed</title>' }));
  // Standalone production harnesses can start on their real page/layout and
  // install API fixtures before WASM mounts; no reload of a live app needed.
  await seed.setup?.({ page, context });
  progress.phase = 'seed';
  debug(progress.phase);
  await page.goto(origin + '/__seed');
  await page.evaluate(({ layout, theme, choice }) => {
    localStorage.setItem('c6.home.workspace.v2', JSON.stringify(layout));
    localStorage.setItem('c6.theme', theme);
    if (choice !== null) localStorage.setItem('c6.motion', choice);
  }, { layout: seed.layout || layout, theme, choice: motion.choice });
  progress.phase = 'application document';
  debug(progress.phase);
  await page.goto(origin + '/r/home' + (seed.path || '/sessions/parent'), { waitUntil: 'domcontentloaded' });
  // This checks pre-hydration policy, not a late correction after WASM starts.
  assert.equal(await page.locator('html').getAttribute('data-motion'), motion.effective, 'motion must resolve in the head script');
  progress.phase = 'WASM hydration';
  debug(progress.phase);
  await Promise.race([page.locator(seed.ready || 'textarea').first().waitFor(), crash]);
  progress.phase = 'initial animation completion';
  debug(progress.phase);
  if (process.env.DEBUG_FIXTURE) console.log('  INITIAL EFFECTS', await page.evaluate(() => document.getAnimations().map(a => ({
    name: a.animationName || a.transitionProperty, state: a.playState, duration: a.effect?.getTiming().duration,
    iterations: a.effect?.getTiming().iterations, end: a.effect?.getComputedTiming().endTime,
    classes: String(a.effect?.target?.className?.baseVal ?? a.effect?.target?.className),
    visibility: a.effect?.target instanceof Element ? getComputedStyle(a.effect.target).visibility : null,
  }))));
  await Promise.race([settle(page), crash]);
  const send = message => { for (const socket of streams) socket.send(JSON.stringify(message)); };
  return Object.assign(progress, {
    setMarks: given => { marks = given; for (const live of lives) live.send(JSON.stringify({ type: 'marks', marks })); },
    setSuggestion: text => { suggestion = text === null ? null : { text, for_seq: 65 }; },
    deferViewWrite: () => {
      let received, release;
      const request = new Promise(resolve => { received = resolve; });
      const reply = new Promise(resolve => { release = resolve; });
      deferredView = { received, reply };
      return { request, release };
    },
    send: event => send({ type: 'event', event: { seq: sequence++, ts: Date.now(), ...event } }),
    stream: send,
    output: text => send({ type: 'tool_output', call_id: 'motion-tool', text }),
    setState: async (next, patch = {}) => {
      assert(lives.size, 'live fixture connected'); state = next;
      for (const live of lives) {
        live.send(JSON.stringify({ type: 'sessions', sessions: [{ ...session('parent', state), ...patch }] }));
        live.send(JSON.stringify({ type: 'attention' }));
      }
    },
  });
}

// Animation completion is the condition. Infinite working indicators are not
// a completion signal; captured separately by assertMotion(). No fixed sleeps.
// Poll state rather than awaiting a stale CSS finished promise: WebKit can
// suspend those promises inside closed native details indefinitely.
async function settle(page) {
  await page.evaluate(async () => {
    const frame = () => new Promise(resolve => requestAnimationFrame(resolve));
    await frame(); await frame();
  });
  await page.waitForFunction(() => !document.getAnimations().some(animation => {
    if (animation.effect?.getTiming().iterations === Infinity || !['running', 'pending'].includes(animation.playState)) return false;
    const target = animation.effect?.target;
    if (!(target instanceof Element)) return true;
    const closed = target.closest('details:not([open])');
    if (closed && !closed.querySelector('summary')?.contains(target)) return false;
    return target.getClientRects().length > 0 && getComputedStyle(target).visibility !== 'hidden';
  }), null, { polling: 'raf' });
}

async function assertMotion(f) {
  const result = await f.page.evaluate(() => {
    const effects = __motionTest.effects;
    const seconds = value => value.split(',').some(v => parseFloat(v) > 0);
    const activeCSS = [...document.querySelectorAll('body *')].filter(el => {
      const css = getComputedStyle(el);
      return seconds(css.transitionDuration) || (css.animationName !== 'none' && seconds(css.animationDuration));
    }).map(el => `${el.tagName}.${String(el.className?.baseVal ?? el.className).slice(0, 100)}`);
    const moving = frame => {
      if (frame.transform && frame.transform !== 'none') {
        try { if (!new DOMMatrix(frame.transform).isIdentity) return true; } catch (_) { return true; }
      }
      if (frame.translate && frame.translate !== 'none' && frame.translate.split(/\s+/).some(v => parseFloat(v) !== 0)) return true;
      if (frame.scale && frame.scale !== 'none' && frame.scale.split(/\s+/).some(v => parseFloat(v) !== 1)) return true;
      if (frame.rotate && frame.rotate !== 'none' && parseFloat(frame.rotate) !== 0) return true;
      return false;
    };
    return { effects, activeCSS, travel: effects.filter(effect => effect.frames.some(moving)) };
  });
  if (f.motion.effective === 'off') {
    assert.deepEqual(result.activeCSS, [], 'Off must remove CSS durations, not merely hide the animation');
    assert.deepEqual(result.effects.filter(e => Number(e.duration) > 0), [], 'Off must not start timed effects');
  } else if (f.motion.effective === 'reduced') {
    assert.deepEqual(result.travel, [], 'Reduced must not travel, scale, or rotate');
    assert.deepEqual(result.effects.filter(e => e.iterations === Infinity || /spin|pulse|breathe|ring-arc/.test(e.name)), [], 'Reduced must not spin or pulse');
    assert.deepEqual(result.effects.filter(e => Number(e.duration) > 160), [], 'Reduced effects must be short');
    assert.deepEqual(result.effects.filter(e => e.frames.some(frame =>
      Object.keys(frame).some(key => /^(height|width|blockSize|inlineSize)$/.test(key)))), [], 'Reduced must fade, not interpolate layout');
  }
}

async function policy(f) {
  const { page, motion } = f;
  f.phase = 'policy/initial';
  assert.deepEqual(await page.evaluate(() => ({ choice: c6Motion.get(), reduced: c6Motion.reduced(), off: c6Motion.off(), stored: localStorage.getItem('c6.motion') })),
    { choice: motion.choice || 'system', reduced: motion.effective !== 'full', off: motion.effective === 'off', stored: motion.choice });
  // Browser-scoped, not a head preference: update live, persist over reload,
  // and share with another realm URL and another tab on this origin.
  for (const choice of ['off', 'reduced', 'full', 'system']) {
    f.phase = `policy/set-${choice}`;
    await page.evaluate(choice => c6Motion.set(choice), choice);
    const effective = choice === 'system' ? (motion.os === 'reduce' ? 'reduced' : 'full') : choice;
    assert.equal(await page.locator('html').getAttribute('data-motion'), effective);
    assert.equal(await page.evaluate(() => localStorage.getItem('c6.motion')), choice);
  }
  f.phase = 'policy/os-change';
  await page.emulateMedia({ reducedMotion: motion.os === 'reduce' ? 'no-preference' : 'reduce' });
  await page.waitForFunction(expected => document.documentElement.dataset.motion === expected,
    motion.os === 'reduce' ? 'full' : 'reduced');
  await page.evaluate(() => c6Motion.set('full'));
  await page.emulateMedia({ reducedMotion: 'reduce' });
  assert.equal(await page.locator('html').getAttribute('data-motion'), 'full', 'explicit Full overrides OS reduction');
  f.phase = 'policy/appearance-hydration';
  const tab = await f.context.newPage();
  tab.setDefaultTimeout(DEADLINE);
  tab.on('pageerror', error => f.errors.push(`Appearance tab: ${error.stack || error}`));
  await tab.goto(new URL('/r/other/settings/appearance', page.url()).href, { waitUntil: 'domcontentloaded' });
  assert.equal(await tab.evaluate(() => c6Motion.get()), 'full', 'motion is not realm-scoped');
  await tab.getByRole('heading', { name: 'Motion', exact: true }).waitFor();
  const choices = tab.locator('input[name|="motion"]');
  assert.equal(await choices.count(), 4, 'Appearance exposes all four real motion choices');
  for (const [index, choice] of ['system', 'full', 'reduced', 'off'].entries()) {
    f.phase = `policy/appearance-${choice}`;
    await choices.nth(index).check();
    assert.equal(await tab.evaluate(() => c6Motion.get()), choice);
    assert.equal(await tab.locator('html').getAttribute('data-motion'), choice === 'system' ? (motion.os === 'reduce' ? 'reduced' : 'full') : choice);
  }
  f.phase = 'policy/cross-tab';
  await page.waitForFunction(() => document.documentElement.dataset.motion === 'off');
  await tab.close();
  await page.emulateMedia({ reducedMotion: motion.os });
  await page.evaluate(choice => c6Motion.set(choice || 'system'), motion.choice);
  f.phase = 'policy/reload';
  await page.reload();
  await page.locator('textarea').first().waitFor();
  assert.equal(await page.evaluate(() => c6Motion.get()), motion.choice || 'system');
  assert.equal(await page.locator('html').getAttribute('data-motion'), motion.effective);
  await settle(page);
  // Do not include the deliberately exercised Full policy in Reduced/Off's
  // effect audit. Subsequent checks keep exactly their advertised preference.
  await page.evaluate(() => { __motionTest.effects.length = 0; });
}

// CDP emits native Chromium emulated touch, including pointer cancellation,
// scrolling and compatibility clicks. Synthetic dispatched pointer events
// alone cannot prove that nested scrolling remains native.
async function drawer(f) {
  if (!f.label.startsWith('chromium-')) { console.log('    drawer: Chromium touch emulation only'); return; }
  const { page, context, motion } = f;
  const opener = page.getByRole('button', { name: /^Menu,/ }).first();
  const panel = opener.locator('..').locator('.phone-drawer');
  const cdp = await context.newCDPSession(page);
  const touch = (type, x, y) => cdp.send('Input.dispatchTouchEvent', {
    type, touchPoints: type === 'touchEnd' || type === 'touchCancel' ? [] : [{ x, y, id: 1 }],
  });
  const point = async () => panel.evaluate(el => {
    const box = el.getBoundingClientRect();
    return { x: box.left + box.width - 40, y: box.top + Math.min(130, box.height / 2), width: box.width };
  });
  const open = async () => {
    await opener.tap();
    // Wait for this one native tap's result; no retries or JS substitution.
    await page.waitForFunction(el => el.dataset.open === 'true', await panel.elementHandle());
    await settle(page);
    assert.equal(await panel.getAttribute('data-open'), 'true');
  };
  const picture = () => panel.evaluate(el => {
    const css = getComputedStyle(el);
    return { left: el.getBoundingClientRect().left, dragging: el.hasAttribute('data-dragging'), inert: el.inert,
      duration: css.transitionDuration.split(',').map(value => parseFloat(value) * 1000),
      travel: Number(css.getPropertyValue('--travel')), enter: css.getPropertyValue('--dur-large').trim(),
      exit: css.getPropertyValue('--dur-sheet-exit').trim() };
  });
  try {
    await open();
    await panel.evaluate(el => { window.__swipeDrawer = el; });
    const tokens = await picture();
    assert.equal(tokens.travel, motion.effective === 'full' ? 1 : 0);
    assert.equal(tokens.enter, motion.effective === 'full' ? '280ms' : motion.effective === 'off' ? '0ms' : '80ms');
    assert.equal(tokens.exit, motion.effective === 'full' ? '200ms' : motion.effective === 'off' ? '0ms' : '80ms');
    assert(await panel.evaluate(el => getComputedStyle(el).touchAction.includes('pan-y')), 'native vertical pan remains enabled');

    // A normal tap still opens the live, nested realm menu, not the drawer's
    // close handler. Keep it shut afterward so fixture height stays stable.
    const summary = panel.locator('summary').first();
    const wasOpen = await summary.evaluate(el => el.parentElement.open);
    await summary.tap();
    assert.equal(await summary.evaluate(el => el.parentElement.open), !wasOpen);
    assert.equal(await panel.getAttribute('data-open'), 'true', 'tap is not stolen');
    await summary.tap();
    await settle(page);

    // Start well inside the drawer, cross the close threshold, then reverse
    // before release. Net travel, not the furthest point or velocity, decides.
    let p = await point();
    await touch('touchStart', p.x, p.y);
    await touch('touchMove', p.x - 35, p.y + 1);
    const following = await picture();
    assert(following.dragging && !following.inert);
    assert(Math.abs(following.left - (motion.effective === 'full' ? -35 : 0)) < 2,
      `finger drives Full only: ${JSON.stringify(following)}`);
    await touch('touchMove', p.x - 125, p.y + 2);
    await touch('touchMove', p.x - 28, p.y);
    assert.equal(await panel.getAttribute('data-open'), 'true', 'crossing threshold alone does not close');
    await touch('touchEnd');
    const returning = await picture();
    assert(!returning.dragging && returning.duration.includes(motion.effective === 'full' ? 280 : motion.effective === 'off' ? 0 : 80),
      `snap-back reuses enter tokens: ${JSON.stringify(returning)}`);
    await settle(page);
    assert.equal(await panel.getAttribute('data-open'), 'true', 'short reversal snaps back without a compatibility click closing it');
    assert(Math.abs((await picture()).left) < 1);
    assert(await panel.evaluate(el => el === __swipeDrawer), 'no remount during reversal');

    // Nested overflow fixture uses native DOM only, not rendered app HTML.
    // Real touch must scroll it; checking only that the drawer stays open
    // would miss a preventDefault that silently breaks nested scrolling.
    await panel.evaluate(el => {
      const scroll = document.createElement('div');
      scroll.id = 'drawer-scroll-fixture';
      Object.assign(scroll.style, { height: '180px', overflowY: 'auto', touchAction: 'pan-y', flexShrink: '0' });
      scroll.addEventListener('scrollend', () => { scroll.dataset.ended = 'true'; }, { once: true });
      const content = document.createElement('div');
      Object.assign(content.style, { height: '1000px', background: 'linear-gradient(#aaa, #ddd)' });
      scroll.append(content); el.prepend(scroll); el.scrollTop = 0;
    });
    const nested = page.locator('#drawer-scroll-fixture');
    const box = await nested.boundingBox();
    const x = box.x + box.width / 2, y = box.y + 145;
    await touch('touchStart', x, y);
    for (const dy of [15, 40, 70, 100]) await touch('touchMove', x - dy / 10, y - dy);
    await touch('touchEnd');
    await page.waitForFunction(() => document.querySelector('#drawer-scroll-fixture').scrollTop > 20);
    // Native scrolling outlives touchEnd. Keep its target attached until the
    // browser reports completion, rather than tearing it out during a fling.
    await page.waitForFunction(() => document.querySelector('#drawer-scroll-fixture').dataset.ended === 'true');
    assert.equal(await panel.getAttribute('data-open'), 'true', 'nested vertical scroll does not close');
    assert(!(await picture()).dragging);
    await nested.evaluate(el => el.remove());

    // Long pull stays open until release, then exits inert on the same node.
    p = await point();
    await touch('touchStart', p.x, p.y);
    await touch('touchMove', p.x - 30, p.y);
    assert((await picture()).dragging, 'the next native gesture reaches and is claimed by the drawer');
    // Test a deliberate distance-based pull/release, not a 115px one-step
    // flick. Fast-release diagnostics delivered the next tap's down/up with
    // no click; report that separately rather than retrying the missed tap.
    for (const dx of [60, 90, 120, 130, 138, 142, 144, 145]) await touch('touchMove', p.x - dx, p.y + 2);
    assert.equal(await panel.getAttribute('data-open'), 'true');
    await touch('touchEnd');
    await page.waitForFunction(() => __swipeDrawer.dataset.open === 'false' && __swipeDrawer.inert);
    const closing = await picture();
    assert(closing.duration.includes(motion.effective === 'full' ? 200 : motion.effective === 'off' ? 0 : 80),
      `close reuses exit tokens: ${JSON.stringify(closing)}`);
    await settle(page);
    assert.equal(await panel.getAttribute('data-open'), 'false', 'compatibility click must not reopen/select anything');

    await open();
    const bounds = await panel.boundingBox();
    await page.touchscreen.tap(Math.min(f.viewport.width - 8, bounds.x + bounds.width + 24), bounds.y + 80);
    await page.waitForFunction(() => __swipeDrawer.dataset.open === 'false');
    await settle(page);
    await open();
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => __swipeDrawer.dataset.open === 'false' && __swipeDrawer.inert);
    await settle(page);
  } finally { await cdp.detach(); }
}

async function menu(f) {
  const { page } = f;
  if (phoneViewport(f.viewport)) {
    const drawerFrame = await page.getByRole('button', { name: /^Menu,/ }).first().evaluate(async opener => {
      opener.click();
      await new Promise(resolve => requestAnimationFrame(resolve));
      const drawer = document.querySelector('.phone-drawer[data-open="true"]');
      if (!drawer) return { missing: true };
      const input = drawer.querySelector('input');
      if (!input) return { missingInput: true };
      input.focus();
      const box = input.getBoundingClientRect();
      const x = box.x + box.width / 2, y = box.y + box.height / 2;
      const onscreen = x >= 0 && x < innerWidth && y >= visualViewport.offsetTop && y < visualViewport.offsetTop + visualViewport.height;
      const hit = document.elementFromPoint(x, y);
      input.click(); // Dispatch is live even if the entering picture is offscreen.
      return { inert: !!input.closest('[inert]'), disabled: input.disabled, focus: document.activeElement === input,
        hit: !onscreen || hit === input || input.contains(hit),
        diagnostic: { input: { x: box.x, y: box.y, width: box.width, height: box.height },
          onscreen, hit: hit?.outerHTML.slice(0, 250), translate: getComputedStyle(drawer).translate, viewport: innerHeight } };
    });
    const { diagnostic, ...inputState } = drawerFrame;
    assert.deepEqual(inputState, { inert: false, disabled: false, focus: true, hit: true }, `drawer accepts focus and input in its first frame: ${JSON.stringify(diagnostic)}`);
    await settle(page);
    await page.screenshot({ path: path.join(SHOTS, `${f.label}-menu-open.png`) });
    const drawer = page.getByRole('button', { name: /^Menu,/ }).first().locator('..').locator('.phone-drawer');
    const axis = await drawer.evaluate(el => {
      window.__drawer = el;
      el.querySelector('input').blur();
      return { x: getComputedStyle(el).getPropertyValue('--sheet-x').trim(),
        y: getComputedStyle(el).getPropertyValue('--sheet-y').trim() };
    });
    assert.deepEqual(axis, { x: '-100%', y: '0px' }, 'phone drawer travels left, not down');
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => __drawer.dataset.open === 'false');
    assert(await drawer.evaluate(el => el.inert), 'closed retained drawer is inert');
    await settle(page);
    const closed = await drawer.evaluate(el => {
      const [x, y = '0px'] = getComputedStyle(el).translate.split(' ');
      return { x, y };
    });
    assert.equal(parseFloat(closed.y), 0, 'drawer never travels vertically');
    assert.equal(parseFloat(closed.x), f.motion.effective === 'full' ? -100 : 0);
    const reversal = await drawer.evaluate(async (el, full) => {
      const toggle = async () => {
        el.parentElement.querySelector('button[aria-label^="Menu,"]').click();
        await Promise.resolve(); await Promise.resolve();
      };
      const x = () => parseFloat(getComputedStyle(el).translate);
      await toggle();
      if (!full) return null;
      // Seek the actual CSS transition, synchronously with opening it. A slow
      // host cannot miss an intermediate frame and hang waiting for it again.
      const transition = el.getAnimations().find(a => a.transitionProperty === 'translate');
      if (!transition) return { missing: true };
      transition.pause();
      transition.currentTime = Number(transition.effect.getTiming().duration) * .16;
      const before = x();
      await toggle();
      const closing = x();
      await toggle();
      return { before, closing, reopened: x(), same: el === __drawer, inert: el.inert };
    }, f.motion.effective === 'full');
    if (reversal) assert(reversal.same && !reversal.inert && reversal.before > -100 && reversal.before < 0 &&
      Math.abs(reversal.before - reversal.closing) < 1 && Math.abs(reversal.before - reversal.reopened) < 1,
      `drawer reverses from its displayed position: ${JSON.stringify(reversal)}`);
    await settle(page);
    // VisualViewport keyboard fixture changes only visible space, like Android
    // Chrome's overlay keyboard; no synthetic resize of the layout viewport.
    await page.evaluate(() => __keyboard(Math.min(innerHeight, 280)));
    await settle(page);
    assert(await drawer.evaluate(el => {
      const r = el.getBoundingClientRect();
      return r.width <= innerWidth && r.height <= visualViewport.height - 44 + 1 &&
        getComputedStyle(el).overflowY === 'auto';
    }), 'drawer remains bounded and scrollable above the keyboard');
    await page.screenshot({ path: path.join(SHOTS, `${f.label}-menu-keyboard.png`) });
    await page.evaluate(() => __keyboard(innerHeight));
    await page.keyboard.press('Escape');
    await settle(page);
  }
  // Execute in the first rAF after the production action handler, rather than
  // letting Playwright wait out a scale/fade before clicking its first item.
  const first = await page.getByRole('button', { name: 'Actions', exact: true }).first().evaluate(async opener => {
    opener.click();
    await new Promise(resolve => requestAnimationFrame(resolve));
    // Plain workspace popovers are enter-only and unmount on selection;
    // presence-managed popovers additionally expose the open/inert contract.
    const pop = [...document.querySelectorAll('.c6-pop[data-open="true"], .c6-pop:not([data-open])')].find(el => el.textContent.includes('Find in session'));
    if (!pop) return { missing: true };
    const item = [...pop.querySelectorAll('button')].find(el => el.textContent.includes('Find in session'));
    item.focus();
    const box = item.getBoundingClientRect();
    const hit = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
    const result = { missing: false, inert: !!item.closest('[inert]'), focused: document.activeElement === item,
      hit: hit === item || item.contains(hit), connected: item.isConnected };
    window.__menu = pop;
    item.click();
    return result;
  });
  assert.deepEqual(first, { missing: false, inert: false, focused: true, hit: true, connected: true }, 'menu is focusable and clickable in its first frame');
  await page.getByPlaceholder('Find in this session').waitFor();
  assert(await page.evaluate(() => !__menu.isConnected || (__menu.dataset.open === 'false' && __menu.inert)), 'exiting menu is inert immediately');
  await page.keyboard.press('Escape');
  await settle(page);
  await page.getByRole('button', { name: 'Actions', exact: true }).first().click();
  const toggle = page.getByRole('switch', { name: 'Catch-up', exact: true });
  await toggle.waitFor();
  await settle(page);
  await page.screenshot({ path: path.join(SHOTS, `${f.label}-actions-open.png`) });
  for (const on of [false, true]) {
    assert.equal(await toggle.getAttribute('aria-checked'), String(!on));
    const pending = f.deferViewWrite();
    try {
      await toggle.click();
      assert.deepEqual(await pending.request, { catchup: on, fold: false }, 'shared Switch sends the new boolean and preserves folding');
      assert.equal(await toggle.getAttribute('aria-checked'), String(!on), 'Switch waits for server success');
    } finally { pending.release(); }
    await page.waitForFunction(on => document.querySelector('[role="switch"][aria-label="Catch-up"]')?.getAttribute('aria-checked') === String(on), on);
  }
  // A plain menu can unmount instantly; choose its real action to close it.
  await page.getByRole('button', { name: /^Find in session/ }).click();
  await page.getByPlaceholder('Find in this session').waitFor();
  await page.keyboard.press('Escape');
  await settle(page);
}

async function visibleBounds(locator) {
  if (process.env.DEBUG_FIXTURE) console.log('  BOUNDS', await locator.evaluate(el => ({
    y: el.getBoundingClientRect().y, bottom: el.getBoundingClientRect().bottom, top: visualViewport.offsetTop,
    end: visualViewport.offsetTop + visualViewport.height, translate: getComputedStyle(el).translate,
    css: { bottom: getComputedStyle(el).bottom, margin: getComputedStyle(el).marginBottom,
      transform: getComputedStyle(el).transform, scale: getComputedStyle(el).scale },
    ancestors: (() => { const rows = []; for (let parent = el.parentElement; parent; parent = parent.parentElement) {
      const css = getComputedStyle(parent); rows.push({ tag: parent.tagName, classes: String(parent.className).slice(0, 80),
        transform: css.transform, translate: css.translate, scale: css.scale, contain: css.contain, y: parent.getBoundingClientRect().y });
    } return rows; })(),
  })));
  await locator.page().waitForFunction(el => {
    const box = el.getBoundingClientRect();
    const tail = Math.max(0, -parseFloat(getComputedStyle(el).marginBottom));
    return box.x >= -1 && box.right <= innerWidth + 1 && box.y >= visualViewport.offsetTop - 1 &&
      box.bottom <= visualViewport.offsetTop + visualViewport.height + tail + 1;
  }, await locator.elementHandle(), { polling: 'raf' });
  const metrics = await locator.evaluate(el => {
    const box = el.getBoundingClientRect();
    return { x: box.x, y: box.y, right: box.right, bottom: box.bottom,
      top: visualViewport.offsetTop, end: visualViewport.offsetTop + visualViewport.height, width: innerWidth,
      tail: Math.max(0, -parseFloat(getComputedStyle(el).marginBottom)) };
  });
  // The negative-margin tail is decorative spring coverage, not hidden input.
  // It uses viewport-relative safe-area padding, not a fixed eight pixels.
  assert(metrics.x >= -1 && metrics.right <= metrics.width + 1 && metrics.y >= metrics.top - 1 && metrics.bottom <= metrics.end + metrics.tail + 1,
    `outside the visible viewport: ${JSON.stringify(metrics)}`);
}

async function sheet(f) {
  const { page, motion } = f;
  const step = name => { f.phase = `sheet: ${name}`; if (process.env.DEBUG_FIXTURE) console.log(`  ${f.phase}`); };
  const ring = page.getByRole('button', { name: /Open session sheet/ }).first();
  const dialog = page.getByRole('dialog', { name: 'Session details', exact: true });
  step('initial open');
  await ring.click();
  await dialog.waitFor();
  assert(await dialog.evaluate(el => el.classList.contains('c6-sheet') && el.dataset.open === 'true' && !el.inert));
  assert(await page.locator('.c6-scrim[data-open="true"]').count(), 'production scrim uses the same presence state');
  await settle(page);
  await visibleBounds(dialog);
  await page.screenshot({ path: path.join(SHOTS, `${f.label}-sheet-open.png`) });
  await dialog.getByRole('button', { name: 'Close', exact: true }).click();
  await dialog.waitFor({ state: 'detached' });
  step('overlay keyboard');
  await ring.click(); await dialog.waitFor();
  // Overlay/offset keyboards and resized-layout keyboards are different paths.
  await page.evaluate(() => __keyboard(400, 20));
  await settle(page);
  await visibleBounds(dialog);
  await dialog.getByRole('button', { name: 'Close', exact: true }).click();
  await dialog.waitFor({ state: 'detached' });
  await page.evaluate(() => __keyboard(innerHeight));
  await page.setViewportSize({ width: 390, height: 500 });
  step('resized keyboard');
  await page.evaluate(() => __keyboard(innerHeight));
  await ring.click(); await dialog.waitFor(); await settle(page); await visibleBounds(dialog);
  await dialog.getByRole('button', { name: 'Close', exact: true }).click();
  await dialog.waitFor({ state: 'detached' });
  await page.setViewportSize(f.viewport);
  await page.evaluate(() => __keyboard(innerHeight));
  await settle(page);

  // Product boundaries, not sleeps: interruption and disposal use a controlled
  // browser clock. A loaded host cannot accidentally outlast the exit before
  // the test gets its turn to reopen it.
  await page.clock.install();
  step('clock interruption/disposal');
  await page.clock.pauseAt(new Date(Date.now() + 1000));
  try {
    await page.evaluate(() => {
      __motionTest.timers = [];
      const timeout = window.setTimeout;
      const clear = window.clearTimeout;
      // Playwright allocates fake IDs above 1e12; web-sys correctly models
      // browser timeout handles as i32. Map fake IDs into that actual browser
      // contract or WASM cancellation truncates them and invents a stale timer.
      const handles = new Map();
      let next = 1000000;
      window.setTimeout = (callback, delay, ...args) => {
        const record = { delay, at: performance.now() };
        const id = next++;
        const actual = timeout(() => { handles.delete(id); record.fired = performance.now(); callback(...args); }, delay);
        handles.set(id, actual);
        record.id = id; __motionTest.timers.push(record); return id;
      };
      window.clearTimeout = id => {
        const timer = __motionTest.timers.find(timer => timer.id === id);
        if (timer) timer.cleared = performance.now();
        const actual = handles.get(id) ?? id;
        handles.delete(id);
        return clear(actual);
      };
    });
    await ring.evaluate(el => el.click());
    await page.clock.runFor(32); // Advance rAF/effects explicitly under the fake clock.
    await page.waitForFunction(() => document.querySelector('.c6-sheet[data-open="true"]'));
    await dialog.evaluate(el => { window.__retained = el; el.querySelector('button[aria-label="Close"]').click(); });
    if (motion.effective !== 'off') {
      assert(await page.evaluate(() => __retained.isConnected && __retained.inert && __retained.dataset.open === 'false'));
      const exit = await page.evaluate(() => {
        const css = getComputedStyle(__retained);
        return { properties: css.transitionProperty, durations: css.transitionDuration };
      });
      const milliseconds = exit.durations.split(',').map(value => parseFloat(value) * (value.trim().endsWith('ms') ? 1 : 1000));
      assert(milliseconds.includes(motion.effective === 'full' ? 200 : 80), `sheet exit duration: ${JSON.stringify(exit)}`);
      const retention = motion.effective === 'full' ? 300 : 80;
      await page.clock.runFor(32);
      await ring.evaluate(el => el.click());
      await page.clock.runFor(retention + 1);
      assert(await page.evaluate(() => __retained.isConnected && !__retained.inert && __retained.dataset.open === 'true'),
        'interrupted exit keeps the same body and cancels its stale disposal');
      await dialog.evaluate(el => el.querySelector('button[aria-label="Close"]').click());
      await page.clock.runFor(retention - 1);
      const retained = await page.evaluate(() => ({ connected: __retained.isConnected, now: performance.now(), timers: __motionTest.timers }));
      assert(retained.connected, `retained through ${retention - 1}ms: ${JSON.stringify(retained)}`);
      await page.clock.runFor(2);
    } else await page.clock.runFor(1);
    await page.waitForFunction(() => !__retained.isConnected);
  } finally { await page.clock.resume(); }
}

async function sessionsPicker(f) {
  if (!phoneViewport(f.viewport)) return;
  const { page } = f;
  const opener = page.getByRole('button', { name: /^Switch sessions and pages/ });
  const dialog = page.getByRole('dialog', { name: 'Sessions', exact: true });
  const open = async () => { await opener.click(); await dialog.waitFor(); await settle(page); };
  const closed = async () => { await page.waitForFunction(() => !document.querySelector('[role="dialog"][aria-label="Sessions"][data-open="true"]')); await settle(page); };
  await open();
  const header = await dialog.evaluate(el => {
    const h = el.querySelector('h2').parentElement;
    const [ask, bell] = h.querySelectorAll('button');
    const rowX = el.querySelector('button[aria-label="Close this session or page"]');
    const box = e => { const r = e.getBoundingClientRect(); return { right: r.right, cy: r.y + r.height / 2, height: r.height }; };
    return { buttons: h.querySelectorAll('button').length, title: box(h.querySelector('h2')),
      ask: box(ask), askPill: box(ask.querySelector('span')), bell: box(bell), bellPill: box(bell.querySelector('span')), rowX: box(rowX),
      icons: h.querySelectorAll('svg').length, handle: el.querySelector('button[aria-label="Close Sessions"]')?.tagName };
  });
  assert.equal(header.buttons, 2, 'header has Ask and bell, not an extra X');
  assert.equal(header.icons, 2, 'Ask and bell keep their drawn SVG icons');
  assert.equal(header.handle, 'BUTTON', 'screen readers can activate the named close handle');
  assert.equal(header.askPill.height, header.bellPill.height, JSON.stringify(header));
  assert(Math.abs(header.title.cy - header.ask.cy) < 1 && Math.abs(header.ask.cy - header.bell.cy) < 1, JSON.stringify(header));
  assert(Math.abs(header.bell.right - header.rowX.right) < 1, `header ends at the row close column: ${JSON.stringify(header)}`);
  await page.screenshot({ path: path.join(SHOTS, `${f.label}-sessions-picker.png`) });
  await dialog.getByRole('button', { name: 'Close Sessions', exact: true }).click(); await closed();
  await open(); await page.keyboard.press('Escape'); await closed();
  await open(); await page.locator('.c6-scrim[data-open="true"]').click({ position: { x: 2, y: 2 } }); await closed();
  await open();
  await dialog.getByRole('button', { name: 'Close Sessions', exact: true }).evaluate(el => {
    el.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, clientY: 10, pointerId: 1 }));
    el.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, clientY: 60, pointerId: 1 }));
    el.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, clientY: 60, pointerId: 1 }));
  });
  await closed();
}

async function modelPicker(f) {
  const { page } = f;
  const phone = phoneViewport(f.viewport);
  const chip = page.getByRole('button', { name: /^Model: .*change it/ }).first();
  const dialog = page.getByRole('dialog', { name: 'Model', exact: true });
  const input = page.getByPlaceholder('model id', { exact: true });
  const modelsAsked = () => f.requests.filter(r => r.endsWith('/connections/test/models')).length;
  await page.waitForFunction(() => document.querySelector('button[aria-label^="Model:"]'));
  // The policy check deliberately reloads the document. Every such page owns
  // a new cache; the picker must not add requests to its current page's warm list.
  const prefetched = modelsAsked();
  assert(prefetched > 0, 'composer prefetched its shared model list');
  await chip.click();
  await input.waitFor(); await settle(page);
  assert.equal(await dialog.count(), phone ? 1 : 0, 'only phones use the existing bottom sheet');
  const originalModel = await input.inputValue();
  const originalEffort = await page.getByRole('combobox', { name: 'Reasoning effort', exact: true }).inputValue();
  await input.fill('custom-model-id');
  assert.equal(await input.inputValue(), 'custom-model-id', 'free-form model selection is unchanged');
  const last = page.getByRole('button', { name: 'fixture-model-32', exact: true });
  await last.waitFor();
  assert.equal(modelsAsked(), prefetched, 'opening/searching uses the prefetched model list');
  if (phone) {
    await page.evaluate(() => __keyboard(Math.min(innerHeight, 300), 12));
    await settle(page);
    await visibleBounds(dialog);
    assert(await dialog.evaluate(el => {
      const input = el.querySelector('input');
      return getComputedStyle(input.parentElement.nextElementSibling).position === 'static';
    }), 'model choices share the keyboard-bounded sheet scroll, not a clipped popover');
    const layout = await dialog.evaluate(el => {
      const box = node => { const r = node.getBoundingClientRect(); return { left: r.left, right: r.right, top: r.top, bottom: r.bottom }; };
      const row = el.querySelector('input').parentElement;
      const content = el.lastElementChild;
      const style = getComputedStyle(content);
      return { row: box(row), list: box(row.nextElementSibling), label: box(row.parentElement.previousElementSibling),
        labelText: row.parentElement.previousElementSibling.textContent,
        left: content.getBoundingClientRect().left + parseFloat(style.paddingLeft),
        right: content.getBoundingClientRect().right - parseFloat(style.paddingRight),
        headerButtons: el.firstElementChild.querySelectorAll('button').length };
    });
    assert.equal(layout.labelText, 'Model');
    assert.equal(layout.headerButtons, 1, 'the named close handle is the only model-sheet header button, with no top X');
    for (const part of ['label', 'row', 'list']) {
      assert(Math.abs(layout[part].left - layout.left) < 1, `${part} starts at the sheet padding: ${JSON.stringify(layout)}`);
      assert(Math.abs(layout[part].right - layout.right) < 1, `${part} uses the full sheet width: ${JSON.stringify(layout)}`);
    }
    assert(layout.row.top >= layout.label.bottom && layout.row.top - layout.label.bottom <= 12,
      `Model sits directly above its field: ${JSON.stringify(layout)}`);
    await page.screenshot({ path: path.join(SHOTS, `${f.label}-model-picker-keyboard.png`) });
  }
  await last.click();
  assert.equal(await input.inputValue(), 'fixture-model-32');
  const effort = page.getByRole('combobox', { name: 'Reasoning effort', exact: true });
  await effort.scrollIntoViewIfNeeded();
  await effort.selectOption('high');
  assert.equal(await effort.inputValue(), 'high', 'effort selection stays usable after choosing a model');
  assert(!f.requests.some(r => r.startsWith('POST ') && r.endsWith('/messages')), 'choosing a model does not send a message');
  await page.screenshot({ path: path.join(SHOTS, `${f.label}-model-picker.png`) });
  if (phone && f.motion.effective !== 'off') {
    const retained = await dialog.evaluate(async (el, opener) => {
      el.querySelector('button[aria-label="Close Model"]').click();
      await Promise.resolve(); await Promise.resolve();
      const closed = el.inert && el.dataset.open === 'false';
      const transition = el.getAnimations().find(a => a.transitionProperty === 'translate');
      if (transition) {
        transition.pause();
        transition.currentTime = Number(transition.effect.getTiming().duration) / 2;
      }
      const before = getComputedStyle(el).translate;
      opener.click();
      await Promise.resolve(); await Promise.resolve();
      return { closed, same: document.querySelector('[role="dialog"][aria-label="Model"]') === el,
        open: !el.inert && el.dataset.open === 'true', before, after: getComputedStyle(el).translate };
    }, await chip.elementHandle());
    assert(retained.closed && retained.same && retained.open && retained.before === retained.after,
      `interrupted model-sheet exit keeps the live body at its displayed position: ${JSON.stringify(retained)}`);
    await settle(page);
  }
  // Leave no pending-change badge to wrap the toolbar in later composer checks.
  await input.fill(originalModel);
  await effort.selectOption(originalEffort);
  await page.keyboard.press('Escape');
  await settle(page);
  await input.waitFor({ state: 'hidden' });
  if (!phone) return;
  await page.evaluate(() => __keyboard(innerHeight));
  const open = async () => { await chip.click(); await dialog.waitFor(); await settle(page); };
  const closed = async () => { await page.waitForFunction(() => !document.querySelector('[role="dialog"][aria-label="Model"][data-open="true"]')); await settle(page); };
  await open();
  await dialog.getByRole('button', { name: 'Close Model', exact: true }).click(); await closed();
  await open();
  await page.locator('.c6-scrim[data-open="true"]').click({ position: { x: 2, y: 2 } }); await closed();
  await open();
  await dialog.getByRole('button', { name: 'Close Model', exact: true }).evaluate(el => {
    el.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, clientY: 10, pointerId: 1 }));
    el.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, clientY: 60, pointerId: 1 }));
  });
  await closed();
  assert.equal(modelsAsked(), prefetched, 'all picker openings share the latency build cache');
}

async function modelForms(f) {
  const { page } = f;
  const step = phase => { f.phase = `modelForms: ${phase}`; if (process.env.DEBUG_FIXTURE) console.log(`  ${f.phase}`); };
  for (const [pathname, name, cold] of [
    ['/ask', 'ask', false], ['/sessions', 'new-session', false],
    ['/ask', 'ask', true], ['/sessions', 'new-session', true],
  ]) {
    const route = `${name}-${cold ? 'cold' : 'warm'}`;
    step(`${route} navigation`);
    // Real same-page navigation also proves these forms can reuse the
    // composer's prefetch; direct loads separately cover a cold page cache.
    if (cold) {
      await page.evaluate(next => localStorage.setItem('c6.home.workspace.v2', JSON.stringify(next)), {
        ...layout, tiles: [{ ...layout.tiles[0], columns: [{ key: 3, view: { view: name === 'ask' ? 'chats' : 'sessions' } }] }],
      });
      await page.goto(new URL(`/r/home${pathname}`, page.url()).href, { waitUntil: 'domcontentloaded' });
    } else if (pathname === '/ask') {
      await page.keyboard.press('Control+j');
      await page.getByRole('dialog', { name: 'Ask carbon6', exact: true }).getByRole('button', { name: /^Your conversations/ }).click();
    } else {
      if (phoneViewport(f.viewport)) await page.getByRole('button', { name: /^Menu,/ }).first().click();
      await page.getByRole('link', { name: 'Sessions', exact: true }).first().click();
    }
    // Desktop keeps the previous tile visible beside the new one. Scope the
    // fields to their page so a still-open Ask form cannot stand in for Sessions.
    const formPage = page.locator('main').filter({ has: page.locator('h1').filter({ hasText: name === 'ask' ? /^Ask$/ : /^Sessions$/ }) }).last();
    const input = formPage.getByPlaceholder('model id', { exact: true });
    step(`${route} fields`);
    if (name === 'new-session') {
      await formPage.getByTitle(phoneViewport(f.viewport) ? 'Where and how it runs' : 'Change where and how it runs', { exact: true }).click();
    }
    await input.waitFor(); await settle(page);
    assert.equal(await page.getByRole('dialog', { name: 'Model', exact: true }).count(), 0, 'form fields remain inline, not nested sheets');
    const controls = input.locator('..').locator('..').locator('..').locator('..');
    step(`${route} connection`);
    const prefetched = f.requests.filter(r => r.endsWith('/connections/test/models')).length;
    await controls.locator('select').first().selectOption('test');
    // Connection changes reset the form's default model in a reactive effect.
    // Finish that update before exercising a subsequent user edit.
    await settle(page);
    await input.evaluate(el => { window.__formModelInput = el; });
    await input.fill('custom-form-model');
    await settle(page);
    assert.equal(await input.inputValue(), 'custom-form-model');
    assert(await input.evaluate(el => el === window.__formModelInput && el === document.activeElement), `${route} retains its input and focus while models load`);
    if (phoneViewport(f.viewport)) {
      await page.evaluate(() => __keyboard(Math.min(innerHeight, 300), 12));
      await settle(page);
      await input.evaluate(el => el.scrollIntoView({ block: 'center' })); await visibleBounds(input);
    }
    const offered = formPage.getByRole('button', { name: 'fixture-model-32', exact: true });
    step(`${route} model`);
    await offered.evaluate(el => el.scrollIntoView({ block: 'center' })); await visibleBounds(offered);
    await offered.click();
    if (!cold) assert.equal(f.requests.filter(r => r.endsWith('/connections/test/models')).length, prefetched, `${route} shares the composer's prefetched list`);
    assert.equal(await input.inputValue(), 'fixture-model-32', `${name} still selects offered models`);
    const effort = controls.getByRole('combobox', { name: 'Reasoning effort', exact: true });
    step(`${route} effort`);
    await effort.evaluate(el => el.scrollIntoView({ block: 'center' }));
    await visibleBounds(effort);
    await effort.selectOption('high');
    assert.equal(await effort.inputValue(), 'high');
    const asked = f.requests.filter(r => r.endsWith('/connections/test/models')).length;
    await controls.getByTitle('Every model the connection offers', { exact: true }).click();
    await offered.waitFor();
    assert.equal(f.requests.filter(r => r.endsWith('/connections/test/models')).length, asked, `${name} reuses its shared model list`);
    await page.keyboard.press('Escape');
    await page.screenshot({ path: path.join(SHOTS, `${f.label}-${route}-models.png`) });
    await page.evaluate(() => __keyboard(innerHeight));
  }
}

async function assertFollowing(page, reason) {
  await settle(page);
  // A fit queued by an effect can run on the frame *after* the row's RO
  // snap. Do not accept its transient gap=0 before that measurement runs.
  const position = await page.evaluate(async () => {
    const frame = () => new Promise(resolve => requestAnimationFrame(resolve));
    await frame(); await frame();
    return { top: __transcript.scrollTop, height: __transcript.scrollHeight,
      viewport: __transcript.clientHeight,
      gap: __transcript.scrollHeight - __transcript.clientHeight - __transcript.scrollTop };
  });
  if (process.env.DEBUG_FIXTURE) console.log('  FOLLOW', reason, JSON.stringify(position));
  assert(Math.abs(position.gap) < 1, `${reason}: ${JSON.stringify(position)}`);
}

async function composerAutosize(f) {
  const { page } = f;
  const composer = page.locator('textarea').first();
  f.phase = 'phone: text growth autosize';
  await composer.fill('Short draft');
  await settle(page);
  const short = await composer.evaluate(el => el.getBoundingClientRect().height);
  await composer.fill(Array.from({ length: 5 }, (_, i) => `Real draft line ${i + 1}`).join('\n'));
  await page.waitForFunction(short => __composer.getBoundingClientRect().height > short + 1, short);
  assert(await composer.evaluate(el => el.clientHeight >= el.scrollHeight - 1), 'growing text is not clipped below the cap');

  f.phase = 'phone: width autosize';
  await composer.fill('A width-sensitive draft with ordinary words. '.repeat(4));
  await page.setViewportSize({ width: 480, height: f.viewport.height });
  await page.evaluate(() => __keyboard(innerHeight));
  await settle(page);
  const wide = await composer.evaluate(el => el.getBoundingClientRect().height);
  await page.setViewportSize({ width: 320, height: f.viewport.height });
  await page.evaluate(() => __keyboard(innerHeight));
  await page.waitForFunction(wide => __composer.getBoundingClientRect().height > wide + 1, wide);
  await page.setViewportSize(f.viewport);
  await page.evaluate(() => __keyboard(innerHeight));
  await settle(page);

  f.phase = 'phone: empty suggestion autosize';
  await composer.fill('');
  await page.waitForFunction(short => __composer.getBoundingClientRect().height <= short + 1, short);
  const empty = await composer.evaluate(el => el.getBoundingClientRect().height);
  const hint = 'Suggested follow-up\nInspect the result in detail\nDescribe the remaining checks';
  f.setSuggestion(hint);
  await f.setState('working');
  await page.waitForFunction(() => !!document.querySelector('[data-phone-status]')?.closest('[data-open="true"]'));
  await f.setState('idle');
  await page.waitForFunction(({ hint, empty }) => __composer.value === '' && __composer.placeholder === hint &&
    __composer.getBoundingClientRect().height > empty + 1, { hint, empty });
  const suggested = await composer.evaluate(el => el.getBoundingClientRect().height);
  await f.setState('working');
  await page.waitForFunction(({ hint, suggested }) => __composer.value === '' && __composer.placeholder !== hint &&
    __composer.getBoundingClientRect().height < suggested - 1, { hint, suggested });
  f.setSuggestion(null);
  await f.setState('idle');
  await page.waitForFunction(() => !document.querySelector('[data-phone-status]')?.closest('[data-open="true"]'));
  assert(await composer.evaluate(el => el === window.__composer && document.activeElement === el && el.value === ''),
    'text, width, and empty-hint autosizing retain the composer and focus');
}

async function phone(f) {
  const { page } = f;
  const step = name => { f.phase = `phone: ${name}`; if (process.env.DEBUG_FIXTURE) console.log(`  ${f.phase}`); };
  step('composer');
  const composer = page.locator('textarea').first();
  // Keep the original 320px soft-wrap reproduction and also exercise an
  // explicitly multiline draft on wider phones.
  const draft = 'Keep the draft and composer focused';
  await composer.fill(draft);
  await composer.focus();
  await settle(page);
  await composer.evaluate(el => {
    window.__composer = el;
    window.__composerY = el.getBoundingClientRect().y;
    const candidates = [...document.querySelectorAll('div')].filter(e => e.clientHeight > 100 &&
      e.scrollHeight > e.clientHeight + 300 && getComputedStyle(e).overflowY === 'auto');
    window.__transcript = candidates.sort((a, b) => b.scrollHeight - a.scrollHeight)[0];
  });
  assert(await page.evaluate(() => !!__transcript), 'long production transcript is scrollable');
  for (const multiline of [false, true]) {
    if (multiline) {
      await composer.fill(`${draft}\nPreserve this wrapped draft.`);
      await settle(page);
      await composer.evaluate(el => { __composerY = el.getBoundingClientRect().y; });
    }
    step(multiline ? 'multiline following' : 'soft-wrap following');
    await page.waitForFunction(() => Math.abs(__transcript.scrollHeight - __transcript.clientHeight - __transcript.scrollTop) < 1);
    await assertFollowing(page, `following after ${multiline ? 'multiline' : 'soft-wrap'} draft input`);
    for (const state of ['working', 'idle', 'working', 'idle']) {
      step(`${multiline ? 'multiline' : 'soft-wrap'} ${state}`);
      await f.setState(state);
      await page.waitForFunction(open => !!document.querySelector('[data-phone-status]')?.closest('[data-open="true"]') === open, state === 'working');
      await settle(page);
      assert(await page.evaluate(() => __composer.isConnected && document.activeElement === __composer &&
        Math.abs(__composer.getBoundingClientRect().y - __composerY) < 1), 'status motion must not move/remount/defocus the composer');
      await assertFollowing(page, `following stays at the end after ${multiline ? 'multiline' : 'soft-wrap'} ${state}`);
      if (state === 'working') await page.screenshot({ path: path.join(SHOTS, `${f.label}-status-working.png`) });
    }
  }
  // Record every rendered frame across row insertion/removal, not just its
  // final position. Sampling ends on the same explicit state/completion signal.
  await page.evaluate(() => {
    __motionTest.rowSamples = []; __motionTest.sampleRow = true;
    const sample = () => {
      if (!__motionTest.sampleRow) return;
      __motionTest.rowSamples.push({ y: __composer.getBoundingClientRect().y,
        gap: __transcript.scrollHeight - __transcript.clientHeight - __transcript.scrollTop });
      requestAnimationFrame(sample);
    };
    requestAnimationFrame(sample);
  });
  step('sample row insertion');
  await f.setState('working');
  await page.locator('[data-phone-status]').getByText('Working…', { exact: true }).waitFor();
  await settle(page);
  await f.setState('idle');
  step('sample row removal');
  await page.waitForFunction(() => !document.querySelector('[data-phone-status]')?.closest('[data-open="true"]'));
  await settle(page);
  const samples = await page.evaluate(() => { __motionTest.sampleRow = false; return { y: __composerY, samples: __motionTest.rowSamples }; });
  assert(samples.samples.length > 0);
  assert(samples.samples.every(sample => Math.abs(sample.y - samples.y) < 1), JSON.stringify(samples));
  // rAF runs before ResizeObserver. The production observer can correct the
  // end position later in that same paint, so don't mistake a pre-observer
  // measurement for a painted jump.
  await assertFollowing(page, 'following after sampled insertion/removal');

  // Real keyboard scroll intent, not a scrollTop assignment that bypasses the
  // production follow controller. Row changes must also preserve an older view.
  await page.evaluate(() => __transcript.focus());
  step('PageUp intent');
  await page.keyboard.press('PageUp');
  await page.waitForFunction(() => __transcript.scrollHeight - __transcript.clientHeight - __transcript.scrollTop > 100);
  await settle(page);
  await page.waitForFunction(() => {
    const top = __transcript.scrollTop;
    __motionTest.stable = top === __motionTest.top ? (__motionTest.stable || 0) + 1 : 0;
    __motionTest.top = top;
    return __motionTest.stable >= 2;
  }, null, { polling: 'raf' });
  const top = await page.evaluate(() => __transcript.scrollTop);
  await f.setState('working'); await page.locator('[data-phone-status]').waitFor(); await settle(page);
  assert(Math.abs(top - await page.evaluate(() => __transcript.scrollTop)) < 2, 'scrolled-up reader must not follow row insertion');
  await f.setState('idle'); await settle(page);
  await composerAutosize(f);
}

async function tool(f) {
  const { page, motion } = f;
  f.send({ type: 'assistant', turn: 100, blocks: [{ type: 'tool_use', id: 'motion-tool', name: 'bash',
    input_json: JSON.stringify({ command: 'printf motion-fixture-output' }) }], stop_reason: 'tool_use' });
  const details = page.locator('details.c6-tool-details').filter({ hasText: 'motion-fixture-output' }).last();
  await details.waitFor();
  assert.equal(await details.getAttribute('open'), null, 'tool arrival stays collapsed');
  const collapsedHeight = (await details.boundingBox()).height;
  await details.evaluate(el => { window.__tool = el; });
  const effectStart = await page.evaluate(() => {
    __motionTest.disclosureSamples = []; __motionTest.disclosureSampling = true;
    const sample = () => {
      if (!__motionTest.disclosureSampling) return;
      __motionTest.disclosureSamples.push(__tool.getBoundingClientRect().height);
      requestAnimationFrame(sample);
    };
    requestAnimationFrame(sample);
    return __motionTest.effects.length;
  });
  await details.locator('summary').click();
  await page.waitForFunction(() => __tool.open);
  await page.waitForFunction(() => {
    const body = __tool.querySelector('summary').nextElementSibling;
    const target = body.scrollHeight + parseFloat(getComputedStyle(body).borderTopWidth);
    return __tool.getBoundingClientRect().height - __tool.querySelector('summary').getBoundingClientRect().height >= target - .5;
  });
  await settle(page);
  // Closed tool bodies are lazy. Capture the real output node only after
  // opening, then require that exact node to survive streaming and completion.
  await details.locator('pre').waitFor({ state: 'attached' });
  await details.evaluate(el => { window.__pre = el.querySelector('pre'); });
  const timing = await details.evaluate(el => {
    const supported = CSS.supports('interpolate-size', 'allow-keywords') && CSS.supports('selector(details::details-content)');
    const css = getComputedStyle(el, '::details-content');
    return { supported, properties: css.transitionProperty, durations: css.transitionDuration };
  });
  if (timing.supported && motion.effective === 'full') {
    assert(timing.properties.includes('block-size'), JSON.stringify(timing));
    assert(timing.durations.split(',').some(value => Math.abs(parseFloat(value) * (value.trim().endsWith('ms') ? 1 : 1000) - 160) < 0.01),
      `tool disclosure must use 160ms: ${JSON.stringify(timing)}`);
  }
  const disclosure = await page.evaluate(start => {
    __motionTest.disclosureSampling = false;
    return { samples: __motionTest.disclosureSamples, effects: __motionTest.effects.slice(start).filter(effect => effect.tool) };
  }, effectStart);
  if (!timing.supported && motion.effective === 'full') assert(disclosure.effects.some(effect => Number(effect.duration) === 160),
    `this engine needs an actual 160ms disclosure fallback: ${JSON.stringify({ timing, ...disclosure })}`);
  console.log('  DISCLOSURE', JSON.stringify({ timing, effects: disclosure.effects.map(effect => ({ name: effect.name, duration: effect.duration })),
    intermediateFrame: disclosure.samples.some(height => height > collapsedHeight + 1 && height < disclosure.samples.at(-1) - 1) }));
  await details.evaluate(el => {
    window.__output = [...el.querySelectorAll('div')].find(e => e.querySelector('pre') && getComputedStyle(e).overflowY === 'auto');
    window.__toolHeights = { card: el.getBoundingClientRect().height, output: __output.getBoundingClientRect().height };
  });
  assert.equal(await page.evaluate(() => __toolHeights.output), 192, 'D2 reserves the same bounded output area while running');
  assert((await details.boundingBox()).height >= collapsedHeight + 192, 'native details body actually reveals, including on the fallback browser');
  await page.screenshot({ path: path.join(SHOTS, `${f.label}-tool-open.png`) });
  f.output('Live motion-fixture-output\n');
  await details.getByText('Live motion-fixture-output', { exact: false }).waitFor();
  await settle(page);
  const check = async () => assert(await page.evaluate(() => __tool.isConnected && __tool.open && __pre.isConnected &&
    __pre === __tool.querySelector('pre') && __output.isConnected &&
    Math.abs(__tool.getBoundingClientRect().height - __toolHeights.card) < 1 &&
    Math.abs(__output.getBoundingClientRect().height - __toolHeights.output) < 1), 'D2 output/result must not replace nodes, collapse, or jump height');
  await check();
  f.send({ type: 'tool_result', call_id: 'motion-tool', tool: 'bash', output: 'Final motion-fixture-output\n'.repeat(80),
    is_error: true, truncated: true, duration_ms: 42, images: 0 });
  await details.getByText('Output was truncated for the model.', { exact: true }).waitFor();
  await settle(page); await check();
  await details.locator('summary').click(); await settle(page);
  assert.equal(await details.getAttribute('open'), null);
  // Native ::details-content transitions are not consistently exposed by
  // document.getAnimations(). Its actual collapsed geometry is the signal.
  await page.waitForFunction(() => Math.abs(__tool.getBoundingClientRect().height - __tool.querySelector('summary').getBoundingClientRect().height) < 1);
  const closedMetrics = await details.evaluate(el => ({ height: el.getBoundingClientRect().height,
    summary: el.querySelector('summary').getBoundingClientRect().height,
    content: getComputedStyle(el, '::details-content').blockSize,
    visibility: getComputedStyle(el, '::details-content').contentVisibility }));
  assert(Math.abs(closedMetrics.height - collapsedHeight) < 1,
    `native details actually collapses after its transition: ${JSON.stringify({ collapsedHeight, ...closedMetrics })}`);
  f.send({ type: 'assistant', turn: 101, blocks: [{ type: 'text', text: 'Motion tool finished.' }], stop_reason: 'end_turn' });
  await page.getByText('Motion tool finished.', { exact: true }).waitFor();
  assert.equal(await details.getAttribute('open'), null, 'later events must not reopen a user-closed tool');
}

async function sendStop(f) {
  const { page } = f;
  await f.setState('idle');
  const composer = page.locator('textarea').first();
  await composer.fill('');
  await settle(page);
  const slot = page.locator('.c6-send-slot').first();
  assert.equal(await slot.locator('button').count(), 2, 'both real actions share one persistent slot');
  await slot.evaluate(el => { window.__sendSlot = el; window.__sendNodes = [...el.children]; });
  // Arm before the state/input mutation. A MutationObserver schedules exactly
  // the first frame seeing the new production data-active state.
  const arm = expected => page.evaluate(expected => {
    window.__sendFrame = new Promise(resolve => {
      const observer = new MutationObserver(() => {
        const active = __sendSlot.querySelector('[data-active="true"]');
        if (!active?.getAttribute('aria-label')?.startsWith(expected)) return;
        observer.disconnect();
        requestAnimationFrame(() => {
          const inactive = __sendSlot.querySelector('[data-active="false"]');
          inactive.focus();
          const inactiveFocused = document.activeElement === inactive;
          active.focus();
          const box = active.getBoundingClientRect();
          const hit = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
          const result = { connected: __sendNodes.every((el, i) => el.isConnected && el === __sendSlot.children[i]),
            activeInert: active.inert, activeDisabled: active.disabled, activeFocused: document.activeElement === active,
            inactiveInert: inactive.inert, inactiveDisabled: inactive.disabled, inactiveFocused,
            hit: active === hit || active.contains(hit) };
          inactive.click(); // Native disabled action must do nothing.
          active.click();
          resolve(result);
        });
      });
      observer.observe(__sendSlot, { attributes: true, subtree: true });
    });
  }, expected);
  const expected = { connected: true, activeInert: false, activeDisabled: false, activeFocused: true,
    inactiveInert: true, inactiveDisabled: true, inactiveFocused: false, hit: true };
  const stopped = page.waitForResponse(response => response.url().endsWith('/sessions/parent/interrupt'));
  await arm('Stop'); await f.setState('working');
  assert.deepEqual(await page.evaluate(() => __sendFrame), expected, 'Stop owns input on its first frame');
  await stopped;
  const sent = page.waitForResponse(response => response.url().endsWith('/sessions/parent/messages'));
  await arm('Send'); await composer.fill('First-frame motion message');
  assert.deepEqual(await page.evaluate(() => __sendFrame), expected, 'typing makes Send active immediately, without waiting for its fade');
  await sent;
  assert.equal(f.requests.filter(request => request === 'POST /api/v1/sessions/parent/interrupt').length, 1);
  assert.equal(f.requests.filter(request => request === 'POST /api/v1/sessions/parent/messages').length, 1);
  assert.equal(await composer.inputValue(), '');
  await f.setState('idle'); await settle(page);
}

async function tileAction(page, action) {
  const start = await page.evaluate(() => { __motionTest.reads.length = 0; __motionTest.watching = true; return __motionTest.effects.length; });
  await action();
  await settle(page);
  const audit = await page.evaluate(start => {
    __motionTest.watching = false;
    return { mode: document.documentElement.dataset.motion, frames: [...new Set(__motionTest.reads)], effects: __motionTest.effects.slice(start).filter(e => e.workspace) };
  }, start);
  assert(audit.frames.length <= 2, `FLIP geometry must be batched before/after, never per-frame: ${JSON.stringify(audit)}`);
  if (audit.mode !== 'off') assert(audit.effects.length > 0, `workspace operation must actually animate: ${JSON.stringify(audit)}`);
  for (const effect of audit.effects) for (const frame of effect.frames) {
    assert(!Object.keys(frame).some(key => /^(left|top|right|bottom|width|height|inlineSize|blockSize)$/.test(key)),
      `tile animates layout rather than transform/opacity: ${JSON.stringify(effect)}`);
    assert(!frame.transform || /^(none|translate\([^)]*\))$/.test(frame.transform),
      `tile contents never scale, including opening/closing: ${JSON.stringify(effect)}`);
  }
  return audit;
}

async function tiles(f) {
  const { page } = f;
  const close = page.getByRole('button', { name: 'Close this tile', exact: true });
  assert.equal(await close.count(), 1);
  // Actual production modifier-click creates the second tile. Never mutate
  // saved workspace JSON to pretend an opening/reorder was animated.
  await tileAction(page, async () => {
    await page.getByRole('button', { name: 'Changes', exact: true }).first().click({ modifiers: ['Control'] });
    await page.waitForFunction(() => document.querySelectorAll('button[aria-label="Close this tile"]').length === 2);
  });
  assert.equal(await page.locator('[data-tile-motion]').count(), 2, 'tiles opt into production FLIP roots');
  const addedKey = await page.locator('[data-tile-motion]:not([data-tile-motion="2"])').getAttribute('data-tile-motion');
  const bars = page.locator('[draggable="true"][title^="Drag to move this tile"]');
  assert.equal(await bars.count(), 2);
  // Native DataTransfer with production drag handlers; target the bottom half
  // so this really changes columns/stacks, not an unchanged adjacent position.
  await tileAction(page, async () => {
    await bars.first().evaluate((source) => {
      const targetBar = [...document.querySelectorAll('[draggable="true"][title^="Drag to move this tile"]')][1];
      const target = targetBar.closest('section').parentElement;
      __motionTest.ignored = true;
      const box = target.getBoundingClientRect();
      __motionTest.ignored = false;
      const transfer = new DataTransfer();
      source.dispatchEvent(new DragEvent('dragstart', { bubbles: true, dataTransfer: transfer }));
      const options = { bubbles: true, cancelable: true, dataTransfer: transfer, clientX: box.x + box.width / 2, clientY: box.y + box.height * .75 };
      target.dispatchEvent(new DragEvent('dragover', options));
      target.dispatchEvent(new DragEvent('drop', options));
      source.dispatchEvent(new DragEvent('dragend', { bubbles: true, dataTransfer: transfer }));
    });
    await page.waitForFunction(() => {
      const value = JSON.parse(localStorage.getItem('c6.home.workspace.v2'));
      return value.tiles.length === 2 && value.tiles[0].stack === value.tiles[1].stack;
    });
  });
  // Retain the first tile's composer while another tile closes. Closing an
  // unfocused tile is one real click, not two or an atomic DOM shortcut.
  const composer = await page.locator('textarea').first().elementHandle();
  const button = page.locator(`[data-tile-motion="${addedKey}"]`).getByRole('button', { name: 'Close this tile', exact: true });
  const target = await button.elementHandle();
  const box = await button.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));
  assert(await target.evaluate(el => el.isConnected), 'pointerdown focus must not replace the close control');
  // Pointerdown focus and click-close are two separate state changes, each
  // with its own valid FLIP measurement boundary. Audit the close separately.
  await settle(page);
  await tileAction(page, async () => {
    await page.mouse.up();
    await page.waitForFunction(() => document.querySelectorAll('button[aria-label="Close this tile"]').length === 1);
  });
  assert(await composer.evaluate(el => el.isConnected), 'closing another tile retains the original session');
}

function launchBrowser(playwright, name) {
  const localLibs = '/tmp/c7-browser-libs/usr/lib/x86_64-linux-gnu';
  const libraries = process.env.CHROMIUM_LIBS || (fs.existsSync(localLibs) ? localLibs : '');
  return playwright[name].launch({ headless: true, timeout: DEADLINE,
    executablePath: name === 'webkit' ? (process.env.WEBKIT_EXECUTABLE || '/tmp/c7-webkit.sh') : (process.env.CHROMIUM || '/tmp/c7-chromium/chrome-linux/chrome'),
    ...(name === 'chromium' ? { args: ['--no-sandbox'], env: { ...process.env,
      LD_LIBRARY_PATH: [libraries, process.env.LD_LIBRARY_PATH].filter(Boolean).join(':') } } : {}) });
}

async function browserHelpers(playwright, engine) {
  const source = fs.readFileSync(path.join(ROOT, 'js/motion.js'), 'utf8');
  const { server, origin } = await serve();
  fs.mkdirSync(SHOTS, { recursive: true });
  let failures = 0, count = 0;
  try {
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      for (const choice of ['full', 'reduced', 'off']) {
        const label = `helper-${name}-list-cleanup-${choice}`, errors = [];
        let browser, page;
        try {
          browser = await launchBrowser(playwright, name);
          const context = await browser.newContext({ reducedMotion: 'no-preference', serviceWorkers: 'block' });
          page = await context.newPage(); page.setDefaultTimeout(DEADLINE);
          let fail;
          const crash = new Promise((_, reject) => { fail = reject; });
          crash.catch(() => {});
          page.on('pageerror', error => { errors.push(String(error.stack || error)); fail(error); });
          await context.route('**/*', route => {
            if (route.request().url() === origin + '/__helper') return route.fulfill({ contentType: 'text/html', body:
              '<!doctype html><style>#list>div{height:40px;width:200px}</style><div id="list"><div data-motion-key="a">A</div><div data-motion-key="b">B</div></div>' });
            errors.push(`Blocked helper request: ${route.request().url()}`);
            return route.abort();
          });
          console.log(`RUN ${label}`);
          await within(label, () => Promise.race([crash, (async () => {
            await page.goto(origin + '/__helper');
            // Execute the built helper unchanged. This read-only test adapter
            // closes over its private WeakMap; detached DOM alone cannot prove
            // the state released its reference to the removed picture.
            await page.addScriptTag({ content: source + '\nwindow.__listState = element => c6Lists.get(element);' });
            const initial = await page.evaluate(async choice => {
              c6Motion.set(choice);
              window.__list = document.querySelector('#list');
              c6FlipList(__list, JSON.stringify(['a', 'b']));
              await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
              c6FlipList(__list, JSON.stringify(['b']));
              __list.firstElementChild.remove();
              window.__ghost = __listState(__list)?.ghosts[0];
              return { retained: __listState(__list)?.ghosts.length || 0,
                inert: __ghost?.inert, hidden: __ghost?.getAttribute('aria-hidden'),
                durations: __ghost?.getAnimations().map(a => a.effect.getTiming().duration) || [],
                effects: document.getAnimations().length };
            }, choice);
            if (choice === 'off') {
              assert.equal(initial.retained, 0); assert.equal(initial.effects, 0);
            } else {
              assert.equal(initial.retained, 1); assert.equal(initial.inert, true); assert.equal(initial.hidden, 'true');
              assert.deepEqual(initial.durations, [choice === 'full' ? 150 : 80]);
              await page.waitForFunction(() => !__ghost.isConnected);
              assert.equal(await page.evaluate(() => __listState(__list).ghosts.length), 0,
                'native animation completion must release the state reference, not merely detach the ghost');
            }
            assert.deepEqual(errors, []);
            await page.screenshot({ path: path.join(SHOTS, `${label}.png`) });
            assert.deepEqual(errors, []);
            console.log(`PASS ${label} ${JSON.stringify(initial)}`); count++;
          })()]));
        } catch (error) {
          failures++;
          await page?.screenshot({ path: path.join(SHOTS, `${label}-FAIL.png`) }).catch(() => {});
          fs.writeFileSync(path.join(SHOTS, `${label}-FAIL.json`), JSON.stringify({ error: error.stack, errors }, null, 2));
          console.error(`FAIL ${label}: ${error.stack || error}`);
        } finally { await browser?.close(); }
      }
    }
    console.log(`HELPERS: ${count} pass, ${failures} fail. Screenshots: ${SHOTS}`);
    assert.equal(failures, 0, 'helper cases failed; no retries were performed');
  } finally { await new Promise(resolve => server.close(resolve)); }
}

async function main() {
  if (process.argv.includes('--unit')) { unitTests(); return; }
  if (process.argv.includes('--sizes')) { bundleSizes(); return; }
  if (process.argv.includes('--help')) {
    console.log('node crates/c6-ui/tests/motion.cjs\n' +
      'ROOT=crates/c6-ui/dist PLAYWRIGHT_CORE=/path/to/playwright-core CHROMIUM=/path/to/chrome WEBKIT_EXECUTABLE=/path/to/webkit-wrapper\n' +
      'CHROMIUM_LIBS=/path/to/shared-libraries (local /tmp/c7-browser-libs default when present)\n' +
      'ENGINE=chromium|webkit|both (default chromium; WEBKIT=1 also supported)\n' +
      'ISOLATE_BROWSER=1 owns a fresh browser process per case (useful for the local old WPE WebKit)\n' +
      'ONLY=policy,menu,drawer,sheet,sessionsPicker,modelPicker,modelForms,phone,tool,tiles,sendStop THEME=light,dark VIEWPORT=desktop,phone,narrow,landscape (default desktop,phone) MOTION_CASE=system,system-reduce,full,full-reduce,reduced,off\n' +
      'narrow=320x568; landscape=900x480; both opt-in views use the phone/short-height checks\n' +
      '--helpers tests real-DOM production list cleanup in Full/Reduced/Off (ENGINE=both selects all six)\n' +
      '--unit runs node:test VM checks of the production motion.js without a browser/build\n' +
      '--sizes reports built HTML/JS/CSS/WASM raw and deterministic gzip-9 sizes as JSON (ROOT selects before/after build)\n' +
      'SCREENSHOTS=/tmp/m1-screenshots (final and failure screenshots plus JSON diagnostics)');
    return;
  }
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  const engine = process.env.ENGINE || (process.env.WEBKIT ? 'webkit' : 'chromium');
  assert(['chromium', 'webkit', 'both'].includes(engine), `Unknown ENGINE=${engine}`);
  for (const [env, allowed] of Object.entries({ ONLY: ['policy', 'menu', 'drawer', 'sheet', 'sessionsPicker', 'modelPicker', 'modelForms', 'phone', 'tool', 'tiles', 'sendStop'],
    VIEWPORT: VIEWPORTS.map(([name]) => name), THEME: ['light', 'dark'], MOTION_CASE: CASES.map(motion => motion.name) })) {
    if (process.env[env]) assert(process.env[env].split(',').every(value => allowed.includes(value)), `Unknown ${env}=${process.env[env]}`);
  }
  if (process.argv.includes('--helpers')) { await browserHelpers(playwright, engine); return; }
  const { server, origin } = await serve();
  fs.mkdirSync(SHOTS, { recursive: true });
  const isolate = process.env.ISOLATE_BROWSER === '1';
  let count = 0;
  try {
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      const launch = () => launchBrowser(playwright, name);
      let browser;
      try {
        for (const [view, viewport] of VIEWPORTS) {
          if (!(process.env.VIEWPORT || 'desktop,phone').split(',').includes(view)) continue;
          for (const theme of ['light', 'dark'].filter(theme => selected('THEME', theme))) {
            for (const motion of CASES.filter(motion => selected('MOTION_CASE', motion.name))) {
              const label = `${name}-${view}-${theme}-${motion.name}`;
              console.log(`RUN ${label}`);
              let f;
              try {
                browser ||= await launch();
                f = await within(`${label}/fixture`, () => fixture(browser, origin, viewport, theme, motion, partial => { f = partial; }));
                f.label = label;
                for (const [test, check] of Object.entries({ policy, menu, modelPicker, ...(phoneViewport(viewport) ? { drawer, sheet, sessionsPicker, phone } : { tiles }), tool, sendStop, modelForms })) {
                  if (!selected('ONLY', test)) continue;
                  f.phase = test;
                  console.log(`  ${test}`); await within(`${label}/${test}`, () => Promise.race([check(f), f.crash]));
                }
                await assertMotion(f);
                assert.deepEqual(f.errors, [], 'no browser crashes, invalid stream messages, or external requests');
                await f.page.screenshot({ path: path.join(SHOTS, `${label}.png`) });
                assert.deepEqual(f.errors, [], 'no errors while capturing the final frame');
                console.log(`PASS ${label}`); count++;
              } catch (error) {
                if (f) {
                  await f.page.screenshot({ path: path.join(SHOTS, `${label}-FAIL.png`) }).catch(() => {});
                  const diagnostics = await f.page.evaluate(() => ({ motion: document.documentElement.dataset.motion,
                    effects: window.__motionTest?.effects, html: document.body.innerHTML })).catch(() => ({}));
                  fs.writeFileSync(path.join(SHOTS, `${label}-FAIL.json`), JSON.stringify({ error: error.stack, phase: f.phase,
                    errors: f.errors, console: f.consoleMessages, requests: f.requests, ...diagnostics }, null, 2));
                }
                throw new Error(`${label}: ${error.stack || error}`);
              } finally {
                await f?.context.close();
                if (isolate) { await browser?.close(); browser = undefined; }
              }
            }
          }
        }
      } finally { await browser?.close(); }
    }
    assert(count > 0, 'filters must select at least one case');
    console.log(`ALL PASS (${count} contexts). Screenshots: ${SHOTS}`);
  } finally { await new Promise(resolve => server.close(resolve)); }
}

function unitTests() {
  const { test } = require('node:test');
  const vm = require('node:vm');
  const source = fs.readFileSync(path.join(__dirname, '../js/motion.js'), 'utf8');
  const load = ({ choice, reduced = false, failRead = false, failWrite = false, cssAnimations = false } = {}) => {
    const storage = new Map(choice === undefined ? [] : [['c6.motion', choice]]);
    const events = new Map(), mediaEvents = new Map();
    const media = { matches: reduced, addEventListener: (name, callback) => mediaEvents.set(name, callback) };
    let canceled = 0, cssCanceled = 0, transitionCanceled = 0, changed = 0, choiceChanged = 0;
    class CSSAnimation { cancel() { cssCanceled++; } }
    class CSSTransition { cancel() { transitionCanceled++; } }
    const animations = [{ cancel() { canceled++; } }];
    if (cssAnimations) animations.push(new CSSAnimation(), new CSSTransition());
    const context = {
      localStorage: { getItem: key => { if (failRead) throw Error('storage denied'); return storage.get(key) ?? null; },
        setItem: (key, value) => { if (failWrite) throw Error('quota'); storage.set(key, value); } },
      matchMedia: query => { assert.equal(query, '(prefers-reduced-motion: reduce)'); return media; },
      document: { documentElement: { dataset: {} }, getAnimations: () => animations },
      addEventListener: (name, callback) => events.set(name, callback),
      dispatchEvent: event => {
        if (event.type === 'c6-motion') changed++;
        if (event.type === 'c6-motion-choice') choiceChanged++;
        events.get(event.type)?.(event);
      }, Event,
    };
    if (cssAnimations) Object.assign(context, { CSSAnimation, CSSTransition });
    context.window = context;
    vm.runInNewContext(source, context, { filename: 'production motion.js' });
    return { api: context.c6Motion, window: context, storage, root: context.document.documentElement,
      os: value => { media.matches = value; mediaEvents.get('change')(); },
      external: value => { storage.set('c6.motion', value); events.get('storage')({ key: 'c6.motion' }); },
      canceled: () => canceled, cssCanceled: () => cssCanceled, transitionCanceled: () => transitionCanceled,
      changed: () => changed, choiceChanged: () => choiceChanged };
  };
  test('motion.js resolves default/invalid System and follows OS changes', () => {
    for (const choice of [undefined, 'bogus', 'system']) {
      const f = load({ choice });
      assert.equal(f.api.get(), 'system'); assert.equal(f.root.dataset.motion, 'full');
      f.os(true); assert.equal(f.root.dataset.motion, 'reduced'); assert.equal(f.api.reduced(), true);
      f.os(false); assert.equal(f.root.dataset.motion, 'full'); assert.equal(f.api.off(), false);
    }
  });
  test('motion.js persists all choices, handles cross-tab changes, and Full overrides the OS', () => {
    const f = load({ reduced: true });
    for (const choice of ['full', 'reduced', 'off', 'system']) {
      f.api.set(choice);
      assert.equal(f.api.get(), choice); assert.equal(f.storage.get('c6.motion'), choice);
      assert.equal(f.root.dataset.motion, choice === 'system' ? 'reduced' : choice);
      assert.equal(f.api.off(), choice === 'off');
    }
    f.api.set('full'); const changes = f.changed(); f.os(false); f.os(true);
    assert.equal(f.root.dataset.motion, 'full'); assert.equal(f.api.reduced(), false);
    assert.equal(f.changed(), changes, 'Full ignores OS changes without interrupting in-flight effects');
    f.external('off'); assert.equal(f.root.dataset.motion, 'off');
    assert(f.canceled() > 0, 'non-Full preference cancels in-flight effects'); assert(f.changed() > 1);
  });
  test('motion.js tolerates unavailable storage and failed writes before first paint', () => {
    const denied = load({ reduced: true, failRead: true, failWrite: true });
    assert.equal(denied.root.dataset.motion, 'reduced');
    assert.doesNotThrow(() => denied.api.set('off'));
    assert.equal(denied.api.get(), 'system');
    const quota = load({ choice: 'full', reduced: true, failWrite: true });
    assert.doesNotThrow(() => quota.api.set('reduced'));
    assert.equal(quota.root.dataset.motion, 'full', 'failed persistence retains the stored preference');
  });
  test('motion choices notify mounted pickers even when the effective policy stays Full', () => {
    const f = load({ choice: 'system' });
    const changes = f.changed(), canceled = f.canceled();
    f.api.set('full');
    f.api.set('system');
    f.external('full');
    assert.equal(f.choiceChanged(), 3, 'same-document and cross-tab choices notify pickers');
    assert.equal(f.changed(), changes, 'preference-only changes do not signal an effective-policy change');
    assert.equal(f.canceled(), canceled, 'System/Full switches do not interrupt motion');
  });
  test('motion.js non-Full cancels JavaScript/captured transitions but preserves variable-driven CSS keyframes', () => {
    const f = load({ choice: 'full', cssAnimations: true });
    assert.equal(f.canceled(), 0);
    f.api.set('reduced'); assert.equal(f.canceled(), 1); assert.equal(f.cssCanceled(), 0); assert.equal(f.transitionCanceled(), 1);
    f.api.set('off'); assert.equal(f.canceled(), 2); assert.equal(f.cssCanceled(), 0); assert.equal(f.transitionCanceled(), 2);
  });
  test('production drawer swipe claims only clear left touch drags and releases through shared CSS', () => {
    const setup = (choice = 'full') => {
      const f = load({ choice }), listeners = new Map(), global = new Map(), mediaListeners = new Map();
      const attributes = new Map(), styles = new Map(), captured = new Set();
      let closed = 0, reads = 0, prevented = 0;
      const media = { matches: true, addEventListener: (type, fn) => mediaListeners.set(type, fn),
        removeEventListener: type => mediaListeners.delete(type) };
      f.window.matchMedia = query => { assert.equal(query, '(width < 48rem), (height < 32rem)'); return media; };
      f.window.addEventListener = (type, fn) => global.set(type, fn);
      f.window.removeEventListener = type => global.delete(type);
      f.window.getComputedStyle = () => { reads++; return { translate: '0px' }; };
      const element = { dataset: { open: 'true' },
        getBoundingClientRect: () => { reads++; return { width: 288, left: 0 }; },
        style: { setProperty: (key, value) => styles.set(key, value), removeProperty: key => styles.delete(key) },
        setAttribute: (key, value) => attributes.set(key, value), removeAttribute: key => attributes.delete(key),
        setPointerCapture: id => captured.add(id), hasPointerCapture: id => captured.has(id),
        releasePointerCapture: id => captured.delete(id),
        addEventListener: (type, fn) => listeners.set(type, fn), removeEventListener: type => listeners.delete(type) };
      const dispose = f.window.c6DrawerSwipe(element, () => { closed++; element.dataset.open = 'false'; });
      const send = (type, x = 200, y = 100, extra = {}) => listeners.get(type)?.({ pointerType: 'touch', isPrimary: true,
        pointerId: 1, clientX: x, clientY: y, target: element, preventDefault: () => { prevented++; }, ...extra });
      return { send, element, attributes, styles, captured, global, media, mediaListeners, dispose,
        closed: () => closed, reads: () => reads, prevented: () => prevented, listeners };
    };
    for (const choice of ['full', 'reduced', 'off']) {
      const f = setup(choice);
      f.send('pointerdown'); f.send('pointermove', 195, 102);
      assert.equal(f.captured.size, 0, 'tap slop does not capture'); assert.equal(f.prevented(), 0);
      f.send('pointerup'); assert.equal(f.closed(), 0);
      f.send('pointerdown'); f.send('pointermove', 160, 103);
      assert(f.attributes.has('data-dragging')); assert(f.captured.has(1));
      f.send('lostpointercapture', 160, 103, { target: {} });
      assert(f.attributes.has('data-dragging'), 'implicit capture transferring from a child does not cancel the drawer drag');
      assert.equal(f.styles.get('--drawer-pull'), '-40px');
      const reads = f.reads();
      f.send('pointermove', 60, 103); f.send('pointermove', 175, 101);
      assert.equal(f.reads(), reads, 'moves never measure layout');
      assert.equal(f.styles.get('--drawer-pull'), '-25px');
      assert.equal(f.closed(), 0, 'crossing close threshold waits for release');
      f.send('pointerup'); assert.equal(f.closed(), 0, 'short reversal returns to open');
      assert.equal(f.captured.size, 0); assert.equal(f.styles.size, 0); assert.equal(f.attributes.size, 0);
      let stopped = 0;
      f.send('click', 0, 0, { detail: 1, stopImmediatePropagation: () => { stopped++; } });
      assert.equal(stopped, 1, 'compatibility click cannot activate a link or close after snap-back');
      f.send('pointerdown'); f.send('pointermove', 50, 100); f.send('pointerup');
      assert.equal(f.closed(), 1, `${choice} retains gesture semantics; CSS alone scales motion`);
      f.dispose(); assert.equal(f.listeners.size, 0); assert.equal(f.global.size, 0); assert.equal(f.mediaListeners.size, 0);
    }
    for (const [dx, dy] of [[-5, 40], [-25, 25], [30, 0]]) {
      const f = setup(); f.send('pointerdown'); f.send('pointermove', 200 + dx, 100 + dy);
      f.send('pointermove', 20, 100); f.send('pointerup');
      assert.equal(f.prevented(), 0, 'vertical, diagonal and rightward starts stay native for the entire gesture');
      assert.equal(f.closed(), 0); assert.equal(f.captured.size, 0); f.dispose();
    }
    for (const end of ['pointercancel', 'lostpointercapture', 'policy', 'escape', 'resize', 'dispose']) {
      const f = setup(); f.send('pointerdown'); f.send('pointermove', 40, 100);
      if (end === 'policy') f.global.get('c6-motion')();
      else if (end === 'escape') f.global.get('keydown')({ key: 'Escape' });
      else if (end === 'resize') f.mediaListeners.get('change')();
      else if (end === 'dispose') f.dispose();
      else f.send(end);
      f.send('pointerup'); assert.equal(f.closed(), 0, `${end} must cancel, not dismiss`);
      assert.equal(f.captured.size, 0); assert.equal(f.attributes.size, 0); assert.equal(f.styles.size, 0);
      f.dispose();
    }
    for (const mode of ['mouse', 'pen', 'secondary', 'desktop', 'closed']) {
      const f = setup();
      if (mode === 'desktop') f.media.matches = false;
      if (mode === 'closed') f.element.dataset.open = 'false';
      f.send('pointerdown', 200, 100, mode === 'secondary' ? { isPrimary: false } :
        ['mouse', 'pen'].includes(mode) ? { pointerType: mode } : {});
      f.send('pointermove', 20, 100); f.send('pointerup');
      assert.equal(f.closed(), 0); assert.equal(f.prevented(), 0); f.dispose();
    }
    const f = setup(); f.send('pointerdown'); f.send('pointerup');
    let stopped = false;
    f.send('click', 0, 0, { detail: 1, stopImmediatePropagation: () => { stopped = true; } });
    assert(!stopped, 'ordinary taps keep their normal click'); f.dispose();
    const css = fs.readFileSync(path.join(__dirname, '../style/input.css'), 'utf8');
    assert.match(css, /\.phone-drawer\s*\{[^}]*touch-action:\s*pan-y pinch-zoom/);
    assert.match(css, /\.phone-drawer\[data-open="true"\]\[data-dragging\]\s*\{[^}]*var\(--drawer-pull, 0px\)\s*\*\s*var\(--travel\)[^}]*transition:\s*none/);
  });
  test('Off hides outgoing count and notice decoration instead of exposing animation base styles', () => {
    const css = fs.readFileSync(path.join(__dirname, '../style/input.css'), 'utf8');
    for (const name of ['c6-count-out', 'c6-crossfade-out']) {
      const rule = new RegExp(`:root\\[data-motion="off"\\][^{}]*\\.${name}[^{}]*\\{([^{}]*)\\}`, 'g');
      assert([...css.matchAll(rule)].some(match => /\bdisplay\s*:\s*none\s*[;!}]/.test(match[1])),
        `Off must hide .${name}, not merely disable its animation`);
    }
  });
  test('notice host retains its last content, closes inert, and has no ticking countdown', () => {
    const source = fs.readFileSync(path.join(__dirname, '../src/util.rs'), 'utf8');
    const start = source.indexOf('pub fn NoticeHost()');
    assert(start >= 0);
    const host = source.slice(start, source.indexOf('/// How something that pops up', start));
    assert.match(host, /let last = RwSignal::new/);
    assert.match(host, /class="c6-presence notice-safe/);
    assert.match(host, /data-open=[^\n]*Option::is_some[^\n]*inert=[^\n]*Option::is_none/);
    assert.match(host, /c6-rise[^\n]*role="status"/);
    assert.match(host, /last\.get\(\)/);
    assert.match(host, /notice\.shown\.set\(None\);\s*u\.run\(\(\)\)/);
    assert.doesNotMatch(host, /<progress\b|role="progressbar"|countdown|set_interval\(|request_animation_frame\(|\bclock\(/i,
      'notice expiry is a timeout, not a ticking visual countdown');
    const css = fs.readFileSync(path.join(__dirname, '../style/input.css'), 'utf8');
    assert.match(css, /\.c6-presence\[data-open="false"\][^{]*\{[^}]*pointer-events:\s*none/);
    assert.match(css, /\[data-open="false"\]\s*>\s*\.c6-rise/);
  });
  test('production list FLIP batches geometry, enters/removes keys, and cancels interrupted frames', () => {
    assert.match(source, /state\.ghosts = state\.ghosts\.filter\(item => item !== ghost\)/,
      'finished removal pictures must be released, not retained until another list change');
    for (const choice of ['full', 'reduced', 'off']) {
      const f = load({ choice }), raf = new Map(), reads = [], effects = [], ghosts = [];
      let frame = 0, next = 1, canceledFrames = 0;
      f.window.requestAnimationFrame = callback => { const id = next++; raf.set(id, callback); return id; };
      f.window.cancelAnimationFrame = id => { if (raf.delete(id)) canceledFrames++; };
      f.window.document.body = { append: ghost => ghosts.push(ghost) };
      const flush = () => { frame++; const work = [...raf.values()]; raf.clear(); work.forEach(callback => callback()); };
      const node = (key, top) => ({
        dataset: { motionKey: key }, top, style: {}, attributes: {}, isConnected: true,
        getBoundingClientRect() { reads.push(frame); return { left: 0, top: this.top, width: 100, height: 20 }; },
        cloneNode() { return node(key + '-ghost', this.top); },
        setAttribute(name, value) { this.attributes[name] = value; },
        removeAttribute(name) { if (name === 'data-motion-key') delete this.dataset.motionKey; delete this.attributes[name]; },
        querySelectorAll() { return []; }, remove() { this.isConnected = false; },
        animate(frames, options) {
          const effect = { key, frames, ...options, canceled: false };
          effects.push(effect);
          return { cancel() { effect.canceled = true; this.oncancel?.(); } };
        },
      });
      const a = node('a', 0), b = node('b', 20), c = node('c', 0);
      const list = { children: [a, b], isConnected: true };
      f.window.c6FlipList(list, JSON.stringify(['a', 'b'])); flush();
      assert.equal(effects.length, 0, 'initial mounted list does not animate');
      f.window.c6FlipList(list, JSON.stringify(['c', 'a', 'b']));
      list.children = [c, a, b]; a.top = 20; b.top = 40;
      flush();
      if (choice === 'off') {
        assert.equal(reads.length, 0); assert.equal(effects.length, 0); assert.equal(raf.size, 0);
        continue;
      }
      const entered = effects.find(effect => effect.key === 'c');
      assert.equal(entered.duration, choice === 'full' ? 200 : 80);
      assert.equal(entered.frames[0].opacity, 0);
      assert.equal(entered.frames[0].transform, choice === 'full' ? 'translateY(6px)' : undefined);
      assert(effects.filter(effect => effect.key !== 'c').every(effect => effect.duration === (choice === 'full' ? 160 : 80)));
      f.window.c6FlipList(list, JSON.stringify(['a', 'b']));
      assert(effects.some(effect => effect.canceled), 'interrupted entry/moves are canceled');
      const ghost = ghosts.at(-1);
      assert(ghost.inert); assert.equal(ghost.attributes['aria-hidden'], 'true');
      assert.equal(effects.at(-1).duration, choice === 'full' ? 150 : 80);
      list.children = [a, b]; a.top = 0; b.top = 20; flush();
      // Two changes before a paint must replace the pending frame, not queue
      // an obsolete layout measurement behind it.
      f.window.c6FlipList(list, JSON.stringify(['a', 'b', 'c']));
      list.children.push(c);
      f.window.c6FlipList(list, JSON.stringify(['a', 'b']));
      list.children = [a, b];
      assert.equal(raf.size, 1); assert(canceledFrames > 0); flush();
      const count = reads.length; flush(); flush();
      assert.equal(reads.length, count, 'no geometry reads on animation frames after the first/last boundary');
      assert(effects.every(effect => effect.frames.every(keyframe => !('height' in keyframe) && !('width' in keyframe))));
      if (choice === 'reduced') assert(effects.every(effect => effect.frames.every(keyframe => !('transform' in keyframe))));
    }
  });
  test('bounded resize FLIP reconstructs the displayed height when growth interrupts a move', () => {
    for (const choice of ['full', 'reduced', 'off']) {
      const f = load({ choice }), effects = [];
      let callback, height = 100, scale = 1, opacity = .7, reads = 0, disconnected = false;
      f.window.ResizeObserver = class {
        constructor(onResize) { callback = onResize; }
        observe() {} disconnect() { disconnected = true; }
      };
      f.window.getComputedStyle = () => ({ opacity });
      const el = { style: {},
        getBoundingClientRect() { reads++; return { height: height * scale }; },
        animate(frames, options) {
          const effect = { frames, ...options, cancel() { scale = 1; this.oncancel?.(); } };
          effects.push(effect); return effect;
        },
      };
      const dispose = f.window.c6FlipResize(el);
      callback(); assert.equal(effects.length, 0, 'initial observer notification does not animate');
      height = 200; callback();
      if (choice === 'off') { assert.equal(effects.length, 0); dispose(); continue; }
      const first = effects.at(-1);
      assert.equal(first.duration, choice === 'full' ? 220 : 80);
      if (choice === 'full') {
        assert.equal(first.frames[0].transform, 'scaleY(0.5)');
        scale = .75; // The previous picture is currently 150px tall.
        height = 300; callback();
        assert.equal(effects.at(-1).frames[0].transform, 'scaleY(0.5)', 'start at displayed 150px, not new-layout 225px');
        first.onfinish(); // A stale completion cannot clear the replacement.
        scale = .5; height = 400; callback();
        assert.equal(effects.at(-1).frames[0].transform, 'scaleY(0.375)');
      } else {
        height = 300; callback();
        assert.equal(effects.at(-1).frames[0].opacity, opacity, 'interrupted fade starts at its displayed opacity');
        assert(effects.every(effect => effect.frames.every(frame => !('transform' in frame))));
      }
      const sampled = reads;
      for (const effect of effects) effect.onfinish?.();
      assert.equal(reads, sampled, 'completion does not measure geometry');
      dispose(); assert(disconnected);
    }
  });
  test('production glide shares the 200ms arrival clock, batches reads, and cancels or snaps immediately', () => {
    const setup = choice => {
      const f = load({ choice }), frames = new Map(), listeners = new Map(), writes = [];
      let time = 0, serial = 0, reads = 0, top = 0, height = 200;
      f.window.performance = { now: () => time };
      f.window.requestAnimationFrame = callback => { const id = ++serial; frames.set(id, callback); return id; };
      f.window.cancelAnimationFrame = id => frames.delete(id);
      f.window.Element = class { closest() { return null; } };
      const el = new f.window.Element();
      Object.assign(el, {
        isConnected: true,
        addEventListener: (name, callback) => listeners.set(name, callback),
        removeEventListener: name => listeners.delete(name),
        scrollTo(options) { assert.equal(options.behavior, 'instant'); top = options.top; writes.push(top); },
      });
      for (const [name, value] of Object.entries({ scrollTop: () => top, scrollHeight: () => height, clientHeight: () => 100 })) {
        Object.defineProperty(el, name, { get() { reads++; return value(); } });
      }
      return { ...f, el, frames, writes, listeners,
        top: () => top, reads: () => reads, grow: value => { height = value; },
        frame(at) { time = at; const work = [...frames.values()]; frames.clear(); work.forEach(callback => callback(at)); },
      };
    };
    const f = setup('full');
    f.window.c6GlideToEnd(f.el);
    assert.equal(f.reads(), 3);
    f.frame(50); assert(f.top() > 0 && f.top() < 100);
    assert.equal(f.reads(), 3, 'frames write scroll position without remeasuring');
    f.window.c6GlideToEnd(f.el); // Same destination must retain its original deadline.
    f.frame(200); assert.equal(f.top(), 100); assert.equal(f.frames.size, 0); assert.equal(f.reads(), 6);

    const interrupted = setup('full');
    interrupted.window.c6GlideToEnd(interrupted.el); interrupted.frame(60);
    const displayed = interrupted.top(); interrupted.grow(250);
    interrupted.window.c6GlideToEnd(interrupted.el);
    assert.equal(interrupted.top(), displayed, 'a new destination starts from the displayed position');
    interrupted.frame(260); assert.equal(interrupted.top(), 150); assert.equal(interrupted.reads(), 6);

    for (const input of ['wheel', 'touchstart', 'touchmove', 'pointerdown', 'keydown']) {
      const canceled = setup('full');
      canceled.window.c6GlideToEnd(canceled.el); canceled.frame(40);
      canceled.listeners.get(input)({ key: 'ArrowUp', target: canceled.el });
      const count = canceled.writes.length; canceled.frame(200);
      assert.equal(canceled.frames.size, 0); assert.equal(canceled.listeners.size, 0);
      assert.equal(canceled.writes.length, count, `${input} cancels without later writes`);
      assert.equal(canceled.reads(), 3);
    }
    const snapped = setup('full');
    snapped.window.c6GlideToEnd(snapped.el); snapped.frame(40);
    snapped.window.c6StopGlide(snapped.el); snapped.el.scrollTo({ top: 100, behavior: 'instant' });
    snapped.frame(200); assert.equal(snapped.top(), 100); assert.equal(snapped.reads(), 3);
    const changed = setup('full');
    changed.window.c6GlideToEnd(changed.el); changed.frame(40); changed.api.set('reduced');
    assert.equal(changed.top(), 100); assert.equal(changed.frames.size, 0); assert.equal(changed.reads(), 3);
    for (const choice of ['reduced', 'off']) {
      const still = setup(choice); still.window.c6GlideToEnd(still.el);
      assert.equal(still.top(), 100); assert.equal(still.frames.size, 0); assert.equal(still.reads(), 3);
    }
    const far = setup('full'); far.grow(400); far.window.c6GlideToEnd(far.el);
    assert.equal(far.top(), 300); assert.equal(far.frames.size, 0);
    const detached = setup('full');
    detached.window.c6GlideToEnd(detached.el); detached.frame(40); detached.el.isConnected = false;
    const count = detached.writes.length; detached.frame(200);
    assert.equal(detached.writes.length, count); assert.equal(detached.frames.size, 0);
  });
}

function bundleSizes() {
  const zlib = require('node:zlib');
  assert(fs.existsSync(path.join(ROOT, 'index.html')), `No built UI at ${ROOT}`);
  const files = [];
  const visit = directory => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const file = path.join(directory, entry.name);
      if (entry.isDirectory()) visit(file);
      else if (/\.(html|js|css|wasm)$/.test(entry.name)) {
        const content = fs.readFileSync(file);
        files.push({ path: path.relative(ROOT, file), raw: content.length, gzip9: zlib.gzipSync(content, { level: 9 }).length });
      }
    }
  };
  visit(ROOT);
  files.sort((a, b) => a.path.localeCompare(b.path));
  const totals = {};
  for (const file of files) {
    const type = path.extname(file.path).slice(1);
    const total = totals[type] ||= { raw: 0, gzip9: 0, files: 0 };
    total.raw += file.raw; total.gzip9 += file.gzip9; total.files++;
  }
  console.log(JSON.stringify({ root: ROOT, compression: 'per-file gzip level 9 estimate; not head transfer measurements', totals, files }, null, 2));
}

module.exports = { serve, fixture, settle, within, launchBrowser };
if (require.main === module) main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
