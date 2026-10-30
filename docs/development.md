# Developing rayfin-local

This page covers building, testing and releasing rayfin-local.
[`AGENTS.md`](../AGENTS.md) has the conventions in full, and [architecture.md](architecture.md) explains how the crates fit together.

## Prerequisites

- Rust 1.96 (`rustup toolchain install 1.96.0`), with `rustfmt` and `clippy`.
- Node.js 20 or later (CI uses 22) and npm.
- Optional, for the tests that use them:
  - the reference app, `fabric-apps/apps/explorer`, checked out next to this repository with its packages installed (`npm ci`): it provides the Rayfin SDK 1.36.2;
  - Playwright's Chromium (`npx playwright install chromium`), or an installed Microsoft Edge or Chrome;
  - a signed-in Azure CLI (`az login`).

No C toolchain beyond the platform's usual one is needed: TLS is rustls with ring, and SQLite is bundled.

## Build and run

```sh
cargo build --workspace
cargo run -p rayfin-local -- serve -C <project>
```

To try it on the fixture app:

```sh
cd tests/fixtures/apps/basic
npm ci
npm ci --prefix rayfin/functions
cd ../../../..
cargo run -p rayfin-local -- serve -C tests/fixtures/apps/basic
```

`-v` shows rayfin-local's debug logs, and `RUST_LOG` (for example `rl_graphql=trace`) replaces the default filters.

## Lint and format

CI runs these on every push to `main` and every pull request, with Rust 1.96.0:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

While other work is in progress, lint and format one crate: `cargo clippy -p <crate> --all-targets -- -D warnings` and `cargo fmt -p <crate>`.

## Tests

```sh
cargo test --workspace     # everything; CI runs this on Windows, Linux and macOS
cargo test -p <crate>      # one crate
```

Unit tests sit next to the code (`#[cfg(test)]`); integration tests are in `crates/<crate>/tests/`.

### Tests that use Node.js and the real SDK

Several integration tests drive the real Rayfin SDK 1.36.2, or the project's own packages, with Node.js against rayfin-local's routes.
They print a `skipped: …` note and pass when Node.js or the packages they need are missing, so `cargo test --workspace` works on any machine.

| Test | What it checks | Needs |
| --- | --- | --- |
| `rl-graphql` `tests/sdk.rs` | The SDK's data client against the GraphQL API, signed in through `rl-auth`, on the reference app's model. | The SDK |
| `rl-connectors` `tests/sdk_decode.rs` | The SDK's `fabricSemanticModel()` through rayfin-local, compared with its direct path to the same mock Power BI. | The SDK |
| `rl-storage` `tests/sdk.rs` | The SDK's `StorageFolderClient` against the storage routes. | `npm install` in `crates/rl-storage/tests/sdk` |
| `rl-model` `tests/analyze_js.rs` | `js/analyze.mjs` on the fixture app, against the golden analysis; in connectors mode on the `connectors` fixture app. | `npm install` in `tests/fixtures/apps/basic` and `tests/fixtures/apps/connectors` |
| `rl-connectors` `tests/sql_sdk.rs` | The SDK's Category A connector client (`createConnectorsApi` with the entity classes, for a SQL database, a Warehouse and a SQL analytics endpoint) through rayfin-local on a real SQL Server. | `npm install` in `tests/fixtures/apps/connectors`, and [SQL Server](#sql-server-tests) |
| `rl-functions` `tests/host.rs`, `tests/invoke.rs` | The function host and the invoke routes on `tests/fixtures/functions/basic`, which they install and build on first use; also the reference app's functions when present. | Node.js and npm |
| `rl-frontend` `tests/vite.rs` | The Vite dev server and the static build, on copies of the reference app and small Vite apps; automatic sign-in answered before `rayfinLocalDev()`'s, on the fixture app and the apps in `RAYFIN_LOCAL_TEST_VITE_APPS`. | The reference app; `npm ci` in `tests/fixtures/apps/basic` |
| `rayfin-local` `tests/host_contract.rs` | `serve --json` of the fixture app ([embedding.md](embedding.md)): its events from `starting` to `ready`, the app and the backend answering, and a clean stop when stdin closes. | `npm ci` in `tests/fixtures/apps/basic` |
| `rayfin-local` `tests/control_api.rs` | The control API and automatic sign-in of the fixture app under `serve --json`: status, users, identities, the dev server's session token used on `/graphql`, rows, a reload and a data reset. | `npm ci` in `tests/fixtures/apps/basic` |
| `rayfin-local` `tests/live_reload.rs` | `serve --json` of a copy of the fixture app (its `node_modules` linked): `rayfin.yml` edits applied on the same ports, `secret set` restarting only the function host, Vite started again after it is killed, and the control API, `POST /reload` (refused and forced schema changes, invalid files) and automatic sign-in across changes. | `npm ci` in `tests/fixtures/apps/basic` and its `rayfin/functions` |

These variables point the tests at what they need:

| Variable | Default |
| --- | --- |
| `RAYFIN_SDK_NODE_MODULES` | `../fabric-apps/apps/explorer/node_modules`: a `node_modules` with the Rayfin SDK 1.36.2. |
| `RAYFIN_LOCAL_EXPLORER` | `../fabric-apps/apps/explorer`: the reference app, for `rl-frontend`. |
| `RAYFIN_LOCAL_EXPLORER_FUNCTIONS` | `../fabric-apps/apps/explorer/rayfin/functions`: the reference app's functions, for `rl-functions`. |
| `RAYFIN_APPS_DIR` | `../fabric-apps/apps`: every app's `rayfin.yml`, for `rl-project`'s ignored test. |
| `RAYFIN_LOCAL_TEST_VITE_APPS` | None: more apps with their packages installed (a list of folders, separated as in `PATH`), for `rl-frontend`'s automatic sign-in test, such as an app on another Vite version. |
| `RAYFIN_LOCAL_NODE` | `node` on `PATH`. |

The defaults are relative to this repository's folder: they expect `fabric-apps` checked out beside it.

### SQL Server tests

`rl-connectors`' `sql_server.rs` and `sql_sdk.rs` run the SQL connectors against a real SQL Server, and print a `skipped: …` note and pass without one.
Each test creates its own database and drops it at the end.
Start SQL Server 2022 in Docker, then point the tests at it with `RAYFIN_LOCAL_TEST_MSSQL` (`host,port;user;password`):

```sh
docker run -d --name rayfin-local-mssql -p 127.0.0.1:14330:1433 \
  -e ACCEPT_EULA=Y -e "MSSQL_SA_PASSWORD=<a strong password>" \
  mcr.microsoft.com/mssql/server:2022-latest
RAYFIN_LOCAL_TEST_MSSQL="127.0.0.1,14330;sa;<the password>" \
  cargo test -p rl-connectors --test sql_server --test sql_sdk
docker rm -f rayfin-local-mssql
```

SQL Server takes a few seconds to accept logins after the container starts.
`sql_sdk.rs` also needs the `connectors` fixture's packages (`npm ci` in `tests/fixtures/apps/connectors`).
CI's `sql-connectors` job runs both on Linux, with a SQL Server service container.

### Ignored tests

Tests that need a signed-in Azure CLI, the network or a browser are `#[ignore]`d.
Run them explicitly; they print no tokens and no identity.

| Command | What it does |
| --- | --- |
| `cargo test -p rl-auth --test sdk -- --ignored` | The SDK's auth flows: sign-up, sign-in, refresh, JWKS, sign-out, 401 then refresh then retry. |
| `cargo test -p rl-auth --test portal_e2e -- --ignored --nocapture` | The sign-in popup in a real browser with the SDK (see [`tests/e2e/portal/README.md`](../tests/e2e/portal/README.md)). |
| `cargo test -p rl-auth --test entra_smoke -- --ignored --nocapture` | The Entra exchange with your real Power BI token. |
| `cargo test -p rl-tokens --test smoke -- --ignored --nocapture` | Tokens from your Azure CLI. |
| `cargo test -p rl-connectors --test real_power_bi -- --ignored --nocapture` | A query against the reference app's semantic model in the real Power BI, as you. |
| `cargo test -p rl-connectors --test real_fabric_sql -- --ignored --nocapture` | A read-only query of `INFORMATION_SCHEMA.TABLES` in a real Fabric SQL database, as you; `RAYFIN_LOCAL_REAL_SQL="<workspace id>;<item id>[;<connector type>]"` picks the item. |
| `cargo test -p rl-functions --test invoke -- --ignored` | The reference app's functions with your tokens. |
| `cargo test -p rl-project --test rayfin_yml -- --ignored --nocapture` | Parsing every app's `rayfin.yml` in the `fabric-apps` repository (`RAYFIN_APPS_DIR` points at its `apps` folder). |

### JS helper tests

`js/test/` tests the helpers with Node's test runner.
The session stamp's tests need nothing else: `node --test js/test/session-stamp.test.mjs`.
The function host's tests need the fixture functions package installed and built first:

```sh
cd tests/fixtures/functions/basic
npm ci
npm run build
cd ../../../..
node --test js/test/function-host.test.mjs
```

### End-to-end tests

`tests/e2e/` runs `rayfin-local serve` on real apps and drives them with Playwright:

```sh
cd tests/e2e
npm ci
npx playwright install chromium   # or set PLAYWRIGHT_CHANNEL=msedge (or chrome)
npm run test:fixture              # the fixture app, with fake tokens and a mock Power BI
npm run test:explorer             # the reference app, with your real tokens (az login)
```

- The run builds rayfin-local first (`cargo build -p rayfin-local`); `RAYFIN_LOCAL_BIN` uses another binary, and `RAYFIN_LOCAL_SKIP_BUILD=1` skips the build.
- Each suite starts `serve` with `--reset` on free ports, and installs the fixture app's packages when they are missing.
- The reference app comes from `EXPLORER_DIR`, else `../fabric-apps/apps/explorer`; run `npm ci` in it first.
  Its suite puts back the app's own `.rayfin-local/` and checks that git sees no new changes in the app.
- `HEADED=1` shows the browser.
- Logs go to `test-results/logs/`, and traces and screenshots of failures to `test-results/`.

### Fixtures

| Fixture | Used for |
| --- | --- |
| `tests/fixtures/apps/basic` | The end-to-end app: every service, with one button per scenario. Its [README](../tests/fixtures/apps/basic/README.md) lists the scenarios and how to regenerate its generated files. |
| `tests/fixtures/apps/connectors` | An app with one SQL connector, `inventory`: entity classes, `metadata.json` and `@role` policies, for the connector analysis and the SQL Server tests. |
| `tests/fixtures/functions/basic` | A functions package for the function host's tests. |
| `crates/rl-model/tests/fixtures/*.analysis.json` | Golden analyses of the fixture app and the reference app. |
| `crates/rl-model/tests/fixtures/inventory.connector.json` | The golden connector analysis of the `connectors` fixture app. |
| `crates/rl-functions/tests/fixtures/` | Small functions packages for build and host edge cases. |

## Conventions

The essentials of [`AGENTS.md`](../AGENTS.md):

- **Rust:** edition 2024, Rust 1.96.
  Shared dependency versions live in the root `[workspace.dependencies]`, every crate has `[lints] workspace = true`, and `cargo fmt` (100 columns) and `cargo clippy -D warnings` must pass.
- **TLS** is rustls with ring only, never aws-lc-rs.
  Tests that build a `reqwest` client install the provider first: `let _ = rustls::crypto::ring::default_provider().install_default();`.
- **Crypto:** `ring` for ES256, RS256 verification, SHA-256, HMAC and random numbers; `argon2` for passwords.
- **Crate contracts:** feature crates take `Arc<dyn CallerResolver>` and `Arc<dyn TokenSource>`, and expose an `axum::Router` with its state applied.
- **Errors are actionable:** say what failed and what to do.
- **Privacy:** never log tokens, passwords, secrets or Entra tokens, and keep identity claims out of info-level logs.
- **Local state:** everything rayfin-local writes goes under `<project>/.rayfin-local/`.
- **Rayfin behavior:** check the SDK 1.36.2 dist, not memory; `AGENTS.md` lists the sources of truth.
- **Tests:** focused tests for the behavior you add; never test constants or framework behavior.
- **JS helpers:** plain ESM with no dependencies of their own; see [`js/README.md`](../js/README.md).
- **Docs:** one sentence per line.

## Releasing

1. Set the new version in `[workspace.package]` of the root `Cargo.toml`, and commit (with the updated `Cargo.lock`).
2. Tag the commit `v<version>` (for example `v0.2.0`, or `v0.2.0-rc.1` for a prerelease) and push the tag.

The [release workflow](../.github/workflows/release.yml) then:

- checks that the tag matches the crate version;
- builds `rayfin-local` with Rust 1.96.0 for `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin` and `x86_64-apple-darwin` (the Linux builds run on Ubuntu 22.04, which sets their glibc requirement, and print it);
- packages each binary with `README.md` and `LICENSE` as `rayfin-local-<version>-<target>.zip` (Windows) or `.tar.gz`;
- publishes a GitHub Release with the archives, `SHA256SUMS.txt`, `install.sh` and `install.ps1`, marked as a prerelease when the tag has a `-` suffix;
- installs the published release with `install.sh` on Linux (x64 and ARM64) and macOS, and with `install.ps1` on Windows (PowerShell 7, and Windows PowerShell 5.1 through `irm | iex`), and checks the installed version. For a final release, it also checks that the scripts install it as the latest release.

The README's one-line installers fetch `install.sh` and `install.ps1` from the `main` branch, and those install the latest release (prereleases excluded) unless `RAYFIN_LOCAL_VERSION` is set.
Keep the scripts compatible with the releases they may install: they expect the archive names above and `SHA256SUMS.txt`.

To try the scripts before a release, run them against an existing one: `RAYFIN_LOCAL_VERSION=<version> RAYFIN_LOCAL_INSTALL_DIR=/tmp/rl sh install.sh`, or in PowerShell set `$env:RAYFIN_LOCAL_VERSION`, `$env:RAYFIN_LOCAL_INSTALL_DIR` and `$env:RAYFIN_LOCAL_NO_MODIFY_PATH = '1'`, then run `./install.ps1`.
Lint them with `shellcheck --shell=sh install.sh` and [actionlint](https://github.com/rhysd/actionlint) for the workflows.
