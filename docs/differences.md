# Differences from Fabric

rayfin-local runs an unmodified Rayfin app (SDK 1.36.x) against local stand-ins for the Rayfin services.
It answers the SDK with the same routes, bodies, statuses and errors as Fabric wherever it can.
This page lists where it can't, per SDK surface.

Each row has one of three statuses:

- **Supported:** works as on Fabric.
- **Differs:** works, with the differences in the notes.
- **Not supported:** not available locally.

## Who acts

The biggest difference is identity: everything that reaches Fabric or Azure runs as you, the developer.

| Caller | On Fabric | Locally |
| --- | --- | --- |
| Data API and storage | The signed-in app user. | The signed-in local user. |
| Functions' `ctx.Tokens.<Audience>` | The app's identity (for Fabric apps, the owner of the app item). | You: your Azure CLI or Rayfin CLI login, or `RAYFIN_LOCAL_TOKEN_<AUDIENCE>`. |
| Delegated connectors (`fabric-semanticmodel`) | The signed-in user, on behalf of. | You, with your Power BI token. |
| SQL connectors (`fabric-sqldatabase`, `fabric-warehouse`, `fabric-sqlanalytics`) | The app's identity (`auth: application`, the default) or the signed-in user (`auth: delegated`). | You, with your Entra token for the database, whatever `auth.type` says. The app's `@role` grants and policies still apply to the signed-in local user. |
| "Sign in with Fabric" | The user's Fabric account. | The sign-in identity (`--sign-in-as`), without asking; with `none`, a local user, or you with "Continue as". |

So locally a function or a query can reach what your account can reach, which may be more or less than the app's identity or its users.
Security that a database enforces itself (its permissions, row-level security) applies to you, not to the app's user.
Keep the backend on loopback (the default `--host 127.0.0.1`): any other address lets other machines act as you.

## Auth (`client.auth`)

| Feature | Status | Notes |
| --- | --- | --- |
| Username/password sign-up and sign-in | Supported | Users live in `.rayfin-local/db.sqlite`, with argon2id password hashes. |
| Password sign-in for an unknown email | Differs | Creates the account (when sign-up is allowed, even if `password.enabled` is false) instead of answering `invalid_grant`, so app templates that sign in with a fixed dev account work; SDK 1.36.2 breaks their sign-up fallback. `[auth] auto_create_users = false` restores Fabric's answer. |
| Static `customClaims` | Differs | A custom claim named like a reserved claim (`sub`, `email`, `role`, `name`, `scope`, `sid`, `iss`, `aud`, `exp`, `iat`, `nbf`, `jti`) is ignored with a warning; Fabric's token issuer would put it in place of the standard claim in every user's token. Other custom claims are added as on Fabric. |
| Sessions: access tokens, refresh-token rotation, sign-out, sign-out everywhere | Supported | ES256 access tokens shaped like the service's, with the lifetimes of `services.auth` in `rayfin.yml`. |
| JWKS and runtime settings (`/.well-known/jwks.json`, `/api/projectRuntimeSettings`) | Supported | The settings mirror `services.auth` in `rayfin.yml`. |
| "Sign in with Fabric" (`ensureSignedInWithFabric`) | Differs | The popup opens rayfin-local's sign-in page (`/.rayfin-local/portal`) instead of the Fabric portal. As Fabric signs in whoever is signed in to it, the page signs in as the sign-in identity (`--sign-in-as`, `[auth] sign_in_as`) without asking, and closes, when it's opened on localhost. With `none`, it offers local sign-in, sign-up and "Continue as &lt;you&gt;". The handshake with the app is the SDK's own (`brokeredAuth.*` messages, then a one-time handoff code). |
| Embedded sign-in (`initEmbeddedAuth`) | Differs | Fabric frames the app and answers its `fabric-auth` handoff requests. Locally, open `/.rayfin-local/portal/embed` (the banner's **Embedded** URL): you sign in on that page, and it frames the app with `?fabricEmbedded=true` and answers the same requests for you. It doesn't stamp Fabric's `_fu` user hint, so the app signs in afresh on each load, as with an older Fabric host. Other host requests answer `NOT_SUPPORTED`. |
| Automatic sign-in in development (`rayfinLocalDev({ autoLogin: true })`) | Differs | rayfin-local's dev server answers the page's `/.rayfin/dev/session-token` before `rayfinLocalDev()` does, so the Rayfin CLI never signs you in interactively. The app signs in as the sign-in identity (`--sign-in-as`, `[auth] sign_in_as`): you, as your Entra user when the Azure CLI or the Rayfin CLI's login has a token without a prompt and the backend listens on loopback, else `developer@example.com`; a local user; or nobody, which answers 401 so the app offers its own sign-in. |
| Entra users | Differs | Local users are not Entra users. "Continue as" signs you in with your own Entra account (the one your tokens come from), and is offered only when the backend listens on loopback. |
| The `email` claim | Differs | Set only when the username is an email address; "Continue as" sets it to your user principal name. Policies on `claims.email` need such a user. |
| Entra token exchange (`POST /api/auth/v1/brokered/token`: `signInWithEntraToken`, the Vite plugin's auto-login) | Differs | Accepted on loopback even when `services.auth.fabric.externalEntraExchange` is off, with a warning; Fabric needs `services.auth.fabric.enabled` and `externalEntraExchange`. Tokens are checked against Microsoft's signing keys. |
| Passwordless sign-in (magic links) | Not supported | The routes answer as the service does when the feature is disabled. |
| Email verification and password-reset emails | Not supported | No email is sent. Reset a local password with `rayfin-local users reset-password`. |

## Data (`client.data`, `/graphql`)

The data API is a DAB-compatible GraphQL API compiled to SQL over SQLite.
It generates the schema DAB generates for your entities and answers with DAB's data, errors and HTTP statuses.

| Feature | Status | Notes |
| --- | --- | --- |
| Queries: lists, by key, filters, `orderBy`, cursor pagination | Supported | |
| Relationships (`@one`, `@many`) | Supported | Nested selections and relationship filters. |
| Mutations: create, update, delete | Supported | |
| Aggregations (`groupBy`, `aggregate`) | Supported | |
| `@role` policies and field permissions | Supported | The `anonymous` and `authenticated` roles, with DAB's per-action inheritance. A policy that reads a claim the caller doesn't have fails the request, as on Fabric. |
| The `anonymous` role | Differs | Allowed, as on a tenant where an administrator turned on "Enable anonymous data access for Fabric Apps (Preview)"; that setting is off by default, and without it `rayfin up` refuses the schema and requests without a token get a 401. `serve` warns when entities grant `anonymous`; `[fabric] anonymous_data_access = false` in `rayfin-local.toml` acts like a tenant without the setting. |
| Constraints | Supported | The database enforces required fields, lengths, `@set` values, the email shape, minimum lengths, uniqueness and foreign keys, as on Fabric. Patterns and other `min`/`max` options are checked only by the SDK's form validation, on both. |
| Text comparisons | Differs | Case-insensitive like Fabric SQL's default collation, but SQLite's `NOCASE` folds ASCII letters only: `'É' = 'é'` is false locally and true on Fabric. On the `postgresql` dialect, text compares case-sensitively. |
| Decimals | Differs | Stored as doubles (`REAL`), with output rounded to the column's scale. About 15 significant digits are exact, so a `DECIMAL(18,2)` above about 10^13 loses cents; Fabric is exact to the declared precision. |
| Dates | Differs | Stored as UTC ISO-8601 text with 100 ns precision. Responses use Fabric's format (`YYYY-MM-DDTHH:MM:SS.fffZ`), so the difference shows only in the SQLite file. |
| Schema changes | Differs | `serve` applies them from `rayfin/data` when it starts and as you edit the entities, not on a deployment. Changes that would lose data are refused until you pass `--force-schema`. A data package (`services.data.path`) is built with `services.data.buildCommand` first, as `rayfin up` and `rayfin dev` do, at every start and when its sources change. |
| Introspection (`__schema`, `__type`) | Not supported | Refused unless `services.data.allowIntrospection` is true, as on Fabric; when allowed, rayfin-local still answers an error (no introspection resolver yet). `__typename` always works. |
| Data from a deployed app | Not supported | The local database starts empty; nothing is copied from Fabric. |

## Functions (`/functions/<name>/invoke`)

rayfin-local builds your functions package and runs it on your Node.js, with the package's own `@azure/functions` and `@microsoft/fabric-user-data-functions`, without Azure Functions Core Tools.

| Feature | Status | Notes |
| --- | --- | --- |
| Invocation and the response envelope | Supported | HTTP 200 with `x-fabric-udf-status`, so failed functions surface as `FunctionsError`, as deployed. This holds even when `local.settings.json` sets `IsLocal` to true. Anonymous calls are allowed, as deployed. |
| Arguments and return values | Supported | Parsed and serialized by the UDF package itself. |
| `ctx.getDataClient()` | Supported | Calls the local data API as the caller. |
| `ctx.Tokens.<Audience>` (`Sql`, `Storage`, `Fabric`, `AzureAI`, `ADO`) | Differs | Tokens for you, not the app's identity. A token that can't be minted is left out: the function still runs, reading it throws "No token available for audience…", and `serve` logs why. |
| `ctx.Secrets.<NAME>` | Differs | Local values, from `local.settings.json` `Values`, the environment, or `.rayfin-local/secrets.json` (`rayfin-local secret set`). |
| Error messages | Differs | Deployed, 5xx bodies hide their cause behind `internal_server_error`; locally the message says what failed. |
| Limits | Differs | The 250-second timeout (408) is the UDF ceiling, and results over 30 MiB fail with `ResponseTooLarge`, as deployed. Request bodies over 30 MiB get 413, a limit of rayfin-local's own. |
| Logs | Differs | `console.*` output goes to `serve`'s output, prefixed `[fn]`, not to Fabric's monitoring. |

## Connectors (`client.connectors`, `/connector-invoke/<name>`, `/connectors/<name>/graphql`)

| Feature | Status | Notes |
| --- | --- | --- |
| `fabric-semanticmodel` queries | Differs | Sent to the real semantic model through Power BI, as you rather than the signed-in user. The app user must still be signed in, as deployed. Results, DAX errors and Power BI failures reach the SDK in the shapes it normalizes on Fabric. |
| `fabric-semanticmodel` with an Azure CLI token | Differs | Power BI refuses `executeDaxQueries` (Arrow) to tokens without a dataset scope, which includes every Azure CLI token, so these queries run through `executeQueries` (JSON). Columns then have no `dataType` (the SDK reports `'unknown'`), DateTimes lose their milliseconds, and empty results have no columns. Results are capped at 100,000 rows (an `overflow` error beyond that), and `culture`, `schemaOnly` and `queryTimeout` are ignored. |
| Category A SQL connectors (`fabric-sqldatabase`, `fabric-warehouse`, `fabric-sqlanalytics`): queries, keys, relationships, filters, `orderBy`, cursor pagination | Differs | The DAB-style GraphQL API that `rayfin up` would deploy, answered by rayfin-local over the connector's real Fabric item. They run as you, with your Entra token, even for `auth: application` connectors (see [Who acts](#who-acts)); a `delegated` connector still needs a signed-in user. [sql-connectors.md](sql-connectors.md) has the details. |
| Category A mutations | Differs | Run as you on the real item. A SQL database answers the row, a Warehouse `DbOperationResult`, and a SQL analytics endpoint has no mutations, as on Fabric. A create on a keyless entity of a SQL database answers `null` (the row is written). |
| Category A `@role` grants, field lists and policies | Supported | Enforced by rayfin-local in every statement, against the signed-in local user, as for data entities. Grants beyond the connector's `operations:` (which `rayfin up` refuses) are dropped with a warning. |
| Category A values and errors | Differs | DAB's formats and error codes. Scalars follow the entity decorators (`@int` is `Int` even on a `bigint` column, where Fabric's DAB says `Long`). Statements the database refuses answer DAB's `DatabaseOperationFailed` with the server's message as `extensions.detail`, which Fabric hides; a database that can't be reached or refuses your sign-in answers a GraphQL error that says what to do. |
| Category A aggregations (`groupBy`, `aggregate`) | Not supported | Answer an error that names them. |
| Category A setup checks | Differs | Entity names that are reserved or collide across connectors are not refused as `rayfin up` refuses them. |
| `kusto` and other connector types | Not supported | Answer 501 `not_supported`. |

## Storage (`/api/storage/*`)

| Feature | Status | Notes |
| --- | --- | --- |
| Upload, download, list and delete (`StorageFolderClient`) | Supported | The 1.36.2 routes and answers, on files under `.rayfin-local/storage`. |
| `@blob` folder rules and checks | Supported | `anonymous` and `authenticated` rules; checks compare strings case-insensitively, as Fabric's collation does. |
| Signed URLs | Differs | They point at the backend (`/.rayfin-local/storage/…`), not Azure Blob Storage; the HMAC token in the URL is the credential. The default lifetime is 15 minutes. |
| Long-running operations | Differs | Every step completes at once, so the first status poll already sees the result. |
| Size limits | Differs | 25 MiB per file, the service's limit (a folder's `maxSize` can lower it), and 1 GiB for all files together. |

## Frontend and hosting

| Feature | Status | Notes |
| --- | --- | --- |
| The app's Vite dev server | Supported | Your `vite.config` and plugins. rayfin-local starts Vite itself with the local environment, never `npm run dev`. |
| `rayfinLocalDev()` (`@microsoft/rayfin-local-dev/vite`) | Supported | Used when your Vite config has it; otherwise rayfin-local adds a plugin that serves `/rayfin.config.json` and proxies the SDK's routes. |
| Static hosting | Differs | `--static` builds once with `services.staticHosting.buildCommand` and serves the output from the backend origin, with the SPA fallback to `indexDocument`; it doesn't rebuild on changes. |

## Platform

| Feature | Status | Notes |
| --- | --- | --- |
| Deployment (`rayfin up`), Fabric items, workspaces and their permissions | Not supported | Nothing is deployed, and no Fabric item is created or changed. |
| Workspace, item and tenant ids | Differs | Taken from `rayfin/.env` when the app was deployed, else generated. The app id and the publishable key (`pk-local-…`) are local. |
| SDK versions | Differs | rayfin-local implements 1.36.x; with another version, `serve` warns that behavior may differ. Before 1.36, apps with database policies (`@role` with `policy`) can't be analyzed, and `serve` says to upgrade. |

## Local only

These exist only in rayfin-local:

- Hot reload of functions: `build:watch` rebuilds them, and the host restarts when the compiled output changes.
- Live reload of the configuration: changes to `rayfin.yml`, the SQL connectors' entities and the secret values apply while `serve` runs, on the same ports; on Fabric they need `rayfin up`.
- The Vite dev server is started again when it stops by itself.
- `serve --inspect[=PORT]`: the Node.js inspector on the function host.
- `RAYFIN_LOCAL_TOKEN_<AUDIENCE>`: fixed tokens for tests and CI.
- `[connectors] powerbi_base_url` in `rayfin-local.toml`: a mock or sovereign-cloud Power BI.
- The `users`, `secret`, `reset`, `doctor` and `schema` commands.
