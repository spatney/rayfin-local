# `basic`: the end-to-end fixture app

A small Rayfin app on SDK and CLI 1.36.2 that uses every service `rayfin-local serve` emulates.
It is modeled on the reference app (`fabric-apps/apps/explorer`): the same packages, `vite.config.ts`
with `rayfinLocalDev({ autoLogin: true })`, a client bootstrapped from `import.meta.env.VITE_*`
(`src/services/bootstrap.ts`), `ConnectorsRayfinClient` and `ensureSignedInWithFabric`.

| Part | Where | What it exercises |
| --- | --- | --- |
| Data | `rayfin/data` | Six entities: every scalar decorator, `@one`/`@many`, role rules and policies. |
| Storage | `rayfin/storage` | `@blob` folders `todo-attachments` (fields, content types, overwrite) and `user-avatars`. |
| Auth | `rayfin/rayfin.yml` | Fabric sign-in (the popup) and passwords; localhost ports 5173 and 7171 as redirect origins. |
| Functions | `rayfin/functions` | `ping`, `greet`, `echo`, `whoAmI`, `countTodos` (data as the caller), `tokenInfo` (`AudienceType.Fabric`), `secretInfo` (`GREETING_SECRET`), `fail` (throws). |
| Connector | `rayfin/rayfin.yml` | `salesmodel`: `fabric-semanticmodel`, delegated, `executeQuery`, placeholder workspace and item IDs. |
| Frontend | `index.html`, `src/` | One button per scenario, below. |

`crates/rl-model/tests/fixtures/basic.analysis.json` is recorded from `rayfin/data` and
`rayfin/storage`, and that analysis compiles every file under `rayfin/` except `functions/`.
So keep those files as they are, add nothing else under `rayfin/` outside `functions/` (the
connector schema lives in `src/services/rayfinClient.ts`), and keep the functions project free
of a project reference to `rayfin/` (a `tsc --build` would write `rayfin/.temp`).

## Setup

```powershell
npm ci; npm ci --prefix rayfin/functions
```

`npm run build` builds the frontend (`dist`); in `rayfin/functions` it builds the functions
(`dist`). `npm run typecheck` type-checks the frontend (`tsconfig.app.json`).

## Scenarios

Run `rayfin-local serve -C tests/fixtures/apps/basic` and open the frontend. Once
`[data-testid=app-status]` reads `ready`, click `run-<name>`; `<pre data-testid="result-<name>">`
gets `data-status="running"`, then `ok` or `error`, and the JSON
`{"ok": true, "value": ...}` or `{"ok": false, "error": {name, message, code?, status?}}`.

| Name | Does |
| --- | --- |
| `config` | Shows the API URL and the Fabric sign-in options from the env. |
| `sign-in`, `sign-out`, `session` | `ensureSignedInWithFabric` (the popup at `<api>/.rayfin-local/portal`), `signOut`, `getSession`: `{isAuthenticated, user}`. |
| `auto-sign-in` | What apps from Rayfin's templates do when they load: `fetchRayfinLocalSessionToken()` from the dev server, then `signInWithBrokeredToken`, with no popup: `{isAuthenticated, user}`. |
| `todo-crud` | Creates, lists, updates and deletes a Todo. |
| `todo-paginate` | Three Todos, two per page: `[{titles: ["1","2"], hasNextPage: true}, {titles: ["3"], ...}]`. |
| `todo-category` | A Todo with `category.name`, and the Category with `todos.title`. |
| `todo-aggregate` | `groupBy(['status']).aggregate(...)`: `[{status: "doing", todos: 2, totalCost: 4}, {status: "done", todos: 1, totalCost: 4}]`. |
| `fn-ping`, `fn-greet`, `fn-echo`, `fn-echo-null`, `fn-who-am-i`, `fn-count-todos`, `fn-token-info`, `fn-secret-info`, `fn-fail` | Invoke the functions. |
| `connector-query` | `salesmodel.executeQuery` with `EVALUATE ROW("answer", 42)`. |
| `storage-roundtrip` | Uploads `fixture-note.pdf` to `todo-attachments`, lists, downloads, deletes it. |

The data, function and storage scenarios need a signed-in user: the Todo create policy requires
`user_id` (set to the session's `user.id`) to equal the token's `sub`. Data scenarios title their
rows with a tag unique to the run, report titles without it and delete what they create.

- `fn-secret-info` reports `{matchesFixture: true}` when the function sees `GREETING_SECRET` from
  `rayfin/functions/local.settings.json` (`local-secret-value`, a test value, not a secret).
- `fn-token-info` reports only `fabricTokenLength`; set `RAYFIN_LOCAL_TOKEN_FABRIC` to any string
  to avoid a real sign-in.
- `connector-query` goes to Power BI. Point it at a mock with a `rayfin-local.toml` at the
  project root and a fake token in `RAYFIN_LOCAL_TOKEN_POWERBI`:

  ```toml
  [connectors]
  powerbi_base_url = "http://127.0.0.1:9000"
  ```

## Generated files

`rayfin/functions/src/types.ts`, `src/secrets.generated.ts` and `runtimemetadata.json` were
written by the Rayfin CLI 1.36.2 generator; `js/udf-metadata.mjs` writes an identical
`runtimemetadata.json`. After changing `function_app.ts` or the `secrets` in `rayfin.yml`, run
this from the fixture root (it works locally and does not contact Fabric):

```powershell
node --input-type=module -e "const m = await import('./node_modules/@microsoft/rayfin-cli/dist/utils/functions-types-generator.js'); await m.generateFunctionsTypes(process.argv[1])" "$PWD/rayfin/functions"
```
