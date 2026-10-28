// Runs the real Rayfin SDK 1.36.2 aggregation chain (`where`/`groupBy`/`aggregate`/`execute`)
// against rayfin-local's /graphql, on the basic model's todos (the explorer app has no numeric
// fields). Started by tests/sdk.rs, which seeds the database and passes the settings in
// RL_SDK_TEST (JSON).
import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';

const cfg = JSON.parse(process.env.RL_SDK_TEST ?? '{}');
const load = (path) => import(pathToFileURL(`${cfg.nodeModules}/${path}`).href);
const { RayfinClient } = await load('@microsoft/rayfin-client/dist/index.js');

// ApiClient logs every POST; keep the output to the results.
console.log = () => {};
const print = (line) => process.stdout.write(`${line}\n`);

const client = () => new RayfinClient({ baseUrl: cfg.api, publishableKey: cfg.publishableKey });
const alice = client();
await alice.auth.signIn({ email: cfg.alice.email, password: cfg.password });
const todos = alice.data.Todo;
const out = {};

// Groups of alice's todos and bob's public one (the read policy), fields de-duplicated.
out.byStatus = await todos
  .groupBy(['status', 'status'])
  .aggregate({
    n: { count: 'estimate' },
    total: { sum: 'estimate' },
    cost: { sum: 'cost' },
    mean: { avg: 'cost' },
    top: { max: 'cost' },
  })
  .execute();
assert.deepEqual(out.byStatus, [
  { fields: { status: 'doing' }, aggregations: { n: 3, total: 12, cost: 103.34, mean: 34.446667, top: 99.99 } },
  { fields: { status: 'done' }, aggregations: { n: 1, total: 3, cost: 10.2, mean: 5.1, top: 10 } },
]);

// A grand total over the `where` rows: `fields` is empty; avg over an int is an int on Fabric SQL.
out.lowTotals = await todos
  .where({ priority: { eq: 'low' } })
  .aggregate({
    n: { count: 'estimate' },
    mean: { avg: 'estimate' },
    lightest: { min: 'weight' },
    heaviest: { max: 'weight' },
  })
  .execute();
assert.deepEqual(out.lowTotals, [{ fields: {}, aggregations: { n: 2, mean: 3, lightest: 0.1, heaviest: 2.25 } }]);

// `having` and `distinct`.
out.big = await todos
  .groupBy(['status'])
  .aggregate({
    total: { sum: { field: 'estimate', having: { gt: 5 } } },
    unique: { count: { field: 'estimate', distinct: true } },
  })
  .execute();
assert.deepEqual(out.big, [{ fields: { status: 'doing' }, aggregations: { total: 12, unique: 3 } }]);

// Grouped values go through the row deserializer: booleans and dates.
const byPublic = await todos.groupBy(['isPublic']).aggregate({ n: { count: 'estimate' } }).execute();
assert.deepEqual(byPublic, [
  { fields: { isPublic: false }, aggregations: { n: 2 } },
  { fields: { isPublic: true }, aggregations: { n: 2 } },
]);
const byDay = await todos
  .where({ status: { eq: 'done' } })
  .groupBy(['createdAt'])
  .aggregate({ n: { count: 'estimate' } })
  .execute();
assert.equal(byDay.length, 2);
assert.ok(byDay[0].fields.createdAt instanceof Date);
out.byDay = byDay.map((g) => [g.fields.createdAt.toISOString(), g.aggregations.n]);
assert.deepEqual(out.byDay, [
  ['2026-01-02T00:00:00.000Z', 0],
  ['2026-01-06T00:00:00.000Z', 1],
]);

// No rows: one grand total, `count` 0 and the others null; no groups.
out.none = await todos
  .where({ title: { eq: 'nope' } })
  .aggregate({ n: { count: 'estimate' }, total: { sum: 'cost' } })
  .execute();
assert.deepEqual(out.none, [{ fields: {}, aggregations: { n: 0, total: null } }]);
assert.deepEqual(
  await todos.where({ title: { eq: 'nope' } }).groupBy(['status']).aggregate({ n: { count: 'estimate' } }).execute(),
  [],
);

// Anonymous callers read public todos without `estimate`: DAB's error.
const anonymous = client();
out.forbidden = await anonymous.data.Todo.aggregate({ total: { sum: 'estimate' } })
  .execute()
  .then(
    () => null,
    (e) => e.message,
  );
assert.equal(
  out.forbidden,
  "GraphQL errors: Access forbidden to field 'estimate' referenced in the aggregation function 'sum'.",
);

for (const c of [alice, anonymous]) c.auth.destroy();
print('RESULT ' + JSON.stringify(out));
process.exit(0);
