# JavaScript helpers

These ES modules are compiled into the `rayfin-local` binary with `include_str!`.
Before one runs, `rl_node::Helper` writes it to `<project>/.rayfin-local/cache/js/<name>-<hash>.mjs`.
They run on the developer's own Node.js (20 or later), in a folder of the project (its root, the functions package or the frontend folder), so they can import the project's installed packages (`@microsoft/rayfin-core`, `typescript`, `@microsoft/rayfin-cli`, `vite`, ...).

| Helper | Owner crate | Kind | Purpose |
| --- | --- | --- | --- |
| `analyze.mjs` | `rl-model` | One-shot | Compile `rayfin/data` (and `rayfin/storage`) and run the project's `@microsoft/rayfin-core` analyzer, as `rayfin dev db apply` does; in connectors mode, analyze the SQL connectors' entities in `rayfin/connectors`, as `rayfin up` does. |
| `rayfin-token.mjs` | `rl-tokens` | One-shot | Mint a token with the developer's Rayfin CLI login (`@microsoft/rayfin-cli/auth`). |
| `udf-metadata.mjs` | `rl-functions` | One-shot | Generate `runtimemetadata.json` from the `udf.func(...)` declarations, byte for byte as the Rayfin CLI does. |
| `function-host.mjs` | `rl-functions` | Long-running: stdout announcements, then TCP JSON lines | Host the app's functions on the real UDF package, without Azure Functions Core Tools. |
| `vite-dev.mjs` | `rl-frontend` | Long-running: stdout announcements and signals | Run the app's own Vite dev server, with the `rayfin-local:shim` plugin that wires it to the local backend. |

## Rules

- Plain ESM (`.mjs`), with no dependencies of their own: only Node built-ins and the project's packages.
- Resolve project packages from the project, not from the helper's location, for example with `createRequire(path.join(projectRoot, 'package.json'))` or `import.meta.resolve` relative to the project.
- Never print tokens, secrets or passwords.
- Each file's header comment documents its protocol; keep it in sync with the Rust owner.

## One-shot helpers

They follow the `rl_node::run_helper` protocol: `node <helper> --input <in.json> --output <out.json>`.
The helper reads its JSON input from the `--input` file and writes exactly one JSON document to the `--output` file: `{"ok": true, "result": ...}` or `{"ok": false, "error": {"message": "...", "stack": "..."}}`.
Stdout and stderr are free for logs, since user modules may print while being imported.
Both files live in `.rayfin-local/cache/tmp/` and are deleted after the run.

| Helper | Input | Result |
| --- | --- | --- |
| `analyze.mjs` | `projectRoot`, `dataDir`, `storageDir?`, `tsconfigPath`, `outDir`, `dialect`, `storage`, `verbose?` | `coreVersion`, `typescriptVersion`, `dialect`, `featureFlags`, `entities`, `config`, `policies`, `storage`, `sources`, `diagnostics` |
| `analyze.mjs` with `mode: "connectors"` | `projectRoot`, `tsconfigPath`, `outDir`, `connectors` (`name`, `dir`, `dialect`), `verbose?` | `coreVersion`, `typescriptVersion`, `featureFlags`, `connectors` (`name`, `dialect`, then `entities`, `config` and `policies`, or `error`), `sources`, `diagnostics` |
| `rayfin-token.mjs` | `scopes`, `tenantId?` | `token`, `expiresOnTimestamp` (ms since the epoch), `tenantId?` |
| `udf-metadata.mjs` | `functionsDir`, `resolvePaths?`, `outputPath?` | `json`, `metadata`, `written`, `diagnostics`, `typescript` (`version`, `path`), `typeChecked`, `sourceFiles` |

## Long-running helpers

They announce themselves on stdout with single lines that start with `@@rayfin-local:` and carry a JSON payload.
The Rust owner reads those lines and passes every other line on to `serve`'s output, labeled (`[fn]`, `[vite]`).
Keep announcements short: the reader splits lines beyond 64 KiB.

### `function-host.mjs`

The owner, `crates/rl-functions/src/host.rs`, documents the protocol in full.

- `node function-host.mjs --entry <file>`, in the functions package directory.
- `RAYFIN_LOCAL_FUNCTION_HOST_SECRET` in the environment is the invoke socket's secret; the host removes it from `process.env`.
- Stdout `@@rayfin-local:ready {"port":N}` once the entry point loaded and the invoke socket listens, or `@@rayfin-local:load-error {"name","message","stack"}`, after which the host exits 1.
- Requests go over TCP to `127.0.0.1:<port>`: one request per connection, one JSON line each way, each request carrying the secret.
  - `{type: 'describe', secret}` → `{ok: true, programmingModel, functions}`
  - `{type: 'invoke', secret, invocationId, functionName, method, url, headers, body, bodyEncoding, bindings}` → `{ok: true, response: {status, headers: [[name, value]], body, bodyEncoding}}` or `{ok: false, error: {kind, name, message, stack}}`
  - `{type: 'shutdown', secret, drainMs}` → `{ok: true}`; in-flight invocations get `drainMs`, then the host exits 0.
- Bodies are UTF-8 text (`'utf8'`) or, when they are not valid UTF-8, one char per byte (`'latin1'`).
- The UDF package replaces `console.*` with loggers bound to the running invocation, so the host writes to `process.stdout` and `process.stderr` directly and never uses `console`.

### `vite-dev.mjs`

The owner is `crates/rl-frontend/src/dev.rs`.

- `node vite-dev.mjs --root <frontend dir> --host <host> --port <port> --backend <origin>`, in the frontend directory.
- The Rayfin environment (`RAYFIN_PUBLIC_*`, `VITE_*`) comes in the process environment, which wins over the app's `.env` files in Vite's `loadEnv` and in `rayfinLocalDev()`.
- Stdout `@@rayfin-local:vite-ready {"urls","local","network","version","rayfinPlugin"}` once the server listens (Vite's own URL banner follows), or `@@rayfin-local:vite-error {"code","message","stack"}` when it can't start, after which the helper exits 1.
- SIGINT, SIGTERM and SIGBREAK (Ctrl+Break on Windows) close the server, and the helper exits 0.
- The `rayfin-local:shim` plugin always proxies `/.well-known/*` and `/.rayfin-local/*` to the backend.
  When the app doesn't use `rayfinLocalDev()` from `@microsoft/rayfin-local-dev`, the shim also does what that plugin does for a local backend: it serves `/rayfin.config.json` (with this dev server's origin as `apiUrl`) and proxies `/api`, `/graphql`, `/connector-invoke`, `/connectors` and `/functions` to the backend.

## Tests

`js/test/` holds tests run with Node's test runner.
The function host's tests need the fixture functions package installed and built (`npm ci` and `npm run build` in `tests/fixtures/functions/basic`), then: `node --test js/test/function-host.test.mjs`.
The owner crates' integration tests run the helpers too, for example `rl-model`'s `tests/analyze_js.rs`, `rl-functions`' `tests/host.rs` and `rl-frontend`'s `tests/vite.rs`.
