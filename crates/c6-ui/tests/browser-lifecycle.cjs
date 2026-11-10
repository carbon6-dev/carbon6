'use strict';

// Passive, synchronous diagnostics. No timers, protocol calls, promise handlers,
// retries or error classification. Only session-sheet opts in for now.
const identities = new WeakMap();
const closeHooks = new WeakMap();
let serial = 0;
const identity = (object, kind) => {
  if (!identities.has(object)) identities.set(object, `${kind}-${++serial}`);
  return identities.get(object);
};
const describeError = error => ({ name: error?.name || typeof error,
  message: String(error?.message ?? error), stack: String(error?.stack ?? error) });

// One wrapper per method owner, not a stack of observer-owned wrappers. Either
// disposal order removes only its subscriber; the last restores the exact
// original descriptor, never an already-disposed observer's wrapper.
function subscribeClose(object, notify) {
  let hook = closeHooks.get(object);
  if (!hook || object.close !== hook.wrapper) {
    const original = object.close, descriptor = Object.getOwnPropertyDescriptor(object, 'close');
    hook = { subscribers: new Set(), wrapper: undefined };
    const subscribers = hook.subscribers;
    // Not async: preserve receiver, arguments, synchronous throw and exact
    // return (including promise identity). Never inspect argument properties.
    hook.wrapper = function close(...args) {
      for (const subscriber of subscribers) {
        try { subscriber(args.length); } catch { /* Diagnostics cannot prevent the real close. */ }
      }
      return Reflect.apply(original, this, args);
    };
    Object.defineProperty(object, 'close', { configurable: descriptor?.configurable ?? true, writable: true,
      enumerable: descriptor?.enumerable ?? false, value: hook.wrapper });
    hook.restore = () => {
      // Do not erase a method installed by a different owner after ours.
      if (object.close === hook.wrapper) {
        if (descriptor) Object.defineProperty(object, 'close', descriptor);
        else delete object.close;
      }
      if (closeHooks.get(object) === hook) closeHooks.delete(object);
    };
    closeHooks.set(object, hook);
  }
  hook.subscribers.add(notify);
  return () => {
    hook.subscribers.delete(notify);
    if (hook.subscribers.size === 0) hook.restore();
  };
}

function browserLifecycle({ browser, label, clock = () => performance.now(), utc = () => Date.now(),
  cpu = () => process.cpuUsage(), stack = () => new Error('intentional close').stack }) {
  const started = clock(), startedUtc = utc(), events = [], secondaryErrors = [], removals = [];
  const observed = new WeakSet();
  const contexts = new Map(), pages = new Map();
  let phase = 'fixture', firstError, disposed = false, ended;
  let observerCalls = 0, observerWallMs = 0, observerCpuMicros = 0;
  const measure = action => {
    const begin = clock(), usage = cpu();
    try { return action(); }
    finally {
      const endUsage = cpu();
      observerCalls++;
      observerWallMs += clock() - begin;
      observerCpuMicros += endUsage.user - usage.user + endUsage.system - usage.system;
    }
  };
  const stamp = () => ({ ms: clock() - started, utcMs: utc(), phase });
  const record = (kind, target, detail = {}) => events.push({ ...stamp(), kind, target, ...detail });
  const secondary = (stage, error) => secondaryErrors.push({ ...stamp(), stage, error: describeError(error) });
  const read = action => {
    try { return action(); }
    catch (error) { return { unavailable: describeError(error) }; }
  };
  const inventory = () => {
    // Live collections remove closed targets. Retain explicitly observed ones
    // independently, but capture only plain values in the first-error snapshot.
    const liveContexts = read(() => browser.contexts());
    const liveContextSet = Array.isArray(liveContexts) ? new Set(liveContexts) : undefined;
    return { browser: identity(browser, 'browser'), connected: read(() => browser.isConnected()),
      liveContexts: liveContextSet ? [...liveContextSet].map(context => identity(context, 'context')) : liveContexts,
      contexts: [...new Set([...contexts.keys(), ...(liveContextSet || [])])].map(context => {
        const state = contexts.get(context), livePages = read(() => context.pages());
        const livePageSet = Array.isArray(livePages) ? new Set(livePages) : undefined;
        const inLiveContexts = liveContextSet ? liveContextSet.has(context) : null;
        const retainedPages = [...pages].filter(([, state]) => state.context === context).map(([page]) => page);
        return { id: identity(context, 'context'), observed: !!state, inLiveContexts,
          // BrowserContext has no public isClosed(). Removal alone is not proof.
          closed: state?.closed ? true : inLiveContexts === true ? false : null,
          closedSource: state?.closed ? 'contextclose-event' : inLiveContexts === true ? 'live-browser-collection' : 'unknown',
          livePages: livePageSet ? [...livePageSet].map(page => identity(page, 'page')) : livePages,
          pages: [...new Set([...retainedPages, ...(livePageSet || [])])].map(page => {
            const state = pages.get(page), closed = read(() => page.isClosed()), url = read(() => page.url());
            return { id: identity(page, 'page'), observed: !!state,
              inLivePages: livePageSet ? livePageSet.has(page) : null,
              closed: state?.closed ? true : typeof closed === 'boolean' ? closed : null,
              closedSource: state?.closed ? 'pageclose-event' : typeof closed === 'boolean' ? 'page.isClosed' : 'unknown',
              closedQueryError: closed?.unavailable, closeObserved: !!state?.closed, crashObserved: !!state?.crashed,
              url: typeof url === 'string' ? url : state?.url ?? null,
              urlSource: typeof url === 'string' ? 'page.url' : state?.url != null ? 'last-observed' : 'unknown',
              urlQueryError: url?.unavailable };
          }) };
      }) };
  };
  const listen = (object, event, callback) => {
    object.on(event, callback);
    removals.push(() => object.removeListener(event, callback));
  };
  const wrapClose = (object, kind) => {
    if (typeof object.close !== 'function') return;
    try {
      removals.push(subscribeClose(object, argumentCount => {
        if (!disposed) try {
          measure(() => record('intentionalclose', identity(object, kind), { method: `${kind}.close`,
            argumentCount, callsite: stack() }));
        } catch (error) { secondary('intentionalclose instrumentation', error); }
      }));
    } catch (error) { secondary(`${kind}.close instrumentation`, error); }
  };
  const observePage = (page, context) => {
    if (observed.has(page)) return;
    observed.add(page);
    const id = identity(page, 'page');
    const state = { context, closed: false, crashed: false, url: null };
    const rememberUrl = () => {
      const url = read(() => page.url());
      if (typeof url === 'string') state.url = url;
    };
    pages.set(page, state);
    rememberUrl();
    record('observepage', id);
    listen(page, 'close', () => measure(() => { state.closed = true; rememberUrl(); record('pageclose', id); }));
    listen(page, 'crash', () => measure(() => { state.crashed = true; rememberUrl(); record('pagecrash', id); }));
    wrapClose(page, 'page');
  };
  measure(() => {
    const id = identity(browser, 'browser');
    record('observebrowser', id);
    listen(browser, 'disconnected', () => measure(() => record('browserdisconnect', id)));
    wrapClose(browser, 'browser');
  });
  return {
    observe({ context, page }) {
      if (disposed) return;
      measure(() => {
        if (!observed.has(context)) {
          observed.add(context);
          const id = identity(context, 'context');
          const state = { closed: false };
          contexts.set(context, state);
          record('observecontext', id);
          listen(context, 'close', () => measure(() => { state.closed = true; record('contextclose', id); }));
          listen(context, 'page', page => measure(() => observePage(page, context)));
          wrapClose(context, 'context');
          for (const existing of context.pages()) observePage(existing, context);
        }
        observePage(page, context);
      });
    },
    phase(value) { if (!disposed) measure(() => { phase = value; }); },
    firstError(error) {
      if (disposed || firstError) return;
      measure(() => { firstError = { ...stamp(), error: describeError(error), inventory: inventory() }; });
    },
    secondary(stage, error) { if (!disposed) measure(() => secondary(stage, error)); },
    snapshot() {
      const result = measure(() => ({ label, pid: process.pid, startedUtcMs: startedUtc,
        durationMs: (ended ?? clock()) - started, disposed, phase,
        retainedReferences: { contexts: contexts.size, pages: pages.size },
        firstError, events: events.slice(), secondaryErrors: secondaryErrors.slice() }));
      result.overhead = { observerCalls, observerWallMs, observerCpuMicros,
        scope: 'synchronous observer callbacks/setup/disposal; excludes wrapper dispatch, evidence serialization and file IO' };
      return result;
    },
    dispose() {
      if (disposed) return;
      measure(() => {
        for (const remove of removals.reverse()) {
          try { remove(); } catch (error) { secondary('dispose', error); }
        }
        removals.length = 0;
        contexts.clear(); pages.clear();
        disposed = true;
        ended = clock();
      });
    },
  };
}

module.exports = { browserLifecycle };
