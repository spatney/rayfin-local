// The rayfin-local sign-in page: the popup `ensureSignedInWithFabric` opens. The handshake is
// described in crates/rl-auth/src/portal.rs.
(() => {
  'use strict';

  const config = JSON.parse(document.getElementById('rl-config').textContent);
  const el = (id) => document.getElementById(id);
  const opener = window.opener;
  const READY_INTERVAL_MS = 500;
  const READY_ATTEMPTS = 20;
  const CANCELLED = 'Sign-in was cancelled.';
  const appLabel = config.appName || 'the app';

  // The app's challenge once it passed the browser-side checks, and where it came from.
  let challenge = null;
  let appOrigin = null;
  // Posted to the app when the window closes without a sign-in: { error, description }.
  let failure = { error: 'access_denied', description: CANCELLED };
  let finished = false;
  let busy = false;
  let mode = 'signin';
  let identity = null;
  let readyTimer = 0;

  function setStatus(text) {
    el('status').textContent = text;
    el('status').hidden = !text;
  }

  function showError(id, text) {
    el(id).textContent = text;
    el(id).hidden = !text;
  }

  function showMessage(title, text, closable) {
    el('signin').hidden = true;
    el('message').hidden = false;
    el('message-title').textContent = title;
    el('message-text').textContent = text;
    el('message-close').hidden = !closable;
    setStatus('');
    if (closable) el('message-close').focus();
  }

  // Tells the app the sign-in ended without a code, so it does not wait for its timeout.
  function reportFailure() {
    if (finished || !appOrigin || !opener || opener.closed) return;
    finished = true;
    try {
      opener.postMessage(
        {
          type: 'brokeredAuth.error',
          state: challenge ? challenge.state : '',
          error: failure.error,
          errorDescription: failure.description,
        },
        appOrigin,
      );
    } catch {
      // The app window navigated away.
    }
  }

  function fail(title, text, error) {
    failure = { error, description: text };
    showMessage(title, text, true);
  }

  async function api(path, body) {
    let response;
    try {
      response = await fetch(`${config.api}/${path}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-rayfin-local-csrf': config.csrf },
        body: JSON.stringify(body || {}),
        cache: 'no-store',
        credentials: 'omit',
        redirect: 'error',
      });
    } catch {
      return {
        ok: false,
        status: 0,
        body: null,
        message: 'rayfin-local is not answering. Check that `rayfin-local serve` is still running, then try again.',
      };
    }
    let payload = null;
    try {
      payload = await response.json();
    } catch {
      // Not JSON.
    }
    const message = (payload && typeof payload.message === 'string' && payload.message)
      || `rayfin-local answered HTTP ${response.status}.`;
    return { ok: response.ok, status: response.status, body: payload, message };
  }

  function announce(attempt) {
    if (appOrigin) return;
    if (attempt >= READY_ATTEMPTS) {
      showMessage(
        'The app did not answer',
        `Close this window and use the Sign in button of ${appLabel} again.`,
        true,
      );
      return;
    }
    try {
      // The ready signal carries nothing, so any origin may read it.
      opener.postMessage({ type: 'brokeredAuth.ready' }, '*');
    } catch {
      // Retried below.
    }
    readyTimer = setTimeout(() => announce(attempt + 1), READY_INTERVAL_MS);
  }

  function problemWith(data, origin) {
    const { state, codeChallenge, codeChallengeMethod, returnOrigin } = data;
    if (typeof state !== 'string' || state.length === 0 || state.length > 512) {
      return 'its state is missing or too long';
    }
    if (codeChallengeMethod !== 'S256') return 'its codeChallengeMethod is not S256';
    if (typeof codeChallenge !== 'string' || !/^[A-Za-z0-9_-]{43,128}$/.test(codeChallenge)) {
      return 'its PKCE code challenge is malformed';
    }
    let target;
    try {
      target = new URL(returnOrigin).origin;
    } catch {
      return 'its returnOrigin is not a URL';
    }
    if (target !== origin) return `its returnOrigin (${returnOrigin}) is not the app's origin (${origin})`;
    return null;
  }

  function onMessage(event) {
    if (appOrigin || event.source !== opener) return;
    const data = event.data;
    if (!data || typeof data !== 'object' || data.type !== 'brokeredAuth.challenge') return;
    clearTimeout(readyTimer);
    appOrigin = event.origin;
    const problem = problemWith(data, event.origin);
    challenge = {
      codeChallenge: data.codeChallenge,
      codeChallengeMethod: data.codeChallengeMethod,
      state: typeof data.state === 'string' ? data.state : '',
      returnOrigin: data.returnOrigin,
    };
    el('message').hidden = true;
    if (problem) {
      fail('This sign-in request is invalid', `The app's sign-in request cannot be used: ${problem}.`, 'invalid_request');
      return;
    }
    if (!config.authEnabled) {
      fail(
        'Sign-in is turned off',
        'This app has authentication turned off (services.auth.enabled is false in rayfin/rayfin.yml).',
        'auth_disabled',
      );
      return;
    }
    checkChallenge();
  }

  async function checkChallenge() {
    setStatus('Checking the sign-in request…');
    const answer = await api('challenge', challenge);
    if (!answer.ok) {
      fail(`${appLabel === 'the app' ? 'The app' : appLabel} cannot sign in here`, answer.message, 'invalid_request');
      return;
    }
    el('target-origin').textContent = answer.body.returnOrigin;
    el('target').hidden = false;
    setStatus('');
    el('signin').hidden = false;
    el('fields').disabled = false;
    el('tabs').hidden = !config.signUp;
    updateEntraButton();
    el('username').focus();
  }

  function setMode(next) {
    mode = next;
    const signUp = next === 'signup';
    for (const [id, selected] of [['tab-signin', !signUp], ['tab-signup', signUp]]) {
      el(id).setAttribute('aria-selected', String(selected));
      el(id).tabIndex = selected ? 0 : -1;
    }
    el('form').setAttribute('aria-labelledby', signUp ? 'tab-signup' : 'tab-signin');
    el('confirm-row').hidden = !signUp;
    el('password').autocomplete = signUp ? 'new-password' : 'current-password';
    el('submit').textContent = signUp ? 'Create account' : 'Sign in';
    showError('form-error', '');
  }

  function onTabKey(event) {
    const order = ['tab-signin', 'tab-signup'];
    const index = order.indexOf(event.target.id);
    let next = null;
    if (event.key === 'ArrowRight' || event.key === 'ArrowLeft') next = order[(index + 1) % 2];
    if (event.key === 'Home') next = order[0];
    if (event.key === 'End') next = order[1];
    if (!next) return;
    event.preventDefault();
    setMode(next === 'tab-signup' ? 'signup' : 'signin');
    el(next).focus();
  }

  function setBusy(value, text) {
    busy = value;
    el('fields').disabled = value;
    el('form').setAttribute('aria-busy', String(value));
    updateEntraButton();
    setStatus(value ? text : '');
  }

  function updateEntraButton() {
    el('entra-button').disabled = busy || !identity || el('signin').hidden;
  }

  // Hands the code to the app, which redeems it and closes this window.
  function complete(handoffCode) {
    if (!opener || opener.closed) {
      fail('The app window was closed', `Open ${appLabel} again and sign in from there.`, 'access_denied');
      return;
    }
    finished = true;
    opener.postMessage({ type: 'brokeredAuth.handoff', handoffCode, state: challenge.state }, appOrigin);
    el('signin').hidden = true;
    el('done').hidden = false;
    setStatus('');
    window.close();
  }

  async function onSubmit(event) {
    event.preventDefault();
    if (busy || finished || !challenge) return;
    const username = el('username').value.trim();
    const password = el('password').value;
    if (!username || !password) {
      showError('form-error', 'Enter a username and a password.');
      (username ? el('password') : el('username')).focus();
      return;
    }
    if (mode === 'signup' && password !== el('confirm').value) {
      showError('form-error', 'The passwords do not match.');
      el('confirm').focus();
      return;
    }
    showError('form-error', '');
    setBusy(true, mode === 'signup' ? 'Creating your account…' : 'Signing in…');
    const answer = await api(mode, { ...challenge, username, password });
    setBusy(false);
    if (answer.ok && answer.body && typeof answer.body.handoffCode === 'string') {
      complete(answer.body.handoffCode);
      return;
    }
    showError('form-error', answer.message);
    if (answer.status === 401) {
      el('password').select();
      el('password').focus();
    } else if (answer.status === 409) {
      el('username').focus();
    }
  }

  async function loadIdentity() {
    el('entra').hidden = false;
    const answer = await api('whoami');
    if (answer.ok && answer.body && answer.body.available) {
      identity = answer.body;
      const { name, username } = identity;
      const who = name && username ? `${name} (${username})` : username || name || 'your Microsoft account';
      el('entra-button').textContent = `Continue as ${who}`;
      el('entra-button').hidden = false;
      el('entra-note').textContent = 'Uses your Azure CLI sign-in.';
      updateEntraButton();
      return;
    }
    const body = answer.body || {};
    const hint = body.hint || 'Sign in to the Azure CLI (`az login`), then open this window again.';
    el('entra-note').textContent = body.message
      ? `To continue with your Microsoft account: ${hint} (${body.message})`
      : `To continue with your Microsoft account: ${hint}`;
  }

  async function onEntra() {
    if (busy || finished || !challenge || !identity) return;
    showError('entra-error', '');
    setBusy(true, 'Signing in with your Microsoft account…');
    const answer = await api('entra', challenge);
    setBusy(false);
    if (answer.ok && answer.body && typeof answer.body.handoffCode === 'string') {
      complete(answer.body.handoffCode);
      return;
    }
    showError('entra-error', answer.message);
  }

  function close() {
    reportFailure();
    window.close();
  }

  el('form').addEventListener('submit', onSubmit);
  el('tab-signin').addEventListener('click', () => setMode('signin'));
  el('tab-signup').addEventListener('click', () => setMode('signup'));
  el('tabs').addEventListener('keydown', onTabKey);
  el('entra-button').addEventListener('click', onEntra);
  el('cancel').addEventListener('click', close);
  el('message-close').addEventListener('click', close);
  window.addEventListener('pagehide', reportFailure);

  if (!opener) {
    showMessage(
      'Open this from the app',
      `This window signs you in to ${appLabel} while it runs on this machine with rayfin-local. Use the app's Sign in button to open it.`,
      false,
    );
    return;
  }
  window.addEventListener('message', onMessage);
  announce(0);
  if (config.authEnabled && config.entra) loadIdentity();
})();
