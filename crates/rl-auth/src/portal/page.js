// The rayfin-local sign-in page: the popup `ensureSignedInWithFabric` opens, or the local Fabric
// host that frames an app using `initEmbeddedAuth` (`config.embed`). The handshakes are
// described in crates/rl-auth/src/portal.rs and crates/rl-auth/src/portal/embed.rs.
(() => {
  'use strict';

  const config = JSON.parse(document.getElementById('rl-config').textContent);
  const el = (id) => document.getElementById(id);
  // On the Fabric host: { src: the app's URL in embedded mode, origin: the app's origin }.
  const embed = config.embed || null;
  const opener = embed ? null : window.opener;
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

  // Where a sign-in goes and what it sends besides the credentials, or null while the page
  // cannot sign in: the popup sends the app's challenge, the Fabric host asks for a session.
  function signInRequest(path) {
    if (embed) return { path: `embed/${path}`, body: {} };
    if (finished || !challenge) return null;
    return { path, body: challenge };
  }

  // Takes a successful sign-in's answer: a code for the app, or the Fabric host's session.
  function signedIn(body) {
    if (!body) return false;
    if (embed) {
      if (typeof body.hostToken !== 'string') return false;
      startSession(body);
      return true;
    }
    if (typeof body.handoffCode !== 'string') return false;
    complete(body.handoffCode);
    return true;
  }

  async function onSubmit(event) {
    event.preventDefault();
    const request = signInRequest(mode);
    if (busy || !request) return;
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
    const answer = await api(request.path, { ...request.body, username, password });
    setBusy(false);
    if (answer.ok && signedIn(answer.body)) return;
    showError('form-error', answer.message);
    if (answer.status === 401) {
      el('password').select();
      el('password').focus();
    } else if (answer.status === 409) {
      el('username').focus();
    }
  }

  // Up to two letters for the account tile: "Dev Eloper" is DE, "ada@contoso.test" is A.
  function initials(name, username) {
    const words = (name || '').trim().split(/\s+/).filter(Boolean);
    const letters = words.length > 1
      ? words[0][0] + words[words.length - 1][0]
      : (words[0] || username || '?')[0];
    return letters.toUpperCase();
  }

  async function loadIdentity() {
    el('entra').hidden = false;
    const answer = await api('whoami');
    if (answer.ok && answer.body && answer.body.available) {
      identity = answer.body;
      const { name, username } = identity;
      const who = name && username ? `${name} (${username})` : username || name || 'your Microsoft account';
      el('entra-button').setAttribute('aria-label', `Continue as ${who}`);
      el('entra-initials').textContent = initials(name, username);
      el('entra-name').textContent = name || username || 'Your Microsoft account';
      el('entra-email').textContent = name && username ? username : '';
      el('entra-email').hidden = !(name && username);
      el('entra-button').hidden = false;
      // A local account's sign-in steps back behind the developer's own.
      el('signin').classList.add('has-account');
      el('entra-note').textContent = 'Uses your Azure CLI sign-in.';
      updateEntraButton();
      return;
    }
    const body = answer.body || {};
    const again = embed ? 'reload this page' : 'open this window again';
    const hint = body.hint || `Sign in to the Azure CLI (\`az login\`), then ${again}.`;
    el('entra-note').textContent = body.message
      ? `To continue with your Microsoft account: ${hint} (${body.message})`
      : `To continue with your Microsoft account: ${hint}`;
  }

  async function onEntra() {
    const request = signInRequest('entra');
    if (busy || !request || !identity) return;
    showError('entra-error', '');
    setBusy(true, 'Signing in with your Microsoft account…');
    const answer = await api(request.path, request.body);
    setBusy(false);
    if (answer.ok && signedIn(answer.body)) return;
    showError('entra-error', answer.message);
  }

  function close() {
    reportFailure();
    window.close();
  }

  // ---- The local Fabric host ----

  // The developer's session on this page ({ hostToken, user }), kept for the tab and the app.
  const SESSION_KEY = embed ? `rayfin-local fabric host ${embed.origin}` : '';
  const BRIDGE_CHANNEL = 'fabric-auth';
  let session = null;
  let frame = null;
  let identityRequested = false;

  function storedSession() {
    try {
      const value = JSON.parse(sessionStorage.getItem(SESSION_KEY) || 'null');
      if (value && typeof value.hostToken === 'string') return value;
    } catch {
      // Nothing usable stored.
    }
    return null;
  }

  function describeUser(user) {
    const { name, email, username } = user || {};
    const account = email || username || '';
    return name && name !== account ? `${name}${account ? ` (${account})` : ''}` : account;
  }

  function remember(value) {
    session = value;
    try {
      if (value) sessionStorage.setItem(SESSION_KEY, JSON.stringify(value));
      else sessionStorage.removeItem(SESSION_KEY);
    } catch {
      // No session storage: the sign-in lasts until the page reloads.
    }
    el('bar-user').textContent = value ? `Signed in as ${describeUser(value.user)}` : 'Not signed in';
    el('reload-app').hidden = !value;
    el('switch-user').hidden = !value;
  }

  // Loads the app in a new frame: no history entries, and the old frame's messages are ignored.
  function loadApp() {
    const next = document.createElement('iframe');
    next.title = appLabel;
    next.src = embed.src;
    el('frame').replaceChildren(next);
    frame = next;
    el('card').hidden = true;
    el('frame').hidden = false;
  }

  function showCard() {
    el('frame').hidden = true;
    el('card').hidden = false;
  }

  function showSignIn(note) {
    showCard();
    el('message').hidden = true;
    el('signin').hidden = false;
    el('fields').disabled = busy;
    el('tabs').hidden = !config.signUp;
    setStatus('');
    showError('form-error', note || '');
    updateEntraButton();
    el('username').focus();
    if (config.entra && !identityRequested) {
      identityRequested = true;
      loadIdentity();
    }
  }

  function startSession(body) {
    remember({ hostToken: body.hostToken, user: body.user || null });
    el('password').value = '';
    el('confirm').value = '';
    loadApp();
  }

  function switchUser() {
    remember(null);
    el('frame').replaceChildren();
    frame = null;
    showSignIn('');
  }

  const refusal = (code, message) => ({ success: false, error: { code, message } });

  // Answers the app's `auth.requestHandoff` with a code for the signed-in developer.
  async function handOff(payload) {
    if (!session) {
      if (el('card').hidden || el('signin').hidden) showSignIn('');
      return refusal('ACCESS_DENIED', 'Nobody is signed in on the rayfin-local Fabric host.');
    }
    const { callbackUrl, codeChallenge, codeChallengeMethod, state } = payload || {};
    let target = null;
    try {
      target = new URL(callbackUrl).origin;
    } catch {
      // Refused below.
    }
    if (target !== embed.origin) {
      return refusal(
        'INVALID_REQUEST',
        `The handoff request's callbackUrl (${callbackUrl}) is not the app's origin (${embed.origin}).`,
      );
    }
    const answer = await api('embed/handoff', {
      hostToken: session.hostToken,
      returnOrigin: callbackUrl,
      codeChallenge,
      codeChallengeMethod,
      state,
    });
    if (answer.ok && answer.body && typeof answer.body.handoffCode === 'string') {
      return { success: true, result: { handoffCode: answer.body.handoffCode, state: answer.body.state } };
    }
    if (answer.status === 401) {
      remember(null);
      showSignIn(answer.message);
    } else if (answer.status === 403) {
      showCard();
      showMessage('This page has expired', answer.message, false);
    }
    const code = answer.status === 401 || answer.status === 403
      ? 'ACCESS_DENIED'
      : (answer.body && answer.body.error) || 'HANDOFF_FAILED';
    return refusal(code, answer.message);
  }

  async function onHostMessage(event) {
    if (!frame || event.source !== frame.contentWindow || event.origin !== embed.origin) return;
    const data = event.data;
    if (!data || typeof data !== 'object' || typeof data.requestId !== 'string') return;
    const { channel, kind, requestId } = data;
    // The SDK's `externalEmbed.ready` goes unanswered, as in Fabric.
    if (typeof kind !== 'string' || kind === 'response' || kind === 'externalEmbed.ready') return;
    const source = event.source;
    const outcome = channel === BRIDGE_CHANNEL && kind === 'auth.requestHandoff'
      ? await handOff(data.payload)
      : refusal('NOT_SUPPORTED', `rayfin-local's Fabric host doesn't support ${channel}/${kind}.`);
    const reply = {
      channel: typeof channel === 'string' ? channel : BRIDGE_CHANNEL,
      version: 1,
      kind: 'response',
      requestId,
      ...outcome,
    };
    try {
      source.postMessage(reply, embed.origin);
    } catch {
      // The frame is gone.
    }
  }

  function startHost() {
    el('cancel').hidden = true;
    el('target-origin').textContent = embed.origin;
    el('target').hidden = false;
    el('reload-app').addEventListener('click', loadApp);
    el('switch-user').addEventListener('click', switchUser);
    window.addEventListener('message', onHostMessage);
    const stored = storedSession();
    remember(stored);
    if (stored) loadApp();
    else showSignIn('');
  }

  el('form').addEventListener('submit', onSubmit);
  el('tab-signin').addEventListener('click', () => setMode('signin'));
  el('tab-signup').addEventListener('click', () => setMode('signup'));
  el('tabs').addEventListener('keydown', onTabKey);
  el('entra-button').addEventListener('click', onEntra);
  if (embed) {
    startHost();
    return;
  }
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
