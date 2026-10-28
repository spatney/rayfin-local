import { expect, type Page } from '@playwright/test';

import type { App } from './fixtures.ts';
import { signInThroughPopup, type PortalOutcome } from './portal.ts';

/*
 * The fixture app's page (tests/fixtures/apps/basic/src/main.ts): `run-<name>` buttons, each
 * writing `{"ok": true, "value": ...}` or `{"ok": false, "error": ...}` into `result-<name>` and
 * setting its `data-status` to `ok` or `error`.
 */

export interface ScenarioError {
  name?: string;
  message: string;
  code?: unknown;
  status?: unknown;
}

export type Outcome =
  | { status: 'ok'; ok: true; value: unknown }
  | { status: 'error'; ok: false; error: ScenarioError };

export interface SessionInfo {
  isAuthenticated: boolean;
  user: { id: string; email: string; role: string | null } | null;
}

/** Opens the app (once per page) and waits until its buttons are wired. */
export async function openFixtureApp(app: App): Promise<void> {
  const url = app.info.serve.frontendUrl;
  if (!url) throw new Error('serve started no frontend for the fixture app');
  if (!app.page.url().startsWith(new URL(url).origin)) {
    await app.page.goto(url);
  }
  await expect(app.page.getByTestId('app-status')).toHaveText('ready');
}

/** Clicks `run-<name>` and returns the scenario's outcome. */
export async function runScenario(page: Page, name: string, timeoutMs = 90_000): Promise<Outcome> {
  await page.getByTestId(`run-${name}`).click();
  return scenarioOutcome(page, name, timeoutMs);
}

/** Waits until `result-<name>` is `ok` or `error` and parses it. */
export async function scenarioOutcome(page: Page, name: string, timeoutMs = 90_000): Promise<Outcome> {
  const result = page.getByTestId(`result-${name}`);
  await expect(result, `scenario ${name} to finish`).toHaveAttribute('data-status', /^(ok|error)$/, {
    timeout: timeoutMs,
  });
  const status = (await result.getAttribute('data-status')) as 'ok' | 'error';
  const outcome = JSON.parse((await result.textContent()) ?? 'null') as Omit<Outcome, 'status'>;
  return { status, ...outcome } as Outcome;
}

/** Runs `name`, expects it to succeed, and returns its value. */
export async function expectOk<T = any>(page: Page, name: string, timeoutMs?: number): Promise<T> {
  const outcome = await runScenario(page, name, timeoutMs);
  expectOutcomeOk(name, outcome);
  return outcome.value as T;
}

export function expectOutcomeOk(name: string, outcome: Outcome): asserts outcome is Extract<Outcome, { ok: true }> {
  const error = outcome.ok ? '' : `: ${JSON.stringify(outcome.error)}`;
  expect(outcome.status, `scenario ${name} reported ${outcome.status}${error}`).toBe('ok');
}

/** Signs in through the popup unless the page's client has a session already. */
export async function ensureSignedIn(app: App): Promise<PortalOutcome | 'already'> {
  await openFixtureApp(app);
  const session = await expectOk<SessionInfo>(app.page, 'session');
  if (session.isAuthenticated) return 'already';
  const { outcome } = await signInThroughPopup(
    app.page,
    () => app.page.getByTestId('run-sign-in').click(),
    app.info.user,
  );
  expectOutcomeOk('sign-in', await scenarioOutcome(app.page, 'sign-in'));
  return outcome;
}
