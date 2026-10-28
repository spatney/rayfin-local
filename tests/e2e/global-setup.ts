import type { FullConfig } from '@playwright/test';

import { startExplorerSuite } from './support/explorer-suite.ts';
import { removeStaleToml, startFixtureSuite } from './support/fixture-suite.ts';
import { REPO_ROOT } from './support/paths.ts';
import { killLeftovers } from './support/pids.ts';
import { run } from './support/project.ts';
import { SuiteHost } from './support/suites.ts';

/**
 * Builds rayfin-local (unless RAYFIN_LOCAL_SKIP_BUILD=1) and serves the suites workers start on
 * demand (support/suites.ts). The returned function is the global teardown: it stops them all.
 */
export default async function globalSetup(_config: FullConfig): Promise<() => Promise<void>> {
  for (const leftover of killLeftovers()) console.log(`[e2e] killed ${leftover} left over by an earlier run`);
  removeStaleToml();
  if (process.env.RAYFIN_LOCAL_SKIP_BUILD !== '1' && !process.env.RAYFIN_LOCAL_BIN) {
    console.log('[e2e] cargo build -p rayfin-local');
    await run('cargo', ['build', '-p', 'rayfin-local'], REPO_ROOT, 'cargo-build');
  }
  const host = await SuiteHost.start({ fixture: startFixtureSuite, explorer: startExplorerSuite });
  process.env.RAYFIN_LOCAL_E2E_CONTROL = host.url;
  return () => host.stopAll();
}
