import {
  AudienceType,
  type FabricItem,
  type RayfinContext,
} from '@microsoft/fabric-user-data-functions';
import * as rayfin from '@microsoft/fabric-user-data-functions';

import './more/extra.js';
import { udf } from './udf.js';

type AppSchema = { notes: { id: string; text: string } };
type AdoAccess = AudienceType.ADO;

// No parameters.
udf.func('ping', async () => 'pong');

// Only the context, without audiences.
udf.func('whoAmI', async (ctx: RayfinContext<AppSchema>) => typeof ctx);

// Parameters and a context with one audience.
udf.func(
  'addNote',
  async (text: string, pinned: boolean, ctx: RayfinContext<AppSchema, AudienceType.Sql>) => ({
    text,
    pinned,
    ok: typeof ctx,
  }),
);

// A union of audiences, written over several lines and not in sorted order.
udf.func(
  'askAgent',
  async (
    question: string,
    ctx: RayfinContext<
      AppSchema,
      AudienceType.AzureAI | AudienceType.ADO
    >,
  ) => `${question}${typeof ctx}`,
);

// Optional, defaulted and rest parameters.
udf.func(
  'search',
  async (query: string, limit?: number, offset = 0, ...tags: string[]) => ({
    query,
    limit,
    offset,
    tags,
  }),
);

// Object, array, union and null types.
udf.func(
  'filterItems',
  async (
    filter: { status: 'open' | 'closed'; tags?: string[] },
    ids: number[],
    mode: 'fast' | 'full' | null,
    rows: Array<Record<string, unknown>>,
    when: Date | string,
  ) => ids.length + rows.length + String(filter.status).length + String(mode).length + String(when).length,
);

// Audiences through a type alias resolve to the audience.
udf.func('backlog', async (refresh: boolean, ctx: RayfinContext<AppSchema, AdoAccess>) =>
  refresh ? typeof ctx : '',
);

// A context named `context` counts as a Fabric parameter.
udf.func(
  'legacyContext',
  async (context: RayfinContext<AppSchema, AudienceType.Storage | AudienceType.Fabric>) =>
    typeof context,
);

// A connection parameter, and an unannotated one.
udf.func('lakeFile', async (lakeConnection: FabricItem, path, verbose: boolean = false) =>
  verbose ? `${path}${typeof lakeConnection}` : path,
);

// A namespaced context.
udf.func(
  'namespaced',
  async (ctx: rayfin.RayfinContext<AppSchema, rayfin.AudienceType.Sql>) => typeof ctx,
);

// A context that declares no audiences explicitly.
udf.func('nothingDeclared', async (ctx: RayfinContext<AppSchema, never>) => typeof ctx);

// A destructured parameter.
udf.func('destructured', async ({ a, b }: { a: string; b: number }) => `${a}${b}`);

// Not registrations: a computed name, a handler passed by reference, a template literal name.
const computedName = 'computed';
udf.func(computedName, async () => 0);
const byReference = async () => 0;
udf.func('byReference', byReference);
udf.func(`template`, async () => 0);
