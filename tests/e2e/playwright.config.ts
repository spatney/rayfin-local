import { defineConfig, devices } from '@playwright/test';

/*
 * One worker, tests in file order: the suites share one `rayfin-local serve` per app.
 * PLAYWRIGHT_CHANNEL=msedge (or chrome) uses an installed browser instead of Playwright's Chromium.
 */
const channel = process.env.PLAYWRIGHT_CHANNEL || undefined;

export default defineConfig({
  testDir: '.',
  testIgnore: ['portal/**', 'node_modules/**'],
  outputDir: 'test-results',
  globalSetup: './global-setup.ts',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  forbidOnly: !!process.env.CI,
  reporter: 'list',
  timeout: 120_000,
  expect: { timeout: 30_000 },
  use: {
    ...devices['Desktop Chrome'],
    channel,
    headless: process.env.HEADED !== '1',
    actionTimeout: 30_000,
    navigationTimeout: 60_000,
    // The specs share one browser context per worker (support/fixtures.ts), which records its
    // own trace chunk and screenshots per test and keeps them on failure.
    trace: 'off',
    screenshot: 'off',
  },
  projects: [
    { name: 'fixture', testMatch: 'fixture.spec.ts' },
    { name: 'explorer', testMatch: 'explorer.spec.ts' },
  ],
});
