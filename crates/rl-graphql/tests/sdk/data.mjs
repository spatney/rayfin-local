// Runs the real Rayfin SDK 1.36.2 data client against rayfin-local's /graphql (rl-graphql behind
// rl-server, with rl-auth's sign-in), on the explorer app's model. Started by tests/sdk.rs, which
// seeds the database and passes the settings in RL_SDK_TEST (JSON).
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
const data = alice.data;
const out = {};

// select / where / orderBy / execute, with a to-one dot path.
const messages = await data.AskMessage.select(['id', 'role', 'content', 'createdAt', 'thread.title'])
  .where({ role: { eq: 'user' } })
  .orderBy({ createdAt: 'desc' })
  .execute();
out.messages = messages;
assert.equal(messages.length, 2);
assert.equal(messages[0].content, 'second question');
assert.ok(messages[0].createdAt instanceof Date);
assert.deepEqual(messages[0].thread, { title: 'Alice one' });

// A to-many dot path is unwrapped from its connection.
const threads = await data.AskThread.select(['id', 'title', 'messages.content'])
  .orderBy({ createdAt: 'asc' })
  .execute();
out.threads = threads.map((t) => ({ title: t.title, messages: t.messages.map((m) => m.content) }));
assert.deepEqual(out.threads, [
  { title: 'Alice one', messages: ['first question', 'an answer', 'second question'] },
  { title: 'Alice two', messages: [] },
  { title: 'alice three', messages: [] },
]);

// Operators the SDK emits.
const titles = async (where) =>
  (await data.AskThread.select(['title']).where(where).orderBy({ title: 'asc' }).execute()).map(
    (t) => t.title,
  );
assert.deepEqual(await titles({ title: { contains: 'ALICE' } }), ['Alice one', 'alice three', 'Alice two']);
assert.deepEqual(await titles({ title: { startsWith: 'alice t' } }), ['alice three', 'Alice two']);
assert.deepEqual(await titles({ title: { in: ['Alice two', 'nope'] } }), ['Alice two']);
assert.deepEqual(await titles({ responseId: { isNull: false } }), ['Alice two']);
assert.deepEqual(await titles({ createdAt: { gte: new Date('2026-01-02T00:00:00Z') } }), ['alice three', 'Alice two']);
assert.deepEqual(
  await titles({ or: [{ title: { eq: 'alice ONE' } }, { title: { endsWith: 'three' } }] }),
  ['Alice one', 'alice three'],
);
assert.deepEqual(await titles({ title: 'Alice two' }), ['Alice two']);
const merged = await data.AskThread.select(['title'])
  .where({ title: { contains: 'alice' } })
  .where({ title: { endsWith: 'two' } })
  .execute();
assert.deepEqual(merged.map((t) => t.title), ['Alice two']);

// Pagination with cursors.
const pages = [];
let cursor;
do {
  let query = data.AskThread.select(['title']).orderBy({ title: 'desc' }).first(2);
  if (cursor) query = query.after(cursor);
  const page = await query.executePaginated();
  pages.push(page.items.map((t) => t.title));
  cursor = page.hasNextPage ? page.endCursor : undefined;
} while (cursor);
out.pages = pages;
assert.deepEqual(pages, [['Alice two', 'alice three'], ['Alice one']]);

// findMany / findFirst / findById select only `id`.
const all = await data.AskThread.findMany();
assert.equal(all.length, 3);
assert.deepEqual(Object.keys(all[0]), ['id']);
const first = await data.AskThread.findFirst({ title: { eq: 'Alice two' } });
assert.deepEqual(first, { id: cfg.threads.two });
assert.equal(await data.AskThread.findFirst({ title: { eq: 'nope' } }), null);
assert.deepEqual(await data.AskThread.findById(cfg.threads.one.toUpperCase()), { id: cfg.threads.one });
// Bob's thread is hidden by the policy; a missing one is null too.
assert.equal(await data.AskThread.findById(cfg.threads.bob), null);
assert.equal(await data.AskThread.findById('00000000-0000-4000-8000-0000000000ff'), null);
const builderFirst = await data.AskMessage.select(['content'])
  .orderBy({ createdAt: 'asc' })
  .findFirst();
assert.deepEqual(builderFirst, { content: 'first question' });

// GraphQL errors reach the app as `GraphQL errors: <messages>`.
const failure = await data.AskThread.select(['nope']).execute().then(
  () => null,
  (e) => e.message,
);
out.unknownField = failure;
assert.equal(failure, 'GraphQL errors: The field `nope` does not exist on the type `AskThread`.');
const notFilter = await data.AskThread.select(['id']).where({ not: { title: { eq: 'x' } } }).execute().then(
  () => null,
  (e) => e.message,
);
assert.equal(notFilter, 'GraphQL errors: The specified input object field `not` does not exist.');

// Bob sees only his own thread.
const bob = client();
await bob.auth.signIn({ email: cfg.bob.email, password: cfg.password });
const bobs = await bob.data.AskThread.select(['title']).execute();
assert.deepEqual(bobs.map((t) => t.title), ['Bob one']);

// Mutations, as explorer's thread store makes them (src/lib/threads.ts).
const failed = (promise) => promise.then(() => null, (e) => e.message);
const now = new Date('2026-02-01T10:00:00.000Z');
const created = await data.AskThread.create({
  user_id: cfg.alice.id,
  title: 'New thread',
  createdAt: now,
  updatedAt: now,
});
assert.match(created.id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
assert.deepEqual({ ...created, id: undefined }, {
  id: undefined,
  user_id: cfg.alice.id,
  title: 'New thread',
  createdAt: now,
  updatedAt: now,
});
// A relationship object is sent as its foreign key, and nested again in the answer.
const hello = await data.AskMessage.create({
  user_id: cfg.alice.id,
  thread: { id: created.id },
  role: 'user',
  content: 'hello',
  steps: undefined,
  createdAt: new Date('2026-02-01T10:00:01.000Z'),
});
assert.deepEqual(hello.thread, { id: created.id });
await data.AskMessage.create({
  user_id: cfg.alice.id,
  thread_id: created.id,
  role: 'assistant',
  content: 'hi',
  createdAt: new Date('2026-02-01T10:00:02.000Z'),
});
const renamed = await data.AskThread.update({ id: created.id }, { title: 'Renamed', updatedAt: new Date() });
assert.equal(renamed.title, 'Renamed');
assert.ok(renamed.updatedAt instanceof Date);
// upsert: a read by key, then an update (or a create without the key).
const upserted = await data.AskThread.upsert({ id: created.id }, {}, { responseId: 'resp_9' });
assert.deepEqual(upserted, { id: created.id, responseId: 'resp_9' });
const fresh = await data.AskThread.upsert(
  { id: '00000000-0000-4000-8000-0000000000fe' },
  { user_id: cfg.alice.id, title: 'Upserted', createdAt: now, updatedAt: now },
  { title: 'unused' },
);
assert.notEqual(fresh.id, '00000000-0000-4000-8000-0000000000fe');
assert.deepEqual(await data.AskThread.delete({ id: fresh.id }), { id: fresh.id });
// The foreign key has no cascade: the messages go first, then the thread.
const inThread = await data.AskMessage.select(['id', 'content'])
  .where({ thread_id: { eq: created.id } })
  .orderBy({ createdAt: 'asc' })
  .first(-1)
  .execute();
assert.deepEqual(inThread.map((m) => m.content), ['hello', 'hi']);
const blocked = await failed(data.AskThread.delete({ id: created.id }));
assert.equal(blocked, 'GraphQL errors: While processing your request the database ran into an error.');
for (const m of inThread) {
  assert.deepEqual(await data.AskMessage.delete({ id: String(m.id) }), { id: m.id });
}
assert.deepEqual(await data.AskThread.delete({ id: created.id }), { id: created.id });
assert.equal(await data.AskThread.findById(created.id), null);
// Bob can't write alice's threads, nor create one in her name.
const bobUpdate = await failed(bob.data.AskThread.update({ id: cfg.threads.one }, { title: 'mine' }));
assert.equal(bobUpdate, `GraphQL errors: Could not find item with <id: ${cfg.threads.one}>`);
const bobDelete = await failed(bob.data.AskThread.delete({ id: cfg.threads.one }));
assert.equal(bobDelete, 'Failed to extract result from deleteAskThread mutation');
const forged = await failed(
  bob.data.AskThread.create({ user_id: cfg.alice.id, title: 'x', createdAt: now, updatedAt: now }),
);
assert.equal(forged, 'GraphQL errors: Could not insert row with given values.');
assert.equal((await data.AskThread.findMany()).length, 3);
out.mutations = { created: created.title, renamed: renamed.title, blocked, bobUpdate, bobDelete, forged };

// An anonymous client gets 401 (refresh, retry, then the error).
const anonymous = client();
const anonymousError = await anonymous.data.AskThread.select(['id']).execute().then(
  () => null,
  (e) => ({ name: e.name, status: e.status, message: e.message }),
);
out.anonymous = anonymousError;
assert.equal(anonymousError.status, 401);

for (const c of [alice, bob, anonymous]) c.auth.destroy();
print('RESULT ' + JSON.stringify(out));
process.exit(0);
