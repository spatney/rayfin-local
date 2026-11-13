import {
  UserDataFunctions,
  type RayfinContext,
} from '@microsoft/fabric-user-data-functions';

type AppSchema = { Todo: { id: string; user_id: string; title: string } };
type AppContext = RayfinContext<AppSchema>;

const udf = new UserDataFunctions();

udf.func('greet', async (name: string): Promise<string> => `Hello, ${name}!`, []);

udf.func(
  'countTodos',
  async (ctx: AppContext): Promise<{ count: number; hasCallerToken: boolean }> => {
    const todos = await ctx.getDataClient().Todo.select(['id']).execute();
    return { count: todos.length, hasCallerToken: ctx.accessToken.length > 0 };
  },
  [],
);
