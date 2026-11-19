'use strict';

const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');
const { runInNewContext } = require('node:vm');

const script = path.join(__dirname, '../js/viewer-controls.js');
const source = readFileSync(script, 'utf8');

async function loadMore(dataset, codeClass) {
  let click, input;
  const root = {
    dataset,
    querySelector(selector) {
      if (selector === ':scope > .fv-source') return { textContent: 'fixture data\n' };
      if (selector.includes('code[class*=')) return { className: codeClass };
      return null;
    },
  };
  const target = { closest: () => root, hasAttribute: () => false, dataset: { fvAction: 'more' } };
  runInNewContext(source, {
    window: { c6Viewers: { render(value) { input = value; return ''; } } },
    document: {
      addEventListener(event, callback) { if (event === 'click') click = callback; },
      createElement() { return {}; },
    },
  }, { filename: script });
  await click({ target: { closest: () => target } });
  return input;
}

test('file redraw preserves path and MIME dispatch even with a source-code grammar', async () => {
  for (const [kind, name, mime, grammar] of [
    ['jsonlines', 'fixture.jsonl', 'application/x-ndjson', 'json'],
    ['csv', 'malformed.csv', 'text/csv', 'text'],
  ]) {
    const input = await loadMore({ path: name, mime, kind, mode: 'full', language: '' }, `language-${grammar}`);
    assert.equal(input.language, '', 'code rows do not override file dispatch');
    assert.equal(input.path, name);
    assert.equal(input.mime, mime);
  }
});

test('numbered fences keep the declared renderer language and grow from 2000 to 4000 lines', async () => {
  for (const [language, grammar] of [['rust', 'rust'], ['text', 'text'], ['json-code', 'json']]) {
    const input = await loadMore({ kind: 'code', mode: 'inline', language }, `language-${grammar}`);
    assert.equal(input.language, language);
    assert.equal(input.limit, 4000);
  }
});

test('copy feedback preserves the small icon and announces the result', async () => {
  let click, copied, status;
  const root = {
    querySelector: () => status,
    append(value) { status = value; },
  };
  const target = {
    dataset: { fvCopyJson: JSON.stringify('/a\r\u0000') }, innerHTML: '<svg></svg>',
    closest: () => root, hasAttribute: name => name === 'data-fv-copy-json',
  };
  runInNewContext(source, {
    navigator: { clipboard: { async writeText(value) { copied = value; } } },
    document: {
      addEventListener(event, callback) { if (event === 'click') click = callback; },
      createElement() { return { setAttribute() {} }; },
    },
  }, { filename: script });
  await click({ target: { closest: () => target }, preventDefault() {}, stopPropagation() {} });
  assert.equal(copied, '/a\r\u0000');
  assert.equal(target.innerHTML, '<svg></svg>');
  assert.equal(target.textContent, undefined);
  assert.equal(target.title, 'Copied');
  assert.equal(status.className, 'fv-copy-status');
  assert.equal(status.textContent, 'Copied');
});

test('Pretty and Raw are explicit segmented choices, not two toggle buttons', () => {
  let click;
  const source = { hidden: true, closest: () => root };
  const rendered = { hidden: false, closest: () => root };
  const segment = view => ({
    dataset: { fvAction: 'source', fvView: view }, closest: () => root,
    setAttribute(name, value) { this[name] = value; },
  });
  const pretty = segment('pretty'), raw = segment('raw');
  const root = {
    dataset: { mode: 'inline' },
    querySelectorAll(selector) {
      return selector === '.fv-source-view' ? [source] : selector === '.fv-rendered' ? [rendered] : [pretty, raw];
    },
  };
  const bridge = path.join(__dirname, '../js/viewers.js');
  runInNewContext(readFileSync(bridge, 'utf8'), {
    window: {}, location: new URL('http://localhost'), URL,
    Node: { ELEMENT_NODE: 1 },
    MutationObserver: class { observe() {} },
    document: { body: {}, addEventListener(event, callback) { if (event === 'click') click = callback; } },
  }, { filename: bridge });
  const choose = button => click({ target: { closest: () => button } });
  choose(raw);
  assert.equal(source.hidden, false);
  assert.equal(raw['aria-pressed'], 'true');
  assert.equal(pretty['aria-pressed'], 'false');
  choose(raw);
  assert.equal(source.hidden, false);
  choose(pretty);
  assert.equal(source.hidden, true);
  assert.equal(pretty['aria-pressed'], 'true');
});
