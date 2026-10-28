import { spawn, type ChildProcess } from 'node:child_process';
import { createWriteStream, mkdirSync, type WriteStream } from 'node:fs';
import { join } from 'node:path';

import { LOG_DIR, rayfinLocalBin, requireFile } from './paths.ts';
import { forgetPid, killTree, rememberPid } from './pids.ts';
import { freePorts, sleep, waitForPortsClosed } from './ports.ts';

export interface ServeOptions {
  /** The Rayfin project (`-C`). */
  projectDir: string;
  /** Names the log: `test-results/logs/<label>-serve.log`. */
  label: string;
  /** Added to the inherited environment; `undefined` removes a variable. */
  env?: Record<string, string | undefined>;
  /** Start from an empty database (`--reset`). Default true. */
  reset?: boolean;
  /** Wait for the frontend (the Vite dev server) too. Default true. */
  frontend?: boolean;
  /** More `serve` arguments, e.g. `--no-functions`. */
  args?: string[];
  /** Default 5 minutes: the first start builds the functions and Vite optimizes dependencies. */
  startupTimeoutMs?: number;
}

/** A running `rayfin-local serve`, as tests (in other processes) need it. */
export interface ServeInfo {
  label: string;
  pid: number;
  projectDir: string;
  /** The backend's origin, e.g. `http://127.0.0.1:23456`. */
  apiUrl: string;
  apiPort: number;
  /** The frontend's URL from the banner, e.g. `http://localhost:23457/`; null without frontend. */
  frontendUrl: string | null;
  frontendPort: number;
  logFile: string;
}

const ANSI = /\x1b\[[0-9;?]*[A-Za-z]/g;
const LOG_TAIL_CHARS = 64 * 1024;
/** How serve (Rust's AddrInUse, Windows and Unix) or Vite report a port that is taken. */
const PORT_TAKEN = /os error (10048|98|48)\b|address already in use|port \d+ is (already )?in use/i;

class PortTakenError extends Error {}

export class ServeProcess {
  readonly info: ServeInfo;
  readonly #child: ChildProcess;
  readonly #log: WriteStream;
  readonly #exited: Promise<number | null>;
  readonly #killOnExit: () => void;
  #output = '';
  #stopped = false;

  private constructor(info: ServeInfo, child: ChildProcess, log: WriteStream) {
    this.info = info;
    this.#child = child;
    this.#log = log;
    this.#exited = new Promise((resolve) => child.once('exit', (code) => resolve(code)));
    log.on('error', () => {});
    const capture = (chunk: Buffer) => {
      // Output can still arrive after 'exit', when stop() has ended the log.
      if (!log.writableEnded) log.write(chunk);
      this.#output = (this.#output + chunk.toString('utf8')).slice(-LOG_TAIL_CHARS);
    };
    child.stdout?.on('data', capture);
    child.stderr?.on('data', capture);
    // Covers the runner exiting without its teardown. (If the runner is killed, Windows ends
    // serve with the runner's job object, and serve ends its own children.)
    this.#killOnExit = () => killTree(info.pid);
    process.once('exit', this.#killOnExit);
  }

  /** Starts `rayfin-local serve` on free ports and waits until the backend and frontend answer. */
  static async start(options: ServeOptions): Promise<ServeProcess> {
    const bin = requireFile(
      rayfinLocalBin(),
      'build it with `cargo build -p rayfin-local` or set RAYFIN_LOCAL_BIN',
    );
    for (let attempt = 1; ; attempt++) {
      const [apiPort, frontendPort] = await freePorts(2);
      try {
        return await ServeProcess.#startOnce(bin, options, apiPort, frontendPort);
      } catch (error) {
        // Another process may take a port between the check and serve's bind.
        if (attempt >= 3 || !(error instanceof PortTakenError)) throw error;
      }
    }
  }

  static async #startOnce(
    bin: string,
    options: ServeOptions,
    apiPort: number,
    frontendPort: number,
  ): Promise<ServeProcess> {
    const wantFrontend = options.frontend ?? true;
    const args = [
      'serve',
      '--verbose',
      '-C',
      options.projectDir,
      '--port',
      String(apiPort),
      '--frontend-port',
      String(frontendPort),
      ...(options.reset === false ? [] : ['--reset']),
      ...(wantFrontend ? [] : ['--no-frontend']),
      ...(options.args ?? []),
    ];
    mkdirSync(LOG_DIR, { recursive: true });
    const logFile = join(LOG_DIR, `${options.label}-serve.log`);
    const log = createWriteStream(logFile, { flags: 'a' });
    log.write(`> ${bin} ${args.join(' ')}\n\n`);

    const env: NodeJS.ProcessEnv = { ...process.env, NO_COLOR: '1' };
    for (const [name, value] of Object.entries(options.env ?? {})) {
      if (value === undefined) delete env[name];
      else env[name] = value;
    }
    const child = spawn(bin, args, {
      cwd: options.projectDir,
      env,
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
      // Elsewhere, its own process group, so stop() can kill the group.
      detached: process.platform !== 'win32',
    });
    const spawned = await new Promise<Error | null>((resolve) => {
      child.once('spawn', () => resolve(null));
      child.once('error', resolve);
    });
    if (spawned || child.pid === undefined) {
      log.end();
      throw new Error(`Could not start ${bin}: ${spawned?.message ?? 'no pid'}`);
    }
    rememberPid(child.pid, options.label);
    const serve = new ServeProcess(
      {
        label: options.label,
        pid: child.pid,
        projectDir: options.projectDir,
        apiUrl: `http://127.0.0.1:${apiPort}`,
        apiPort,
        frontendUrl: null,
        frontendPort,
        logFile,
      },
      child,
      log,
    );
    try {
      await serve.#waitUntilReady(wantFrontend, options.startupTimeoutMs ?? 300_000);
    } catch (error) {
      await serve.stop().catch(() => {});
      throw error;
    }
    return serve;
  }

  /** What serve printed so far, without colors. */
  output(): string {
    return this.#output.replace(ANSI, '');
  }

  /** Kills the process tree and checks that its ports are free again. */
  async stop(): Promise<void> {
    if (this.#stopped) return;
    this.#stopped = true;
    process.off('exit', this.#killOnExit);
    if (this.#child.exitCode === null && this.#child.signalCode === null) {
      killTree(this.info.pid);
      const exited = await Promise.race([this.#exited.then(() => true), sleep(15_000).then(() => false)]);
      if (!exited) throw new Error(`rayfin-local (pid ${this.info.pid}) did not exit after it was killed.`);
    }
    forgetPid(this.info.pid);
    await new Promise<void>((resolve) => this.#log.end(resolve));
    await waitForPortsClosed([this.info.apiPort, this.info.frontendPort], 15_000);
  }

  async #waitUntilReady(wantFrontend: boolean, timeoutMs: number): Promise<void> {
    const started = Date.now();
    let exitCode: number | null | undefined;
    void this.#exited.then((code) => (exitCode = code));
    let apiReady = false;
    for (;;) {
      if (exitCode !== undefined) {
        const reason = `exited with code ${exitCode} before it was ready`;
        if (PORT_TAKEN.test(this.output())) throw new PortTakenError(this.#startupError(reason).message);
        throw this.#startupError(reason);
      }
      if (Date.now() - started > timeoutMs) {
        throw this.#startupError(`was not ready after ${Math.round(timeoutMs / 1000)} s`);
      }
      const banner = parseBanner(this.output());
      if (banner.api) this.info.apiUrl = banner.api.replace(/\/+$/, '');
      if (!apiReady) apiReady = await answers(`${this.info.apiUrl}/.rayfin-local/health`);
      if (apiReady && !wantFrontend) return;
      if (apiReady) {
        // The banner's app URL, else (should its wording change) the dev server's port.
        const bannerUrl = banner.frontend?.match(/^https?:\/\/\S+/)?.[0];
        for (const url of [bannerUrl, `http://localhost:${this.info.frontendPort}/`]) {
          if (url && (await answers(url))) {
            this.info.frontendUrl = url;
            return;
          }
        }
        if (banner.frontend !== undefined && !bannerUrl) {
          throw this.#startupError(`started no frontend: "${banner.frontend}"`);
        }
      }
      await sleep(250);
    }
  }

  #startupError(reason: string): Error {
    const tail = this.output().trimEnd().split(/\r?\n/).slice(-60).join('\n');
    return new Error(
      `rayfin-local serve for ${this.info.label} ${reason}. Log: ${this.info.logFile}\n` +
        `--- last lines ---\n${tail}`,
    );
  }
}

/** The `API` and `App` (formerly `Frontend`) lines of the startup banner. */
export function parseBanner(output: string): { api?: string; frontend?: string } {
  const start = output.search(/is running locally/);
  const banner = start >= 0 ? output.slice(start) : '';
  const api = /^\s*API\s+(https?:\/\/\S+)\s*$/m.exec(banner)?.[1];
  const frontend = /^\s*(?:App|Frontend)\s+(.+?)\s*$/m.exec(banner)?.[1];
  return { api, frontend };
}

async function answers(url: string): Promise<boolean> {
  try {
    const response = await fetch(url, { signal: AbortSignal.timeout(5_000) });
    await response.arrayBuffer();
    return response.ok;
  } catch {
    return false;
  }
}
