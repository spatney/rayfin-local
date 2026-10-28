import { readFileSync } from 'node:fs';
import http from 'node:http';
import type { AddressInfo } from 'node:net';

import { ROW_ARROWS } from './paths.ts';

/*
 * A stand-in for the Power BI REST API's dataset queries, for the `fabric-semanticmodel`
 * connector (`[connectors] powerbi_base_url` in rayfin-local.toml):
 *
 * - POST /v1.0/myorg[/groups/{workspace}]/datasets/{item}/executeDaxQueries answers the Arrow stream
 *   rl-connectors recorded for `EVALUATE ROW("x", 1)` (tests/fixtures/row.arrows), whatever the query.
 * - POST .../executeQueries (the JSON API rayfin-local falls back to for Azure CLI tokens) answers
 *   Power BI's JSON for the same result.
 * - A bearer token other than the expected one gets Power BI's 401.
 * - GET /__mock/requests lists the requests it received (tests run in other processes).
 */

export interface RecordedRequest {
  method: string;
  path: string;
  /** Whether `Authorization` was `Bearer <the expected token>`; the token itself isn't kept. */
  tokenMatches: boolean;
  hasAuthorization: boolean;
  /** The DAX query from `query` or `queries[0].query`. */
  query: string | null;
  body: unknown;
}

const DATASET_QUERY = /^\/v1\.0\/myorg(?:\/groups\/([^/]+))?\/datasets\/([^/]+)\/(executeDaxQueries|executeQueries)$/;
const UNAUTHORIZED = {
  error: { code: 'Unauthorized', 'pbi.error': { code: 'Unauthorized', parameters: {}, details: [], exceptionCulprit: 1 } },
};
const NOT_FOUND = { error: { code: 'NotFound', message: 'rayfin-local e2e mock: no such Power BI route' } };

export class MockPowerBi {
  readonly baseUrl: string;
  readonly #server: http.Server;
  readonly #requests: RecordedRequest[] = [];

  private constructor(server: http.Server, baseUrl: string) {
    this.#server = server;
    this.baseUrl = baseUrl;
  }

  /** Listens on 127.0.0.1 (a free port unless `port` is given). */
  static async start(expectedToken: string, port = 0): Promise<MockPowerBi> {
    const arrow = readFileSync(ROW_ARROWS);
    let mock: MockPowerBi | undefined;
    const server = http.createServer((request, response) => {
      const chunks: Buffer[] = [];
      request.on('data', (chunk: Buffer) => chunks.push(chunk));
      request.on('end', () => mock!.#handle(request, Buffer.concat(chunks), response, expectedToken, arrow));
    });
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject);
      server.listen(port, '127.0.0.1', resolve);
    });
    const address = server.address() as AddressInfo;
    mock = new MockPowerBi(server, `http://127.0.0.1:${address.port}`);
    return mock;
  }

  requests(): RecordedRequest[] {
    return [...this.#requests];
  }

  async stop(): Promise<void> {
    this.#server.closeAllConnections();
    await new Promise<void>((resolve) => this.#server.close(() => resolve()));
  }

  #handle(
    request: http.IncomingMessage,
    raw: Buffer,
    response: http.ServerResponse,
    expectedToken: string,
    arrow: Buffer,
  ): void {
    const path = new URL(request.url ?? '/', 'http://mock').pathname;
    if (request.method === 'GET' && path === '/__mock/requests') {
      return send(response, 200, 'application/json', JSON.stringify(this.#requests));
    }
    let body: unknown = null;
    try {
      body = raw.length ? JSON.parse(raw.toString('utf8')) : null;
    } catch {
      body = raw.toString('utf8');
    }
    const authorization = request.headers.authorization;
    const tokenMatches = authorization === `Bearer ${expectedToken}`;
    this.#requests.push({
      method: request.method ?? '',
      path,
      tokenMatches,
      hasAuthorization: authorization !== undefined,
      query: daxQuery(body),
      body,
    });

    const route = DATASET_QUERY.exec(path);
    if (request.method !== 'POST' || !route) {
      return send(response, 404, 'application/json', JSON.stringify(NOT_FOUND));
    }
    if (!tokenMatches) {
      return send(response, 401, 'application/json; charset=utf-8', JSON.stringify(UNAUTHORIZED), 'e2e-mock-401');
    }
    if (route[3] === 'executeDaxQueries') {
      return send(response, 200, 'application/vnd.apache.arrow.stream', arrow, 'e2e-mock-row');
    }
    const json = {
      informationProtectionLabel: { id: '9fbde396', name: 'Internal Only' },
      results: [{ tables: [{ rows: [{ '[x]': 1 }] }] }],
    };
    return send(response, 200, 'application/json; charset=utf-8', JSON.stringify(json), 'e2e-mock-json-row');
  }
}

function daxQuery(body: unknown): string | null {
  const value = body as { query?: unknown; queries?: { query?: unknown }[] } | null;
  const query = value?.query ?? value?.queries?.[0]?.query;
  return typeof query === 'string' ? query : null;
}

function send(
  response: http.ServerResponse,
  status: number,
  contentType: string,
  body: string | Buffer,
  requestId?: string,
): void {
  response.writeHead(status, {
    'content-type': contentType,
    ...(requestId ? { requestid: requestId } : {}),
  });
  response.end(body);
}
