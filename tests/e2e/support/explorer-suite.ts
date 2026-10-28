import { spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, readdirSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { join, relative } from 'node:path';

import { CACHE_DIR, explorerDir, requireFile } from './paths.ts';
import { assertNotServed, newUser, readState } from './project.ts';
import { ServeProcess } from './serve.ts';
import type { RunningSuite } from './suites.ts';

/**
 * The reference app (EXPLORER_DIR) on a fresh database, with the developer's real tokens (its
 * connector queries the real semantic model). stop() deletes the `.rayfin-local` folder serve
 * created, puts back one that was there before, and checks that git sees no new changes.
 */
export async function startExplorerSuite(): Promise<RunningSuite> {
  const dir = explorerDir();
  requireFile(join(dir, 'rayfin', 'rayfin.yml'), 'set EXPLORER_DIR to the reference app (fabric-apps/apps/explorer)');
  requireFile(join(dir, 'node_modules'), 'run `npm ci` in the reference app first');
  const stateDir = join(dir, '.rayfin-local');
  await assertNotServed(dir);
  recoverInterruptedRun(stateDir);
  const git = gitStatus(dir);
  const backup = existsSync(stateDir) ? moveAside(stateDir) : null;
  let serve: ServeProcess | undefined;
  const stop = async () => {
    try {
      await serve?.stop();
    } finally {
      rmSync(stateDir, { recursive: true, force: true });
      if (backup) move(backup, stateDir);
    }
    const after = gitStatus(dir);
    const added = after.lines.filter((line) => !git.lines.includes(line));
    if (added.length) {
      throw new Error(`The explorer run left changes in ${git.root}:\n${added.join('\n')}`);
    }
  };
  try {
    serve = await ServeProcess.start({ projectDir: dir, label: 'explorer' });
    writeFileSync(join(stateDir, RUN_MARKER), 'tests/e2e is using this folder; it deletes it afterwards.\n');
    return {
      info: { name: 'explorer', serve: serve.info, state: readState(dir), user: newUser(), tokens: {} },
      stop,
    };
  } catch (error) {
    await stop().catch(() => {});
    throw error;
  }
}

/** Marks the `.rayfin-local` folder of a run, so a later run knows it isn't the developer's. */
const RUN_MARKER = 'rayfin-local-e2e-run';
const BACKUP_PREFIX = 'explorer-state-';

/** Undoes what a run that was killed before its teardown left: its state, the developer's moved away. */
function recoverInterruptedRun(stateDir: string): void {
  if (existsSync(join(stateDir, RUN_MARKER))) rmSync(stateDir, { recursive: true, force: true });
  const backups = existsSync(CACHE_DIR)
    ? readdirSync(CACHE_DIR).filter((name) => name.startsWith(BACKUP_PREFIX)).sort()
    : [];
  const latest = backups.at(-1);
  if (latest && !existsSync(stateDir)) {
    move(join(CACHE_DIR, latest), stateDir);
    console.log(`[e2e] put back ${stateDir}, which an interrupted run had moved aside`);
  }
}

/** `git status --short` of `dir` in its repository. */
function gitStatus(dir: string): { root: string; lines: string[] } {
  const git = (...args: string[]) => {
    const result = spawnSync('git', ['-C', dir, ...args], { encoding: 'utf8', windowsHide: true });
    if (result.status !== 0) throw new Error(`git ${args.join(' ')} in ${dir} failed: ${result.stderr}`);
    return result.stdout;
  };
  const root = git('rev-parse', '--show-toplevel').trim();
  const lines = git('-C', root, 'status', '--short', '--untracked-files=all', '--', relative(root, dir) || '.')
    .split('\n')
    .map((line) => line.trimEnd())
    .filter(Boolean);
  return { root, lines };
}

/** Moves the developer's own local state out of the way of `--reset`. */
function moveAside(stateDir: string): string {
  const backup = join(CACHE_DIR, `${BACKUP_PREFIX}${Date.now()}`);
  mkdirSync(CACHE_DIR, { recursive: true });
  move(stateDir, backup);
  console.log(`[e2e] moved ${stateDir} to ${backup} for the run; it is put back afterwards`);
  return backup;
}

function move(from: string, to: string): void {
  try {
    renameSync(from, to);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'EXDEV') {
      throw new Error(`Could not move ${from} to ${to} (is a rayfin-local serving the app?): ${String(error)}`);
    }
    cpSync(from, to, { recursive: true });
    rmSync(from, { recursive: true, force: true });
  }
}
