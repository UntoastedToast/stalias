import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../src/web/static/app.js', import.meta.url), 'utf8');

function app({ blockedStorage = false } = {}) {
  const listeners = new Map();
  const storage = new Map([['htmx-history-cache', 'old page with a plaintext token']]);
  vm.runInNewContext(source, {
    sessionStorage: {
      removeItem(key) {
        if (blockedStorage) throw new Error('storage blocked');
        storage.delete(key);
      },
    },
    localStorage: { getItem: () => null },
    window: { matchMedia: () => ({ matches: false }) },
    document: {
      documentElement: { classList: { toggle() {} } },
      addEventListener(name, callback) { listeners.set(name, callback); },
    },
  });
  return {
    storage,
    dispatch(name, event) { listeners.get(name)?.(event); },
  };
}

test('a rendered HTTP 200 error preserves alias form input', () => {
  const runtime = app();
  let name = 'invalid..name';
  const form = { matches: () => true, reset() { name = ''; } };
  runtime.dispatch('htmx:afterRequest', { detail: { successful: true }, target: form });
  assert.equal(name, 'invalid..name');
  runtime.dispatch('aliasCreated', { target: form });
  assert.equal(name, '');
});

test('an alias creation event does not reset unrelated forms', () => {
  const runtime = app();
  let resets = 0;
  runtime.dispatch('aliasCreated', { target: { matches: () => false, reset() { resets++; } } });
  assert.equal(resets, 0);
});

test('old htmx history snapshots containing secrets are purged on load', () => {
  assert.equal(app().storage.has('htmx-history-cache'), false);
});

test('blocked browser storage does not break alias form handling', () => {
  const runtime = app({ blockedStorage: true });
  let resets = 0;
  runtime.dispatch('aliasCreated', { target: { matches: () => true, reset() { resets++; } } });
  assert.equal(resets, 1);
});
