import {
  AudienceType,
  UserDataFunctions,
  type RayfinContext,
} from '@microsoft/fabric-user-data-functions';

/*
 * The functions of the rayfin-local end-to-end fixture. They report facts about tokens and
 * secrets (whether they are there, their length, a match), never their values.
 */

/** The part of the data model (`rayfin/data/Todo.ts`) that `countTodos` reads. */
type AppSchema = {
  Todo: { id: string; user_id: string; title: string; isPublic: boolean };
};

/** The local value of `GREETING_SECRET`, from `local.settings.json`. */
const FIXTURE_SECRET = 'local-secret-value';

const udf = new UserDataFunctions();

udf.func('ping', async (): Promise<string> => 'pong', []);

udf.func(
  'greet',
  async (name: string, excited: boolean): Promise<string> =>
    `Hello, ${name}${excited ? '!' : '.'}`,
  [],
);

udf.func(
  'echo',
  async (
    payload: { a: number; tags: string[] } | null,
  ): Promise<{ payload: { a: number; tags: string[] } | null; tagCount: number }> => ({
    payload,
    tagCount: payload?.tags.length ?? 0,
  }),
  [],
);

/** Whether the invocation carried the caller's Rayfin token, and the backend it points at. */
udf.func(
  'whoAmI',
  async (ctx: RayfinContext<AppSchema>): Promise<{ hasCallerToken: boolean; baseUrl: string }> => ({
    hasCallerToken: typeof ctx.accessToken === 'string' && ctx.accessToken.length > 0,
    baseUrl: ctx.baseUrl,
  }),
  [],
);

/** The Todos the caller may read, counted through the app's data API as the caller. */
udf.func(
  'countTodos',
  async (ctx: RayfinContext<AppSchema>): Promise<{ count: number }> => {
    const todos = await ctx.getDataClient().Todo.select(['id']).execute();
    return { count: todos.length };
  },
  [],
);

udf.func(
  'tokenInfo',
  async (
    ctx: RayfinContext<AppSchema, AudienceType.Fabric>,
  ): Promise<{ fabricTokenLength: number }> => ({
    fabricTokenLength: ctx.Tokens.Fabric.length,
  }),
  [],
);

udf.func(
  'secretInfo',
  async (ctx: RayfinContext<AppSchema>): Promise<{ matchesFixture: boolean; length: number }> => {
    const secret = ctx.Secrets.GREETING_SECRET;
    return { matchesFixture: secret === FIXTURE_SECRET, length: secret.length };
  },
  [],
);

udf.func(
  'fail',
  async (message: string): Promise<never> => {
    throw new Error(message);
  },
  [],
);
