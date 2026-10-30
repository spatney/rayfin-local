# AGENTS.md: working in the rayfin-local repo

`rayfin-local` is a Rust CLI that runs an existing [Rayfin](https://rayfin.ai) project (SDK 1.36.x) entirely on the developer's machine.
`rayfin-local serve`, run in the project, starts every backend service locally and the unmodified app works against it through the Rayfin SDK.
Each service has a local stand-in:

- **Database:** SQLite.
- **Data API:** a DAB-compatible GraphQL API.
- **Auth:** username/password, with an emulated Fabric sign-in popup.
- **Functions:** the app's own UDF code on a Node host.
- **Connectors:** semantic models queried through Power BI, and SQL databases, warehouses and SQL analytics endpoints over TDS, with the developer's token.
- **Storage:** the local file system.

## Layout

| Path | Purpose |
| --- | --- |
| `crates/rayfin-local` | The binary: CLI (`clap`), `serve` orchestration and supervision. |
| `crates/rl-project` | Project discovery; `rayfin.yml`, `rayfin/.env`, `package.json`, `rayfin-local.toml`; the `.rayfin-local/` state folder. |
| `crates/rl-node` | Node discovery, embedded JS helpers (`run_helper`), supervised process trees (`ManagedChild`). |
| `crates/rl-model` | The app model (IR) from `@microsoft/rayfin-core`'s analyzer; `Caller`/`CallerResolver` (request identity). |
| `crates/rl-db` | The SQLite handle (`Db`), component migrations, entity DDL and schema migrations. |
| `crates/rl-graphql` | `POST /graphql`: DAB-compatible queries, mutations, aggregations and policies over SQLite; the same front end over T-SQL (`tsql`) for SQL connectors. |
| `crates/rl-auth` | `/api/auth/v1/*`, JWKS, runtime settings, the sign-in popup portal, the Entra exchange; implements `CallerResolver`. |
| `crates/rl-tokens` | `TokenSource`: Azure tokens for the developer (az CLI, Rayfin CLI login, env overrides); `Audience` scopes. |
| `crates/rl-functions` | Functions build/watch, `runtimemetadata.json`, the Node function host, `/functions/<name>/invoke`. |
| `crates/rl-connectors` | `/connector-invoke/<name>`: `fabric-semanticmodel` through Power BI; `/connectors/<name>/graphql`: SQL connectors over TDS (see `docs/sql-connectors.md`); 501 for other types. |
| `crates/rl-storage` | `/api/storage/*` on the local file system. |
| `crates/rl-frontend` | The project's Vite dev server or static build, with the local Rayfin env. |
| `crates/rl-server` | Router composition: `/rayfin.config.json`, CORS, publishable-key and Host checks, static hosting. |
| `js/` | JS helpers embedded in the binary (see `js/README.md`). |
| `tests/` | Fixture projects, SDK conformance suite (Node + real `@microsoft/rayfin-*` 1.36.2), e2e. |

## Commands

| Command | What it does |
| --- | --- |
| `cargo build --workspace` | Build everything. |
| `cargo test -p <crate>` | One crate's tests. Prefer this while other work is in progress. |
| `cargo test --workspace` | All Rust tests. |
| `cargo clippy -p <crate> --all-targets -- -D warnings` | Lint one crate (CI lints the workspace with `-D warnings`). |
| `cargo fmt -p <crate>` / `cargo fmt --all --check` | Format (100 columns). |
| `cargo run -p rayfin-local -- serve -C <project>` | Run against a project. |

## Sources of truth for Rayfin behavior

Do not rely on memory for Rayfin APIs or wire formats; they are version-locked (1.36.2).
These are the sources of truth:

The paths below are relative to this repository's parent folder, where the related repositories are checked out side by side.

- The SDK 1.36.2 dist installed in the reference app: `fabric-apps/apps/explorer/node_modules/@microsoft/rayfin-*` (and `rayfin/functions/node_modules/@microsoft/fabric-user-data-functions`).
- The wire contract written up by micro-fabric: `micro-fabric/docs/compatibility.md`.
  It is a reference only; this project is a clean-room implementation, so never copy micro-fabric code.
- Rayfin's source (1.37 alpha; may differ from 1.36.2): `project-rayfin/packages/*`.
- The docs: `https://rayfin.ai/docs/...`; append `.md` to any page for Markdown.

The reference app for end-to-end checks is `fabric-apps/apps/explorer`.
Never modify files in it, except generated artifacts that `rayfin dev` would also regenerate.

## Conventions

- **Rust:** edition 2024, Rust 1.96.
  Shared dependency versions live in the root `[workspace.dependencies]`, and crates use `dep.workspace = true`.
  Every crate has `[lints] workspace = true`.
  `cargo fmt` and `cargo clippy -D warnings` must pass.
- **TLS is rustls with ring only.** Never aws-lc-rs.
  `reqwest` uses `rustls-no-provider`, and the binary installs ring as rustls' default provider at startup.
  Tests that build a `reqwest` client install it too: `let _ = rustls::crypto::ring::default_provider().install_default();`.
- **Crypto:** `ring` for ES256, RS256 verification, SHA-256, HMAC and random; `argon2` for passwords.
- **Contracts between crates:**
  - Feature crates take `Arc<dyn rl_model::CallerResolver>` and `Arc<dyn rl_tokens::TokenSource>`, never concrete auth or token types.
    Test them with fakes.
  - Each feature crate exposes an `axum::Router` (state already applied); `rl-server` merges them.
- **Errors are actionable:** say what failed and what to do (for example, "run `az login`").
- **Privacy:** never log access tokens, refresh tokens, passwords, secrets or Entra tokens.
  Keep identity claims (email, oid) out of info-level logs.
- **Local state:** everything rayfin-local writes goes under `<project>/.rayfin-local/`.
  That folder has its own `.gitignore` containing `*`.
- **Tests:** focused tests for the behavior you add.
  Unit tests sit next to the code (`#[cfg(test)]`); integration tests go in `crates/<crate>/tests/`.
  Never test constants or framework behavior.
- **Comments:** only for non-obvious reasoning.
- **Git:** do not commit; the coordinator commits at milestones.
