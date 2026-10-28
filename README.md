# rayfin-local

Run a [Rayfin](https://rayfin.ai) app, including its whole backend, on your own machine:

```sh
cd my-rayfin-app
npm install
rayfin-local serve
```

`rayfin-local serve` reads your project (`rayfin/rayfin.yml`, the entities in `rayfin/data`, your functions and connectors) and starts a local stand-in for every Rayfin service:

| Service | Local stand-in |
| --- | --- |
| Database | SQLite, in `.rayfin-local/db.sqlite`. |
| Data API | A DAB-compatible GraphQL API with your `@role` policies. |
| Auth | Username/password users, and an emulated "Sign in with Fabric" popup. |
| Functions | Your Fabric User Data Functions, on the real `@microsoft/fabric-user-data-functions` package, on Node. |
| Connectors | `fabric-semanticmodel` queries, sent to Power BI with your own token. |
| Storage | Files in `.rayfin-local/storage`. |

It then starts your app's own Vite dev server, pointed at all of that.
Nothing is deployed to Fabric, and your source and `.env` files are not changed.

rayfin-local implements the Rayfin SDK 1.36.x contract.
It is in development; see [what differs from Fabric](docs/differences.md).

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [What runs where](#what-runs-where)
- [Signing in](#signing-in)
- [Identity and Azure tokens](#identity-and-azure-tokens)
- [Secrets](#secrets)
- [Local state and reset](#local-state-and-reset)
- [Schema changes](#schema-changes)
- [Configuration file](#configuration-file)
- [Commands and flags](#commands-and-flags)
- [Troubleshooting](#troubleshooting)
- [More documentation](#more-documentation)

## Install

Prebuilt binaries for Windows (x64), Linux (x64) and macOS (Apple silicon and Intel) are on the repository's GitHub Releases page, `https://github.com/<owner>/rayfin-local/releases`.
Each archive holds the `rayfin-local` binary, this README and the license; put the binary on your `PATH`.
`SHA256SUMS.txt` lists the archives' checksums: check yours with `sha256sum -c --ignore-missing SHA256SUMS.txt`, or `Get-FileHash <archive>` in PowerShell.
The macOS binaries are not notarized: if Gatekeeper blocks the binary, run `xattr -d com.apple.quarantine rayfin-local`.

With Rust 1.96 or later, build and install it with Cargo instead:

```sh
cargo install --locked --git https://github.com/<owner>/rayfin-local rayfin-local
```

From a clone of this repository: `cargo install --locked --path crates/rayfin-local`.

### Requirements

- **Node.js 20 or later**, on `PATH` (or set `RAYFIN_LOCAL_NODE` to the `node` executable).
  rayfin-local runs your project's own packages with it: `@microsoft/rayfin-core`, `vite`, `typescript` and your functions.
- **Your project's npm packages**: run `npm install` in the project, and in `rayfin/functions` when it has its own `package.json`.
- **Optional: a signed-in Azure CLI (`az login`) or Rayfin CLI (`npx rayfin login`)**, for functions that use `ctx.Tokens`, for semantic-model connectors, and for "Continue as" on the sign-in page.
  See [Identity and Azure tokens](#identity-and-azure-tokens).

## Quick start

```sh
cd my-rayfin-app
npm install
rayfin-local doctor   # optional: checks Node.js, packages, Azure sign-in and ports
rayfin-local serve
```

`serve` prints the API URL, the app's URL and how to sign in, then keeps running until you press Ctrl+C.
Open the app's URL (`http://localhost:5173` by default); signing in works as on Fabric, with local users (see [Signing in](#signing-in)).
Output from Vite, the functions and builds is prefixed with `[vite]`, `[fn]` and `[build]`.

Other ways to run it:

```sh
rayfin-local serve --static        # build the app once and serve it from the backend
rayfin-local serve --no-frontend   # only the backend, for your own dev server or tests
rayfin-local serve -C path/to/app  # a project other than the current directory
```

## What runs where

The backend has one origin, `http://localhost:7171` by default.
Its port is remembered per project; when it is busy, rayfin-local takes the next free one.

| Path | What it serves |
| --- | --- |
| `/rayfin.config.json` | The app's config: `apiUrl`, `publishableKey`, workspace, item and tenant ids, `portalUrl`. |
| `/api/auth/v1/*` | Auth: sign-up, username/password sign-in, refresh tokens, sign-out. |
| `/.well-known/jwks.json` | The public keys that verify access tokens. |
| `/api/projectRuntimeSettings` | The auth settings from `rayfin.yml`. |
| `/graphql` | The data API: DAB-compatible GraphQL over SQLite, with `@role` policies. |
| `/functions/<name>/invoke` | Your functions, on a Node host (no Azure Functions Core Tools). |
| `/connector-invoke/<name>` | Connectors: `fabric-semanticmodel` through Power BI's `executeDaxQueries`, or `executeQueries` for Azure CLI tokens. |
| `/api/storage/*` | Storage, on files under `.rayfin-local/storage`. |
| `/.rayfin-local/portal` | The local "Sign in with Fabric" page. |
| `/.rayfin-local/health` | `{"ok":true}` once the backend is up. |

As on Fabric, `/api/*`, `/graphql`, `/functions/*` and `/connector-invoke/*` need an `X-Publishable-Key` header; the local key is in `/rayfin.config.json`.

The app runs on its own Vite dev server, with your `vite.config` and plugins.
Its port is `RAYFIN_PUBLIC_FRONTEND_PORT` from `rayfin/.env`, else 5173.
rayfin-local starts Vite itself, never `npm run dev` (which may call `rayfin up` or `rayfin dev`).
Vite gets the local backend's values in its process environment, where they beat the deployed values in your `.env` files: `VITE_RAYFIN_API_URL`, `VITE_RAYFIN_PUBLISHABLE_KEY`, `VITE_FABRIC_PORTAL_URL`, the matching `RAYFIN_PUBLIC_*` variables, and your other `RAYFIN_PUBLIC_*` values.

With `--static`, rayfin-local builds the app once with `services.staticHosting.buildCommand` (default `npm run build`), with the same environment, and serves the output from the backend origin.

## Signing in

Users are local: they live in `.rayfin-local/db.sqlite`, not in Entra ID.

- **Username and password.** The app's own sign-up and sign-in work as on Fabric, with any username or email address.
- **"Sign in with Fabric".** `ensureSignedInWithFabric` opens rayfin-local's sign-in page (`/.rayfin-local/portal`) in the popup instead of the Fabric portal.
  There you can sign in with a local user, create one, or choose **Continue as &lt;you&gt;** to sign in as your own Entra user (the account of your `az` or Rayfin CLI login).
  "Continue as" is offered only when the backend listens on a loopback address.
- **The `email` claim** is set only when the username is an email address; "Continue as" sets it to your user principal name.
  Policies that compare `claims.email` need such a user.

`rayfin-local users` lists, adds and deletes local users and resets their passwords.
`rayfin-local sign-in-link` prints a one-time link that signs a user in.
Passwordless sign-in, email verification and password-reset emails are not available locally.

## Identity and Azure tokens

Deployed, functions get their tokens for the app's identity, and delegated connectors run as the signed-in user.
Locally, both run as **you, the developer**:

- `ctx.Tokens.<Audience>` in functions (`Sql`, `Storage`, `Fabric`, `AzureAI`, `ADO`) holds a token for your account.
- `fabric-semanticmodel` connectors query Power BI with your token (the app user must still be signed in).

So a function or query can reach exactly what your account can reach.

`--token-source` (or `[tokens] source` in `rayfin-local.toml`) chooses where tokens come from:

| Source | Tokens from |
| --- | --- |
| `auto` (default) | The Azure CLI, then the Rayfin CLI's login. |
| `az` | `az account get-access-token --scope <scope>`: run `az login` first. |
| `rayfin` | The Rayfin CLI's login, through the project's `@microsoft/rayfin-cli/auth`: run `npx rayfin login` first. It may open a browser window to sign in. |

`RAYFIN_LOCAL_TOKEN_<AUDIENCE>` beats every source for one audience: `SQL`, `STORAGE`, `FABRIC`, `AZUREAI`, `ADO` or `POWERBI`.
It is meant for tests and CI; the token is used as given.

Each audience has a default scope, which `[tokens.scopes]` in `rayfin-local.toml` can override:

| Audience | Default scope |
| --- | --- |
| `Sql` | `https://database.windows.net/.default` |
| `Storage` | `https://storage.azure.com/.default` |
| `Fabric` | `https://api.fabric.microsoft.com/.default` |
| `AzureAI` | `https://ai.azure.com/.default` |
| `ADO` | `499b84ac-1321-427f-aa17-267ca6975798/.default` |
| `PowerBI` (connectors, "Continue as") | `https://analysis.windows.net/powerbi/api/.default` |

`--tenant` (or `[tokens] tenant`) picks the Entra tenant; the default is your login's.
rayfin-local caches tokens in memory and never logs them.

## Secrets

Functions read the secrets declared in `rayfin.yml` as `ctx.Secrets.<NAME>`.
Locally, a secret's value comes from the `Values` of the functions package's `local.settings.json` (`rayfin/functions/local.settings.json`), from the environment, or from `.rayfin-local/secrets.json`.
`rayfin-local secret set`, `secret list` and `secret delete` manage `.rayfin-local/secrets.json`, which is never committed.

## Local state and reset

rayfin-local keeps its state in `.rayfin-local/` at the project root.
The folder has its own `.gitignore` (`*`), so none of it is committed.
Outside it, only build output is written: what your build commands produce (such as `dist/`) and the functions' `runtimemetadata.json`, which `rayfin dev` also generates.

| Path | Holds |
| --- | --- |
| `db.sqlite` | The app's data, users and sessions. |
| `keys/` | Token signing keys. |
| `storage/` | Uploaded files. |
| `cache/` | Analysis output and the helper scripts rayfin-local runs with Node. |
| `state.json` | The local app id, publishable key, workspace, item and tenant ids, and the last ports. |
| `secrets.json` | Local secret values. |

`rayfin-local reset` deletes the folder after asking (`--yes` skips the question).
`rayfin-local serve --reset` deletes it before starting, so the app starts with an empty database.
Both delete users, stored files and local secrets too.

## Schema changes

Each time `serve` starts, it analyzes `rayfin/data` with the project's own `@microsoft/rayfin-core`, as the Rayfin CLI does, and brings the SQLite tables up to date.

- Changes that lose no data apply automatically, such as new entities, new optional fields, fields with a default and new indexes.
- Changes that would lose data are refused, with a list of exactly what would be lost: dropped entities or fields that hold values, values that can't be converted to a new type or would be rounded to a smaller scale, and required fields without a default that existing rows have no value for.
  Run `serve --force-schema` to apply them anyway, or `serve --reset` to start over.

`rayfin-local schema` prints the app model and the SQLite schema derived from `rayfin/data`.

## Configuration file

An optional `rayfin-local.toml` at the project root holds rayfin-local's own settings.
Every key is optional, unknown keys are errors, and command-line flags beat the file.

```toml
[server]
port = 7171             # --port
host = "127.0.0.1"      # --host

[frontend]
port = 5175             # --frontend-port

[tokens]
source = "auto"         # --token-source: "auto" | "az" | "rayfin"
tenant = "contoso.onmicrosoft.com"   # --tenant

[tokens.scopes]         # audience = OAuth scope
AzureAI = "https://cognitiveservices.azure.com/.default"

[connectors]
powerbi_base_url = "https://api.powerbi.com"   # for mocks and sovereign clouds

[functions]
inspect_port = 9229     # --inspect
```

A port set by a flag or in this file must be free, or `serve` stops with an error; remembered and default ports move to the next free one.

## Commands and flags

| Command | What it does |
| --- | --- |
| `rayfin-local serve` | Start the local backend and the app. |
| `rayfin-local doctor` | Check Node.js, installed packages, Azure and Rayfin sign-in, and ports. |
| `rayfin-local schema` | Print the app model and the SQLite schema derived from `rayfin/data`. |
| `rayfin-local reset [--yes]` | Delete `.rayfin-local/`: database, users, keys, stored files and secrets. |
| `rayfin-local users list\|add\|delete\|reset-password` | Manage local users. |
| `rayfin-local secret set\|list\|delete` | Manage local secret values (`.rayfin-local/secrets.json`). |
| `rayfin-local sign-in-link` | Print a one-time sign-in link for a local user. |

`rayfin-local <command> --help` shows each command's arguments.
Every command takes these options:

| Option | Meaning |
| --- | --- |
| `-C, --project <DIR>` | The project to use. Default: the current directory, or the nearest parent with `rayfin/rayfin.yml`. |
| `-v, --verbose` | Show rayfin-local's debug logs. `RUST_LOG` replaces the default log filters. |

`serve` flags:

| Flag | Meaning |
| --- | --- |
| `--port <PORT>` | Backend port. Default: `[server] port`, else this project's last port, else 7171. |
| `--frontend-port <PORT>` | Vite port. Default: `[frontend] port`, else `RAYFIN_PUBLIC_FRONTEND_PORT` from `rayfin/.env`, else 5173. |
| `--static` | Build the app once and serve it from the backend instead of running Vite. |
| `--no-frontend` | Run only the backend. |
| `--no-functions` | Don't build or run the functions. |
| `--host <HOST>` | Interface to listen on. Default: `[server] host`, else `127.0.0.1`. **Anything but loopback exposes your Azure identity** to the network, through functions and connectors. |
| `--token-source <auto\|az\|rayfin>` | Where Azure tokens come from. Default: `[tokens] source`, else `auto`. |
| `--tenant <TENANT>` | Entra tenant for tokens. Default: your login's tenant. |
| `--force-schema` | Apply schema changes that lose data. |
| `--reset` | Delete `.rayfin-local/` first and start from an empty database. |
| `--inspect[=PORT]` | Start the function host with the Node.js inspector, on port 9229 by default. |
| `--open` | Open the app in the browser once it is ready. |

### Environment variables

| Variable | Effect |
| --- | --- |
| `RAYFIN_LOCAL_NODE` | The `node` executable to use instead of the one on `PATH`. |
| `RAYFIN_LOCAL_TOKEN_<AUDIENCE>` | A fixed token for one audience (see [above](#identity-and-azure-tokens)). |
| `RAYFIN_ENV_FILE` | The env file to read instead of `rayfin/.env`, as for the Rayfin CLI. |
| `RUST_LOG` | Log filters, such as `rl_graphql=debug`. |
| `NO_COLOR` | Turn off colored output. |

## Troubleshooting

Run `rayfin-local doctor` first: it checks what `serve` needs and says what to fix.

- **"Node.js was not found" or "Node.js … is too old".**
  Install Node.js 20 or later from <https://nodejs.org>, or set `RAYFIN_LOCAL_NODE` to a `node` executable.
- **"@microsoft/rayfin-core is not installed".**
  Run `npm install` in the project; rayfin-local uses the project's own packages.
- **"Port … is already in use".**
  A port you set with a flag or in `rayfin-local.toml` is taken: stop the program using it, or pick another port.
  Ports you didn't set move to the next free one by themselves.
- **Azure sign-in errors, or "No token available for audience …" from a function.**
  Run `az login` (with `--tenant <tenant>` for another tenant), or sign in with the Rayfin CLI (`npx rayfin login`) and use `--token-source rayfin`.
  A function still runs without a token; reading `ctx.Tokens.<Audience>` throws, and `serve` logs why the token is missing.
- **Consent errors (`AADSTS65001`).**
  The Azure CLI's app is not consented for that resource in your tenant.
  Use the Rayfin CLI's login (`--token-source rayfin`, or `source = "rayfin"` under `[tokens]`), or override the audience's scope under `[tokens.scopes]`.
  If the Rayfin CLI's app is the one without consent, use `--token-source az`.
- **"… is not the 1.36 SDK rayfin-local implements".**
  rayfin-local implements the wire contract of `@microsoft/rayfin-*` 1.36.x.
  With another version, behavior may differ from Fabric; use 1.36.x for full fidelity.
- **A schema change is refused.**
  The message lists the data the change would lose; rerun with `--force-schema` to accept that, or `--reset` to start over.
- **"Continue as" is missing from the sign-in page.**
  It needs a loopback `--host` and a token from `az login` or the Rayfin CLI's login.

## More documentation

- [Differences from Fabric](docs/differences.md): what is supported, what differs and what is missing, per SDK surface.
- [Architecture](docs/architecture.md): the crates, the request flow, the frontend wiring, the function host and the token broker.
- [Development](docs/development.md): building, testing and releasing rayfin-local.

## License

[MIT](LICENSE).
