# Embedding rayfin-local

This page is the contract between `rayfin-local` and programs that run it for the developer, such as [Fabricator](https://github.com/spatney/rayfin-fabricator).
A host starts `rayfin-local serve --json` as a child process, reads events from its stdout, controls it through a loopback HTTP API, and stops it by closing its stdin.
Contract version: **1**.
Additive changes (new event types, new fields, new endpoints) keep the version; hosts ignore what they don't know.
Anything else bumps it.

## Versions

`rayfin-local version --json` prints one JSON object and exits 0:

```json
{"version":"0.2.0","contract":1,"sdk":{"supported":">=1.36.0 <1.37.0","tested":"1.36.2"}}
```

- `contract` is the version of this contract.
- `sdk.supported` is the semver range of `@microsoft/rayfin-*` packages rayfin-local implements; `sdk.tested` is the version its conformance tests use.

`rayfin-local version` without `--json` prints `rayfin-local 0.2.0`, as `--version` does.

## Starting

```sh
rayfin-local serve --json -C <project> [--sign-in-as developer|none|user:<username>] [--force-schema] [--reset] [--no-functions] [--port N] [--frontend-port N]
```

With `--json`:

- **stdout** carries only events, one JSON object per line (NDJSON, UTF-8, `\n`).
- **stderr** is not part of the contract; hosts may log it.
  Human status lines, the banner and colors are not printed.
- **stdin**: when it reaches end of file (the host closed it, or the host died), `serve` stops gracefully, as on Ctrl+C, and exits 0.
  This holds from the start: closing stdin before `ready` stops the start too.
  Hosts should keep stdin open (a pipe they never write to) while they want the app to run.
- Child output (Vite, the function host, builds) becomes `log` events instead of `[vite]`-style lines.
- The browser is never opened (`--open` is ignored).

The environment configures the rest:

| Variable | Effect |
| --- | --- |
| `RAYFIN_LOCAL_CONTROL_TOKEN` | Turns on the [control API](#control-api); requests must send it as a bearer token. At least 32 characters; shorter values are refused with a fatal `config-invalid` error. An empty value counts as unset. rayfin-local removes it from its environment, so the processes it starts (Vite, the function host, the app's code) don't see it. |
| `RAYFIN_LOCAL_NODE` | The `node` executable. |
| `RAYFIN_CONFIG_DIR` | The Rayfin CLI's config folder, for the `rayfin` token source (the account a host has signed in). |
| `NO_COLOR` | Ignored with `--json` (there are no colors). |

## Events

Every event has `v` (the contract version), `type` and `time` (RFC 3339, UTC, with milliseconds):

```json
{"v":1,"type":"ready","time":"2026-10-30T03:12:09.512Z", ...}
```

Events in the order a successful start emits them: `starting`, `analysis`, `schema`, `functions` (when the functions service is on), `vite` (unless `--no-frontend` or `--static`), `ready`.
`log`, `warning` and `error` can come at any time.
After `ready`, live changes emit `analysis`, `schema`, `functions`, `vite` and `config-changed` again, and a change that applies is followed by a new `ready`.
The last event is always `stopped`.
A failure before the project is found and read (`project-not-found`, an invalid `rayfin.yml`) emits only `error` and `stopped`.

### `starting`

```json
{"v":1,"type":"starting","time":"…","pid":4242,"version":"0.2.0","project":{"root":"C:\\apps\\notes","name":"notes"}}
```

### `analysis`

The app model was analyzed (at start, and after each live change of the entity sources).
With a data package (`services.data.buildCommand`), its build command runs first, as at start, after changes of its sources or of `services.data`, and for `POST /reload`; a failed build is an `analysis` with `ok: false`.

```json
{"v":1,"type":"analysis","time":"…","ok":true,"sdk":{"version":"1.36.2","supported":true},"entities":["Note","Tag"],"storageFolders":[],"message":null}
```

- `ok: false` with `message` when the analysis failed (a compile error in `rayfin/data`, a failed data build).
  At start this is followed by a fatal `analysis-failed` error; while running, the previous model stays in use.
- `sdk.version` is `null` when the packages aren't installed.

### `schema`

The database was brought up to date with the model, or wasn't.

```json
{"v":1,"type":"schema","time":"…","result":"applied","changes":["added Tag","changed Note"],"losses":[],"message":null,"live":false}
```

| `result` | Meaning |
| --- | --- |
| `unchanged` | Nothing to do (also when `services.data` is off). |
| `applied` | `changes` lists what changed: `added Tag`, `removed Tag` or `changed Note` for each entity (`dialect mssql` when only the dialect changed), then the number of database steps, such as `3 database changes`. With `--force-schema`, `losses` lists the data that was dropped. |
| `refused` | The change would lose data; `losses` lists exactly what (dropped entities or fields that hold values, values that can't be converted, required fields without a default). At start this is followed by a fatal `schema-refused` error; while running (`live: true`), the previous schema stays in use. Apply it with `POST /reload` and `"forceSchema": true`, or restart with `--force-schema` or `--reset`. |
| `failed` | Applying failed; `message` says why. The previous schema stays in use. |

`live` is `true` for changes made while serving.

### `functions`

```json
{"v":1,"type":"functions","time":"…","state":"ready","names":["getBacklog","askAnalyst"],"errors":[],"message":null}
```

`state` is `building`, `ready`, `failed` (with the build's `errors`, one string per compiler message, and the previous build keeps serving if there was one) or `off` (with the reason in `message`).
With `failed`, `names` lists the functions that still run: those of the previous build, or of output the compiler emitted despite the errors (`tsc` does), which is loaded but stays `failed` until a build succeeds.
`serve` starts without a `functions` event when the service is off (`--no-functions`, or `services.functions` disabled in `rayfin.yml`).
Turning `services.functions` off while serving emits `off`; turning it on emits `building`, then `ready` or `failed`.
New secret values restart the function host, which then emits `ready` again.

### `vite`

```json
{"v":1,"type":"vite","time":"…","state":"ready","url":"http://localhost:5173/","version":"8.0.16","message":null}
```

`state` is `ready`, `exited` (it stopped on its own; `message` says how), `restarting` (rayfin-local is starting it again, with backoff) or `failed` (it could not start, or kept exiting; `message` says why).
The URL never changes while `serve` runs: a restarted Vite takes the same port.
After Vite exits, it is started again after 1, 2 and then 5 seconds: `restarting`, then `ready`, or the next `restarting` when that start fails (its `message` says why).
When it stops a fourth time within two minutes (three restarts failed), rayfin-local stops trying and emits `failed`; the backend keeps running, and a restart of `serve` starts Vite again.

### `ready`

Everything started.
Hosts show the app once they see this event.
It comes again after each `config-changed` that applied, with the new features; the URLs stay the same.

```json
{
  "v": 1, "type": "ready", "time": "…",
  "appUrl": "http://localhost:5173/",
  "apiUrl": "http://localhost:7171",
  "embedUrl": "http://localhost:7171/.rayfin-local/portal/embed",
  "portalUrl": "http://localhost:7171/.rayfin-local/portal",
  "dataDir": "C:\\apps\\notes\\.rayfin-local",
  "signInAs": {"kind": "developer"},
  "sdk": {"version": "1.36.2", "supported": true},
  "features": {
    "data": {"enabled": true, "entities": ["Note", "Tag"]},
    "functions": {"enabled": true, "names": ["getBacklog"]},
    "storage": {"enabled": false, "folders": []},
    "auth": {"enabled": true, "fabric": true, "password": false},
    "connectors": [
      {"name": "sales", "type": "fabric-semanticmodel", "supported": true},
      {"name": "logs", "type": "kusto", "supported": false}
    ]
  }
}
```

- `appUrl` is `null` with `--no-frontend`; with `--static` it is the backend's origin.
- `embedUrl` is `null` when the app has no Fabric sign-in, or no `appUrl`.
- `signInAs` is the [automatic sign-in](#automatic-sign-in) identity: `{"kind":"developer"}`, `{"kind":"user","username":"ana@example.com"}` or `{"kind":"none"}`.
- `features.data.enabled` is `false` while the data service is off, or when it was turned on while serving with a schema change that was refused.
- `connectors[].supported` is `false` for connector types rayfin-local answers with 501.

### `config-changed`

`rayfin.yml`, `rayfin-local.toml`, the storage folders, the SQL connectors' entities (`rayfin/connectors/`) or the secret values (`.rayfin-local/secrets.json`, the functions' `local.settings.json`) changed while serving.

```json
{"v":1,"type":"config-changed","time":"…","applied":true,"restartRequired":false,"what":["storage","functions"],"message":null}
```

- `what` lists what changed: `auth` (`services.auth`, `[auth]`), `name`, `data`, `functions` (`services.functions`, `[functions]`), `storage` (`services.storage`, the storage folders), `connectors` (the connectors, their entities, `[connectors]`), `secrets` (the declared secrets, their values) and `fabric` (`[fabric]`) apply while serving; `frontend` (`services.staticHosting` moving the app, `[frontend]`), `server` (`[server]`) and `tokens` (`[tokens]`) need a restart.
  Settings set by a command-line flag don't count.
- `applied: true`: the services were rebuilt on the same ports, and a new `ready` event follows with the new features.
  The listeners, the database, the users and their sessions, the signing keys and the [sign-in identity](#automatic-sign-in) (unless `[auth] sign_in_as` changed) stay, and so does the [control API](#control-api).
  New secret values restart only the function host.
- `restartRequired: true`: some of the change can't be applied while running (for example the frontend's folder moved); `message` says what.
  Nothing of `rayfin.yml` or `rayfin-local.toml` applied then; `applied` tells whether something else did.
  Hosts restart `serve`.
- A `rayfin.yml` or `rayfin-local.toml` that can't be read emits a `config-invalid` error that isn't fatal, and the previous configuration stays in use.

### `log`

```json
{"v":1,"type":"log","time":"…","source":"vite","level":"info","line":"page reload src/App.tsx"}
```

- `source`: `vite`, `functions` (the function host and its build), `build` (the data package build, and the app's build with `--static`), or `rayfin-local` (its own logs, including failed requests).
- `level`: `debug`, `info`, `warn` or `error` (child output on stderr is `warn`).
  `debug` lines are emitted only with `--verbose`.

### `warning` and `error`

```json
{"v":1,"type":"error","time":"…","code":"schema-refused","message":"The change would delete data: …","hint":"Restart with --force-schema to apply it, or --reset to start over.","fatal":true,"details":{"losses":["…"]}}
```

`code` is stable; hosts act on it.
`message` and `hint` are for people and may change.
`details` carries the code's structured data, listed below (`{}` for codes without any).
A `fatal` error is followed by `stopped`; other errors leave `serve` running.

| Code | Kind | Meaning, and `details` |
| --- | --- | --- |
| `node-missing` | error | Node.js was not found. |
| `node-too-old` | error | Node.js is older than 20. `details`: `{"version"}`. |
| `project-not-found` | error | No `rayfin/rayfin.yml` in the folder or its parents. |
| `config-invalid` | error | `rayfin.yml`, `rayfin-local.toml`, `.rayfin-local/state.json`, the functions' folder, `package.json` or `local.settings.json`, or the control token is invalid. Not fatal when `rayfin.yml` or `rayfin-local.toml` changes while serving: the previous configuration stays in use. |
| `packages-missing` | error | The project's npm packages (or the functions') aren't installed. `details`: `{"dir"}`, the folder to run `npm install` in. For the functions' packages it is not fatal: it comes before their build, which then fails, once they have stayed missing for five seconds (an `npm install` that is still running when the functions start doesn't raise it). |
| `sdk-unsupported` | warning | The app's Rayfin packages are outside `sdk.supported`. `details`: `{"version","supported"}`. |
| `analysis-failed` | error | The model could not be analyzed, or `rayfin-local.toml` refuses it (`[fabric] anonymous_data_access = false` and entities that grant `anonymous`). |
| `schema-refused` | error | See [`schema`](#schema). `details`: `{"losses"}`. |
| `schema-failed` | error | The database could not be updated. |
| `port-in-use` | error | A port set with a flag is busy. `details`: `{"port"}`. |
| `already-serving` | error | Another `serve` runs this project. `details`: `{"pid","appUrl","apiUrl"}` from `.rayfin-local/serve.json`. |
| `vite-failed` | error | Vite could not start (fatal at start; while running, see `vite` events). |
| `functions-build-failed` | warning | The functions' build failed (once per failure; the `functions` event has the compiler's messages). Until a build succeeds, invocations answer 503, or run what the compiler emitted despite the errors. |
| `secret-missing` | warning | A declared secret has no local value. `details`: `{"name"}`. |
| `anonymous-role` | warning | Entities grant the `anonymous` role, which most Fabric tenants refuse. `details`: `{"entities"}`. |
| `connector-unsupported` | warning | A connector's type runs only on Fabric. `details`: `{"name","type"}`. |
| `azure-sign-in` | warning | No Azure token could be had (functions' `ctx.Tokens`, connectors and the developer identity need one); once per audience. `details`: `{"audience","scope"}` (`audience` is `null` for other scopes). |
| `internal` | error | Anything else. |

### `stopped`

```json
{"v":1,"type":"stopped","time":"…","reason":"stdin-closed","exitCode":0}
```

`reason` is `stdin-closed`, `signal`, `error` (after a fatal error) or `backend-exited`.

### Exit codes

`0` after a graceful stop with `--json`, `1` after an error, `130` after Ctrl+C without `--json`.
A second Ctrl+C while `serve` stops ends it at once, with `130` (and, with `--json`, a `stopped` event with reason `signal` and `exitCode` 130).

## Automatic sign-in

Apps built from Rayfin's templates sign in during development with `rayfinLocalDev({ autoLogin: true })` from `@microsoft/rayfin-local-dev`.
Its page asks the dev server for `GET /.rayfin/dev/session-token`, which on Fabric needs a `rayfin login` session.
Under rayfin-local, the dev server answers that route itself, ahead of `rayfinLocalDev()`, for the **sign-in identity**, without any Entra sign-in in the browser:

| Identity | `--sign-in-as` | Signs the app in as |
| --- | --- | --- |
| `developer` (default) | `developer` | You: your Entra identity from the Azure CLI or the Rayfin CLI's cached sign-in when a token for it comes without a prompt and the backend listens on loopback, else a local account, `developer@example.com`. |
| `user` | `user:<username>` | That local user (created without a password when missing). |
| `none` | `none` | Nobody: the route answers 401 `{"error":"Automatic sign-in is off.","hint":"…"}`. |

`[auth] sign_in_as` in `rayfin-local.toml` sets the identity too (`"developer"`, `"none"` or `"user:<username>"`); `--sign-in-as` wins.
Changing it while `serve` runs switches the identity (a `config-changed` event with `auth` in `what`), unless `--sign-in-as` was given; other live changes keep the identity.
When `developer` finds no Entra identity, it asks again after five minutes, or at once when the identity is set again (`PUT /identity`), for example once the developer signed in to Azure.
The answer is a session like any other, in the shape `fetchRayfinLocalSessionToken()` expects: `{"accessToken","tokenType","expiresIn","refreshToken","scope"}`.
Failures are `{"error","hint"}`, which the page shows: 403 for requests from other machines or other origins, 404 when the app's auth is off (`services.auth.enabled: false`), 500 when signing in failed.
Changing the identity (through the control API, or `[auth] sign_in_as` while `serve` runs) revokes every local session, so an app that reloads signs in again as the new identity.
The dev server gets the session from the backend's `GET /.rayfin-local/dev/session-token`, which answers only rayfin-local's own dev server: loopback requests that carry a secret of the run, which no page and none of the app's code sees.

## Control API

With `RAYFIN_LOCAL_CONTROL_TOKEN` set, the backend serves `{apiUrl}/.rayfin-local/control/*`.

- Only loopback peers are answered; others get 403.
- Every request needs `Authorization: Bearer <RAYFIN_LOCAL_CONTROL_TOKEN>`; otherwise 401.
- Requests with an `Origin` header (from a browser) get 403, and CORS never allows these routes.
  They need no publishable key.
- Bodies are JSON; a route that takes one also takes an empty body, as `{}`.
  Errors are `{"error":{"code","message","hint"}}` with a 4xx or 5xx status; `code` is stable, and is one of `unauthorized` (401), `forbidden` (403), `invalid-request` (400), `invalid-order-by` (400), `user-not-found` (404), `entity-not-found` (404), `user-exists` (409), `rows-referenced` (409), `config-invalid` (422), `analysis-failed` (422), `internal` (500), `starting` (503) and `stopping` (503).
- Without the variable, these routes answer 404.
- Live reload keeps them: they answer, with the same token, after every change it applies.

| Method and path | Body | Answer |
| --- | --- | --- |
| `GET /status` | | The `ready` payload (without `v`, `type`, `time`) plus `"state":"running"`, `"analysis"` (the last `analysis` event), `"schema"` (the last `schema` event), `"functions"` (the last `functions` event) and `"vite"` (the last `vite` event). `signInAs`, `features.data.entities` and `features.functions.names` are as they are now: the identity may have changed since the last `ready`, and changed entities and rebuilt functions apply without a new `ready`. 503 `starting` before `ready`. |
| `GET /identity` | | `{"kind":"developer","user":{…}}`, `{"kind":"user","user":{…}}` or `{"kind":"none","user":null}`. `user` is the [user](#users) the identity signs in as: for `developer`, the account it signed in as last in this run (`null` until first used); for `user`, `null` while that user doesn't exist. |
| `PUT /identity` | `{"kind":"developer"}`, `{"kind":"user","user":"<username or id>","create":true}` or `{"kind":"none"}` | The new identity, as `GET /identity`. Revokes every local session. A missing user is 404 `user-not-found` unless `create` is `true` (default `false`), which creates it without a password. |
| `GET /users` | | `{"users":[user, …]}`, oldest first. |
| `POST /users` | `{"username":"ana@example.com","password":"…"}` (`password` optional: without one the user can only be signed in by rayfin-local: automatic sign-in and `POST /token`) | `user`, 201. 409 `user-exists` when the username exists (usernames are unique without regard to case). |
| `DELETE /users/{username or id}` | | 204. Signs out the user's sessions. 404 `user-not-found`. |
| `POST /token` | `{"user":"<username or id>","create":false}`, or `{"user":null}` (or no body) for the developer identity | `{"accessToken","tokenType":"Bearer","expiresIn","user":user}`: an access token for calls a host makes as that user (`create: true` creates a missing user, without a password). It has no refresh token; changing the identity or deleting the user ends it. |
| `GET /entities` | | `{"entities":[{"name":"Note","rows":12}, …]}`, in the model's order; empty when `services.data` is off. |
| `GET /entities/{name}/rows?offset=0&limit=50&orderBy=<field>&desc=false` | | `{"columns":[{"name","type","key":bool}],"rows":[{…}],"total":12}`. Every row, regardless of permissions; values formatted as the data API formats them. `columns` are the entity's fields with their GraphQL type (`UUID`, `String`, `Int`, `Decimal`, `Boolean` or `DateTime`); `key` marks the primary key. Rows are ordered by `orderBy`, then by the primary key. `limit` is 1 to 500 (default 50). 400 for other values or an unknown `orderBy`; 404 `entity-not-found`. |
| `DELETE /entities/{name}/rows` | | `{"deleted":12}`. 409 `rows-referenced` when other entities' rows still refer to them; the message says which, and how many. |
| `POST /data/reset` | | `{"ok":true}`. Deletes every entity's rows, the local users and their sessions, and the stored files, without a restart. The identity stays: a `user` identity's user is created again at its next automatic sign-in. |
| `POST /reload` | `{"forceSchema":false}` | Reads `rayfin.yml`, `rayfin-local.toml`, the entities, the SQL connectors' entities and the secret values again and applies them now, as after an edit, running the data build command (`services.data.buildCommand`) first, as a start does (`forceSchema`: also a schema change that loses data). Answers `{"schema":<schema event>,"config":<config-changed event or null>}`, the events it also emits; a change that would lose data answers 200 with a `refused` `schema` event. A reload in progress finishes first, and one asked for before `ready` waits for the start. A `rayfin.yml` or `rayfin-local.toml` that can't be read answers 422 `config-invalid`, and entities that can't be built or analyzed 422 `analysis-failed`; what could be read is applied all the same. 503 `stopping` once `serve` stops. |

A <a id="users"></a>**user** is `{"id","username","email","createdAt","signIn":["password","entra"],"developer":bool}`:

- `email` is `null` when the username isn't an email address.
- `createdAt` is RFC 3339, UTC, with milliseconds.
- `signIn` lists how the user signs in: `password`, `entra` (Microsoft Entra ID), or neither for users only rayfin-local signs in.
- `developer` is `true` for your own accounts: `developer@example.com` and Entra users (locally, only you sign in with Entra).

## Calling the app's backend as a user

Hosts (and the agents they run) call the app's own routes with a token from `POST /token` and the publishable key from `GET {apiUrl}/rayfin.config.json`:

```http
POST {apiUrl}/graphql
Authorization: Bearer <accessToken>
X-Publishable-Key: <publishableKey>
Content-Type: application/json

{"query":"{ notes(first: 5) { items { id title } } }"}
```

The same works for `POST /functions/<name>/invoke`, `POST /connector-invoke/<name>` and `POST /connectors/<name>/graphql`.
Leave out `Authorization` to call as an anonymous user.

## Local secret values

`rayfin-local secret set <NAME>` reads the value from stdin when it isn't given as an argument; `secret delete <NAME>` removes it.
`rayfin-local secret list --json` prints `{"secrets":[{"name","declared":true,"description":null,"source":"env"}]}` with `source` one of `env`, `local` (`.rayfin-local/secrets.json`), `settings` (`local.settings.json`) or `none`; values are never printed.
A running `serve` picks up a change made with `secret set` or `secret delete` by itself and restarts only its function host (a `config-changed` event with `"what": ["secrets"]` follows), so hosts don't restart it.

## Doctor

`rayfin-local doctor --json -C <project>` prints `{"ok":true,"checks":[{"id":"node","status":"ok","message":"Node.js 24.13.0 (…)","hint":null}, …]}` and exits 0 when no check failed.
`status` is `ok`, `warn` or `fail`; `id` is one of `node`, `project`, `packages`, `sdk`, `state`, `azure`, `port-backend`, `port-frontend`, each at most once.
A check with several findings (the project's warnings) has the worst status, and the further findings in `hint`, one per line.
`packages`, `sdk` and `state` are left out when the project isn't found.
