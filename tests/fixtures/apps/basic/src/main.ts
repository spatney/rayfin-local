import { ensureSignedInWithFabric, signInWithBrokeredToken } from '@microsoft/rayfin-auth-provider-fabric';
import { fetchRayfinLocalSessionToken } from '@microsoft/rayfin-local-dev';

import type { BasicSchema } from '../rayfin/data/schema';
import { bootstrap, type AppServices } from './services/bootstrap';

/*
 * Every button runs one scenario against the Rayfin backend and writes the outcome as JSON into
 * <pre data-testid="result-<name>">: {"ok": true, "value": ...} or {"ok": false, "error": ...}.
 * The <pre>'s data-status is "running" until it is "ok" or "error".
 *
 * Data scenarios work on rows titled with a tag unique to the run, report titles without it, and
 * delete what they created.
 */

type Todo = BasicSchema['Todo'];

const appStatus = document.querySelector<HTMLElement>('[data-testid="app-status"]')!;

let services: AppServices;
try {
  services = bootstrap();
} catch (error) {
  appStatus.textContent = `error: ${error instanceof Error ? error.message : String(error)}`;
  throw error;
}
const { client, fabricOptions } = services;

const DAX_QUERY = 'EVALUATE ROW("answer", 42)';

const scenarios: Record<string, () => Promise<unknown>> = {
  config: async () => ({ apiUrl: import.meta.env.VITE_RAYFIN_API_URL, ...fabricOptions }),
  'sign-in': async () => sessionInfo(await ensureSignedInWithFabric(client.auth, fabricOptions)),
  'auto-sign-in': autoSignIn,
  'sign-out': async () => {
    await client.auth.signOut();
    return sessionInfo(client.auth.getSession());
  },
  session: async () => sessionInfo(client.auth.getSession()),
  'todo-crud': todoCrud,
  'todo-paginate': todoPaginate,
  'todo-category': todoCategory,
  'todo-aggregate': todoAggregate,
  'fn-ping': () => client.functions.ping.invoke(),
  'fn-greet': () => client.functions.greet.invoke({ name: 'Ada', excited: true }),
  'fn-echo': () => client.functions.echo.invoke({ payload: { a: 1, tags: ['x', 'y'] } }),
  'fn-echo-null': () => client.functions.echo.invoke({ payload: null }),
  'fn-who-am-i': () => client.functions.whoAmI.invoke(),
  'fn-count-todos': () => client.functions.countTodos.invoke(),
  'fn-token-info': () => client.functions.tokenInfo.invoke(),
  'fn-secret-info': () => client.functions.secretInfo.invoke(),
  'fn-fail': () => client.functions.fail.invoke({ message: 'deliberate failure' }),
  'connector-query': async () => ({
    query: DAX_QUERY,
    result: await client.connectors.salesmodel.executeQuery({ query: DAX_QUERY }),
  }),
  'storage-roundtrip': storageRoundTrip,
};

/** What apps from Rayfin's templates do when they load: a session from the dev server, no popup. */
async function autoSignIn() {
  const token = await fetchRayfinLocalSessionToken();
  if (!token) throw new Error('Automatic sign-in is off: use rayfinLocalDev({ autoLogin: true }).');
  return sessionInfo(await signInWithBrokeredToken(client.auth, token));
}

/** Create, list, update and delete one Todo. */
async function todoCrud() {
  const tag = runTag();
  const created = await client.data.Todo.create(todoInput(`${tag} crud`));
  const listed = await client.data.Todo.select(['id', 'title', 'status'])
    .where({ title: { startsWith: tag } })
    .execute();
  const updated = await client.data.Todo.update(
    { id: created.id },
    { title: `${tag} crud edited`, status: 'doing' },
  );
  const deleted = await client.data.Todo.delete({ id: created.id });
  return {
    created: { title: untag(created.title, tag), status: created.status },
    listed: listed.map((todo) => ({ title: untag(todo.title, tag), status: todo.status })),
    updated: { title: untag(updated.title, tag), status: updated.status },
    deleted: deleted.id === created.id,
    foundAfterDelete: (await client.data.Todo.findById(created.id)) !== null,
  };
}

/** Three Todos read two at a time, following the cursor. */
async function todoPaginate() {
  const tag = runTag();
  const created = await createTodos(['1', '2', '3'].map((n) => todoInput(`${tag} ${n}`)));
  try {
    const query = () =>
      client.data.Todo.select(['id', 'title'])
        .where({ title: { startsWith: tag } })
        .orderBy({ title: 'asc' })
        .first(2);
    const first = await query().executePaginated();
    const second = first.endCursor ? await query().after(first.endCursor).executePaginated() : null;
    return [first, second].map(
      (page) =>
        page && {
          titles: page.items.map((todo) => untag(todo.title, tag)),
          hasNextPage: page.hasNextPage,
        },
    );
  } finally {
    await deleteTodos(created);
  }
}

/** A Todo with its Category (`@one`), and the Category with its Todos (`@many`). */
async function todoCategory() {
  const tag = runTag();
  const category = await client.data.Category.create({ name: `${tag} category`, sortOrder: 1 });
  const [todo] = await createTodos([
    { ...todoInput(`${tag} categorized`), category: { id: category.id } },
  ]);
  try {
    const [withCategory] = await client.data.Todo.select(['id', 'title', 'category.name'])
      .where({ id: { eq: todo.id } })
      .execute();
    const [withTodos] = await client.data.Category.select(['id', 'name', 'todos.title'])
      .where({ id: { eq: category.id } })
      .execute();
    return {
      todo: {
        title: untag(withCategory?.title, tag),
        category: untag(withCategory?.category?.name, tag),
      },
      category: {
        name: untag(withTodos?.name, tag),
        todos: withTodos?.todos?.map((item) => untag(item.title, tag)) ?? null,
      },
    };
  } finally {
    await deleteTodos([todo]);
    await client.data.Category.delete({ id: category.id });
  }
}

/** Todos counted and summed per status. */
async function todoAggregate() {
  const tag = runTag();
  const created = await createTodos([
    todoInput(`${tag} a`, 'doing', 1.5),
    todoInput(`${tag} b`, 'doing', 2.5),
    todoInput(`${tag} c`, 'done', 4),
  ]);
  try {
    const rows = await client.data.Todo.where({ title: { startsWith: tag } })
      .groupBy(['status'])
      .aggregate({ todos: { count: 'cost' }, totalCost: { sum: 'cost' } })
      .execute();
    return rows
      .map((row) => ({ status: row.fields.status, ...row.aggregations }))
      .sort((a, b) => a.status.localeCompare(b.status));
  } finally {
    await deleteTodos(created);
  }
}

/** Upload, list, download and delete an object in the `todo-attachments` folder. */
async function storageRoundTrip() {
  const folder = client.storage['todo-attachments'];
  const name = 'fixture-note.pdf';
  const content = '%PDF-1.4 rayfin-local fixture';
  const uploaded = await folder.upload(name, content, {
    contentType: 'application/pdf',
    fields: { team_id: 'team-1', pages: 1 },
  });
  const listed = await folder.list({ limit: 100 });
  const downloaded = await folder.download(name);
  const text = await new Response(downloaded.stream).text();
  const deleted = await folder.delete(name);
  const remaining = await folder.list({ limit: 100 });
  return {
    uploaded: {
      name: uploaded.object.name,
      size: uploaded.object.size,
      contentType: uploaded.object.contentType,
      team_id: uploaded.object.team_id,
      pages: uploaded.object.pages,
    },
    listed: listed.items.map((item) => ({ name: item.name, team_id: item.team_id })),
    downloaded: { text, contentType: downloaded.contentType },
    deleted: { success: deleted.success, deleted: deleted.deleted },
    remaining: remaining.items.map((item) => item.name),
  };
}

function signedInUserId(): string {
  const user = client.auth.getSession().user;
  if (!user) throw new Error('Sign in first.');
  return user.id;
}

/**
 * A new Todo owned by the signed-in user, as the `authenticated` create policy requires.
 * `"it's new"` (the field's default) is not used: rayfin-core strips quotes from `@set` values in
 * the CHECK constraint, so the database (on Fabric too) only accepts `its new`.
 */
function todoInput(title: string, status: Todo['status'] = 'done', cost = 1) {
  return { user_id: signedInUserId(), title, status, cost, isPublic: false, createdAt: new Date() };
}

async function createTodos(inputs: Parameters<typeof client.data.Todo.create>[0][]) {
  const created: Todo[] = [];
  for (const input of inputs) created.push(await client.data.Todo.create(input));
  return created;
}

async function deleteTodos(todos: { id: string }[]) {
  for (const todo of todos) await client.data.Todo.delete({ id: todo.id });
}

/** A title prefix unique to one scenario run. */
function runTag(): string {
  return `run-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

function untag(text: string | undefined, tag: string): string | null {
  if (text === undefined) return null;
  return text.startsWith(tag) ? text.slice(tag.length).trim() : text;
}

function sessionInfo(session: {
  isAuthenticated: boolean;
  user: { id: string; email: string; role?: string } | null;
}) {
  const { user } = session;
  return {
    isAuthenticated: session.isAuthenticated,
    user: user && { id: user.id, email: user.email, role: user.role ?? null },
  };
}

function describeError(error: unknown) {
  if (!(error instanceof Error)) return { message: String(error) };
  const { code, status } = error as Error & { code?: unknown; status?: unknown };
  return { name: error.name, message: error.message, code, status };
}

async function runScenario(run: () => Promise<unknown>, result: HTMLElement) {
  result.dataset.status = 'running';
  result.textContent = '';
  let outcome;
  try {
    outcome = { ok: true, value: await run() };
  } catch (error) {
    outcome = { ok: false, error: describeError(error) };
  }
  result.textContent = JSON.stringify(
    outcome,
    (_key, value: unknown) => (typeof value === 'bigint' ? value.toString() : value),
    2,
  );
  result.dataset.status = outcome.ok ? 'ok' : 'error';
}

const list = document.getElementById('scenarios')!;
for (const [name, run] of Object.entries(scenarios)) {
  const button = document.createElement('button');
  button.type = 'button';
  button.textContent = name;
  button.dataset.testid = `run-${name}`;
  const result = document.createElement('pre');
  result.dataset.testid = `result-${name}`;
  // Synchronous up to the scenario's first await, so sign-in may open its popup.
  button.addEventListener('click', () => void runScenario(run, result));
  const section = document.createElement('section');
  section.append(button, result);
  list.append(section);
}
appStatus.textContent = 'ready';
