// rayfin-local's Vite runner: starts the app's own Vite dev server (the project's `vite` package,
// its vite.config and plugins) with extra plugins: `rayfin-local:shim` and, with a sign-in secret,
// `rayfin-local:sign-in`.
//
// Protocol (owner: crates/rl-frontend/src/dev.rs):
//   node vite-dev.mjs --root <frontend dir> --host <host> --port <port> --backend <origin>
//     cwd: the frontend dir. The Rayfin env (RAYFIN_PUBLIC_*, VITE_*) comes in the process env,
//     which wins over the app's .env files in Vite's loadEnv and in rayfinLocalDev().
//     RAYFIN_LOCAL_SIGN_IN_SECRET: the backend's automatic sign-in secret; removed from the env
//     before Vite loads the app's config.
//   stdout `@@rayfin-local:vite-ready {"urls", "local", "network", "version", "rayfinPlugin"}`
//     once the server listens; Vite's own URL banner follows.
//   stdout `@@rayfin-local:vite-error {"code", "message", "stack"}` when the server can't start;
//     the helper then exits 1.
//   SIGINT, SIGTERM, SIGBREAK (Ctrl+Break on Windows): close the server and exit 0.
//
// The shim plugin always proxies /.well-known/* (the SDK validates shared sessions with
// <apiUrl>/.well-known/jwks.json) and /.rayfin-local/* to the backend. When the app doesn't use
// `rayfinLocalDev()` from @microsoft/rayfin-local-dev, the shim also does what that plugin does for
// a local backend: it serves /rayfin.config.json (apiUrl = this dev server's origin) and proxies
// /api, /graphql, /connector-invoke, /connectors and /functions to the backend.
//
// The sign-in plugin answers GET /.rayfin/dev/session-token, which pages of apps with
// `rayfinLocalDev({ autoLogin: true })` ask for a session (`fetchRayfinLocalSessionToken()`), before
// rayfinLocalDev()'s own middleware, which would sign in with Entra through the Rayfin CLI. It
// answers with the backend's GET /.rayfin-local/dev/session-token: a session for rayfin-local's
// sign-in identity, or `{error, hint}`.

import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const MARKER = '@@rayfin-local:';
const RAYFIN_PLUGIN = 'rayfin-local-dev';
const ALWAYS_PROXIED = '^/(?:\\.well-known|\\.rayfin-local)(?:/|$)';
const BACKEND_PROXIED = '^/(?:api|graphql|connector-invoke|connectors|functions)(?:/|$)';
const CONFIG_PATH = '/rayfin.config.json';
const CONFIG_FIELDS = [
  ['publishableKey', 'VITE_RAYFIN_PUBLISHABLE_KEY'],
  ['workspaceId', 'VITE_FABRIC_WORKSPACE_ID'],
  ['itemId', 'VITE_FABRIC_ITEM_ID'],
  ['portalUrl', 'VITE_FABRIC_PORTAL_URL'],
  ['tenantId', 'VITE_FABRIC_TENANT_ID'],
];
const BACKEND_UNAVAILABLE = JSON.stringify({ error: 'The local Rayfin backend is unavailable.' });
const SESSION_TOKEN_PATH = '/.rayfin/dev/session-token';
const BACKEND_SESSION_TOKEN_PATH = '/.rayfin-local/dev/session-token';
const SIGN_IN_SECRET_VAR = 'RAYFIN_LOCAL_SIGN_IN_SECRET';
const SIGN_IN_SECRET_HEADER = 'x-rayfin-local-sign-in';
// Signing in as the developer may run the Azure CLI first.
const SIGN_IN_TIMEOUT_MS = 120_000;
// Announcements are single stdout lines, which the reader splits beyond 64 KiB.
const ANNOUNCED_TEXT_MAX = 8 * 1024;
const STOP_DEADLINE_MS = 2500;

// Taken before the app's config and plugins load, so none of them sees it.
const signInSecret = process.env[SIGN_IN_SECRET_VAR] || undefined;
delete process.env[SIGN_IN_SECRET_VAR];

let server;
let stopping = false;

function announce(kind, payload) {
  process.stdout.write(`${MARKER}${kind} ${JSON.stringify(payload)}\n`);
}

/** Exits once everything written so far has been handed to the OS (pipes are async on POSIX). */
function exitAfterFlush(code) {
  setTimeout(() => process.exit(code), 2000).unref();
  let pending = 2;
  const done = () => {
    pending -= 1;
    if (pending === 0) process.exit(code);
  };
  process.stdout.write('', done);
  process.stderr.write('', done);
}

class StartError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

function describeError(error) {
  const clip = (text) =>
    typeof text === 'string' && text.length > ANNOUNCED_TEXT_MAX ? `${text.slice(0, ANNOUNCED_TEXT_MAX)}…` : text;
  if (error instanceof Error) {
    const code = typeof error.code === 'string' ? error.code : null;
    return { code, message: clip(error.message), stack: clip(error.stack) ?? null };
  }
  return { code: null, message: clip(String(error)), stack: null };
}

function parseArgs(argv) {
  const options = {};
  for (let i = 0; i < argv.length; i += 2) {
    const name = argv[i];
    if (!name.startsWith('--') || argv[i + 1] === undefined) {
      throw new StartError('USAGE', `vite-dev.mjs: unexpected argument ${name}`);
    }
    options[name.slice(2)] = argv[i + 1];
  }
  for (const required of ['root', 'host', 'port', 'backend']) {
    if (!options[required]) throw new StartError('USAGE', `vite-dev.mjs: --${required} is required`);
  }
  const port = Number(options.port);
  if (!Number.isInteger(port) || port < 0 || port > 65535) {
    throw new StartError('USAGE', `vite-dev.mjs: invalid --port ${options.port}`);
  }
  return {
    root: path.resolve(options.root),
    host: options.host,
    port,
    backend: options.backend.replace(/\/+$/, ''),
  };
}

/** The project's own Vite (hoisted workspace node_modules included): its version and ESM entry. */
function resolveVite(root) {
  const require = createRequire(path.join(root, 'package.json'));
  let manifestPath;
  try {
    manifestPath = require.resolve('vite/package.json');
  } catch {
    throw new StartError('VITE_NOT_FOUND', `Vite is not installed in ${root}.`);
  }
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  const entry = esmEntry(manifest.exports) ?? manifest.module ?? manifest.main ?? 'index.js';
  return {
    version: String(manifest.version ?? 'unknown'),
    url: pathToFileURL(path.join(path.dirname(manifestPath), entry)).href,
  };
}

/** The `import` entry of `exports['.']`: Vite 5 and 6 also export a deprecated CJS build. */
function esmEntry(exports) {
  let target = exports && typeof exports === 'object' && '.' in exports ? exports['.'] : exports;
  for (let depth = 0; target && typeof target === 'object' && depth < 5; depth++) {
    target = Array.isArray(target) ? target[0] : (target.import ?? target.node ?? target.default);
  }
  return typeof target === 'string' ? target : undefined;
}

async function flattenPlugins(plugins) {
  const flat = [];
  const visit = async (item) => {
    const value = await item;
    if (Array.isArray(value)) {
      for (const child of value) await visit(child);
    } else if (value) {
      flat.push(value);
    }
  };
  await visit(plugins ?? []);
  return flat;
}

/** What `rayfinLocalDev()` serves as /rayfin.config.json, from the injected env. */
function runtimeConfig(origin) {
  const config = { apiUrl: origin };
  for (const [field, name] of CONFIG_FIELDS) {
    const value = process.env[name]?.trim();
    if (value) config[field] = value;
  }
  return config;
}

function rayfinLocalShim({ backend }, state) {
  const proxyToBackend = {
    target: backend,
    changeOrigin: true,
    configure(proxy) {
      proxy.on('error', (_error, _request, response) => {
        // WebSocket upgrades pass the socket instead of a response.
        if (!('writeHead' in response)) {
          response.destroy();
          return;
        }
        if (!response.headersSent) response.writeHead(502, { 'content-type': 'application/json' });
        if (!response.writableEnded) response.end(BACKEND_UNAVAILABLE);
      });
    },
  };
  return {
    name: 'rayfin-local:shim',
    apply: 'serve',
    // Detected here rather than in configResolved: the proxy table must be final before Vite
    // resolves the config. The app's plugins come first, so their proxy contexts match first.
    async config(config) {
      const plugins = await flattenPlugins(config.plugins);
      state.rayfinPlugin = plugins.some((plugin) => plugin.name === RAYFIN_PLUGIN && plugin.apply !== 'build');
      const proxy = { [ALWAYS_PROXIED]: proxyToBackend };
      if (!state.rayfinPlugin) proxy[BACKEND_PROXIED] = proxyToBackend;
      return { server: { proxy } };
    },
    configResolved(resolved) {
      if (!state.rayfinPlugin) {
        resolved.logger.info(
          'rayfin-local: the Vite config has no rayfinLocalDev() plugin; serving /rayfin.config.json ' +
            'and proxying the Rayfin routes to the local backend.',
        );
      }
    },
    configureServer(devServer) {
      if (state.rayfinPlugin) return;
      devServer.middlewares.use(CONFIG_PATH, (request, response, next) => {
        const host = request.headers.host;
        if (request.method !== 'GET' || !host) {
          next();
          return;
        }
        const protocol = request.socket.encrypted ? 'https:' : 'http:';
        response.setHeader('content-type', 'application/json');
        response.setHeader('cache-control', 'no-store');
        response.end(JSON.stringify(runtimeConfig(`${protocol}//${host}`)));
      });
    },
  };
}

function rayfinLocalSignIn({ backend }, secret) {
  return {
    name: 'rayfin-local:sign-in',
    apply: 'serve',
    // `configureServer` hooks run in plugin order, which puts `pre` plugins before the app's, so
    // this middleware comes before rayfinLocalDev()'s. Vite's host check (DNS rebinding) and CORS
    // run before any plugin's middleware.
    enforce: 'pre',
    configureServer: {
      order: 'pre',
      handler(devServer) {
        devServer.middlewares.use(SESSION_TOKEN_PATH, (request, response) => {
          answerSessionToken(request, response, backend, secret).catch(() => {
            if (!response.writableEnded) response.destroy();
          });
        });
      },
    },
  };
}

/** A session from the backend, for the page of this dev server that asks; or why not. */
async function answerSessionToken(request, response, backend, secret) {
  const reply = (status, body) => {
    response.statusCode = status;
    response.setHeader('content-type', 'application/json');
    response.setHeader('cache-control', 'no-store');
    response.end(typeof body === 'string' ? body : JSON.stringify(body));
  };
  if (request.method !== 'GET') {
    response.setHeader('allow', 'GET');
    reply(405, { error: 'Method not allowed.' });
    return;
  }
  if (!isLoopbackAddress(request.socket.remoteAddress)) {
    reply(403, { error: 'Local sign-in is only available from the Vite host.' });
    return;
  }
  if (!isSameOrigin(request)) {
    reply(403, { error: 'Cross-origin local sign-in requests are not allowed.' });
    return;
  }
  let answer;
  let body;
  try {
    answer = await fetch(`${backend}${BACKEND_SESSION_TOKEN_PATH}`, {
      headers: { accept: 'application/json', [SIGN_IN_SECRET_HEADER]: secret },
      redirect: 'error',
      signal: AbortSignal.timeout(SIGN_IN_TIMEOUT_MS),
    });
    body = await answer.text();
  } catch {
    reply(502, {
      error: 'The local Rayfin backend is unavailable.',
      hint: 'Check that rayfin-local is still running, then reload.',
    });
    return;
  }
  reply(answer.status, body || { error: 'The local Rayfin backend sent an empty answer.' });
}

function isLoopbackAddress(address) {
  return (
    typeof address === 'string' &&
    (address === '::1' || /^127(?:\.\d{1,3}){3}$/.test(address) || /^::ffff:127(?:\.\d{1,3}){3}$/i.test(address))
  );
}

/** No `Origin` (a same-origin `GET`), or this dev server's own. */
function isSameOrigin(request) {
  const origin = request.headers.origin;
  if (!origin) return true;
  const host = request.headers.host;
  if (!host) return false;
  try {
    const protocol = request.socket.encrypted ? 'https:' : 'http:';
    return new URL(origin).origin === `${protocol}//${host}`;
  } catch {
    return false;
  }
}

async function stop() {
  if (stopping) return;
  stopping = true;
  setTimeout(() => process.exit(0), STOP_DEADLINE_MS).unref();
  try {
    await server?.close();
  } finally {
    process.exit(0);
  }
}

async function main() {
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGBREAK']) process.on(signal, stop);

  const state = { rayfinPlugin: false };
  let vite;
  try {
    const options = parseArgs(process.argv.slice(2));
    vite = resolveVite(options.root);
    const { createServer } = await import(vite.url);
    // No inline `root`: the cwd is the frontend dir, as with the `vite` CLI, so a `root` set in the
    // app's own config still applies.
    server = await createServer({
      mode: 'development',
      clearScreen: false,
      // rayfin-local opens the browser itself (`--open`); a browser Vite started would also be
      // stopped together with Vite's process tree.
      server: { host: options.host, port: options.port, strictPort: true, open: false },
      plugins: [
        rayfinLocalShim(options, state),
        ...(signInSecret ? [rayfinLocalSignIn(options, signInSecret)] : []),
      ],
    });
    await server.listen();
  } catch (error) {
    announce('vite-error', describeError(error));
    await server?.close().catch(() => {});
    exitAfterFlush(1);
    return;
  }
  if (stopping) return;

  const local = server.resolvedUrls?.local ?? [];
  const network = server.resolvedUrls?.network ?? [];
  announce('vite-ready', {
    urls: [...local, ...network],
    local,
    network,
    version: vite.version,
    rayfinPlugin: state.rayfinPlugin,
  });
  server.printUrls();
}

await main();
