import { test as base, expect, type BrowserContext, type Page } from '@playwright/test';

import { BrowserLog, describe, type Allowed, type LogEntry } from './browser-log.ts';
import { suite, type SuiteInfo } from './suites.ts';

/*
 * Playwright fixtures shared by the suites:
 *
 * - `server` (worker): the suite's running `rayfin-local serve` (support/suites.ts).
 * - `app` (worker): a browser context and page kept across a worker's tests, so the app stays
 *   signed in from one scenario to the next. Playwright replaces the worker after a failed test;
 *   the next test then gets a new page, which signs in again.
 * - per test (automatic): a trace chunk and screenshots kept on failure, and a check that the
 *   test caused no console errors or failed requests other than those it allowed with `allowLog`.
 */

export interface App {
  info: SuiteInfo;
  context: BrowserContext;
  page: Page;
  log: BrowserLog;
}

/** What the per-test log check looks at: everything, or only failed calls to the backend. */
export type LogScope = 'all' | 'backend';

/**
 * The Rayfin backend's routes, called on the API's origin or through the dev server's proxies
 * (`rayfinLocalDev()` sends functions to `/.rayfin/api/<name>`); `.rayfin-local` is the sign-in portal.
 */
export const BACKEND_PATH =
  /^\/(graphql|functions|connector-invoke|connectors|api|\.rayfin\/api|\.rayfin-local|rayfin\.config\.json)(\/|$)/;

const ALWAYS_ALLOWED: Allowed[] = [
  // The browser asks every origin for a favicon; the fixture has none.
  /\/favicon\.ico(\s|$)/,
];

interface TestFixtures {
  logScope: LogScope;
  /** Log entries (`"<kind> <status> <url> <text>"`) the test expects, e.g. a function's 500. */
  allowLog: (...patterns: Allowed[]) => void;
  allowedLog: Allowed[];
  artifacts: void;
}

interface WorkerFixtures {
  suiteName: string;
  server: SuiteInfo;
  app: App;
}

export const test = base.extend<TestFixtures, WorkerFixtures>({
  suiteName: ['fixture', { scope: 'worker', option: true }],
  logScope: ['all', { option: true }],

  server: [
    async ({ suiteName }, use) => {
      await use(await suite(suiteName));
    },
    { scope: 'worker', timeout: 16 * 60_000 },
  ],

  app: [
    async ({ browser, server }, use) => {
      const context = await browser.newContext();
      await context.tracing.start({ screenshots: true, snapshots: true, sources: true });
      const log = new BrowserLog(context);
      const page = await context.newPage();
      await use({ info: server, context, page, log });
      await context.close();
    },
    { scope: 'worker', timeout: 60_000 },
  ],

  allowedLog: async ({}, use) => {
    await use([]);
  },

  allowLog: async ({ allowedLog }, use) => {
    await use((...patterns) => {
      allowedLog.push(...patterns);
    });
  },

  artifacts: [
    async ({ app, logScope, allowedLog }, use, testInfo) => {
      const mark = app.log.mark();
      await app.context.tracing.startChunk({ title: testInfo.title });
      await use();

      const unexpected = app.log
        .unexpected(mark, [...ALWAYS_ALLOWED, ...allowedLog])
        .filter((entry) => logScope === 'all' || isBackendFailure(entry));
      const failed = testInfo.status !== testInfo.expectedStatus;
      if (failed || unexpected.length) {
        const trace = testInfo.outputPath('trace.zip');
        await app.context.tracing.stopChunk({ path: trace });
        await testInfo.attach('trace', { path: trace, contentType: 'application/zip' });
        await testInfo.attach('browser-log', {
          body: JSON.stringify(app.log.since(mark), null, 2),
          contentType: 'application/json',
        });
        for (const [index, page] of app.context.pages().entries()) {
          const screenshot = await page.screenshot({ fullPage: true }).catch(() => null);
          if (screenshot) await testInfo.attach(`page-${index}`, { body: screenshot, contentType: 'image/png' });
        }
      } else {
        await app.context.tracing.stopChunk();
      }
      if (!failed) {
        expect(unexpected.map(describe), 'console errors or failed requests the test did not expect').toEqual([]);
      }
    },
    { auto: true },
  ],
});

export { expect };

function isBackendFailure(entry: LogEntry): boolean {
  if (entry.kind !== 'response' && entry.kind !== 'requestfailed') return false;
  try {
    return BACKEND_PATH.test(new URL(entry.url ?? '').pathname);
  } catch {
    return false;
  }
}
