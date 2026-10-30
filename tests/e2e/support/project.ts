import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import {
  cpSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { join, relative, sep } from 'node:path';

import { FIXTURE_DIR, LOG_DIR } from './paths.ts';

/** Runs a command with its output in `test-results/logs/<label>.log`; throws with the log's tail. */
export async function run(command: string, args: string[], cwd: string, label: string): Promise<void> {
  mkdirSync(LOG_DIR, { recursive: true });
  const logFile = join(LOG_DIR, `${label}.log`);
  const log = createWriteStream(logFile);
  log.write(`> ${command} ${args.join(' ')} (in ${cwd})\n\n`);
  let tail = '';
  const code = await new Promise<number | null>((resolve, reject) => {
    // `shell` runs npm.cmd on Windows; the arguments are fixed strings.
    const child = spawn(command, args, { cwd, shell: process.platform === 'win32', windowsHide: true });
    const capture = (chunk: Buffer) => {
      log.write(chunk);
      tail = (tail + chunk.toString('utf8')).slice(-8_000);
    };
    child.stdout.on('data', capture);
    child.stderr.on('data', capture);
    child.once('error', reject);
    child.once('close', resolve);
  });
  await new Promise<void>((resolve) => log.end(resolve));
  if (code !== 0) {
    throw new Error(`\`${command} ${args.join(' ')}\` in ${cwd} exited with ${code}. Log: ${logFile}\n${tail}`);
  }
}

/** `npm ci` (else `npm install`) in `dir` when it has no node_modules. */
export async function ensureNodeModules(dir: string, label: string): Promise<void> {
  if (existsSync(join(dir, 'node_modules'))) return;
  if (existsSync(join(dir, 'package-lock.json'))) {
    try {
      await run('npm', ['ci', '--no-audit', '--no-fund'], dir, `${label}-npm-ci`);
      return;
    } catch {
      // A lock file out of date with package.json: install instead.
    }
  }
  await run('npm', ['install', '--no-audit', '--no-fund'], dir, `${label}-npm-install`);
}

/** A file the harness writes into a project and takes out again, restoring what was there. */
export class ProjectFile {
  readonly path: string;
  readonly #original: string | null;

  constructor(path: string) {
    this.path = path;
    this.#original = existsSync(path) ? readFileSync(path, 'utf8') : null;
  }

  write(content: string): void {
    writeFileSync(this.path, content);
  }

  restore(): void {
    if (this.#original === null) rmSync(this.path, { force: true });
    else writeFileSync(this.path, this.#original);
  }
}

/** `<project>/.rayfin-local/state.json`. */
export function readState(projectDir: string): Record<string, unknown> {
  const file = join(projectDir, '.rayfin-local', 'state.json');
  if (!existsSync(file)) return {};
  try {
    return JSON.parse(readFileSync(file, 'utf8')) as Record<string, unknown>;
  } catch {
    return {};
  }
}

/** Throws when a `rayfin-local serve` (someone else's) answers on the project's persisted port. */
export async function assertNotServed(projectDir: string): Promise<void> {
  const port = readState(projectDir).apiPort;
  if (typeof port !== 'number') return;
  try {
    const response = await fetch(`http://127.0.0.1:${port}/.rayfin-local/health`, { signal: AbortSignal.timeout(2_000) });
    await response.arrayBuffer();
    if (!response.ok) return;
  } catch {
    return;
  }
  throw new Error(
    `Another rayfin-local serve is running for ${projectDir} (http://localhost:${port}); stop it, then run the tests again.`,
  );
}

/** A user to sign up in the sign-in popup; the password meets any reasonable policy. */
export function newUser(): { email: string; password: string } {
  const id = randomBytes(6).toString('hex');
  return { email: `e2e-${id}@example.com`, password: `E2e-${randomBytes(9).toString('base64url')}-9z` };
}

/** A made-up token for a `RAYFIN_LOCAL_TOKEN_<AUDIENCE>` override. */
export function fakeToken(audience: string): string {
  return `e2e-fake-${audience}-token-${randomBytes(8).toString('hex')}`;
}

/** What a copy of the fixture app leaves out: packages, state, build output, the run's toml. */
const NOT_COPIED = new Set(['node_modules', '.rayfin-local', 'dist', 'rayfin-local.toml']);

/**
 * A copy of the fixture app in `dir`, another app to serve, with a `node_modules` that links each
 * of the fixture's packages, so that Vite's caches go to the copy. Run it with `--no-functions`:
 * the functions' packages aren't linked.
 */
export class FixtureCopy {
  readonly dir: string;
  readonly #links: string[] = [];

  constructor(dir: string) {
    this.dir = dir;
    cpSync(FIXTURE_DIR, dir, {
      recursive: true,
      filter: (source) => !relative(FIXTURE_DIR, source).split(sep).some((part) => NOT_COPIED.has(part)),
    });
    const packages = join(FIXTURE_DIR, 'node_modules');
    mkdirSync(join(dir, 'node_modules'));
    for (const name of readdirSync(packages)) {
      const target = join(packages, name);
      if (name.startsWith('.vite') || !statSync(target).isDirectory()) continue;
      const link = join(dir, 'node_modules', name);
      symlinkSync(target, link, 'junction');
      this.#links.push(link);
    }
  }

  /** Deletes the copy, its links first, so that deleting it never reaches the fixture's packages. */
  remove(): void {
    for (const link of this.#links.splice(0)) unlinkSync(link);
    try {
      rmSync(this.dir, { recursive: true, force: true, maxRetries: 5 });
    } catch {
      // A file still in use: Playwright deletes test-results before the next run.
    }
  }
}
