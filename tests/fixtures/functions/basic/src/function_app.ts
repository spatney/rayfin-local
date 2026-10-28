import { app, type HttpRequest } from '@azure/functions';
import {
  AudienceType,
  UserDataFunctions,
  UserThrownError,
  type RayfinContext,
} from '@microsoft/fabric-user-data-functions';

/*
 * Functions for the rayfin-local function host tests. They never return token or
 * secret values, only facts about them (lengths, presence).
 */

const udf = new UserDataFunctions();

interface Note {
  title: string;
}

let calls = 0;

udf.func('hello', async () => 'Hello from rayfin-local!');

udf.func(
  'echo',
  async (
    text: string,
    count: number,
    flag: boolean,
    options: object,
    tags: string[],
    scores: Array<number>,
    note: Note | null,
    greeting = 'hi',
  ) => ({ text, count, flag, options, tags, scores, note, greeting }),
);

udf.func(
  'tokens',
  async (ctx: RayfinContext<Record<string, unknown>, AudienceType.ADO | AudienceType.Fabric>) => ({
    audiences: Object.keys(ctx.Tokens).sort(),
    adoLength: ctx.Tokens.ADO.length,
    fabricLength: ctx.Tokens.Fabric.length,
  }),
);

udf.func('whoami', async (ctx: RayfinContext<Record<string, unknown>>) => ({
  baseUrl: ctx.baseUrl,
  hasPublishableKey: ctx.publishableKey.length > 0,
  hasAccessToken: ctx.accessToken.length > 0,
}));

udf.func('secret', async (ctx: RayfinContext<Record<string, unknown>>) => ({
  length: ctx.Secrets.API_KEY.length,
}));

udf.func('fail', async () => {
  throw new Error('boom');
});

udf.func('reject', async (reason: string) => {
  throw new UserThrownError('Not allowed', { reason });
});

udf.func('logs', async (message: string) => {
  console.log(`log: ${message}`);
  console.warn(`warn: ${message}`);
  console.error(`error: ${message}`);
  return message.length;
});

udf.func('counter', async () => {
  calls += 1;
  return calls;
});

udf.func('slow', async (ms: number) => {
  await new Promise((resolve) => setTimeout(resolve, ms));
  return ms;
});

udf.func('crash', async () => {
  setTimeout(() => {
    throw new Error('deliberate crash');
  }, 20);
  return 'crashing';
});

udf.func('bigint', async () => BigInt(1));

// A raw Azure Functions HTTP trigger and a timer: not UDFs, registered all the same.
app.http('raw', {
  methods: ['GET', 'POST'],
  authLevel: 'anonymous',
  route: 'raw/{id?}',
  handler: async (request: HttpRequest) => ({
    status: 201,
    headers: { 'x-raw': 'yes' },
    jsonBody: {
      method: request.method,
      id: request.params.id ?? null,
      q: request.query.get('q'),
      body: request.method === 'GET' ? null : await request.text(),
    },
  }),
});

app.timer('nightly', {
  schedule: '0 0 3 * * *',
  handler: async () => {},
});
