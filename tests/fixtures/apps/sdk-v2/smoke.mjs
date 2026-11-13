import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { RayfinClient, resolveRayfinConfig, suppliedToken } from '@microsoft/rayfin-client';
import { browserSession } from '@microsoft/rayfin-auth';
import { dataService } from '@microsoft/rayfin-data';
import { functionsService } from '@microsoft/rayfin-functions';
import { AuthError } from '@microsoft/rayfin-lib';
import { storageService } from '@microsoft/rayfin-storage';

console.log = console.info = console.debug = () => {};
const { apiUrl, appUrl } = JSON.parse(process.env.RL_SDK_TEST);
const config = await resolveRayfinConfig({}, {
  configUrl: new URL('rayfin.config.json', appUrl).href,
});
assert.equal(new URL(config.baseUrl).origin, new URL(appUrl).origin);
assert.ok(config.publishableKey);
assert.ok(config.runtimeConfig.itemId);

const clients = [];
const client = () => {
  const instance = RayfinClient.create({
    baseUrl: config.baseUrl,
    publishableKey: config.publishableKey,
    runtimeConfig: config.runtimeConfig,
    auth: browserSession({ storage: false, autoRefreshToken: false, multiTabSync: false }),
    services: {
      data: dataService(),
      functions: functionsService(),
      storage: storageService(),
    },
  });
  clients.push(instance);
  return instance;
};

try {
  const alice = client();
  const bob = client();
  const password = 'correct horse battery';
  const email = `sdk-v2-${randomUUID()}@example.test`;
  const settings = await alice.auth.getAuthSettings();
  assert.ok(settings.availableMethods.includes('password'));
  const user = await alice.auth.signUp({ email, password });
  await assert.rejects(alice.auth.signUp({ email, password }), (error) =>
    error instanceof AuthError && error.code === 'EMAIL_ALREADY_REGISTERED');
  const tokens = await alice.auth.signIn({ email, password });
  assert.equal(alice.auth.getSession().isAuthenticated, true);
  const claims = await alice.auth.getClaims();
  assert.equal(claims.sub, user.userId);
  assert.equal(claims.email, email);

  const bobEmail = `sdk-v2-${randomUUID()}@example.test`;
  await bob.auth.signUp({ email: bobEmail, password });
  await bob.auth.signIn({ email: bobEmail, password });

  const now = new Date('2026-11-12T12:00:00Z');
  const first = await alice.data.Todo.create({
    user_id: user.userId, title: 'First', createdAt: now,
  });
  const second = await alice.data.Todo.create({
    user_id: user.userId, title: 'Second', done: true, createdAt: now,
  });
  const rows = await alice.data.Todo.select(['id', 'title', 'done', 'createdAt'])
    .orderBy({ title: 'asc' }).execute();
  assert.deepEqual(rows.map((row) => [row.title, row.done]), [['First', false], ['Second', true]]);
  assert.equal(rows[0].createdAt, now.toISOString());
  const page = await alice.data.Todo.select(['title']).orderBy({ title: 'asc' }).first(1)
    .executePaginated();
  assert.equal(page.hasNextPage, true);
  const next = await alice.data.Todo.select(['title']).orderBy({ title: 'asc' }).first(1)
    .after(page.endCursor).executePaginated();
  assert.deepEqual(next.items, [{ title: 'Second' }]);
  const groups = await alice.data.Todo.groupBy(['done']).aggregate({ n: { count: 'estimate' } }).execute();
  assert.deepEqual(groups, [
    { fields: { done: false }, aggregations: { n: 1 } },
    { fields: { done: true }, aggregations: { n: 1 } },
  ]);
  const comment = await alice.data.Comment.create({
    user_id: user.userId, content: 'SDK 2 relationship', todo: { id: first.id },
  });
  const comments = await alice.data.Comment.select(['content', 'todo.title']).execute();
  assert.deepEqual(comments, [{ content: 'SDK 2 relationship', todo: { title: 'First' } }]);
  assert.equal((await alice.data.Todo.update({ id: second.id }, { title: 'Renamed' })).title, 'Renamed');
  assert.deepEqual(await bob.data.Todo.findMany(), []);
  await assert.rejects(bob.data.Todo.create({ user_id: user.userId, title: 'Forbidden' }),
    /Could not insert row/);

  assert.equal(await alice.functions.greet.invoke({ name: 'SDK 2' }), 'Hello, SDK 2!');
  assert.deepEqual(await alice.functions.countTodos.invoke({}), { count: 2, hasCallerToken: true });
  assert.deepEqual(await bob.functions.countTodos.invoke({}), { count: 0, hasCallerToken: true });

  const attachments = alice.storage.attachments;
  const name = `${randomUUID()}.txt`;
  const bytes = new TextEncoder().encode('Rayfin 2.0 storage');
  const uploaded = await attachments.upload(name, bytes, {
    contentType: 'text/plain', fields: { description: 'SDK 2 metadata' },
  });
  assert.equal(uploaded.object.path, name);
  const listed = await attachments.list();
  assert.equal(listed.items.length, 1);
  assert.equal(listed.items[0].description, 'SDK 2 metadata');
  assert.deepEqual((await bob.storage.attachments.list()).items, []);
  const downloaded = await attachments.download(name);
  assert.equal(await new Response(downloaded.stream).text(), 'Rayfin 2.0 storage');
  await attachments.delete(name);
  assert.deepEqual((await attachments.list()).items, []);

  const refreshed = await alice.auth.refreshSession();
  assert.ok(refreshed.refreshToken && refreshed.refreshToken !== tokens.refreshToken,
    'refresh must rotate credentials');
  const worker = RayfinClient.create({
    baseUrl: apiUrl,
    publishableKey: config.publishableKey,
    auth: suppliedToken({ accessToken: async () => refreshed.accessToken }),
    services: { data: dataService() },
  });
  clients.push(worker);
  assert.equal('auth' in worker, false);
  assert.equal((await worker.data.Todo.findMany()).length, 2);

  await alice.data.Comment.delete({ id: comment.id });
  await alice.data.Todo.delete({ id: first.id });
  await alice.data.Todo.delete({ id: second.id });
  await alice.auth.signOut();
  await bob.auth.signOut();
  assert.equal(alice.auth.getSession().isAuthenticated, false);
  process.stdout.write('Rayfin 2.0 SDK smoke passed\n');
} finally {
  for (const instance of clients) instance.dispose();
}
