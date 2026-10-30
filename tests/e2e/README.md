# End-to-end tests

Playwright drives real apps against `rayfin-local serve` in Chromium:

| Suite | Spec | App | Runs |
| --- | --- | --- | --- |
| `fixture` | `fixture.spec.ts` | `tests/fixtures/apps/basic` | Always. |
| `explorer` | `explorer.spec.ts` | the reference app, `fabric-apps/apps/explorer` | Only with `RAYFIN_LOCAL_E2E_EXPLORER=1`. |

`portal/` is a separate test of the sign-in popup alone, run by `cargo test -p rl-auth` (see its README).

## Run

```powershell
cd tests/e2e
npm install
npx playwright install chromium   # or set PLAYWRIGHT_CHANNEL=msedge to use the installed Edge
npm run test:fixture              # the fixture suite
npm run test:explorer             # the explorer suite (sets RAYFIN_LOCAL_E2E_EXPLORER=1)
npm test                          # both; explorer is skipped unless RAYFIN_LOCAL_E2E_EXPLORER=1
```

The global setup runs `cargo build -p rayfin-local` first. The fixture suite runs `npm ci` in the
fixture and its `rayfin/functions` when they have no `node_modules`; the explorer suite expects
the reference app's dependencies to be installed already.

Results: `test-results/` (a failed test keeps `trace.zip`, screenshots and the browser log; open a
trace with `npx playwright show-trace <path>`), and `test-results/logs/<suite>-serve.log` with
everything `rayfin-local serve --verbose` printed.

## Environment

| Variable | Default | Effect |
| --- | --- | --- |
| `RAYFIN_LOCAL_BIN` | `target/debug/rayfin-local(.exe)` | The binary to test; setting it skips the build. |
| `RAYFIN_LOCAL_SKIP_BUILD` | | `1`: don't run `cargo build -p rayfin-local`. |
| `RAYFIN_LOCAL_E2E_EXPLORER` | | `1`: run the explorer suite. |
| `EXPLORER_DIR` | `../fabric-apps/apps/explorer` next to this repo | The reference app. |
| `PLAYWRIGHT_CHANNEL` | Playwright's Chromium | `msedge` or `chrome`: an installed browser. |
| `HEADED` | | `1`: show the browser. |

The fixture suite sets `RAYFIN_LOCAL_TOKEN_POWERBI` and `RAYFIN_LOCAL_TOKEN_FABRIC` to fake
tokens for its `serve`, so it needs no Azure sign-in. The explorer suite uses your real tokens
(`az login`), because its connector queries the real semantic model.

## How it works

- **One `serve` per suite** (`support/serve.ts`), started on free ports with `--reset`, so every
  run starts from an empty database. It is ready once `GET <api>/.rayfin-local/health` and the
  frontend URL from the startup banner answer. Stopping kills the process tree
  (`taskkill /T /F` on Windows) and checks that both ports are free again.
- **Suites start in the Playwright runner** (`support/suites.ts`), on the first test that needs
  them, and stop in the global teardown. So they survive Playwright replacing its worker after a
  failed test, and die with the runner if it is killed (Windows ends `serve` with the runner's job
  object). The next run kills any `serve` left over and deletes a `rayfin-local.toml` it wrote.
- **Another `serve` of the same app** (answering on the port in its `.rayfin-local/state.json`)
  fails the suite with a message to stop it: `--reset` can't delete a database that is in use.
- **One browser page per worker** (`support/fixtures.ts`), so the app stays signed in from one
  scenario to the next; a new worker signs in again. Every test checks that it caused no console
  errors, uncaught exceptions, failed requests or HTTP errors beyond the ones it expects.
- **Sign-in** (`support/portal.ts`) clicks through the emulated Fabric popup: the run's first
  sign-in signs up `e2e-<random>@example.com`, later ones sign in.

## The fixture suite

Clicks each scenario button of the fixture app (see its README) and checks the result, in order:

| Test | Checks |
| --- | --- |
| `config` | `VITE_RAYFIN_API_URL` is the backend (or the dev server, which proxies it), the portal URL is the backend's `/.rayfin-local/portal`, and the publishable key the app was served is the one in `.rayfin-local/state.json`. |
| `sign-in` | The popup opens at `<api>/.rayfin-local/portal/secureItemEmbed`; a new user signs up there; the app gets a session for that email. |
| `session` | `getSession()` has the user. |
| `todo-crud`, `todo-paginate`, `todo-category`, `todo-aggregate` | GraphQL through the SDK: the exact values the fixture's README lists. |
| `fn-ping` … `fn-secret-info` | Each function's return value; `fn-who-am-i` gets the caller's token, `fn-token-info` the length of `RAYFIN_LOCAL_TOKEN_FABRIC`, `fn-secret-info` the value in `local.settings.json`. |
| `fn-fail` | The app gets an error with the function's message. |
| `connector-query` | `salesmodel.executeQuery` returns the mock's answer (`support/mock-powerbi.ts`), and every request the mock got was for the connector's dataset, with the query and `RAYFIN_LOCAL_TOKEN_POWERBI` as its bearer token. |
| `storage-roundtrip` | Upload with fields, list, download, delete. |
| `sign-out` | The session ends. |
| `auto-sign-in` | The app signs itself in as Rayfin's templates do (`fetchRayfinLocalSessionToken()`, then `signInWithBrokeredToken`): the dev server answers for the developer (`developer@example.com`, since the suite's Power BI token is fake), no popup opens, and the session works for data. |

The mock Power BI answers `executeDaxQueries` with the Arrow stream rl-connectors recorded for
`EVALUATE ROW("x", 1)` (`crates/rl-connectors/tests/fixtures/row.arrows`) and `executeQueries`
with Power BI's JSON for the same result. The suite points the connector at it with a
`rayfin-local.toml` in the fixture (git-ignored), which the teardown deletes again.

## The explorer suite

Signs in through the popup, then opens each page (`/`, `/features`, `/adoption`, `/ask`,
`/settings`, `/new`) and checks that it shows its content, that no call to the backend
(`/graphql`, `/functions`, `/connector-invoke`, `/api`, the dev server's function proxy, the
portal) failed or answered 400 or more, and that it shows no error banner ("Couldn't load …",
alerts). Other console errors are not checked: the app is not ours.

The teardown deletes the app's `.rayfin-local` folder (a `.rayfin-local` from before the run is
moved aside and put back, also by the next run if this one is killed) and fails if `git status`
shows changes in the app that were not there before the run.

## Changing the harness

`npm run typecheck` type-checks it (Playwright itself only strips the types).
