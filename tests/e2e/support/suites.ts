import http from 'node:http';
import type { AddressInfo } from 'node:net';

import type { ServeInfo } from './serve.ts';

/*
 * Suites (a `rayfin-local serve` and what it needs, such as the mock Power BI) are started by the
 * Playwright runner process on a worker's first request and stopped by the global teardown. So
 * they outlive worker restarts (Playwright replaces a worker after every failed test), start only
 * for the projects that run, and die with the runner.
 *
 * The runner serves this on 127.0.0.1 (RAYFIN_LOCAL_E2E_CONTROL); workers call `suite(name)`.
 */

/** What tests get about a running suite. The tokens are fakes made up per run. */
export interface SuiteInfo {
  name: string;
  serve: ServeInfo;
  /** `<project>/.rayfin-local/state.json` once serve was ready. */
  state: { publishableKey?: string; apiPort?: number; frontendPort?: number };
  user: { email: string; password: string };
  tokens: Record<string, string>;
  mockPowerBiUrl?: string;
}

export interface RunningSuite {
  info: SuiteInfo;
  stop(): Promise<void>;
}

export type SuiteStatus =
  | { status: 'starting' }
  | { status: 'ready'; info: SuiteInfo }
  | { status: 'failed'; error: string };

type Starter = () => Promise<RunningSuite>;

export class SuiteHost {
  readonly url: string;
  readonly #server: http.Server;
  readonly #starters: Record<string, Starter>;
  readonly #starting = new Map<string, Promise<RunningSuite>>();
  readonly #status = new Map<string, SuiteStatus>();
  /** Suites whose start error was reported in full once; later reports are one line. */
  readonly #reported = new Set<string>();

  private constructor(server: http.Server, url: string, starters: Record<string, Starter>) {
    this.#server = server;
    this.url = url;
    this.#starters = starters;
  }

  static async start(starters: Record<string, Starter>): Promise<SuiteHost> {
    let host: SuiteHost | undefined;
    const server = http.createServer((request, response) => {
      const name = /^\/suites\/([\w-]+)$/.exec(request.url ?? '')?.[1];
      if (!name || !(name in starters)) {
        response.writeHead(404).end();
        return;
      }
      if (request.method === 'POST') host!.#ensureStarted(name);
      const status = host!.#report(name);
      response.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(status));
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    host = new SuiteHost(server, `http://127.0.0.1:${(server.address() as AddressInfo).port}`, starters);
    return host;
  }

  #report(name: string): SuiteStatus {
    const status = this.#status.get(name) ?? { status: 'starting' };
    if (status.status !== 'failed') return status;
    if (!this.#reported.has(name)) {
      this.#reported.add(name);
      return status;
    }
    const firstLine = status.error.split('\n')[0];
    return { status: 'failed', error: `${firstLine} (the first test that needed it has the details)` };
  }

  #ensureStarted(name: string): void {
    if (this.#starting.has(name)) return;
    this.#status.set(name, { status: 'starting' });
    const started = this.#starters[name]();
    this.#starting.set(name, started);
    started.then(
      (suite) => this.#status.set(name, { status: 'ready', info: suite.info }),
      (error: unknown) =>
        this.#status.set(name, {
          status: 'failed',
          error: error instanceof Error ? error.message : String(error),
        }),
    );
  }

  /** Stops every suite that started (even those still starting); throws all their errors. */
  async stopAll(): Promise<void> {
    this.#server.close();
    const errors: string[] = [];
    for (const [name, started] of this.#starting) {
      const suite = await started.catch(() => null);
      if (!suite) continue;
      await suite.stop().catch((error: unknown) => errors.push(`${name}: ${String(error)}`));
    }
    if (errors.length) throw new Error(`Stopping the e2e suites failed:\n${errors.join('\n')}`);
  }
}

/** Starts the suite `name` in the runner (once) and waits until it is ready. Call from workers. */
export async function suite(name: string, timeoutMs = 15 * 60_000): Promise<SuiteInfo> {
  const control = process.env.RAYFIN_LOCAL_E2E_CONTROL;
  if (!control) throw new Error('RAYFIN_LOCAL_E2E_CONTROL is not set: run these tests with `npx playwright test`.');
  const url = `${control}/suites/${name}`;
  const deadline = Date.now() + timeoutMs;
  let status = (await (await fetch(url, { method: 'POST' })).json()) as SuiteStatus;
  while (status.status === 'starting') {
    if (Date.now() > deadline) throw new Error(`The ${name} suite did not start within ${timeoutMs / 1000} s.`);
    await new Promise((resolve) => setTimeout(resolve, 500));
    status = (await (await fetch(url)).json()) as SuiteStatus;
  }
  if (status.status === 'failed') throw new Error(status.error);
  return status.info;
}
