// Tests for js/function-host.mjs against tests/fixtures/functions/basic (npm install and
// `npm run build` it first). Run with `node --test js/test/function-host.test.mjs`.

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { randomBytes, randomUUID } from 'node:crypto';
import { existsSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import readline from 'node:readline';
import { after, before, describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const hostScript = path.join(here, '..', 'function-host.mjs');
const fixture = path.resolve(here, '..', '..', 'tests', 'fixtures', 'functions', 'basic');
const MARKER = '@@rayfin-local:';

if (!existsSync(path.join(fixture, 'dist', 'src', 'function_app.js'))) {
  throw new Error(`Build the fixture first: cd ${fixture} && npm install && npm run build`);
}

function startHost({ entry = 'dist/src/function_app.js', env = {} } = {}) {
  const secret = randomBytes(16).toString('hex');
  const child = spawn(process.execPath, [hostScript, '--entry', entry], {
    cwd: fixture,
    env: { ...process.env, ...env, RAYFIN_LOCAL_FUNCTION_HOST_SECRET: secret },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const output = [];
  const exited = new Promise((resolve) => child.on('exit', (code) => resolve(code)));
  const announced = new Promise((resolve, reject) => {
    readline.createInterface({ input: child.stdout }).on('line', (line) => {
      const at = line.indexOf(MARKER);
      if (at < 0) {
        output.push(line);
        return;
      }
      const rest = line.slice(at + MARKER.length);
      const space = rest.indexOf(' ');
      resolve({ kind: rest.slice(0, space), payload: JSON.parse(rest.slice(space + 1)) });
    });
    readline.createInterface({ input: child.stderr }).on('line', (line) => output.push(line));
    exited.then((code) => reject(new Error(`host exited with ${code}:\n${output.join('\n')}`)));
  });
  return { child, secret, announced, exited, output };
}

function send(port, message) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(port, '127.0.0.1');
    const chunks = [];
    socket.on('data', (chunk) => chunks.push(chunk));
    socket.on('error', reject);
    socket.on('end', () => {
      try {
        resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));
      } catch (error) {
        reject(error);
      }
    });
    socket.write(`${JSON.stringify(message)}\n`);
  });
}

function envelope(reply) {
  assert.equal(reply.ok, true, JSON.stringify(reply));
  return JSON.parse(reply.response.body);
}

function header(reply, name) {
  return reply.response.headers.find(([key]) => key === name)?.[1];
}

describe('function host (fixture)', () => {
  let host;
  let ready;
  const invoke = (functionName, body = {}, extra = {}) =>
    send(ready.port, {
      type: 'invoke',
      secret: host.secret,
      invocationId: randomUUID(),
      functionName,
      headers: { 'content-type': 'application/json' },
      body: typeof body === 'string' ? body : JSON.stringify(body),
      ...extra,
    });

  before(async () => {
    host = startHost({
      env: {
        AZURE_FUNCTIONS_ENVIRONMENT: 'Development',
        RAYFIN_API_URL: 'http://127.0.0.1:1/graphql',
        RAYFIN_PUBLISHABLE_KEY: 'pk_test',
        API_KEY: 'from-env',
      },
    });
    const { kind, payload } = await host.announced;
    assert.equal(kind, 'ready');
    const described = await send(payload.port, { type: 'describe', secret: host.secret });
    assert.equal(described.ok, true);
    ready = { port: payload.port, ...described };
  });

  after(async () => {
    await send(ready.port, { type: 'shutdown', secret: host.secret });
    assert.equal(await host.exited, 0);
  });

  test('reports every registration with its trigger and inputs', () => {
    const names = ready.functions.map((f) => f.name);
    assert.deepEqual(names, [
      'hello', 'echo', 'tokens', 'whoami', 'secret', 'fail', 'reject', 'logs', 'counter', 'slow', 'crash', 'bigint',
      'raw', 'nightly',
    ]);
    const hello = ready.functions.find((f) => f.name === 'hello');
    assert.deepEqual([hello.trigger, hello.methods, hello.inputs], ['httpTrigger', ['POST'], []]);
    const tokens = ready.functions.find((f) => f.name === 'tokens');
    assert.deepEqual(
      tokens.inputs.map((i) => [i.type, i.audienceType, i.argName]),
      [['FabricItem', 'ADO', '__generic_ADO'], ['FabricItem', 'Fabric', '__generic_Fabric']],
    );
    assert.match(tokens.inputs[0].name, /^FabricItemInput[0-9a-f]+$/);
    const raw = ready.functions.find((f) => f.name === 'raw');
    assert.deepEqual([raw.methods, raw.route], [['GET', 'POST'], 'raw/{id?}']);
    assert.equal(ready.functions.find((f) => f.name === 'nightly').trigger, 'timerTrigger');
    assert.equal(ready.programmingModel.name, '@azure/functions');
  });

  test('returns the UDF envelope for a function without parameters', async () => {
    const id = randomUUID();
    const reply = await send(ready.port, {
      type: 'invoke', secret: host.secret, invocationId: id, functionName: 'hello', headers: {}, body: '',
    });
    assert.equal(reply.response.status, 200);
    assert.equal(header(reply, 'x-fabric-udf-status'), 'Succeeded');
    assert.equal(header(reply, 'content-type'), 'application/json');
    assert.deepEqual(envelope(reply), {
      functionName: 'hello', invocationId: id, status: 'Succeeded', output: 'Hello from rayfin-local!', errors: [],
    });
  });

  test('binds typed positional parameters from the JSON body', async () => {
    const body = { text: 'hi', count: '3', flag: true, options: { a: 1 }, tags: ['x'], scores: [1, 2], note: null };
    const result = envelope(await invoke('echo', body));
    assert.equal(result.status, 'Succeeded');
    assert.deepEqual(result.output, { ...body, count: 3, greeting: 'hi' });

    const invalid = envelope(await invoke('echo', { ...body, flag: 'yes' }));
    assert.equal(invalid.status, 'BadRequest');
    assert.equal(invalid.errors[0].errorCode, 'InvalidInput');
  });

  test('passes FabricItem binding data to ctx.Tokens', async () => {
    const tokens = ready.functions.find((f) => f.name === 'tokens');
    const bindings = Object.fromEntries(
      tokens.inputs.map((input) => [
        input.name,
        { Endpoints: { [input.audienceType]: { AccessToken: input.audienceType === 'ADO' ? 'a'.repeat(5) : 'f'.repeat(7) } } },
      ]),
    );
    const result = envelope(await invoke('tokens', {}, { bindings }));
    assert.deepEqual(result.output, { audiences: ['ADO', 'Fabric'], adoLength: 5, fabricLength: 7 });

    const missing = envelope(await invoke('tokens'));
    assert.equal(missing.status, 'Failed');
    assert.match(missing.errors[0].message, /No token available for audience 'ADO'/);
  });

  test('builds the context from env and Authorization in Development mode', async () => {
    const reply = await invoke('whoami', {}, { headers: { authorization: 'Bearer abc' } });
    assert.deepEqual(envelope(reply).output, {
      baseUrl: 'http://127.0.0.1:1', hasPublishableKey: true, hasAccessToken: true,
    });
  });

  test('reads secrets from x-ms-rayfin-secrets, then the environment', async () => {
    const fromHeader = envelope(await invoke('secret', {}, { headers: { 'x-ms-rayfin-secrets': '{"API_KEY":"12345"}' } }));
    assert.deepEqual(fromHeader.output, { length: 5 });
    const fromEnv = envelope(await invoke('secret'));
    assert.deepEqual(fromEnv.output, { length: 'from-env'.length });
  });

  test('maps thrown errors to Failed and UserThrownError to BadRequest', async () => {
    const failed = envelope(await invoke('fail'));
    assert.equal(failed.status, 'Failed');
    assert.equal(failed.errors[0].errorCode, 'InternalError');
    assert.deepEqual(failed.errors[0].properties, { error_type: 'Error', error_message: 'boom' });

    const rejected = await invoke('reject', { reason: 'nope' });
    assert.equal(rejected.response.status, 200);
    assert.equal(header(rejected, 'x-fabric-udf-status'), 'BadRequest');
    assert.equal(envelope(rejected).errors[0].errorCode, 'UserThrown');
  });

  test('reports a failure when the worker produces no response', async () => {
    const reply = await invoke('bigint');
    assert.equal(reply.ok, false);
    assert.equal(reply.error.kind, 'failed');
    assert.match(reply.error.message, /BigInt/);
  });

  test('routes console output to the invocation log', async () => {
    const marker = randomUUID();
    await invoke('logs', { message: marker });
    await new Promise((resolve) => setTimeout(resolve, 100));
    const logged = (text) => host.output.some((line) => line.includes(text));
    assert.ok(logged(`logs: [UserDataFunctions] log: ${marker}`), host.output.join('\n'));
    assert.ok(logged(`logs: [UserDataFunctions] error: ${marker}`));
  });

  test('runs raw HTTP functions with their own method, route and response', async () => {
    const reply = await invoke('raw', '', { method: 'GET', url: 'http://localhost/api/raw/7?q=search', headers: {} });
    assert.equal(reply.response.status, 201);
    assert.equal(header(reply, 'x-raw'), 'yes');
    assert.deepEqual(JSON.parse(reply.response.body), { method: 'GET', id: null, q: 'search', body: null });
  });

  test('keeps module state and runs invocations concurrently', async () => {
    const first = envelope(await invoke('counter')).output;
    assert.equal(envelope(await invoke('counter')).output, first + 1);

    const started = Date.now();
    const results = await Promise.all(Array.from({ length: 8 }, () => invoke('slow', { ms: 300 })));
    assert.ok(Date.now() - started < 2000);
    assert.ok(results.every((reply) => envelope(reply).output === 300));
  });

  test('rejects requests without the secret and unknown functions', async () => {
    const unauthorized = await send(ready.port, { type: 'invoke', secret: 'wrong', functionName: 'hello' });
    assert.equal(unauthorized.error.kind, 'unauthorized');
    assert.equal((await invoke('nope')).error.kind, 'notFound');
    assert.equal((await invoke('nightly')).error.kind, 'notInvocable');
  });
});

describe('function host (failures)', () => {
  test('reports a load error with the message and exits 1', async () => {
    const host = startHost({ entry: 'dist/src/missing.js' });
    const { kind, payload } = await host.announced;
    assert.equal(kind, 'load-error');
    assert.match(payload.message, /missing\.js/);
    assert.equal(await host.exited, 1);
  });

  test('IsLocal=true selects the local status codes', async () => {
    const host = startHost({ env: { IsLocal: 'true' } });
    const { payload } = await host.announced;
    const call = (functionName, body) =>
      send(payload.port, {
        type: 'invoke', secret: host.secret, invocationId: randomUUID(), functionName, headers: {},
        body: JSON.stringify(body),
      });
    assert.equal((await call('fail', {})).response.status, 500);
    assert.equal((await call('reject', { reason: 'x' })).response.status, 422);
    assert.equal((await call('echo', {})).response.status, 400);
    host.child.kill();
    await host.exited;
  });

  test('exits when a function crashes the process', async () => {
    const host = startHost();
    const { payload } = await host.announced;
    const reply = await send(payload.port, {
      type: 'invoke', secret: host.secret, invocationId: randomUUID(), functionName: 'crash', headers: {}, body: '{}',
    });
    assert.equal(envelope(reply).output, 'crashing');
    assert.equal(await host.exited, 1);
    assert.ok(host.output.some((line) => line.includes('deliberate crash')));
  });

  test('shutdown lets in-flight invocations finish', async () => {
    const host = startHost();
    const { payload } = await host.announced;
    const slow = send(payload.port, {
      type: 'invoke', secret: host.secret, invocationId: randomUUID(), functionName: 'slow', headers: {},
      body: '{"ms": 300}',
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.deepEqual(await send(payload.port, { type: 'shutdown', secret: host.secret, drainMs: 5000 }), { ok: true });
    assert.equal(envelope(await slow).output, 300);
    assert.equal(await host.exited, 0);
  });
});
