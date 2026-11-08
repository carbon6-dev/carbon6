#!/usr/bin/env node
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const field = (row, title) => row.locator('label').filter({ has: row.page().locator('span').filter({ hasText: new RegExp(`^${title}$`) }) }).locator('input, select, textarea');
async function navigateSection(page, id, title) {
  const picker = page.locator('select').filter({ has: page.locator('option[value="commands"]') });
  if (await picker.isVisible()) await picker.selectOption(id);
  else await picker.locator('..').getByRole('button', { name: title, exact: true }).click();
}

async function check(browser, origin, phone, theme, engine) {
  const saved = {
    commands: [{ name: 'original', description: 'first', body: 'Keep all text', enabled: true }, { name: 'remove-me', enabled: true }],
    templates: [{ name: 'template', connection: 'missing-connection', model: 'test', effort: 'medium', workers: ['old'],
      worktree_options: { base: 'main', setup: ['one', 'two'], fetch: false }, enabled: true }],
    views: [{ name: 'board', kind: 'board', source: { collection: 'deals' }, actions: [], enabled: true }],
    'mcp-servers': [{ name: 'initial', transport: 'http', url: 'https://example.invalid/initial' }],
    routes: {
      root: [{ name: 'root-rule', tools: ['old'], enabled: true }],
      home: [{ name: 'home-rule', tools: [], enabled: true }],
    },
  };
  const writes = [];
  let rejectSave = true;
  const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 }, theme,
    { choice: 'off', effective: 'off', os: 'no-preference' }, () => {}, {}, {
      path: '/settings/commands', ready: 'input[placeholder="name, typed as /name"]', instrumentation: false,
      layout: { tiles: [{ key: 2, columns: [{ key: 3, view: { view: 'settings', section: 'commands' } }],
        cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 4, full: false },
      setup: async ({ context }) => {
        // The shared loopback file server serves realm routes; supply the
        // identical built document for this console route too.
        await context.route('**/admin/settings/routing', route => route.fulfill({ contentType: 'text/html',
          body: fs.readFileSync(path.resolve(process.env.ROOT || path.join(__dirname, '../dist'), 'index.html'), 'utf8') }));
        await context.route('**/api/v1/realms', route => route.fulfill({ json: [{ id: 'home', name: 'Home',
          description: '', created_at: 1, created_by: 'you' }] }));
        await context.route('**/api/v1/settings/**', async route => {
          const url = new URL(route.request().url()), key = url.pathname.split('/').pop();
          if (!Object.hasOwn(saved, key)) return route.fallback();
          const realm = url.searchParams.get('realm');
          if (route.request().method() !== 'PUT') return route.fulfill({ json: key === 'routes' ? saved.routes[realm] : saved[key] });
          const value = route.request().postDataJSON();
          writes.push({ key, realm, value });
          if (rejectSave) return route.fulfill({ status: 400, json: { error: 'Fixture validation error' } });
          if (key === 'routes') saved.routes[realm] = value;
          else saved[key] = value;
          return route.fulfill({ status: 204 });
        });
      },
    });
  try {
    const { page } = f;
    const step = name => { if (process.env.DEBUG_FIXTURE) console.log(`  SETTINGS ${name}`); };
    step('commands');
    const section = page.getByRole('heading', { name: 'Slash commands', exact: true }).locator('xpath=../..');
    const names = section.getByPlaceholder('name, typed as /name');
    assert.equal(await names.count(), 2);
    await names.first().fill('edited');
    await names.first().evaluate(el => { window.__savedListInput = el; });
    await f.setState('running');
    await settle(page);
    assert(await names.first().evaluate(el => el === window.__savedListInput && el === document.activeElement), 'live snapshot retains focused field');
    await page.keyboard.type('-tail');
    assert.equal(await names.first().inputValue(), 'edited-tail');
    let row = names.first().locator('xpath=../..');
    await field(row, 'Applies to').fill('repo:example, env:test');
    await field(row, 'Prompt').fill('Preserved\nmultiline prompt');
    await row.getByRole('checkbox', { name: 'Enabled', exact: true }).uncheck();
    await section.getByRole('button', { name: 'Remove', exact: true }).nth(1).click();
    assert.equal(await names.count(), 1);
    assert(await names.first().evaluate(el => el === window.__savedListInput), 'keyed sibling removal preserves row DOM');
    await section.locator('select').selectOption({ label: 'Empty' });
    assert.equal(await names.count(), 2);
    await names.nth(1).fill('added');
    await section.getByRole('button', { name: 'Save', exact: true }).click();
    await section.getByText('Fixture validation error', { exact: true }).waitFor();
    assert.equal(await names.first().inputValue(), 'edited-tail', 'rejected save retains draft');
    rejectSave = false;
    await section.getByRole('button', { name: 'Save', exact: true }).click();
    await section.getByText('Saved.', { exact: true }).waitFor();
    const command = writes.at(-1);
    assert.equal(command.realm, 'home');
    assert.deepEqual(command.value[0], { name: 'edited-tail', description: 'first', body: 'Preserved\nmultiline prompt',
      enabled: false, applies_to: ['repo:example', 'env:test'] });
    assert.equal(command.value[1].name, 'added');
    await page.reload({ waitUntil: 'domcontentloaded' });
    await names.first().waitFor();
    assert.equal(await names.first().inputValue(), 'edited-tail', 'remount loads saved data');

    step('templates');
    await names.first().evaluate(el => { window.__commandsBeforeNavigation = el; });
    await navigateSection(page, 'automation', 'Templates');
    const template = page.getByPlaceholder('e.g. disk-check');
    await template.waitFor();
    assert(await page.evaluate(() => !window.__commandsBeforeNavigation.isConnected), 'section change releases the old editor');
    row = template.locator('xpath=../..');
    assert.equal(await field(row, 'Connection').inputValue(), 'missing-connection', 'deleted connection remains selectable');
    await field(row, 'Connection').selectOption('Test');
    await field(row, 'Effort').selectOption('high');
    await field(row, 'Base').fill('next');
    await field(row, 'Setup commands').fill('one\n\n  two  \nthree\n');
    await row.getByRole('checkbox', { name: /^Fetch the base first/ }).check();
    const templateSection = template.locator('xpath=ancestor::section[1]');
    await templateSection.getByRole('button', { name: 'Save', exact: true }).click();
    await templateSection.getByText('Saved.', { exact: true }).waitFor();
    assert.equal(writes.at(-1).value[0].effort, 'high');
    assert.equal(writes.at(-1).value[0].worktree_options.base, 'next');
    assert.equal(writes.at(-1).value[0].worktree_options.fetch, true);
    assert.deepEqual(writes.at(-1).value[0].worktree_options.setup, ['one', '  two', 'three']);

    await navigateSection(page, 'commands', 'Slash commands');
    await names.first().waitFor();
    assert.equal(await names.first().inputValue(), 'edited-tail', 'returning to the section remounts its saved list');
    await navigateSection(page, 'automation', 'Templates');
    await template.waitFor();
    assert.equal(await field(template.locator('xpath=../..'), 'Effort').inputValue(), 'high', 'other section retains its saved values');
    step('views');
    await navigateSection(page, 'views', 'Views');
    const viewName = page.getByPlaceholder('e.g. pipeline (its address: /views/pipeline)');
    await viewName.waitFor();
    row = viewName.locator('xpath=../..');
    await field(row, 'Source \\(JSON\\)').fill('{"collection":"clients"}');
    await field(row, 'Actions \\(JSON\\)').fill('unfinished JSON');
    const viewSection = viewName.locator('xpath=ancestor::section[1]');
    await viewSection.getByRole('button', { name: 'Save', exact: true }).click();
    await viewSection.getByText('Saved.', { exact: true }).waitFor();
    assert.deepEqual(writes.at(-1).value[0].source, { collection: 'clients' });
    assert.equal(writes.at(-1).value[0].actions, 'unfinished JSON', 'invalid JSON remains editable text for head validation');

    step('MCP: open');
    const settingRead = () => page.waitForResponse(response => {
      const url = new URL(response.url());
      return response.request().method() === 'GET' && url.pathname.endsWith('/settings/mcp-servers');
    });
    const initialRead = settingRead();
    await navigateSection(page, 'tools', 'MCP servers');
    const connectors = page.getByRole('heading', { name: 'MCP servers (connectors)', exact: true }).locator('xpath=..');
    const json = connectors.locator('textarea');
    await json.waitFor();
    await (await initialRead).finished();
    await page.waitForFunction(el => el.value.includes('"initial"'), await json.elementHandle());
    step('MCP: invalid JSON');
    assert.deepEqual(JSON.parse(await json.inputValue()), saved['mcp-servers'], 'whole-setting JSON loads before editing');
    await json.fill('{unfinished');
    const beforeInvalid = writes.length;
    await connectors.getByRole('button', { name: 'Save', exact: true }).click();
    await connectors.getByText(/^Not valid JSON:/).waitFor();
    step('MCP: valid JSON');
    assert.equal(writes.length, beforeInvalid, 'invalid whole-setting JSON is rejected before sending');
    const mcp = [{ name: 'fixture', transport: 'http', url: 'https://example.invalid/mcp' }];
    await json.fill(JSON.stringify(mcp));
    await connectors.getByRole('button', { name: 'Save', exact: true }).click();
    await connectors.getByText('Saved. New sessions will use it.', { exact: true }).waitFor();
    assert.deepEqual(writes.at(-1).value, mcp);
    step('MCP: remount');
    await navigateSection(page, 'commands', 'Slash commands');
    await names.first().waitFor();
    const remountRead = settingRead();
    await navigateSection(page, 'tools', 'MCP servers');
    await json.waitFor();
    await (await remountRead).finished();
    await page.waitForFunction(el => el.value.includes('"fixture"'), await json.elementHandle());
    assert.deepEqual(JSON.parse(await json.inputValue()), mcp, 'a fresh owner reloads the saved JSON setting');

    step('routing');
    await page.goto(origin + '/admin/settings/routing', { waitUntil: 'domcontentloaded' });
    const rule = page.getByPlaceholder('e.g. Ops on the cheap model');
    await rule.waitFor();
    assert.equal(await rule.inputValue(), 'root-rule');
    row = rule.locator('xpath=../..');
    await field(row, 'Tools').fill('Read,  Grep, , Write ');
    const routingSection = rule.locator('xpath=ancestor::section[1]');
    await routingSection.getByRole('button', { name: 'Save', exact: true }).click();
    await routingSection.getByText('Saved.', { exact: true }).waitFor();
    assert.equal(writes.at(-1).realm, 'root');
    assert.deepEqual(writes.at(-1).value[0].tools, ['Read', 'Grep', 'Write']);
    await rule.evaluate(el => { window.__rootRuleInput = el; });
    await page.getByLabel('Rules for', { exact: true }).selectOption('home');
    await page.waitForFunction(() => document.querySelector('input[placeholder="e.g. Ops on the cheap model"]')?.value === 'home-rule');
    assert(await page.evaluate(() => !window.__rootRuleInput.isConnected), 'realm change replaces old keyed rows');
    await rule.fill('home-edited');
    const homeWrite = page.waitForResponse(response => {
      const url = new URL(response.url());
      return response.request().method() === 'PUT' && url.pathname.endsWith('/settings/routes') && url.searchParams.get('realm') === 'home';
    });
    await routingSection.getByRole('button', { name: 'Save', exact: true }).click();
    // Saved is already present from the previous realm: wait on the mock
    // request, not that pre-existing status text.
    await homeWrite;
    assert.equal(writes.at(-1).realm, 'home');
    assert.equal(writes.at(-1).value[0].name, 'home-edited');
    await page.getByLabel('Rules for', { exact: true }).selectOption('root');
    await page.waitForFunction(() => document.querySelector('input[placeholder="e.g. Ops on the cheap model"]')?.value === 'root-rule');
    assert.equal(saved.routes.root[0].name, 'root-rule', 'saving another realm does not mutate root');
    assert.deepEqual(f.errors, []);
    console.log(`PASS ${engine} ${phone ? 'phone' : 'desktop'} ${theme}: typed lists, keyed rows, focus, labels, text, choices, connections, nested fields, lines, JSON, rejected/successful save, remount and realm switching`);
  } finally { await f.context.close(); }
}

(async () => {
  const { server, origin } = await serve();
  try {
    for (const engine of !process.env.ENGINE || process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : [process.env.ENGINE]) {
      const browser = await launchBrowser(playwright, engine);
      try {
        for (const phone of [false, true]) for (const theme of ['light', 'dark'])
          await within(`${engine} lists`, () => check(browser, origin, phone, theme, engine));
      } finally { await browser.close(); }
    }
  } finally { server.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
