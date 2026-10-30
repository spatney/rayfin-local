import { randomBytes } from 'node:crypto';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';

import { MockPowerBi } from './mock-powerbi.ts';
import { FIXTURE_DIR } from './paths.ts';
import { assertNotServed, ensureNodeModules, fakeToken, newUser, ProjectFile, readState } from './project.ts';
import { ServeProcess } from './serve.ts';
import type { RunningSuite } from './suites.ts';

const TOML_HEADER = '# Written by tests/e2e for its run; deleted afterwards.';
const TOML_PATH = join(FIXTURE_DIR, 'rayfin-local.toml');

/** Deletes the `rayfin-local.toml` a run that was killed before its teardown left in the fixture. */
export function removeStaleToml(): void {
  if (existsSync(TOML_PATH) && readFileSync(TOML_PATH, 'utf8').startsWith(TOML_HEADER)) rmSync(TOML_PATH);
}

/**
 * The fixture app (`tests/fixtures/apps/basic`) on a fresh database, with fake Power BI and Fabric
 * tokens, its connector pointed at the mock Power BI through a `rayfin-local.toml`, which
 * stop() takes out again, and the control API on.
 */
export async function startFixtureSuite(): Promise<RunningSuite> {
  await assertNotServed(FIXTURE_DIR);
  await ensureNodeModules(FIXTURE_DIR, 'fixture');
  await ensureNodeModules(join(FIXTURE_DIR, 'rayfin', 'functions'), 'fixture-functions');

  const tokens = {
    RAYFIN_LOCAL_TOKEN_POWERBI: fakeToken('powerbi'),
    RAYFIN_LOCAL_TOKEN_FABRIC: fakeToken('fabric'),
  };
  const controlToken = randomBytes(24).toString('hex');
  const mock = await MockPowerBi.start(tokens.RAYFIN_LOCAL_TOKEN_POWERBI);
  removeStaleToml();
  const toml = new ProjectFile(TOML_PATH);
  let serve: ServeProcess | undefined;
  const stop = async () => {
    try {
      await serve?.stop();
    } finally {
      toml.restore();
      await mock.stop();
    }
  };
  try {
    toml.write(`${TOML_HEADER}\n[connectors]\npowerbi_base_url = "${mock.baseUrl}"\n`);
    serve = await ServeProcess.start({
      projectDir: FIXTURE_DIR,
      label: 'fixture',
      env: { ...tokens, RAYFIN_LOCAL_CONTROL_TOKEN: controlToken },
    });
    return {
      info: {
        name: 'fixture',
        serve: serve.info,
        state: readState(FIXTURE_DIR),
        user: newUser(),
        tokens,
        mockPowerBiUrl: mock.baseUrl,
        controlToken,
      },
      stop,
    };
  } catch (error) {
    await stop();
    throw error;
  }
}
