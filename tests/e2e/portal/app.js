// The test app: the real `ensureSignedInWithFabric` (SDK 1.36.2) behind a Sign in button, as an
// app would call it; or, when the local Fabric host frames it (`fabricEmbedded=true`), the real
// `initEmbeddedAuth` on load. run.mjs bundles it with the SDK from an app's node_modules.
import { sendBridgeRequest } from '@microsoft/fabric-embedded-host';
import { ensureSignedInWithFabric, initEmbeddedAuth } from '@microsoft/rayfin-auth-provider-fabric';
import { RayfinClient } from '@microsoft/rayfin-client';

const params = new URLSearchParams(location.search);
const apiUrl = params.get('api');
const client = new RayfinClient({ baseUrl: apiUrl, publishableKey: params.get('pk') });
const options = {
  workspaceId: params.get('ws'),
  projectId: params.get('item'),
  fabricPortalUrl: `${apiUrl}/.rayfin-local/portal`,
  returnOrigin: location.origin,
};

const describe = (error) => ({ name: error?.name, code: error?.code, message: error?.message });

// The session must work against the backend too: refresh it.
async function refresh() {
  try {
    await client.auth.refreshSession();
    return client.auth.getSession().isAuthenticated;
  } catch (error) {
    return describe(error);
  }
}

async function signInEmbedded() {
  const started = Date.now();
  const session = await initEmbeddedAuth(client.auth, options);
  const result = session
    ? { ok: true, session: { isAuthenticated: session.isAuthenticated, user: session.user }, refreshed: await refresh() }
    : { ok: false };
  result.ms = Date.now() - started;
  // A request the host does not support must fail at once, not after the bridge's 30 seconds.
  const asked = Date.now();
  try {
    await sendBridgeRequest({ target: window.parent, channel: 'fabric-host', kind: 'e2e.unsupported', payload: {} });
    result.unsupported = { ok: true };
  } catch (error) {
    result.unsupported = { ...describe(error), ms: Date.now() - asked };
  }
  window.__result = result;
}

if (params.get('fabricEmbedded') === 'true') {
  signInEmbedded().catch((error) => {
    window.__result = { ok: false, error: describe(error) };
  });
} else {
  document.getElementById('signin').addEventListener('click', async () => {
    const started = Date.now();
    try {
      const session = await ensureSignedInWithFabric(client.auth, options);
      window.__result = {
        ok: true,
        session: { isAuthenticated: session.isAuthenticated, user: session.user },
        refreshed: await refresh(),
        ms: Date.now() - started,
      };
    } catch (error) {
      window.__result = { ok: false, error: describe(error), ms: Date.now() - started };
    }
  });
  document.getElementById('signin').disabled = false;
}
