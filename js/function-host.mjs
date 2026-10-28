// rayfin-local's function host: runs a Rayfin functions package (Fabric User Data Functions on the
// Azure Functions Node.js programming model) without Azure Functions Core Tools.
//
// It plays the part of the Azure Functions Node.js worker. It provides `@azure/functions-core`,
// the module the worker injects for `@azure/functions`, then imports the package's entry point so
// the app's own `@azure/functions` and `@microsoft/fabric-user-data-functions` register their
// functions. Invocations run through the library's own invocation model (`getArguments`,
// `invokeFunction`, `getResponse`), so the HttpRequest, InvocationContext and HTTP response are
// built by the same code as in Azure.
//
// Protocol (owner: crates/rl-functions/src/host.rs, which documents it in full):
//   node function-host.mjs --entry <file>       cwd: the functions package directory
//   env RAYFIN_LOCAL_FUNCTION_HOST_SECRET       the invoke socket's secret (removed from process.env)
//   stdout `@@rayfin-local:ready {"port":N}`    the entry loaded and the invoke socket listens
//   stdout `@@rayfin-local:load-error {...}`    it did not: {name, message, stack}; the host exits 1
//   TCP 127.0.0.1:<port>                        one request per connection, one JSON line each way:
//     {type: 'describe', secret} -> {ok: true, programmingModel, functions}
//     {type: 'invoke', secret, invocationId, functionName, method, url, headers, body, bodyEncoding, bindings}
//       -> {ok: true, response: {status, headers: [[name, value]], body, bodyEncoding}}
//        | {ok: false, error: {kind, name, message, stack}}
//     {type: 'shutdown', secret, drainMs} -> {ok: true}; in-flight invocations get drainMs, then exit 0
//   Bodies are UTF-8 text ('utf8') or, when they are not valid UTF-8, one char per byte ('latin1').
//
// The UDF package replaces `console.*` with loggers bound to the running invocation, so this file
// writes to process.stdout and process.stderr directly and never uses `console`.

import { timingSafeEqual } from 'node:crypto';
import { readFileSync } from 'node:fs';
import Module, { createRequire } from 'node:module';
import net from 'node:net';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const MARKER = '@@rayfin-local:';
const SECRET_ENV = 'RAYFIN_LOCAL_FUNCTION_HOST_SECRET';
const ERROR_LEVELS = new Set(['warning', 'error', 'critical']);
// Announcements are single stdout lines, which the reader splits beyond 64 KiB.
const ANNOUNCED_TEXT_MAX = 16 * 1024;

const stdout = process.stdout;
const stderr = process.stderr;

const secret = Buffer.from(process.env[SECRET_ENV] ?? '', 'utf8');
delete process.env[SECRET_ENV];

const functionAppDirectory = process.cwd();

function writeLine(stream, text) {
  stream.write(text.endsWith('\n') ? text : `${text}\n`);
}

function announce(kind, payload) {
  stdout.write(`${MARKER}${kind} ${JSON.stringify(payload)}\n`);
}

/** Exits once everything written so far has been handed to the OS (pipes are async on POSIX). */
function exitAfterFlush(code) {
  setTimeout(() => process.exit(code), 2000);
  let pending = 2;
  const done = () => {
    pending -= 1;
    if (pending === 0) process.exit(code);
  };
  stdout.write('', done);
  stderr.write('', done);
}

function describeError(error, maxLength = Infinity) {
  const clip = (text) => (typeof text === 'string' && text.length > maxLength ? `${text.slice(0, maxLength)}…` : text);
  if (error instanceof Error) {
    return { name: error.name, message: clip(error.message), stack: clip(error.stack) ?? null };
  }
  return { name: typeof error, message: clip(String(error)), stack: null };
}

// ---------------------------------------------------------------------------------------------
// `@azure/functions-core`: the surface the Azure Functions Node.js worker exposes.
// ---------------------------------------------------------------------------------------------

class Disposable {
  static from(...items) {
    let pending = items;
    return new Disposable(() => {
      for (const item of pending ?? []) {
        if (item && typeof item.dispose === 'function') item.dispose();
      }
      pending = undefined;
    });
  }

  #onDispose;

  constructor(onDispose) {
    this.#onDispose = onDispose;
  }

  dispose() {
    const onDispose = this.#onDispose;
    this.#onDispose = undefined;
    if (typeof onDispose === 'function') return onDispose();
  }
}

/** Errors the worker raises itself; `@azure/functions` checks the flag (e.g. for log hooks). */
function systemError(message, ErrorType = Error) {
  const error = new ErrorType(message);
  error.isAzureFunctionsSystemError = true;
  return error;
}

/** functionId -> { metadata, callback }, in registration order. */
const registered = new Map();
const hooks = { appStart: [], appTerminate: [], preInvocation: [], postInvocation: [], log: [] };
const appHookData = {};
let programmingModel;
let indexingLocked = false;

const coreApi = Object.freeze({
  // Nominal versions: those of a worker and host that have every API emulated here.
  version: '3.12.0',
  hostVersion: '4.1040.300',
  setProgrammingModel(model) {
    programmingModel = model;
  },
  getProgrammingModel() {
    return programmingModel;
  },
  registerFunction(metadata, callback) {
    if (indexingLocked) {
      throw systemError('A function can only be registered during app startup.');
    }
    const functionId = metadata.functionId || metadata.name;
    if (registered.has(functionId)) {
      throw systemError(`A function with id "${functionId}" has already been registered.`);
    }
    registered.set(functionId, { metadata: { ...metadata, functionId }, callback });
    return new Disposable(() => {
      if (indexingLocked) {
        throw systemError('A function can only be disposed during app startup.');
      }
      registered.delete(functionId);
    });
  },
  registerHook(hookName, callback) {
    if (!Object.hasOwn(hooks, hookName)) {
      throw systemError(`Unrecognized hook "${hookName}"`, RangeError);
    }
    const list = hooks[hookName];
    list.push(callback);
    return new Disposable(() => {
      const index = list.indexOf(callback);
      if (index >= 0) list.splice(index, 1);
    });
  },
  log(level, category, message) {
    writeLog(level, category, message, undefined);
  },
  Disposable,
});

// The worker hands `@azure/functions` this module through `require`, and only through `require`
// (an ESM `import '@azure/functions-core'` fails in Azure too), so that is all this intercepts.
const originalRequire = Module.prototype.require;
Module.prototype.require = new Proxy(originalRequire, {
  apply(target, thisArg, args) {
    if (args[0] === '@azure/functions-core') return coreApi;
    return Reflect.apply(target, thisArg, args);
  },
});

async function runHooks(name, context) {
  for (const hook of [...hooks[name]]) {
    await hook(context);
  }
}

/**
 * Writes a log record from `coreCtx.log` (user and system logs of an invocation) or
 * `coreApi.log` (system logs of the library), after the `log` hooks had their say.
 */
function writeLog(level, category, message, invocation) {
  if (hooks.log.length > 0) {
    const context = {
      hookData: {},
      appHookData,
      level,
      message,
      category,
      invocationContext: invocation?.context,
    };
    for (const hook of [...hooks.log]) hook(context);
    level = context.level;
    message = context.message;
  }
  if (level === 'none') return;
  if (category === 'system' && (level === 'debug' || level === 'trace')) return;
  const prefix = invocation ? `${invocation.functionName}: ` : '';
  writeLine(ERROR_LEVELS.has(level) ? stderr : stdout, `${prefix}${message}`);
}

// ---------------------------------------------------------------------------------------------
// Loading the functions package
// ---------------------------------------------------------------------------------------------

function isTrigger(type) {
  return typeof type === 'string' && /trigger$/i.test(type);
}

/** As the worker decides: `.mjs`/`.cjs`, else the function app's package.json `type`. */
function isEsModule(file) {
  if (file.endsWith('.mjs')) return true;
  if (file.endsWith('.cjs')) return false;
  try {
    const packageJson = JSON.parse(readFileSync(path.join(functionAppDirectory, 'package.json'), 'utf8'));
    return packageJson.type === 'module';
  } catch {
    return false;
  }
}

async function loadEntryPoint(entry) {
  const file = path.resolve(functionAppDirectory, entry);
  if (isEsModule(file)) {
    await import(pathToFileURL(file).href);
  } else {
    createRequire(file)(file);
  }
}

function describeFunctions() {
  return [...registered.values()].map(({ metadata }) => {
    const bindings = Object.entries(metadata.bindings ?? {});
    const trigger = bindings.find(([, binding]) => isTrigger(binding.type))?.[1];
    return {
      name: metadata.name,
      trigger: trigger?.type ?? null,
      methods: Array.isArray(trigger?.methods) ? trigger.methods.map((m) => String(m).toUpperCase()) : [],
      route: trigger?.route ?? null,
      authLevel: trigger?.authLevel ?? null,
      inputs: bindings
        .filter(([name, binding]) => binding.direction === 'in' && !isTrigger(binding.type) && name !== '$return')
        .map(([name, binding]) => {
          const { direction: _direction, properties: _properties, ...rest } = binding;
          return { ...rest, name };
        }),
    };
  });
}

// ---------------------------------------------------------------------------------------------
// Invocations
// ---------------------------------------------------------------------------------------------

function toTypedData(value) {
  if (value === null || value === undefined) return undefined;
  if (typeof value === 'string') return { string: value };
  if (typeof value === 'number') return Number.isInteger(value) ? { int: value } : { double: value };
  return { json: JSON.stringify(value) };
}

function typedDataToBuffer(data) {
  if (!data) return Buffer.alloc(0);
  if (data.bytes !== undefined && data.bytes !== null) return Buffer.from(data.bytes);
  if (data.stream !== undefined && data.stream !== null) return Buffer.from(data.stream);
  if (typeof data.string === 'string') return Buffer.from(data.string, 'utf8');
  if (typeof data.json === 'string') return Buffer.from(data.json, 'utf8');
  if (data.int !== undefined && data.int !== null) return Buffer.from(String(data.int), 'utf8');
  if (data.double !== undefined && data.double !== null) return Buffer.from(String(data.double), 'utf8');
  return Buffer.alloc(0);
}

const utf8 = new TextDecoder('utf-8', { fatal: true });

/** A body for the JSON reply: UTF-8 text, else one char per byte. */
function encodeBody(buffer) {
  try {
    return { body: utf8.decode(buffer), bodyEncoding: 'utf8' };
  } catch {
    return { body: buffer.toString('latin1'), bodyEncoding: 'latin1' };
  }
}

function nullableValue(value) {
  return value && typeof value === 'object' && 'value' in value ? value.value : value;
}

/** A `Set-Cookie` value from the worker's `RpcHttpCookie`. */
function setCookie(cookie) {
  let text = `${cookie.name}=${cookie.value}`;
  const domain = nullableValue(cookie.domain);
  const cookiePath = nullableValue(cookie.path);
  const expires = nullableValue(cookie.expires);
  const maxAge = nullableValue(cookie.maxAge);
  if (domain) text += `; Domain=${domain}`;
  if (cookiePath) text += `; Path=${cookiePath}`;
  if (expires && expires.seconds !== undefined) {
    const millis = Number(expires.seconds) * 1000 + Math.floor(Number(expires.nanos ?? 0) / 1e6);
    text += `; Expires=${new Date(millis).toUTCString()}`;
  }
  if (maxAge !== undefined && maxAge !== null) text += `; Max-Age=${maxAge}`;
  if (nullableValue(cookie.secure)) text += '; Secure';
  if (nullableValue(cookie.httpOnly)) text += '; HttpOnly';
  if (cookie.sameSite === 'lax') text += '; SameSite=Lax';
  else if (cookie.sameSite === 'strict') text += '; SameSite=Strict';
  else if (cookie.sameSite === 'explicitNone') text += '; SameSite=None';
  return text;
}

/** The HTTP response the Functions host would send for the worker's `returnValue`. */
function toHostResponse(returnValue) {
  const http = returnValue?.http;
  if (!http) {
    return { status: 204, headers: [], body: '', bodyEncoding: 'utf8' };
  }
  const headers = Object.entries(http.headers ?? {}).map(([name, value]) => [name, String(value)]);
  for (const cookie of http.cookies ?? []) headers.push(['set-cookie', setCookie(cookie)]);
  return {
    status: Number(http.statusCode ?? 200) || 200,
    headers,
    ...encodeBody(typedDataToBuffer(http.body)),
  };
}

async function runInvocation(request) {
  const functionName = String(request.functionName ?? '');
  const entry = registered.get(functionName);
  if (!entry) {
    return { ok: false, error: { kind: 'notFound', message: `No function named '${functionName}' is registered.` } };
  }
  const { metadata, callback } = entry;
  const bindings = Object.entries(metadata.bindings ?? {});
  const trigger = bindings.find(([, binding]) => isTrigger(binding.type));
  if (!trigger || trigger[1].type.toLowerCase() !== 'httptrigger') {
    return {
      ok: false,
      error: { kind: 'notInvocable', message: `Function '${functionName}' has a ${trigger?.[1].type ?? 'missing'} trigger, not an HTTP trigger.` },
    };
  }
  if (!programmingModel) {
    return { ok: false, error: { kind: 'failed', name: 'Error', message: 'No programming model was set.', stack: null } };
  }

  const invocationId = String(request.invocationId);
  const method = String(request.method ?? 'POST').toUpperCase();
  const http = {
    method,
    url: request.url ?? `http://localhost/api/${trigger[1].route ?? functionName}`,
    headers: request.headers ?? {},
    params: {},
  };
  if (method !== 'GET' && method !== 'HEAD') {
    http.body = { bytes: Buffer.from(request.body ?? '', request.bodyEncoding === 'latin1' ? 'latin1' : 'utf8') };
  }
  const inputData = [{ name: trigger[0], data: { http } }];
  for (const [name, value] of Object.entries(request.bindings ?? {})) {
    inputData.push({ name, data: toTypedData(value) });
  }

  const invocation = { functionName: metadata.name, context: undefined };
  const coreCtx = {
    invocationId,
    request: { invocationId, functionId: metadata.functionId, inputData, triggerMetadata: {} },
    metadata,
    state: undefined,
    log(level, category, message) {
      writeLog(level, category, message, invocation);
    },
  };

  const started = process.hrtime.bigint();
  const elapsed = () => Number((process.hrtime.bigint() - started) / 1_000_000n);
  try {
    const model = programmingModel.getInvocationModel(coreCtx);
    const { context, inputs } = await model.getArguments();
    invocation.context = context;
    const hookData = {};

    const pre = { hookData, appHookData, invocationContext: context, functionCallback: callback, inputs };
    coreCtx.state = 'preInvocationHooks';
    try {
      await runHooks('preInvocation', pre);
    } finally {
      coreCtx.state = undefined;
    }

    const post = { hookData, appHookData, invocationContext: context, inputs: pre.inputs, result: null, error: null };
    coreCtx.state = 'invocation';
    try {
      post.result = await model.invokeFunction(context, pre.inputs, pre.functionCallback);
    } catch (error) {
      post.error = error;
    } finally {
      coreCtx.state = undefined;
    }

    coreCtx.state = 'postInvocationHooks';
    try {
      await runHooks('postInvocation', post);
    } finally {
      coreCtx.state = undefined;
    }
    if (post.error !== null && post.error !== undefined) throw post.error;

    const response = toHostResponse((await model.getResponse(context, post.result)).returnValue);
    writeLine(stdout, `Executed 'Functions.${metadata.name}' (Succeeded, Id=${invocationId}, Duration=${elapsed()}ms)`);
    return { ok: true, response };
  } catch (error) {
    const described = describeError(error);
    writeLine(
      stderr,
      `Executed 'Functions.${metadata.name}' (Failed, Id=${invocationId}, Duration=${elapsed()}ms)\n` +
        `${described.stack ?? `${described.name}: ${described.message}`}`,
    );
    return { ok: false, error: { kind: 'failed', ...described } };
  }
}

function secretMatches(candidate) {
  const given = Buffer.from(typeof candidate === 'string' ? candidate : '', 'utf8');
  return secret.length > 0 && given.length === secret.length && timingSafeEqual(given, secret);
}

let shuttingDown = false;
/** Invocation connections not closed yet: their replies are delivered before the host exits. */
let inFlight = 0;
const idleWaiters = [];

function trackUntilClosed(socket) {
  inFlight += 1;
  socket.once('close', () => {
    inFlight -= 1;
    if (inFlight === 0) for (const resolve of idleWaiters.splice(0)) resolve();
  });
}

/** Resolves once no invocation is in flight, or after `timeoutMs`. */
function whenIdle(timeoutMs) {
  if (inFlight === 0) return Promise.resolve();
  return new Promise((resolve) => {
    idleWaiters.push(resolve);
    setTimeout(resolve, timeoutMs);
  });
}

async function shutdown(code, drainMs) {
  if (shuttingDown) return;
  shuttingDown = true;
  await whenIdle(drainMs);
  try {
    await Promise.race([
      runHooks('appTerminate', { hookData: appHookData, appHookData }),
      new Promise((resolve) => setTimeout(resolve, 2000)),
    ]);
  } catch (error) {
    writeLine(stderr, `appTerminate hook failed: ${describeError(error).message}`);
  }
  exitAfterFlush(code);
}

async function handleRequest(line, socket) {
  let request;
  try {
    request = JSON.parse(line);
  } catch (error) {
    return { ok: false, error: { kind: 'protocol', message: `Invalid request: ${describeError(error).message}` } };
  }
  if (!secretMatches(request?.secret)) {
    return { ok: false, error: { kind: 'unauthorized', message: 'Missing or wrong host secret.' } };
  }
  if (request.type === 'invoke') {
    trackUntilClosed(socket);
    return runInvocation(request);
  }
  if (request.type === 'describe') {
    return {
      ok: true,
      programmingModel: programmingModel ? { name: programmingModel.name, version: programmingModel.version } : null,
      functions: describeFunctions(),
    };
  }
  if (request.type === 'shutdown') {
    const drainMs = Number(request.drainMs);
    setImmediate(() => shutdown(0, Number.isFinite(drainMs) ? drainMs : 0));
    return { ok: true };
  }
  return { ok: false, error: { kind: 'protocol', message: `Unknown request type '${request.type}'.` } };
}

function onConnection(socket) {
  socket.setNoDelay(true);
  socket.on('error', () => {});
  const chunks = [];
  const onData = (chunk) => {
    const newline = chunk.indexOf(0x0a);
    if (newline < 0) {
      chunks.push(chunk);
      return;
    }
    chunks.push(chunk.subarray(0, newline));
    socket.off('data', onData);
    handleRequest(Buffer.concat(chunks).toString('utf8'), socket)
      .catch((error) => ({ ok: false, error: { kind: 'failed', ...describeError(error) } }))
      .then((reply) => {
        let text;
        try {
          text = JSON.stringify(reply);
        } catch (error) {
          text = JSON.stringify({ ok: false, error: { kind: 'failed', ...describeError(error) } });
        }
        socket.end(`${text}\n`);
      });
  };
  socket.on('data', onData);
}

function listen() {
  return new Promise((resolve, reject) => {
    const server = net.createServer(onConnection);
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => resolve(server.address().port));
  });
}

// ---------------------------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------------------------

process.on('uncaughtException', (error) => {
  writeLine(stderr, `Worker uncaught exception: ${describeError(error).stack ?? String(error)}`);
  exitAfterFlush(1);
});
for (const signal of ['SIGINT', 'SIGTERM', 'SIGBREAK']) {
  process.on(signal, () => shutdown(0, 0));
}

async function main() {
  const index = process.argv.indexOf('--entry');
  const entry = index >= 0 ? process.argv[index + 1] : undefined;
  try {
    if (!entry) throw new Error('function-host.mjs needs --entry <file>.');
    await loadEntryPoint(entry);
    await runHooks('appStart', { hookData: appHookData, appHookData, functionAppDirectory });
    const capabilities = programmingModel ? await programmingModel.getCapabilities({}) : {};
    if (capabilities && capabilities.HttpUri) {
      throw new Error(
        'The app enables HTTP streaming (app.setup({ enableHttpStream: true })), which rayfin-local does not support.',
      );
    }
    indexingLocked = true;
  } catch (error) {
    announce('load-error', describeError(error, ANNOUNCED_TEXT_MAX));
    exitAfterFlush(1);
    return;
  }
  announce('ready', { port: await listen() });
}

main().catch((error) => {
  announce('load-error', describeError(error, ANNOUNCED_TEXT_MAX));
  exitAfterFlush(1);
});
