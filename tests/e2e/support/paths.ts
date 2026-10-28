import { existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

/** `tests/e2e`. */
export const E2E_DIR = resolve(here, '..');
export const REPO_ROOT = resolve(E2E_DIR, '..', '..');
export const FIXTURE_DIR = join(REPO_ROOT, 'tests', 'fixtures', 'apps', 'basic');
/** Playwright's `outputDir`; process logs go to its `logs` folder. */
export const RESULTS_DIR = join(E2E_DIR, 'test-results');
export const LOG_DIR = join(RESULTS_DIR, 'logs');
/** Survives Playwright's cleaning of `test-results` at the start of a run. */
export const CACHE_DIR = join(E2E_DIR, 'node_modules', '.cache', 'rayfin-local-e2e');
/** Power BI's `executeDaxQueries` answer to `EVALUATE ROW("x", 1)`, recorded for rl-connectors. */
export const ROW_ARROWS = join(REPO_ROOT, 'crates', 'rl-connectors', 'tests', 'fixtures', 'row.arrows');

export function rayfinLocalBin(): string {
  const configured = process.env.RAYFIN_LOCAL_BIN;
  if (configured) return resolve(configured);
  const exe = process.platform === 'win32' ? 'rayfin-local.exe' : 'rayfin-local';
  return join(REPO_ROOT, 'target', 'debug', exe);
}

/** The reference app: `EXPLORER_DIR`, else `fabric-apps/apps/explorer` next to this repository. */
export function explorerDir(): string {
  return resolve(process.env.EXPLORER_DIR ?? join(REPO_ROOT, '..', 'fabric-apps', 'apps', 'explorer'));
}

export function requireFile(path: string, hint: string): string {
  if (!existsSync(path)) throw new Error(`${path} does not exist: ${hint}`);
  return path;
}
