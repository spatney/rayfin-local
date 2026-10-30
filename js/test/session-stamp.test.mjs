// Tests for js/session-stamp.mjs. Run with `node --test js/test/session-stamp.test.mjs`.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import vm from 'node:vm';

import { SESSION_STAMP_KEY, forgetStaleSession, sessionStampScript } from '../session-stamp.mjs';

/** The Web Storage interface over a Map, as a page's localStorage. */
class MemoryStorage {
  #items;

  constructor(items = {}) {
    this.#items = new Map(Object.entries(items));
  }

  get length() {
    return this.#items.size;
  }

  key(index) {
    return [...this.#items.keys()][index] ?? null;
  }

  getItem(key) {
    return this.#items.get(key) ?? null;
  }

  setItem(key, value) {
    this.#items.set(key, String(value));
  }

  removeItem(key) {
    this.#items.delete(key);
  }

  entries() {
    return Object.fromEntries(this.#items);
  }
}

const SDK_KEYS = {
  authSession: '{"accessToken":"a","user":{}}',
  rayfin_pkce_state1: '{"codeVerifier":"v"}',
  app_authSession: '{"accessToken":"b","user":{}}',
  app_rayfin_pkce_state2: '{"codeVerifier":"w"}',
};
const APP_KEYS = { theme: 'dark', myauthSession: 'kept', authSessionCount: '3' };

test('a new stamp forgets the SDK session and its sign-in state, and nothing else', () => {
  const storage = new MemoryStorage({ ...SDK_KEYS, ...APP_KEYS, [SESSION_STAMP_KEY]: 'app-a:1' });
  assert.equal(forgetStaleSession(storage, 'app-b:1', SESSION_STAMP_KEY), true);
  assert.deepEqual(storage.entries(), { ...APP_KEYS, [SESSION_STAMP_KEY]: 'app-b:1' });
});

test('an origin without a stamp has its session forgotten once', () => {
  const storage = new MemoryStorage({ ...SDK_KEYS });
  assert.equal(forgetStaleSession(storage, 'app-a:1', SESSION_STAMP_KEY), true);
  assert.deepEqual(storage.entries(), { [SESSION_STAMP_KEY]: 'app-a:1' });

  storage.setItem('authSession', SDK_KEYS.authSession);
  assert.equal(forgetStaleSession(storage, 'app-a:1', SESSION_STAMP_KEY), false);
  assert.equal(storage.getItem('authSession'), SDK_KEYS.authSession);
});

test('the page script works on window.localStorage', () => {
  const storage = new MemoryStorage({ ...SDK_KEYS, theme: 'dark', [SESSION_STAMP_KEY]: 'app-a:1' });
  vm.runInNewContext(sessionStampScript('app-a:2'), { window: { localStorage: storage } });
  assert.deepEqual(storage.entries(), { theme: 'dark', [SESSION_STAMP_KEY]: 'app-a:2' });
});

test('the page script does nothing where storage is denied', () => {
  const window = {
    get localStorage() {
      throw new Error('SecurityError: access is denied for this document');
    },
  };
  assert.doesNotThrow(() => vm.runInNewContext(sessionStampScript('app-a:1'), { window }));
});

test('a stamp cannot end the script element', () => {
  const script = sessionStampScript('</script><script>alert(1)//');
  assert.ok(!script.toLowerCase().includes('</script'), script);
  const storage = new MemoryStorage();
  vm.runInNewContext(script, { window: { localStorage: storage } });
  assert.equal(storage.getItem(SESSION_STAMP_KEY), '</script><script>alert(1)//');
});
