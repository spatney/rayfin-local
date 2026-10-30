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
| `RAYFIN_LOCAL_CONTROL_TOKEN` | Turns on the [control API](#control-api); requests must send it as a bearer token. At least 32 characters; shorter values are refused with a fatal `config-invalid` error. An empty value counts as unset. |
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
After `ready`, live changes emit `analysis`, `schema`, `functions`, `vite` and `config-changed` again.
The last event is always `stopped`.
A failure before the project is found and read (`project-not-found`, an invalid `rayfin.yml`) emits only `error` and `stopped`.

### `starting`

```json
{"v":1,"type":"starting","time":"…","pid":4242,"version":"0.2.0","project":{"root":"C:\\apps\\notes","name":"notes"}}
```

### `analysis`

The app model was analyzed (at start, and after each live change of the entity sources).

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

### `vite`

```json
{"v":1,"type":"vite","time":"…","state":"ready","url":"http://localhost:5173/","version":"8.0.16","message":null}
```

`state` is `ready`, `exited` (it stopped on its own; `message` says how), `restarting` (rayfin-local is starting it again, with backoff) or `failed` (it could not start, or kept exiting; `message` says why).
The URL never changes while `serve` runs: a restarted Vite takes the same port.

### `ready`

Everything started.
Hosts show the app once they see this event.

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
- `connectors[].supported` is `false` for connector types rayfin-local answers with 501.

### `config-changed`

`rayfin.yml`, `rayfin-local.toml` or the secret values changed while serving.

```json
{"v":1,"type":"config-changed","time":"…","applied":true,"restartRequired":false,"what":["services","connectors"],"message":null}
```

- `applied: true`: the services were rebuilt on the same ports, and a new `ready` event follows with the new features.
- `restartRequired: true`: the change can't be applied while running (for example the frontend's folder moved); `message` says what.
  Hosts restart `serve`.

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
| `config-invalid` | error | `rayfin.yml`, `rayfin-local.toml`, `.rayfin-local/state.json`, the functions' folder, `package.json` or `local.settings.json`, or the control token is invalid. |
| `packages-missing` | error | The project's npm packages (or the functions') aren't installed. `details`: `{"dir"}`, the folder to run `npm install` in. For the functions' packages it is not fatal: it comes before their build, which then fails. |
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
Under rayfin-local, the dev server answers that route itself, for the **sign-in identity**, without any Entra sign-in in the browser:

| Identity | `--sign-in-as` | Signs the app in as |
| --- | --- | --- |
| `developer` (default) | `developer` | You: your Entra identity from the Azure CLI or the Rayfin CLI's login when one is available without a prompt, else a local account, `developer@example.com`. |
| `user` | `user:<username>` | That local user (created when missing). |
| `none` | `none` | Nobody: the route answers 401 `{"error":"Automatic sign-in is off.","hint":"…"}`. |

The answer has the shape `fetchRayfinLocalSessionToken()` expects: `{"accessToken","tokenType","expiresIn","refreshToken","scope"}`.
Changing the identity (through the control API) revokes every local session, so an app that reloads signs in again as the new identity.
The backend answers the same route at `GET /.rayfin-local/dev/session-token` (loopback only) for apps whose dev server rayfin-local doesn't start.

## Control API

With `RAYFIN_LOCAL_CONTROL_TOKEN` set, the backend serves `{apiUrl}/.rayfin-local/control/*`.

- Only loopback peers are answered.
- Every request needs `Authorization: Bearer <RAYFIN_LOCAL_CONTROL_TOKEN>`; otherwise 401.
- Requests with an `Origin` header (from a browser) get 403.
- Bodies are JSON.
  Errors are `{"error":{"code","message","hint"}}` with a 4xx or 5xx status.
- Without the variable, these routes answer 404.

| Method and path | Body | Answer |
| --- | --- | --- |
| `GET /status` | | The `ready` payload (without `v`, `type`, `time`) plus `"state":"running"`, `"schema"` (the last `schema` event), `"functions"` (the last `functions` event) and `"vite"` (the last `vite` event). |
| `GET /identity` | | `{"kind":"developer","user":{…}}`, `{"kind":"user","user":{…}}` or `{"kind":"none","user":null}`. `user` is the [user](#users) the identity signs in as (for `developer`, `null` until first used). |
| `PUT /identity` | `{"kind":"developer"}`, `{"kind":"user","user":"<username or id>","create":true}` or `{"kind":"none"}` | The new identity, as `GET /identity`. Revokes every local session. |
| `GET /users` | | `{"users":[user, …]}` |
| `POST /users` | `{"username":"ana@example.com","password":"…"}` (`password` optional: without one the user can only be signed in through this API) | `user`, 201. 409 when the username exists. |
| `DELETE /users/{username or id}` | | 204. Signs out the user's sessions. |
| `POST /token` | `{"user":"<username or id>","create":false}`, or `{"user":null}` for the developer identity | `{"accessToken","tokenType":"Bearer","expiresIn","user":user}`: an access token for calls a host makes as that user (`create: true` creates a missing user). |
| `GET /entities` | | `{"entities":[{"name":"Note","rows":12}, …]}` |
| `GET /entities/{name}/rows?offset=0&limit=50&orderBy=<field>&desc=false` | | `{"columns":[{"name","type","key":bool}],"rows":[{…}],"total":12}`. Every row, regardless of permissions; values formatted as the data API formats them. `limit` is at most 500. |
| `DELETE /entities/{name}/rows` | | `{"deleted":12}`. 409 when other entities' rows still refer to them. |
| `POST /data/reset` | | `{"ok":true}`. Deletes every entity's rows, the local users and their sessions, and the stored files, without a restart. |
| `POST /reload` | `{"forceSchema":false}` | Reads `rayfin.yml`, the entities and the secret values again and applies them now. Answers `{"schema":<schema event>,"config":<config-changed event or null>}`. |

A <a id="users"></a>**user** is `{"id","username","email","createdAt","signIn":["password","entra"],"developer":bool}`; `email` is `null` when the username isn't an email address.

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
A running `serve` picks up a change made with `secret set` or `secret delete` by itself (a `config-changed` event follows), so hosts don't restart it.

## Doctor

`rayfin-local doctor --json -C <project>` prints `{"ok":true,"checks":[{"id":"node","status":"ok","message":"Node.js 24.13.0 (…)","hint":null}, …]}` and exits 0 when no check failed.
`status` is `ok`, `warn` or `fail`; `id` is one of `node`, `project`, `packages`, `sdk`, `state`, `azure`, `port-backend`, `port-frontend`, each at most once.
A check with several findings (the project's warnings) has the worst status, and the further findings in `hint`, one per line.
`packages`, `sdk` and `state` are left out when the project isn't found.
