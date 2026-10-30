import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import { BrowserLog, describe } from './support/browser-log.ts';
import {
  ensureSignedIn,
  expectOk,
  openFixtureApp,
  runScenario,
  scenarioOutcome,
  expectOutcomeOk,
  type SessionInfo,
} from './support/fixture-app.ts';
import { expect, test } from './support/fixtures.ts';
import type { RecordedRequest } from './support/mock-powerbi.ts';
import { freePorts } from './support/ports.ts';
import { signInThroughPopup } from './support/portal.ts';
import { fakeToken, FixtureCopy } from './support/project.ts';
import { ServeProcess } from './support/serve.ts';

/*
 * The fixture app (tests/fixtures/apps/basic) against `rayfin-local serve`, scenario by scenario,
 * in this order: config, sign-in, session, data, functions, connector, storage, sign-out,
 * auto-sign-in, then stale sessions (which deletes the local users). Expected values are in the
 * fixture's README.
 */

const DAX_QUERY = 'EVALUATE ROW("answer", 42)';
const WORKSPACE_ID = '00000000-0000-0000-0000-00000000aaaa';
const ITEM_ID = '00000000-0000-0000-0000-00000000bbbb';

test('config: the app talks to the local backend', async ({ app }, testInfo) => {
  await openFixtureApp(app);
  const origin = new URL(app.page.url()).origin;
  const config = await expectOk<{
    apiUrl: string;
    workspaceId: string;
    projectId: string;
    fabricPortalUrl: string;
    returnOrigin: string;
  }>(app.page, 'config');
  // The backend directly, or the dev server, which proxies the backend's routes (rayfinLocalDev()).
  const apiOrigin = new URL(config.apiUrl).origin;
  const backendOrigin = new URL(app.info.serve.apiUrl).origin;
  expect([origin, backendOrigin], 'VITE_RAYFIN_API_URL is the dev server or the backend').toContain(apiOrigin);
  testInfo.annotations.push({
    type: 'apiUrl',
    description: `${config.apiUrl} (${apiOrigin === origin ? 'the Vite origin' : 'the backend origin'})`,
  });
  expect(config.returnOrigin).toBe(origin);
  expect(config.workspaceId).toBeTruthy();
  expect(config.projectId).toBeTruthy();
  const portal = new URL(config.fabricPortalUrl);
  expect(portal.pathname).toMatch(/^\/\.rayfin-local\/portal\/?$/);
  expect(portal.port, 'the sign-in portal is on the backend').toBe(String(app.info.serve.apiPort));

  const state = JSON.parse(
    readFileSync(join(app.info.serve.projectDir, '.rayfin-local', 'state.json'), 'utf8'),
  ) as { publishableKey?: string };
  expect(state.publishableKey, 'state.json has a publishable key').toBeTruthy();
  // Vite inlines import.meta.env into the modules it serves.
  const bootstrap = await app.page.evaluate(async () => (await fetch('/src/services/bootstrap.ts')).text());
  expect(bootstrap, 'VITE_RAYFIN_PUBLISHABLE_KEY is the key in state.json').toContain(state.publishableKey);
});

test('sign-in: a new user signs up in the Fabric popup', async ({ app }) => {
  await openFixtureApp(app);
  const { page, info } = app;
  const { popupUrl, outcome } = await signInThroughPopup(
    page,
    () => page.getByTestId('run-sign-in').click(),
    info.user,
  );
  expect(popupUrl.pathname).toBe('/.rayfin-local/portal/secureItemEmbed');
  expect(popupUrl.port, 'the popup is served by the backend').toBe(String(info.serve.apiPort));
  expect(outcome).toBe('signed-up');
  const signIn = await scenarioOutcome(page, 'sign-in');
  expectOutcomeOk('sign-in', signIn);
  expect(signIn.value).toMatchObject({ isAuthenticated: true, user: { email: info.user.email } });
});

test('session: getSession has the signed-in user', async ({ app }) => {
  await ensureSignedIn(app);
  const session = await expectOk<SessionInfo>(app.page, 'session');
  expect(session).toMatchObject({ isAuthenticated: true, user: { email: app.info.user.email } });
  expect(session.user?.id).toBeTruthy();
});

test.describe('data', () => {
  test.beforeEach(async ({ app }) => {
    await ensureSignedIn(app);
  });

  test('todo-crud', async ({ app }) => {
    expect(await expectOk(app.page, 'todo-crud')).toEqual({
      created: { title: 'crud', status: 'done' },
      listed: [{ title: 'crud', status: 'done' }],
      updated: { title: 'crud edited', status: 'doing' },
      deleted: true,
      foundAfterDelete: false,
    });
  });

  test('todo-paginate', async ({ app }) => {
    expect(await expectOk(app.page, 'todo-paginate')).toEqual([
      { titles: ['1', '2'], hasNextPage: true },
      { titles: ['3'], hasNextPage: false },
    ]);
  });

  test('todo-category', async ({ app }) => {
    expect(await expectOk(app.page, 'todo-category')).toEqual({
      todo: { title: 'categorized', category: 'category' },
      category: { name: 'category', todos: ['categorized'] },
    });
  });

  test('todo-aggregate', async ({ app }) => {
    expect(await expectOk(app.page, 'todo-aggregate')).toEqual([
      { status: 'doing', todos: 2, totalCost: 4 },
      { status: 'done', todos: 1, totalCost: 4 },
    ]);
  });
});

test.describe('functions', () => {
  test.beforeEach(async ({ app }) => {
    await ensureSignedIn(app);
  });

  test('fn-ping', async ({ app }) => {
    expect(await expectOk(app.page, 'fn-ping')).toBe('pong');
  });

  test('fn-greet', async ({ app }) => {
    expect(await expectOk(app.page, 'fn-greet')).toBe('Hello, Ada!');
  });

  test('fn-echo', async ({ app }) => {
    expect(await expectOk(app.page, 'fn-echo')).toEqual({ payload: { a: 1, tags: ['x', 'y'] }, tagCount: 2 });
  });

  test('fn-echo-null', async ({ app }) => {
    expect(await expectOk(app.page, 'fn-echo-null')).toEqual({ payload: null, tagCount: 0 });
  });

  test('fn-who-am-i', async ({ app }) => {
    const value = await expectOk<{ hasCallerToken: boolean; baseUrl: string }>(app.page, 'fn-who-am-i');
    expect(value.hasCallerToken, 'the function got the caller token').toBe(true);
    expect(value.baseUrl).toMatch(/^https?:\/\//);
  });

  test('fn-count-todos', async ({ app }) => {
    const value = await expectOk<{ count: number }>(app.page, 'fn-count-todos');
    // Not exact: a failed data scenario may leave rows behind.
    expect(Number.isInteger(value.count) && value.count >= 0, `count ${value.count}`).toBe(true);
  });

  test('fn-token-info', async ({ app }) => {
    expect(await expectOk(app.page, 'fn-token-info')).toEqual({
      fabricTokenLength: app.info.tokens.RAYFIN_LOCAL_TOKEN_FABRIC.length,
    });
  });

  test('fn-secret-info', async ({ app }) => {
    expect(await expectOk(app.page, 'fn-secret-info')).toEqual({
      matchesFixture: true,
      length: 'local-secret-value'.length,
    });
  });

  test('fn-fail: the function error reaches the app', async ({ app, allowLog }) => {
    // The failed invocation's HTTP status, and Chromium's console line about it, on whichever
    // route the SDK used (`/functions/fail/invoke`, or the dev server's `/.rayfin/api/fail`).
    allowLog(/\/fail(\/invoke)?([\s?]|$)/);
    const outcome = await runScenario(app.page, 'fn-fail');
    expect(outcome.status).toBe('error');
    expect(outcome.ok).toBe(false);
    if (!outcome.ok) expect(outcome.error.message).toContain('deliberate failure');
  });
});

test('connector-query: the semantic model answers through Power BI (mock)', async ({ app }) => {
  await ensureSignedIn(app);
  const value = await expectOk<{ query: string; result: Record<string, unknown> }>(app.page, 'connector-query');
  expect(value.query).toBe(DAX_QUERY);
  // The mock answers the result Power BI gave rl-connectors' recording, `EVALUATE ROW("x", 1)`.
  expect(value.result).toMatchObject({
    status: 'success',
    table: { columns: [{ name: '[x]' }], rows: [[1]] },
  });

  const requests = (await (await fetch(`${app.info.mockPowerBiUrl}/__mock/requests`)).json()) as RecordedRequest[];
  expect(requests.length, 'requests to the mock Power BI').toBeGreaterThan(0);
  for (const request of requests) {
    expect(request).toMatchObject({ method: 'POST', query: DAX_QUERY, hasAuthorization: true });
    expect(request.path).toMatch(
      new RegExp(`^/v1\\.0/myorg/groups/${WORKSPACE_ID}/datasets/${ITEM_ID}/(executeDaxQueries|executeQueries)$`),
    );
    expect(request.tokenMatches, 'the bearer token is RAYFIN_LOCAL_TOKEN_POWERBI').toBe(true);
  }
});

test('storage-roundtrip', async ({ app }) => {
  await ensureSignedIn(app);
  const value = await expectOk<{
    uploaded: unknown;
    listed: unknown[];
    downloaded: unknown;
    deleted: { success: boolean };
    remaining: string[];
  }>(app.page, 'storage-roundtrip');
  const content = '%PDF-1.4 rayfin-local fixture';
  expect(value.uploaded).toEqual({
    name: 'fixture-note.pdf',
    size: content.length,
    contentType: 'application/pdf',
    team_id: 'team-1',
    pages: 1,
  });
  expect(value.listed).toContainEqual({ name: 'fixture-note.pdf', team_id: 'team-1' });
  expect(value.downloaded).toEqual({ text: content, contentType: 'application/pdf' });
  expect(value.deleted).toMatchObject({ success: true });
  expect(value.remaining).not.toContain('fixture-note.pdf');
});

test('sign-out: the session ends', async ({ app }) => {
  await ensureSignedIn(app);
  expect(await expectOk<SessionInfo>(app.page, 'sign-out')).toEqual({ isAuthenticated: false, user: null });
  expect(await expectOk<SessionInfo>(app.page, 'session')).toEqual({ isAuthenticated: false, user: null });
});

test('auto-sign-in: the app signs itself in as the developer, with no popup', async ({ app }) => {
  await openFixtureApp(app);
  const { page } = app;
  let popups = 0;
  const countPopup = () => popups++;
  page.on('popup', countPopup);
  try {
    const answered = page.waitForResponse((response) => response.url().endsWith('/.rayfin/dev/session-token'));
    const session = await expectOk<SessionInfo>(page, 'auto-sign-in');
    const answer = await answered;
    expect(answer.status()).toBe(200);
    expect(new URL(answer.url()).origin, 'the dev server answered').toBe(new URL(page.url()).origin);
    // The suite's Power BI token is fake, so the developer is the local developer account.
    expect(session).toMatchObject({ isAuthenticated: true, user: { email: 'developer@example.com' } });
    expect(popups, 'sign-in windows opened').toBe(0);
    expect(await expectOk<SessionInfo>(page, 'session')).toEqual(session);
    expect(await expectOk(page, 'todo-crud')).toMatchObject({ deleted: true, foundAfterDelete: false });
  } finally {
    page.off('popup', countPopup);
  }
  await expectOk(page, 'sign-out');
});

/*
 * The SDK keeps the app's session in the origin's localStorage. rayfin-local's dev server makes
 * each page forget a session its backend signed out, or another app's that ran on the same port,
 * so the app signs in again rather than get 401s for it (which the log check would catch).
 */
test.describe('stale sessions', () => {
  const signedOut = { isAuthenticated: false, user: null };

  test('after a data reset, a reload signs the app in again', async ({ app }) => {
    await openFixtureApp(app);
    const { page, info } = app;
    expect(await expectOk<SessionInfo>(page, 'auto-sign-in')).toMatchObject({ isAuthenticated: true });
    const reset = await fetch(`${info.serve.apiUrl}/.rayfin-local/control/data/reset`, {
      method: 'POST',
      headers: { authorization: `Bearer ${info.controlToken}` },
    });
    expect(reset.status, await reset.text()).toBe(200);

    await page.reload();
    await expect(page.getByTestId('app-status')).toHaveText('ready');
    expect(await expectOk<SessionInfo>(page, 'session')).toEqual(signedOut);
    expect(await expectOk<SessionInfo>(page, 'auto-sign-in')).toMatchObject({
      isAuthenticated: true,
      user: { email: 'developer@example.com' },
    });
    expect(await expectOk(page, 'todo-crud')).toMatchObject({ deleted: true, foundAfterDelete: false });
  });

  test('another app on the same port starts signed out', async ({ browser }, testInfo) => {
    test.setTimeout(10 * 60_000);
    const [frontendPort] = await freePorts(1);
    const copies = [new FixtureCopy(testInfo.outputPath('app-a')), new FixtureCopy(testInfo.outputPath('app-b'))];
    const env = { RAYFIN_LOCAL_TOKEN_POWERBI: fakeToken('powerbi') };
    const serve = (copy: FixtureCopy, label: string) =>
      ServeProcess.start({ projectDir: copy.dir, label, env, frontendPort, args: ['--no-functions'] });
    const context = await browser.newContext();
    const log = new BrowserLog(context);
    const page = await context.newPage();
    let running: ServeProcess | undefined;
    try {
      running = await serve(copies[0], 'same-port-a');
      const url = running.info.frontendUrl!;
      await page.goto(url);
      await expect(page.getByTestId('app-status')).toHaveText('ready');
      const first = await expectOk<SessionInfo>(page, 'auto-sign-in');
      expect(first.isAuthenticated).toBe(true);
      // Away from the app while its dev server stops, so Vite's client doesn't poll for it.
      await page.goto('about:blank');
      await running.stop();

      running = await serve(copies[1], 'same-port-b');
      expect(new URL(running.info.frontendUrl!).origin).toBe(new URL(url).origin);
      await page.goto(url);
      await expect(page.getByTestId('app-status')).toHaveText('ready');
      expect(await expectOk<SessionInfo>(page, 'session')).toEqual(signedOut);
      const second = await expectOk<SessionInfo>(page, 'auto-sign-in');
      expect(second.isAuthenticated).toBe(true);
      expect(second.user?.id).not.toBe(first.user?.id);
      expect(await expectOk(page, 'todo-crud')).toMatchObject({ deleted: true, foundAfterDelete: false });
      expect(
        log.unexpected(0, [/\/favicon\.ico(\s|$)/]).map(describe),
        'console errors or failed requests',
      ).toEqual([]);
    } finally {
      await running?.stop();
      await context.close();
      for (const copy of copies) copy.remove();
    }
  });
});
