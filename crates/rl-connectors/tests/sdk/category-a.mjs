// Runs the real Rayfin SDK 1.36.2 Category A client (`createConnectorsApi` with the connector's
// entity classes, as an app's `connectorConfig` wires it) against rayfin-local's
// /connectors/<name>/graphql route over a real SQL Server, and checks what the app gets back.
//
// Started by tests/sql_sdk.rs, which passes the settings in RL_SDK_TEST (JSON): `nodeModules`,
// `compiled` (the fixture's compiled connector entities), `api`, and the keys of the rows.
import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';

const cfg = JSON.parse(process.env.RL_SDK_TEST ?? '{}');
const load = (path) => import(pathToFileURL(`${cfg.nodeModules}/${path}`).href);
const { ApiClient } = await load('@microsoft/rayfin-lib/dist/index.js');
const { createConnectorsApi } = await load('@microsoft/rayfin-connectors/dist/index.js');
const entity = async (name) => (await import(pathToFileURL(`${cfg.compiled}/${name}.js`).href))[name];
const [AuditEntry, Category, Product, StockLevel] = await Promise.all(
  ['AuditEntry', 'Category', 'Product', 'StockLevel'].map(entity),
);

// ApiClient logs every POST; keep the output to the results.
console.log = () => {};

const entities = { AuditEntry, Category, Product, StockLevel };
const operations = ['read', 'create', 'update', 'delete'];

function connectors(token) {
  const apiClient = new ApiClient({
    baseUrl: cfg.api,
    publishableKey: 'pk',
    getAccessToken: () => token,
  });
  return createConnectorsApi(
    apiClient,
    {
      inventory: { connector: 'fabric-sqldatabase', operations, entities },
      stock: { connector: 'fabric-warehouse', operations, entities },
      lake: { connector: 'fabric-sqlanalytics', operations: ['read'], entities },
    },
    undefined,
    { type: 'standalone' },
  );
}

const alice = connectors('alice');
const bob = connectors('bob');
const results = [];
async function check(name, fn) {
  try {
    await fn();
    results.push(`ok   ${name}`);
  } catch (error) {
    results.push(`FAIL ${name}\n${error?.stack ?? error}`);
  }
}

await check('findMany with the default columns', async () => {
  const rows = await alice.inventory.Category.findMany();
  assert.deepEqual(
    rows.map((row) => [row.categoryId, row.name, row.description]).sort(),
    [
      [1, 'Tools', 'Hand tools'],
      [2, 'Garden', null],
    ],
  );
});

await check('select, where, orderBy and dotted relationship paths', async () => {
  const rows = await alice.inventory.Product.select(['name', 'price', 'category.name', 'stockLevels.quantity'])
    .where({ inStock: { eq: true } })
    .orderBy({ price: 'desc' })
    .execute();
  assert.deepEqual(rows.map((row) => row.name), ['Hammer', 'Rake']);
  assert.equal(rows[0].price, 12.5);
  assert.equal(rows[0].category.name, 'Tools');
  assert.deepEqual(rows[0].stockLevels.map((level) => level.quantity).sort(), [0, 5]);
});

await check('findByKey reads the full row and deserializes dates', async () => {
  const hammer = await alice.inventory.Product.findByKey({ productId: cfg.hammer });
  assert.equal(hammer.name, 'Hammer');
  assert.equal(hammer.inStock, true);
  assert.ok(hammer.createdAt instanceof Date, `createdAt: ${hammer.createdAt}`);
  assert.equal(hammer.createdAt.toISOString(), '2024-01-15T10:30:00.123Z');
  assert.equal(await alice.inventory.Product.findByKey({ productId: '00000000-0000-0000-0000-000000000000' }), null);
});

await check('composite keys', async () => {
  const level = await alice.inventory.StockLevel.findByKey({ productId: cfg.hammer, warehouseCode: 'NYC' });
  assert.equal(level.quantity, 0);
});

await check('executePaginated and after', async () => {
  const first = await alice.inventory.Product.select(['name']).orderBy({ name: 'asc' }).first(2).executePaginated();
  assert.deepEqual(first.items.map((row) => row.name), ['Hammer', 'Rake']);
  assert.equal(first.hasNextPage, true);
  const next = await alice.inventory.Product.select(['name'])
    .orderBy({ name: 'asc' })
    .first(2)
    .after(first.endCursor)
    .executePaginated();
  assert.deepEqual(next.items.map((row) => row.name), ['Saw "Pro" 50%']);
  assert.equal(next.hasNextPage, false);
});

await check('a keyless entity', async () => {
  const rows = await alice.inventory.AuditEntry.findMany();
  assert.deepEqual(rows.map((row) => row.action).sort(), ['login', 'logout']);
  const login = rows.find((row) => row.action === 'login');
  assert.equal(login.at.toISOString(), '2024-01-15T08:30:00.000Z');
});

await check('create, update and delete on a SQL database return the row', async () => {
  const created = await alice.inventory.Product.create({
    categoryId: 2,
    name: 'Shovel',
    price: 19.99,
    inStock: true,
    ownerEmail: 'alice@contoso.com',
  });
  assert.match(created.productId, /^[0-9a-f-]{36}$/);
  assert.ok(created.createdAt instanceof Date);
  const updated = await alice.inventory.Product.update({ productId: created.productId }, { price: 21 });
  assert.equal(updated.price, 21);
  const deleted = await alice.inventory.Product.delete({ productId: created.productId });
  assert.equal(deleted.name, 'Shovel');
});

await check('policies refuse other users', async () => {
  await assert.rejects(
    bob.inventory.Product.update({ productId: cfg.hammer }, { price: 1 }),
    (error) => /Could not find item with <productId: /.test(error.message),
  );
});

await check('a warehouse answers DbOperationResult', async () => {
  const result = await alice.stock.Product.update({ productId: cfg.rake }, { name: 'Rake Pro' });
  assert.deepEqual(result, { result: 'success' });
  const rake = await alice.stock.Product.findByKey({ productId: cfg.rake }, ['name']);
  assert.equal(rake.name, 'Rake Pro');
});

await check('a SQL analytics endpoint reads', async () => {
  const rows = await alice.lake.Category.select(['name']).orderBy({ name: 'asc' }).execute();
  assert.deepEqual(rows.map((row) => row.name), ['Garden', 'Tools']);
});

process.stdout.write(`${results.join('\n')}\n`);
process.exit(results.some((line) => line.startsWith('FAIL')) ? 1 : 0);
