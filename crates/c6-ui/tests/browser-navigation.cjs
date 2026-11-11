'use strict';

const diagnostic = origin => `Fetch API cannot load ${origin}/api/v1/attention due to access control checks.`;
// Playwright splits WebKit's name/message at the URL colon. Its complete first
// stack line retains the exact diagnostic and endpoint, without substring rules.
const exactDiagnostic = (error, origin) => typeof error.stack === 'string' &&
  error.stack.split('\n')[0] === diagnostic(origin);

async function navigationObserver({ page, engine, origin, fatal, log = console.error }) {
  const ledger = [], rawPageErrors = [];
  let active, replacing = false, serial = 0;
  const end = (window, reason) => {
    if (active !== window) return;
    ledger.push({ kind: 'replacement-end', id: window.id, reason });
    active = undefined;
  };
  page.on('load', () => { if (active) end(active, 'destination-load'); });
  page.on('close', () => { if (active) end(active, 'page-close'); });

  function pageError(error) {
    const raw = { kind: 'pageerror', stack: String(error.stack || error), name: error.name, message: error.message };
    ledger.push(raw); rawPageErrors.push(raw);
    log('PAGEERROR', error.stack || error);
    // Deliberate arrival-window policy: no routing provenance, document tokens,
    // retained execution contexts, or retrospective attribution.
    if (engine === 'webkit' && active && exactDiagnostic(error, origin)) {
      raw.classification = 'webkit-native-attention-navigation-refusal';
      ledger.push({ kind: 'classified-native', id: active.id, stack: raw.stack });
      log('CLASSIFIED NATIVE NAVIGATION DIAGNOSTIC', raw.stack);
    } else fatal(error);
  }
  async function drain() {
    // Keep the existing task-turn wait before final error assertions, without
    // installing error/rejection handlers or collecting document identity.
    await page.evaluate(() => new Promise(resolve => setTimeout(() => setTimeout(resolve, 0), 0)));
  }
  async function replace(destination, options, reload = false) {
    if (replacing) throw new Error('overlapping explicit cold replacement');
    if (new URL(destination).origin !== origin) throw new Error('cold replacement must stay on owned origin');
    replacing = true;
    const window = { id: `cold-replacement-${++serial}` };
    active = window;
    ledger.push({ kind: 'replacement-start', operation: reload ? 'reload' : 'goto', id: window.id, destination });
    try {
      const response = await (reload ? page.reload(options) : page.goto(destination, options));
      // Callers may request DOMContentLoaded, but tolerance lasts only until
      // destination load and must finish before their final error assertions.
      await page.waitForLoadState('load');
      end(window, 'destination-load');
      await drain();
      return response;
    } finally {
      end(window, 'operation-ended');
      replacing = false;
    }
  }
  return { pageError, navigate: (destination, options) => replace(destination, options),
    reload: options => replace(page.url(), options, true), drain, ledger, rawPageErrors };
}

module.exports = { diagnostic, exactDiagnostic, navigationObserver };
