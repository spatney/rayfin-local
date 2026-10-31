// Drives the rayfin-local sign-in popup with a real browser (Playwright) and the real Rayfin SDK
// 1.36.2. Run by crates/rl-auth/tests/portal_e2e.rs, which starts the backend and passes:
//
//   RL_API_URL            the backend origin (it serves the portal and /api/auth/v1/*)
//   RL_PUBLISHABLE_KEY    its publishable key
//   RL_WORKSPACE_ID, RL_ITEM_ID
//   RL_SDK_NODE_MODULES   a node_modules with @microsoft/rayfin-client and
//                         @microsoft/rayfin-auth-provider-fabric 1.36.2
//   RL_USER, RL_PASSWORD  an existing password user
//   RL_NEW_USER           a username to sign up with
//   RL_EMBED_USER         a username to sign up with on the local Fabric host
//
// Prints one line `RESULT <json>` with what each scenario observed; the Rust test asserts on it.
import { createServer } from 'node:http';
import { mkdir, readFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { build } from 'esbuild';
import { chromium } from 'playwright';

const here = dirname(fileURLToPath(import.meta.url));
const env = (name) => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set (run this through crates/rl-auth/tests/portal_e2e.rs)`);
  return value;
};
const api = env('RL_API_URL');

// 1. Bundle the test app with the SDK from the app's node_modules.
await mkdir(join(here, '.build'), { recursive: true });
await build({
  entryPoints: [join(here, 'app.js')],
  outfile: join(here, '.build', 'app.js'),
  bundle: true,
  format: 'esm',
  platform: 'browser',
  target: 'es2022',
  nodePaths: [env('RL_SDK_NODE_MODULES')],
  logLevel: 'error',
});
const bundle = await readFile(join(here, '.build', 'app.js'));

// 2. Serve it from its own origin, like Vite serves an app next to the backend.
const page = `<!doctype html><meta charset="utf-8"><title>portal e2e</title>
<button id="signin" disabled>Sign in</button><script type="module" src="/app.js"></script>`;
const server = createServer((request, response) => {
  if (request.url.startsWith('/app.js')) {
    response.writeHead(200, { 'content-type': 'text/javascript' }).end(bundle);
  } else {
    response.writeHead(200, { 'content-type': 'text/html' }).end(page);
  }
});
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
const appOrigin = `http://127.0.0.1:${server.address().port}`;
const appUrl = `${appOrigin}/?${new URLSearchParams({
  api,
  pk: env('RL_PUBLISHABLE_KEY'),
  ws: env('RL_WORKSPACE_ID'),
  item: env('RL_ITEM_ID'),
})}`;

async function launch() {
  try {
    return await chromium.launch();
  } catch (error) {
    // No downloaded Chromium (`npx playwright install chromium`): try the installed Edge.
    console.error(`bundled Chromium unavailable (${error.message.split('\n')[0]}); trying Edge`);
    return chromium.launch({ channel: 'msedge' });
  }
}

const browser = await launch();
const out = { appOrigin, browser: browser.version() };

/** Opens the app in a fresh profile, clicks Sign in and returns the app page and the popup. */
async function openPopup() {
  const context = await browser.newContext();
  const app = await context.newPage();
  app.on('console', (message) => {
    if (message.type() === 'error') console.error(`[app] ${message.text()}`);
  });
  await app.goto(appUrl);
  await app.waitForSelector('#signin:enabled');
  const [popup] = await Promise.all([app.waitForEvent('popup'), app.click('#signin')]);
  popup.on('console', (message) => {
    if (message.type() === 'error') console.error(`[popup] ${message.text()}`);
  });
  await popup.waitForLoadState();
  return { context, app, popup };
}

async function appResult(app) {
  await app.waitForFunction(() => window.__result !== undefined, null, { timeout: 30_000 });
  return app.evaluate(() => window.__result);
}

async function scenario(name, run) {
  try {
    out[name] = await run();
  } catch (error) {
    out[name] = { failed: String(error?.stack ?? error) };
  }
}

await scenario('password', async () => {
  const { context, app, popup } = await openPopup();
  await popup.waitForSelector('#username:enabled');
  const target = await popup.textContent('#target-origin');
  await popup.fill('#username', env('RL_USER'));
  await popup.fill('#password', 'wrong password');
  await popup.click('#submit');
  await popup.waitForSelector('#form-error:not([hidden])');
  const wrongPassword = await popup.textContent('#form-error');
  await popup.fill('#password', env('RL_PASSWORD'));
  await popup.press('#password', 'Enter');
  const result = await appResult(app);
  const popupClosed = popup.isClosed() || (await popup.waitForEvent('close', { timeout: 5_000 }).then(() => true, () => false));
  await context.close();
  return { target, wrongPassword, result, popupClosed };
});

await scenario('signup', async () => {
  const { context, app, popup } = await openPopup();
  await popup.waitForSelector('#username:enabled');
  await popup.click('#tab-signup');
  await popup.fill('#username', env('RL_NEW_USER'));
  await popup.fill('#password', 'new secret');
  await popup.fill('#confirm', 'new secret');
  await popup.click('#submit');
  const result = await appResult(app);
  await context.close();
  return { result };
});

await scenario('entra', async () => {
  const { context, app, popup } = await openPopup();
  await popup.waitForSelector('#entra-button:enabled');
  const button = await popup.getAttribute('#entra-button', 'aria-label');
  await popup.click('#entra-button');
  const result = await appResult(app);
  await context.close();
  return { button, result };
});

await scenario('cancel', async () => {
  const { context, app, popup } = await openPopup();
  await popup.waitForSelector('#username:enabled');
  await popup.click('#cancel');
  const result = await appResult(app);
  await context.close();
  return { result };
});

await scenario('closed', async () => {
  const { context, app, popup } = await openPopup();
  await popup.waitForSelector('#username:enabled');
  await popup.close({ runBeforeUnload: true });
  const result = await appResult(app);
  await context.close();
  return { result };
});

await scenario('noOpener', async () => {
  const context = await browser.newContext();
  const direct = await context.newPage();
  await direct.goto(`${api}/.rayfin-local/portal/secureItemEmbed`);
  await direct.waitForSelector('#message:not([hidden])');
  const title = await direct.textContent('#message-title');
  await context.close();
  return { title };
});

/** What the app in the Fabric host's (current) frame saw. */
async function frameResult(host) {
  const frame = await (await host.waitForSelector('#frame:not([hidden]) iframe')).contentFrame();
  await frame.waitForFunction(() => window.__result !== undefined, null, { timeout: 30_000 });
  return frame.evaluate(() => window.__result);
}

// The local Fabric host frames the app, which signs in with `initEmbeddedAuth`.
await scenario('embedded', async () => {
  const context = await browser.newContext();
  const host = await context.newPage();
  host.on('console', (message) => {
    if (message.type() === 'error') console.error(`[host] ${message.text()}`);
  });
  await host.goto(`${api}/.rayfin-local/portal/embed?${new URLSearchParams({ app: appUrl })}`);
  await host.waitForSelector('#username:enabled');
  const framedBeforeSignIn = await host.$$eval('#frame iframe', (frames) => frames.length);
  await host.fill('#username', env('RL_USER'));
  await host.fill('#password', env('RL_PASSWORD'));
  await host.click('#submit');
  const first = await frameResult(host);
  const user = await host.textContent('#bar-user');
  // F5 keeps the sign-in: the app signs in again without the form.
  await host.reload();
  const reloaded = await frameResult(host);
  const formAfterReload = await host.isVisible('#card');
  await host.click('#reload-app');
  const reloadedApp = await frameResult(host);
  // Switch user: sign up on the host, and the app reloads as the new account.
  await host.click('#switch-user');
  await host.waitForSelector('#username:enabled');
  await host.click('#tab-signup');
  await host.fill('#username', env('RL_EMBED_USER'));
  await host.fill('#password', 'embed secret');
  await host.fill('#confirm', 'embed secret');
  await host.click('#submit');
  const switched = await frameResult(host);
  const switchedUser = await host.textContent('#bar-user');
  await context.close();
  return { framedBeforeSignIn, first, user, reloaded, formAfterReload, reloadedApp, switched, switchedUser };
});

await browser.close();
server.close();
console.log(`RESULT ${JSON.stringify(out)}`);
