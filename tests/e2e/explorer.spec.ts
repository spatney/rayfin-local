import type { Locator, Page } from '@playwright/test';

import { BACKEND_PATH, expect, test, type App } from './support/fixtures.ts';
import { signInThroughPopup } from './support/portal.ts';

/*
 * The reference app (EXPLORER_DIR, default fabric-apps/apps/explorer) against `rayfin-local serve`:
 * sign in through the popup, then open each page and check that it renders, that no call to the
 * backend failed, and that it shows no error banner. Opt-in (RAYFIN_LOCAL_E2E_EXPLORER=1): it uses
 * the developer's own tokens and the real semantic model. Its teardown deletes the app's
 * `.rayfin-local` folder and checks that git sees no new changes in the app.
 */

test.use({ suiteName: 'explorer', logScope: 'backend' });
test.skip(process.env.RAYFIN_LOCAL_E2E_EXPLORER !== '1', 'set RAYFIN_LOCAL_E2E_EXPLORER=1 to run the explorer suite');

/** The app's routes (src/App.tsx) and something each page always shows. */
const PAGES: { path: string; shows: (page: Page) => Locator }[] = [
  { path: '/', shows: (page) => page.getByText(/^(Themes|The backlog is empty)$/).first() },
  {
    path: '/features',
    shows: (page) =>
      page.getByPlaceholder('Search by title, id, theme or person').or(page.getByText('The backlog is empty')).first(),
  },
  { path: '/adoption', shows: (page) => page.getByText('Fabric Apps usage from Mission Control telemetry') },
  { path: '/ask', shows: (page) => page.getByText('What would you like to know?') },
  { path: '/settings', shows: (page) => page.getByText('Telemetry model') },
  { path: '/new', shows: (page) => page.getByText('New work item').first() },
];

const ERROR_BANNER = /Couldn['’]t (load|refresh)/;

test('sign-in: a new user signs up in the Fabric popup', async ({ app }) => {
  expect(await signIn(app)).toBe('signed-up');
});

for (const { path, shows } of PAGES) {
  test(`page ${path} renders without backend errors`, async ({ app }) => {
    await signIn(app);
    const { page } = app;
    // Client-side, as the app's links do; a reload would start a new session.
    await page.evaluate((to) => {
      window.history.pushState({}, '', to);
      window.dispatchEvent(new PopStateEvent('popstate', { state: {} }));
    }, path);
    await expect(page).toHaveURL((url) => url.pathname === path);
    await expect(shows(page)).toBeVisible({ timeout: 60_000 });
    await backendIdle(page);
    await expect(page.getByText(ERROR_BANNER), 'an error banner').toHaveCount(0);
    await expect(page.locator('[role="alert"]:visible'), 'an alert').toHaveCount(0);
  });
}

/** Opens the app and signs in through the popup unless it is signed in already. */
async function signIn(app: App): Promise<'signed-up' | 'signed-in' | 'already'> {
  const { page, info } = app;
  const url = info.serve.frontendUrl;
  if (!url) throw new Error('serve started no frontend for the explorer app');
  if (!page.url().startsWith(new URL(url).origin)) await page.goto(url);
  const button = page.getByRole('button', { name: 'Sign in with Microsoft' });
  const shell = page.getByRole('link', { name: /^Overview/ });
  await expect(button.or(shell).first()).toBeVisible({ timeout: 60_000 });
  if (await shell.isVisible()) return 'already';
  const { outcome } = await signInThroughPopup(page, () => button.click(), info.user);
  await expect(shell).toBeVisible({ timeout: 60_000 });
  return outcome;
}

/** Waits until no backend call has been in flight for `quietMs`. */
async function backendIdle(page: Page, quietMs = 1_500, timeoutMs = 60_000): Promise<void> {
  const inFlight = new Set<unknown>();
  const isBackend = (url: string) => BACKEND_PATH.test(new URL(url).pathname);
  const started = (request: { url(): string }) => isBackend(request.url()) && inFlight.add(request);
  const ended = (request: unknown) => inFlight.delete(request);
  page.on('request', started);
  page.on('requestfinished', ended);
  page.on('requestfailed', ended);
  try {
    const deadline = Date.now() + timeoutMs;
    let quietSince = Date.now();
    while (Date.now() - quietSince < quietMs) {
      if (Date.now() > deadline) throw new Error(`Backend calls still in flight after ${timeoutMs / 1000} s.`);
      await page.waitForTimeout(250);
      if (inFlight.size > 0) quietSince = Date.now();
    }
  } finally {
    page.off('request', started);
    page.off('requestfinished', ended);
    page.off('requestfailed', ended);
  }
}
