// Runs the real Rayfin SDK 1.36.2 against rayfin-local's /connector-invoke route, and the SDK's own
// direct path (`rayfin connector invoke`) against the same mock Power BI, and checks that the
// `fabricSemanticModel()` runtime normalizes both to the same results. Also checks the
// executeQueries (JSON) fallback rayfin-local uses for Azure CLI tokens.
//
// Started by tests/sdk_decode.rs, which passes the servers in RL_SDK_TEST (JSON).
import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';

const cfg = JSON.parse(process.env.RL_SDK_TEST ?? '{}');
const load = (path) => import(pathToFileURL(`${cfg.nodeModules}/${path}`).href);
const { ApiClient } = await load('@microsoft/rayfin-lib/dist/index.js');
const { createConnectorsApi } = await load('@microsoft/rayfin-connectors/dist/index.js');
const { fabricSemanticModel } = await load(
  '@microsoft/rayfin-connector-fabric-semanticmodel/dist/index.js',
);

// ApiClient logs every POST; keep the output to the results.
console.log = () => {};

const ROW = 'EVALUATE ROW("x", 1)';
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

// The browser's path, as the reference app wires it: host 'standalone', so every call is
// POST /connector-invoke/<name> with the session's bearer token.
function viaRayfinLocal(baseUrl, sessionToken = cfg.session) {
  const apiClient = new ApiClient({
    baseUrl,
    publishableKey: 'pk',
    getAccessToken: () => sessionToken,
  });
  return createConnectorsApi(
    apiClient,
    {
      sales: { connector: 'fabric-semanticmodel' },
      nope: { connector: 'fabric-semanticmodel' },
      telemetry: { connector: 'kusto' },
    },
    { sales: fabricSemanticModel(), nope: fabricSemanticModel() },
    { type: 'standalone' },
  );
}

// `rayfin connector invoke`'s path: host 'cli' with a target calls Power BI directly.
function direct(powerBi) {
  const apiClient = new ApiClient({ baseUrl: '', publishableKey: 'pk' });
  const runtime = fabricSemanticModel({
    target: { workspaceId: cfg.workspaceId, itemId: cfg.itemId },
    baseUrl: `${powerBi}/v1.0/myorg`,
    getToken: () => 'dev-token',
  });
  return createConnectorsApi(
    apiClient,
    { sales: { connector: 'fabric-semanticmodel' } },
    { sales: runtime },
    { type: 'cli' },
  );
}

const local = viaRayfinLocal(cfg.api);
const cli = direct(cfg.powerBi);
const both = async (query) => [
  await local.sales.executeQuery({ query }),
  await cli.sales.executeQuery({ query }),
];

const results = [];
async function check(name, fn) {
  try {
    await fn();
    results.push(`ok   ${name}`);
  } catch (error) {
    results.push(`FAIL ${name}\n${error?.stack ?? error}`);
  }
}

// Arrow answers decode to the same result, except `requestId`: it is a response header, and the
// delegated path decodes the body alone (as on Fabric, where it is always '').
const arrowCases = [
  [
    ROW,
    {
      status: 'success',
      table: { columns: [{ name: '[x]', dataType: 'Int64' }], rows: [[1]] },
      requestId: '',
    },
  ],
  [
    'EVALUATE Typed',
    {
      status: 'success',
      table: {
        columns: [
          { name: 'Sales[Region]', dataType: 'String' },
          { name: 'Sales[OrderDate]', dataType: 'DateTime' },
          { name: '[Total]', dataType: 'Double' },
          { name: '[Orders]', dataType: 'Int64' },
          { name: '[HasProduct]', dataType: 'Boolean' },
        ],
        rows: [
          ['West', '2024-08-02T00:00:00.000', 1713.98, 12, true],
          ['East', '2023-09-27T01:12:12.000', 0.5, 3, false],
          [null, null, null, null, null],
        ],
      },
      requestId: '',
    },
  ],
  [
    'EVALUATE Nope',
    {
      status: 'error',
      error: {
        category: 'query',
        message: "Query (1, 10) Cannot find table 'Nope'.",
        code: '3241803779',
        details: "Cannot find table '%{oii_table/}'.",
      },
      requestId: '',
    },
  ],
  [
    'EVALUATE Big',
    {
      status: 'error',
      error: {
        category: 'overflow',
        message:
          'Integer value 1152921504606846976 in column "[big]" exceeds Number.MAX_SAFE_INTEGER and cannot be safely represented.',
      },
      requestId: '',
    },
  ],
];
for (const [query, expected] of arrowCases) {
  await check(`${query} (Arrow)`, async () => {
    const [ours, theirs] = await both(query);
    assert.deepEqual(ours, expected);
    assert.match(theirs.requestId, /^pbi-/);
    assert.deepEqual(ours, { ...theirs, requestId: '' });
  });
}

// Power BI failures: identical, Power BI's request id included.
for (const query of [
  'EVALUATE BadRequest',
  'EVALUATE Unauthorized',
  'EVALUATE Forbidden',
  'EVALUATE Missing',
]) {
  await check(`${query} (api)`, async () => {
    const [ours, theirs] = await both(query);
    assert.equal(ours.status, 'error');
    assert.equal(ours.error.category, 'api');
    assert.match(ours.requestId, /^pbi-/);
    assert.deepEqual(ours, theirs);
  });
}

// No request id from Power BI: each path reports the id it sent.
await check('EVALUATE Throttled (api, 429)', async () => {
  const [ours, theirs] = await both('EVALUATE Throttled');
  assert.equal(ours.error.category, 'api');
  assert.equal(ours.error.code, '429');
  assert.match(ours.requestId, UUID);
  assert.match(theirs.requestId, UUID);
  assert.deepEqual({ ...ours, requestId: '' }, { ...theirs, requestId: '' });
});

// A 200 that isn't Arrow: both 'unknown' (the direct path quotes its Arrow parser's error).
await check('EVALUATE Html (unknown)', async () => {
  const [ours, theirs] = await both('EVALUATE Html');
  assert.equal(ours.error.category, 'unknown');
  assert.equal(theirs.error.category, 'unknown');
  assert.equal(ours.requestId, theirs.requestId);
});

// Power BI unreachable: both 'network', with the id the request would have sent.
await check('unreachable Power BI (network)', async () => {
  const ours = await viaRayfinLocal(cfg.offlineApi).sales.executeQuery({ query: ROW });
  const theirs = await direct(cfg.offlinePowerBi).sales.executeQuery({ query: ROW });
  assert.equal(ours.status, 'error');
  assert.equal(ours.error.category, 'network');
  assert.equal(theirs.error.category, 'network');
  assert.match(ours.error.message, /^Couldn't reach Power BI at http:\/\/127\.0\.0\.1:\d+: /);
  assert.match(ours.requestId, UUID);
});

await check('no developer token (api, with the hint)', async () => {
  const ours = await viaRayfinLocal(cfg.tokenlessApi).sales.executeQuery({ query: ROW });
  assert.deepEqual(ours.error, {
    category: 'api',
    message: `rayfin-local couldn't get a Power BI token for the developer: ${cfg.tokenError}`,
    recoveryHint: cfg.tokenHint,
  });
  assert.match(ours.requestId, UUID);
});

// An Azure CLI token: executeDaxQueries refuses it, so rayfin-local answers from executeQueries
// (JSON) in the same envelope. The SDK normalizes it like the Arrow results above, without
// dataTypes and with Power BI's request id.
const az = viaRayfinLocal(cfg.azApi);
const jsonCases = [
  [
    ROW,
    {
      status: 'success',
      table: { columns: [{ name: '[x]', dataType: 'unknown' }], rows: [[1]] },
      requestId: 'pbi-json-row',
    },
  ],
  [
    'EVALUATE Typed',
    {
      status: 'success',
      table: {
        columns: ['Sales[Region]', 'Sales[OrderDate]', '[Total]', '[Orders]', '[HasProduct]'].map(
          (name) => ({ name, dataType: 'unknown' }),
        ),
        rows: [
          ['West', '2024-08-02T00:00:00', 1713.98, 12, true],
          ['East', '2023-09-27T01:12:12', 0.5, 3, false],
          [null, null, null, null, null],
        ],
      },
      requestId: 'pbi-json-typed',
    },
  ],
  [
    'EVALUATE Nope',
    {
      status: 'error',
      error: {
        category: 'query',
        message:
          "Query (1, 10) Failed to resolve name 'Nope'. It is not a valid table, variable, or function name.",
        code: '3241803821',
      },
      requestId: 'pbi-json-nope',
    },
  ],
  [
    'EVALUATE EvalError',
    {
      status: 'error',
      error: {
        category: 'query',
        message: "Query (1, 19) Cannot convert value 'a' of type Text to type Number.",
        code: '3241541768',
      },
      requestId: 'pbi-json-eval',
    },
  ],
  [
    'EVALUATE Many',
    {
      status: 'error',
      error: {
        category: 'overflow',
        message:
          'More than 100000 rows have been encountered. Only 100000 rows are allowed given the number of columns returned which is 1.',
        code: 'DaxRowCountNotSupported',
      },
      requestId: 'pbi-json-many',
    },
  ],
  [
    'EVALUATE Two',
    {
      status: 'success',
      table: { columns: [{ name: '[a]', dataType: 'unknown' }], rows: [[1]] },
      requestId: 'pbi-json-two',
    },
  ],
];
for (const [query, expected] of jsonCases) {
  await check(`${query} (az token, executeQueries)`, async () => {
    assert.deepEqual(await az.sales.executeQuery({ query }), expected);
  });
}
await check('EVALUATE Missing (az token, executeQueries, api)', async () => {
  const ours = await az.sales.executeQuery({ query: 'EVALUATE Missing' });
  const theirs = await cli.sales.executeQuery({ query: 'EVALUATE Missing' });
  assert.equal(ours.error.category, 'api');
  assert.deepEqual(ours, theirs);
});

// Requests the route refuses reject with the SDK's NetworkError.
async function rejects(promise, status, message) {
  await assert.rejects(promise, (error) => {
    assert.equal(error.name, 'NetworkError');
    assert.equal(error.status, status);
    if (message !== undefined) assert.equal(error.message, message);
    return true;
  });
}

await check('kusto (501)', () =>
  rejects(
    local.telemetry.executeQuery({ query: 'StormEvents | take 1' }),
    501,
    "Connector type 'kusto' isn't supported by rayfin-local yet; deploy with `npx rayfin up` to use it.",
  ),
);
await check('undeclared connector (404)', () =>
  rejects(
    local.nope.executeQuery({ query: ROW }),
    404,
    "Connector 'nope' is not declared in rayfin/rayfin.yml. Declared connectors: [sales, mine, telemetry, warehouse].",
  ),
);
await check('undeclared operation (400)', () =>
  rejects(local.sales.executeCommand({ query: ROW }), 400),
);
await check('anonymous caller (401)', () =>
  rejects(
    viaRayfinLocal(cfg.api, null).sales.executeQuery({ query: ROW }),
    401,
    "Sign in to use connector 'sales': its queries run as the signed-in user (delegated auth).",
  ),
);
await check('expired session (401)', () =>
  rejects(
    viaRayfinLocal(cfg.api, 'expired').sales.executeQuery({ query: ROW }),
    401,
    'The access token is invalid or has expired.',
  ),
);

process.stdout.write(`${results.join('\n')}\n`);
process.exit(results.some((r) => r.startsWith('FAIL')) ? 1 : 0);
