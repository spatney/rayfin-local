import { spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { basename, join } from 'node:path';

import { CACHE_DIR, rayfinLocalBin } from './paths.ts';

/*
 * The PIDs of the `rayfin-local serve` processes this harness started, so a run that was killed
 * before its teardown (which Windows' job objects normally cover) can't leave one behind.
 */

const FILE = join(CACHE_DIR, 'pids.json');

function read(): Record<string, string> {
  try {
    return JSON.parse(readFileSync(FILE, 'utf8')) as Record<string, string>;
  } catch {
    return {};
  }
}

function write(pids: Record<string, string>): void {
  mkdirSync(CACHE_DIR, { recursive: true });
  writeFileSync(FILE, JSON.stringify(pids, null, 2));
}

export function rememberPid(pid: number, label: string): void {
  write({ ...read(), [pid]: label });
}

export function forgetPid(pid: number): void {
  const pids = read();
  delete pids[pid];
  write(pids);
}

/** Kills the process tree of `pid` (Windows: `taskkill /T /F`; elsewhere its process group). */
export function killTree(pid: number): void {
  if (process.platform === 'win32') {
    spawnSync('taskkill', ['/T', '/F', '/PID', String(pid)], { stdio: 'ignore', windowsHide: true });
    return;
  }
  for (const target of [-pid, pid]) {
    try {
      process.kill(target, 'SIGKILL');
    } catch {
      // Already gone.
    }
  }
}

/** The executable name of a running process, or null when there is none with that PID. */
function imageName(pid: number): string | null {
  if (process.platform === 'win32') {
    const out = spawnSync('tasklist', ['/FI', `PID eq ${pid}`, '/FO', 'CSV', '/NH'], {
      encoding: 'utf8',
      windowsHide: true,
    }).stdout;
    const match = /^"([^"]+)","(\d+)"/m.exec(out ?? '');
    return match && Number(match[2]) === pid ? match[1] : null;
  }
  const out = spawnSync('ps', ['-p', String(pid), '-o', 'comm='], { encoding: 'utf8' }).stdout;
  return out?.trim() || null;
}

/** Kills what an earlier, interrupted run left running; returns what it killed. */
export function killLeftovers(): string[] {
  const killed: string[] = [];
  const image = basename(rayfinLocalBin()).toLowerCase();
  for (const [pid, label] of Object.entries(read())) {
    const name = imageName(Number(pid));
    if (name && basename(name).toLowerCase() === image) {
      killTree(Number(pid));
      killed.push(`${label} (pid ${pid})`);
    }
  }
  write({});
  return killed;
}
