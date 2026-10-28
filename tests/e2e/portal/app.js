// The test app: the real `ensureSignedInWithFabric` (SDK 1.36.2) behind a Sign in button, as an
// app would call it. run.mjs bundles it with the SDK from an app's node_modules.
import { ensureSignedInWithFabric } from '@microsoft/rayfin-auth-provider-fabric';
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

document.getElementById('signin').addEventListener('click', async () => {
  const started = Date.now();
  try {
    const session = await ensureSignedInWithFabric(client.auth, options);
    // The session must work against the backend too: refresh it.
    let refreshed;
    try {
      await client.auth.refreshSession();
      refreshed = client.auth.getSession().isAuthenticated;
    } catch (error) {
      refreshed = describe(error);
    }
    window.__result = {
      ok: true,
      session: { isAuthenticated: session.isAuthenticated, user: session.user },
      refreshed,
      ms: Date.now() - started,
    };
  } catch (error) {
    window.__result = { ok: false, error: describe(error), ms: Date.now() - started };
  }
});
document.getElementById('signin').disabled = false;
