import { expect, type Page } from '@playwright/test';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { RESULTS_DIR } from './paths.ts';

/*
 * The emulated "Sign in with Fabric" popup (`<api>/.rayfin-local/portal/secureItemEmbed`, from
 * crates/rl-auth/src/portal): sign-in and sign-up tabs over one form; it closes itself once the app
 * has its code. Switching tabs and submitting clear the form's error.
 */

export interface Credentials {
  email: string;
  password: string;
}

export type PortalOutcome = 'signed-up' | 'signed-in';

/** Accounts signed up in this run, across worker processes (test-results is new every run). */
const ACCOUNTS_DIR = join(RESULTS_DIR, '.accounts');
const marker = (email: string) => join(ACCOUNTS_DIR, email.replace(/[^\w.@-]/g, '_'));

/**
 * Clicks `trigger` on `page`, which must open the popup, and signs `user` up there on the run's
 * first sign-in, else signs in. Resolves once the popup closed.
 */
export async function signInThroughPopup(
  page: Page,
  trigger: () => Promise<void>,
  user: Credentials,
): Promise<{ popupUrl: URL; outcome: PortalOutcome }> {
  const [popup] = await Promise.all([page.waitForEvent('popup', { timeout: 30_000 }), trigger()]);
  // Settles when the popup or its whole context closes.
  const closed = popup.waitForEvent('close', { timeout: 0 }).then(
    () => true,
    () => true,
  );
  await popup.waitForLoadState('domcontentloaded').catch(() => {});
  const popupUrl = new URL(popup.url());
  await expect(popup.locator('#username'), 'the sign-in form').toBeEnabled({ timeout: 30_000 });

  const canSignUp = await popup.locator('#tab-signup').isVisible();
  const signIn = { tab: 'signin', outcome: 'signed-in' } as const;
  const signUp = { tab: 'signup', outcome: 'signed-up' } as const;
  // The other attempt is a fallback; the 4xx of the first then fails the test's log check.
  const attempts = !canSignUp ? [signIn] : existsSync(marker(user.email)) ? [signIn, signUp] : [signUp, signIn];
  const errors: string[] = [];
  for (const attempt of attempts) {
    if (canSignUp) await popup.locator(`#tab-${attempt.tab}`).click();
    await popup.locator('#username').fill(user.email);
    await popup.locator('#password').fill(user.password);
    if (attempt.tab === 'signup') await popup.locator('#confirm').fill(user.password);
    await popup.locator('#submit').click();
    const error = await closedOrError(popup, closed);
    if (error === null) {
      mkdirSync(ACCOUNTS_DIR, { recursive: true });
      writeFileSync(marker(user.email), attempt.outcome);
      return { popupUrl, outcome: attempt.outcome };
    }
    errors.push(`${attempt.tab}: ${error}`);
  }
  throw new Error(`The sign-in popup refused ${user.email}: ${errors.join('; ')}`);
}

/** Null once the popup closed, else the error its form shows. */
async function closedOrError(popup: Page, closed: Promise<boolean>, timeoutMs = 30_000): Promise<string | null> {
  const formError = popup.locator('#form-error');
  const first = await Promise.race([
    closed.then(() => 'closed' as const),
    formError.waitFor({ state: 'visible', timeout: timeoutMs }).then(
      () => 'error' as const,
      () => 'neither' as const,
    ),
  ]);
  if (first === 'closed' || popup.isClosed()) return null;
  if (first === 'error') return (await formError.textContent())?.trim() || 'an empty error';
  // The wait may have ended because the popup is closing.
  const late = await Promise.race([closed.then(() => true), sleep(2_000).then(() => false)]);
  if (late) return null;
  const shown = await popup.locator('body').innerText().catch(() => '');
  throw new Error(`The sign-in popup neither closed nor showed an error within ${timeoutMs / 1000} s. It shows:\n${shown}`);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
