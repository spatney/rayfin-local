# SQL connectors (Category A)

This note is the design of rayfin-local's Category A connectors: `fabric-sqldatabase`, `fabric-warehouse` and `fabric-sqlanalytics`.
It covers the contract the SDK expects, how the GraphQL schema derives from the connector's files, how rayfin-local finds and signs in to the database, and where it differs from Fabric.
The sources of truth are the SDK 1.36.2 packages: `@microsoft/rayfin-connectors` (`dist/category-a/`), `@microsoft/rayfin-connector-fabric-graphql` (its `assets/docs/`), `@microsoft/rayfin-core` (`ConnectorSchemaAnalyzer`, `ConnectorConfigGenerator`) and `@microsoft/rayfin-cli` (`connector add`, `rayfin up connector apply`).

## Contents

- [What they are](#what-they-are)
- [The contract](#the-contract)
- [The schema](#the-schema)
- [How a request runs](#how-a-request-runs)
- [Finding and signing in to the database](#finding-and-signing-in-to-the-database)
- [Identity](#identity)
- [Not supported yet](#not-supported-yet)
- [Code and tests](#code-and-tests)

## What they are

A Category A connector exposes the tables of an existing Fabric database as typed entities, through a DAB-style GraphQL API.

| Type | Fabric item | Writes | What a write returns |
| --- | --- | --- | --- |
| `fabric-sqldatabase` | SQL database | Yes | The written row, read back (DAB's MSSQL engine). |
| `fabric-warehouse` | Warehouse | Yes | `DbOperationResult { result: "success" }` (DAB's DWSQL engine has no `OUTPUT` clause). |
| `fabric-sqlanalytics` | A Lakehouse's SQL analytics endpoint | No | Read-only. |

A connector lives in `rayfin.yml` (`connectors[]`: `name`, `type`, `config.workspaceId`, `config.itemId`, `auth.type`, `operations`) and in `rayfin/connectors/<name>/`:

- `metadata.json`: the source's catalog that `rayfin connector add` discovered (`schemas[].tables[].columns[]`, keys, foreign keys, server-generated columns) and its server.
- One entity class per table, `@entity()` classes that extend `Source({ schema, table, primaryKey })`, with the usual field decorators (`column:` maps a property to its SQL column), `@one`/`@many` with `sourceFields`/`targetFields`, and `@role`.
- `schema.ts`: the `connectorConfig` the app passes to its client (`connector`, `operations`, `entities`).

On Fabric, `rayfin up` analyzes the entity classes into a DAB config and posts it to the Fabric GraphQL workload, which serves `/connectors/<name>/graphql` over the item.
Locally, rayfin-local serves that route itself, over the same item.

## The contract

### Requests

`client.connectors.<name>.<Entity>` is a `ConnectorEntityClient`; it posts to `POST {apiUrl}/connectors/<name>/graphql` with rayfin-data's `GraphQLClient`.

- Headers: `X-Publishable-Key`, and `Authorization: Bearer <session token>` when a user is signed in.
- Body: `{"query": "<document>"}`: one anonymous operation with one root field, literals inline, no variables.
- Documents:

| Call | Document |
| --- | --- |
| `select(...).where(...).orderBy(...).first(n).after(c).execute()`, `findMany`, `findFirst` | `<lowerFirst(P)>(filter: …, orderBy: …, first: n, after: "c") { items { … } }`, plus `hasNextPage endCursor` for `executePaginated()` |
| `findByKey(where, select?)` | `<lowerFirst(E)>_by_pk(<key>: …, …) { … }`, one argument per key column |
| `create(item)` | `create<E>(item: { … }) { <columns> }`, or `{ result }` on a Warehouse |
| `update(where, item)` | `update<E>(<key>: …, item: { … }) { … }` |
| `delete(where)` | `delete<E>(<key>: …) { … }` |
| `groupBy(...)`, `aggregate(...)` | `<lowerFirst(P)> { groupBy(…) { … } }` |

- Selections: `select(['category.name', 'orderItems.quantity'])` becomes `category { name }` for a `@one` and `orderItems { items { quantity } }` for a `@many`; the SDK reads the cardinality from the entity classes in `connectorConfig.entities`.
- Without a selection, `findMany`, `findFirst` and `findByKey` select the entity's scalar columns from `connectorConfig.entities`.
- `operations` in `connectorConfig` gates the client's methods before any request (`OPERATION_NOT_ALLOWED`).

### Answers

The answers are DAB's, as for the data API (`/graphql`):

- The data shapes: `{ "data": { "<field>": { "items": [...], "hasNextPage": …, "endCursor": … } } }` for lists, an object or `null` for `_by_pk`, `{ "items": [...] }` for to-many relationships.
- Values: UUIDs in lowercase, dates as `YYYY-MM-DDTHH:MM:SS.fffZ` (UTC), decimals and integers as numbers, booleans as `true`/`false`.
- Cursors: DAB's base64 JSON of the order-by values, then the key columns.
- Errors: Hot Chocolate's envelope with DAB's messages and codes (`AUTH_NOT_AUTHORIZED`, `AuthorizationCheckFailed`, `ItemNotFound`, `DatabasePolicyFailure`, `DatabaseOperationFailed`, …), with HTTP 200.
- HTTP statuses: 400 for an unreadable request, 401 `invalid_token` for a bad token, 401 `authentication_required` for a token-less request that needs a signed-in user, 403 for a missing publishable key (checked before the route).
- The SDK throws on `errors` (`GraphQL errors: <message>`); mutations that answer `null` throw `MUTATION_NO_RESULT`.

## The schema

rayfin-local builds the schema DAB generates from the DAB config `rayfin up` would apply, so the same documents validate and resolve.

1. `js/analyze.mjs` (connectors mode) compiles `rayfin/` with the project's TypeScript and `rayfin/tsconfig.json`, imports each connector's modules, and runs the project's `ConnectorSchemaAnalyzer` and `ConnectorConfigGenerator` with the type's dialect (`mssql`, `fabric-warehouse` or `fabric-sqlanalytics`), as `rayfin up connector apply` does.
   It also serializes each `@role` policy's expression tree, as for data entities.
   The result is cached in `.rayfin-local/cache/connectors-analysis.json` while the sources are unchanged.
2. `rl-model` turns each connector's analysis into an `AppModel` (`AppModel::from_connector_analysis`): entity `E`, its DAB source `schema.table`, its columns (`mappings` give the exposed names), its key (`keyFields`: any columns, or none), its relationships and its permissions.
3. `metadata.json` completes the columns (`AppModel::apply_source_metadata`): the database's SQL type, whether the database generates the column (identity, computed, `rowversion`, temporal period columns) and whether it has a default.
4. The grants are limited to the connector's operations (`rl_connectors::sql::restrict_to_operations`).

While `serve` runs, a change to the connector's entity classes, its `metadata.json` or the connectors in `rayfin.yml` runs these steps again and rebuilds the connectors' routes in place, on the same port.
Hosts see a `config-changed` event with `"what": ["connectors"]`, then a new `ready`.

The schema then follows the data API's ([`rl-graphql`'s `schema.rs`](../crates/rl-graphql/src/schema.rs)), with these differences:

- **Names**: `E` is the entity name and `P` the analyzer's plural (the same `pluralize` the SDK uses): `appointments` and `appointments_by_pk` for an entity `Appointments`, `products` and `product_by_pk` for `Product`.
- **Keys**: `_by_pk`, `update<E>` and `delete<E>` take one argument per key column, typed by its decorator; a keyless entity (`primaryKey: []`) has none of them.
- **Inputs**: columns the database generates are left out of `Create<E>Input` and `Update<E>Input`, and columns with a database default are optional on create.
- **Mutation results**: `E` on a SQL database; `DbOperationResult { result: String }` on a Warehouse; no `Mutation` type on a SQL analytics endpoint.
- **Operations**: `operations` in `rayfin.yml` is the connector's ceiling (all the type's operations when none are listed): a `*` grant becomes those actions, and grants beyond them are dropped with a warning at startup, since `rayfin up` refuses such a config.
- **Scalars** follow the decorators (`@int` is `Int`, `@decimal` is `Decimal`, `@date` is `DateTime`), not the database's types.

`@role` grants, `include`/`exclude` field lists and row policies work as on data entities: the roles are `anonymous` and `authenticated`, and policies compare the caller's claims (`sub`, `email`, `role`) with the row's fields.

## How a request runs

```mermaid
sequenceDiagram
  participant App as App (Rayfin SDK)
  participant API as rayfin-local: /connectors/<name>/graphql
  participant Fabric as Fabric REST API
  participant DB as Fabric SQL (TDS)

  App->>API: POST {"query": "{ products(first: 20) { items { name } } }"}
  API->>API: caller from the bearer token; connector; delegated needs a signed-in user
  API->>API: parse, validate, plan (roles, field permissions, policies)
  API->>Fabric: GET workspaces/{ws}/sqlDatabases/{id} (first use; then cached)
  API->>DB: login with your Entra token, then one T-SQL statement per root field
  DB-->>API: one JSON text per row
  API-->>App: DAB's answer
```

- **Front end**: the data API's parser, validation (Hot Chocolate's rules and messages), planner and policy compiler (`rl-graphql`), on the connector's schema.
- **T-SQL** (`rl-graphql`'s `tsql` module): each root field is one statement that builds its rows' JSON with string functions (`STRING_ESCAPE`, `CONVERT`), as DAB's DWSQL builder does, because Warehouses and SQL analytics endpoints refuse `FOR JSON` in subqueries.
  Relationships are `OUTER APPLY` subqueries (`STRING_AGG … WITHIN GROUP` for to-many ones), filters, policies and cursors are `WHERE` clauses, and every value is a parameter of the type of the column it meets.
- **Values** come back as SQL Server formats them and are rewritten to DAB's (dates to `…fffZ`, `datetimeoffset` to UTC); the cursor keeps the database's precision.
- **Mutations** run in a transaction.
  On a SQL database, `INSERT … OUTPUT INSERTED.<key>` (or `UPDATE … OUTPUT …`) returns the key, and the row is read back with the read grant and policy; a delete reads the row first.
  The action's policy is in the write's `WHERE`; a create's is checked on the candidate row, as for data entities.

## Finding and signing in to the database

### Which item, which server

rayfin-local resolves the item in `rayfin.yml` to its TDS server and database with the Fabric REST API, as `rayfin connector add` does (CLI 1.36.2 `SqlEndpointManager`):

| Type | `config.itemId` that `connector add` records | Request | Server | Database |
| --- | --- | --- | --- | --- |
| `fabric-sqldatabase` | The SQL database | `GET /v1/workspaces/{ws}/sqlDatabases/{id}` | `properties.serverFqdn` | `properties.databaseName` |
| `fabric-warehouse` | The warehouse | `GET /v1/workspaces/{ws}/warehouses/{id}` | `properties.connectionString` | The item id |
| `fabric-sqlanalytics` | The Lakehouse's SQL analytics endpoint: `connector add` resolves a Lakehouse id to its endpoint's and writes that back | `GET /v1/workspaces/{ws}/sqlEndpoints/{id}/connectionString` | `connectionString` | The item id |

A Lakehouse id written by hand for `fabric-sqlanalytics` is followed to its endpoint (`GET …/lakehouses/{id}`, `properties.sqlEndpointProperties`).
The answer is cached in `.rayfin-local/cache/connectors/<name>.json` for that type, workspace and item; delete the file to look it up again.

### Signing in

- The Fabric REST API gets your token for `https://api.fabric.microsoft.com/.default` (the `Fabric` audience).
- The database gets your Microsoft Entra token for `https://database.windows.net/.default` (the `Sql` audience) at login (TDS `FEDAUTH`).
  `[tokens.scopes] Sql` in `rayfin-local.toml` changes it; Fabric SQL endpoints also accept the Power BI audience, which the Rayfin CLI uses for discovery.
- Tokens come from the token broker: the Azure CLI, the Rayfin CLI login, or `RAYFIN_LOCAL_TOKEN_SQL` and `RAYFIN_LOCAL_TOKEN_FABRIC`.
- The connection is TDS 7.4 through `tiberius` 0.12, encrypted with rustls on ring, with the server certificate checked against the platform's roots; a redirect to another gateway is followed once.
  (`tiberius` 0.13's rustls feature takes `tokio-rustls` 0.26 with its default `aws-lc-rs` provider, which this project never builds.)
- Each connector has a pool of up to 4 connections; idle ones close after 5 minutes and every one after 45.
  Logins time out after 40 seconds and statements after 120.

### When it fails

Failures answer HTTP 200 with a GraphQL error whose message says what to do, and `serve` logs the same line:

| Failure | Message |
| --- | --- |
| No token (`az login` missing or expired) | The connector needs your Azure sign-in, the token source's reason, and its hint (run `az login`). |
| The item isn't found or you can't open it (Fabric REST 403/404) | Check `config.workspaceId` and `config.itemId`, and ask the workspace's admin for access. |
| The database refuses your login (18456, 4060, 916) | rayfin-local connects as you: ask the item's owner for access, or `az login` with the account that has it. |
| A firewall rule refuses your address (40615) | Connect from an allowed network, or ask the owner to allow your address. |
| The database is busy, paused or unavailable (40613, 40501, …) | Try again; a paused Fabric capacity must be resumed first. |
| The server can't be reached (network, DNS, TCP 1433 blocked) | Check the network, VPN and firewall. |
| A statement fails in the database (an entity out of step with its table, a constraint) | DAB's `DatabaseOperationFailed` error, with the server's message as `extensions.detail` (for example `Invalid column name 'zip'`). |
| The entities can't be analyzed (a TypeScript error) | 500 `CONNECTOR_MISCONFIGURED` with the analyzer's message, and a warning when they are analyzed (at startup, and after each change). |

## Identity

On Fabric, a connector's queries run as the app (`auth: application`, the default for Category A) or as the signed-in user (`auth: delegated`, on-behalf-of).
Locally they always run as you, the developer, with your Entra token:

- You read and write what your account can reach, which may be more or less than the app or its users can.
- Security the database itself enforces (permissions, row-level security policies) applies to you, not to the app's user.
- The app's own rules still apply to the app's signed-in user: `@role` grants, field lists and policies are enforced by rayfin-local before the database sees a statement.
- A `delegated` connector still needs a signed-in user (401 `DELEGATED_SESSION_EXPIRED` otherwise); an `application` connector serves the roles its entities grant, `anonymous` included.

Keep `serve` on loopback (the default): anyone who can reach the backend acts on these databases as you.

## Not supported yet

- **Aggregations** (`groupBy`, `aggregate`) on connector entities answer an error that names them.
- **A create on a keyless entity** of a SQL database answers `null` (there is no key to read the row back by), so the SDK reports `MUTATION_NO_RESULT`; the row is written.
- **Scalars** follow the decorators, so a `bigint` column decorated `@int` refuses values beyond 32 bits, where DAB on Fabric exposes `Long`.
- **`rayfin up`'s checks** of connector configs (reserved and duplicate entity names across connectors) are not repeated.

## Code and tests

| Where | What |
| --- | --- |
| `js/analyze.mjs` (`mode: "connectors"`) | The connector analysis with the project's `@microsoft/rayfin-core`. |
| `crates/rl-model/src/convert.rs`, `connector.rs` | The connector model and `metadata.json`. |
| `crates/rl-graphql/src/tsql/` | The T-SQL executor, emitter and mutations, and the `SqlBackend` trait. |
| `crates/rl-connectors/src/sql/` | The route, operations, location (`location.rs`) and the TDS pool (`tds.rs`). |
| `crates/rayfin-local/src/connectors.rs` | The analysis at startup and after changes, its cache and warnings. |

- `crates/rl-model/tests/connector_model.rs` and `analyze_js.rs`: the model of the `inventory` connector of `tests/fixtures/apps/connectors`, and the helper against its golden analysis.
- `crates/rl-graphql/tests/tsql.rs`: the schema of that connector per type, and the statements its documents become, on a fake database.
- `crates/rl-connectors/tests/sql_server.rs`: the routes against a real SQL Server (queries, roles and policies, composite and keyless keys, mutations per type, failures).
- `crates/rl-connectors/tests/sql_sdk.rs`: the real SDK 1.36.2 client with the fixture's entity classes against the same.
- `crates/rl-connectors/tests/real_fabric_sql.rs` (ignored): a read-only query of a real Fabric SQL database as you.

[development.md](development.md#sql-server-tests) has the commands.
