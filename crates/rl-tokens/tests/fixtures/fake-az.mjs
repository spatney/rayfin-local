// Stands in for the Azure CLI in rl-tokens' tests. Run as
// `node fake-az.mjs <state dir> <behavior> account get-access-token --scope <s> --output json
// [--tenant <t>]`; each run records its arguments and timing in `<state dir>/call-<pid>.json`.

import fs from 'node:fs';
import path from 'node:path';

const [stateDir, behavior, ...args] = process.argv.slice(2);
const option = (name) => (args.includes(name) ? args[args.indexOf(name) + 1] : undefined);
const scope = option('--scope');
const tenant = option('--tenant');
const start = Date.now();

function record() {
  const call = { args, scope, tenant, start, end: Date.now() };
  fs.writeFileSync(path.join(stateDir, `call-${process.pid}.json`), JSON.stringify(call));
}

function jwt(claims) {
  const encode = (value) => Buffer.from(JSON.stringify(value)).toString('base64url');
  return `${encode({ alg: 'RS256', typ: 'JWT' })}.${encode(claims)}.signature`;
}

function succeed(lifetimeSeconds, token = `fake-token-for-${scope}-${process.pid}`) {
  const expires = new Date(start + lifetimeSeconds * 1000);
  const pad = (n, width = 2) => String(n).padStart(width, '0');
  const expiresOn =
    `${expires.getFullYear()}-${pad(expires.getMonth() + 1)}-${pad(expires.getDate())} ` +
    `${pad(expires.getHours())}:${pad(expires.getMinutes())}:${pad(expires.getSeconds())}.000000`;
  record();
  const output = {
    accessToken: token,
    expiresOn,
    expires_on: Math.floor(expires.getTime() / 1000),
    subscription: '00000000-0000-0000-0000-000000000000',
    tenant: tenant ?? '11111111-1111-1111-1111-111111111111',
    tokenType: 'Bearer',
  };
  process.stdout.write(`${JSON.stringify(output, null, 2)}\n`);
}

function fail(stderr) {
  record();
  process.stderr.write(stderr);
  process.exitCode = 1;
}

switch (behavior) {
  case 'ok':
    succeed(60 * 60);
    break;
  case 'short-lived':
    succeed(4 * 60);
    break;
  case 'slow':
    await new Promise((resolve) => setTimeout(resolve, 1000));
    succeed(60 * 60);
    break;
  case 'user':
    succeed(
      60 * 60,
      jwt({
        aud: 'https://analysis.windows.net/powerbi/api',
        upn: 'dev@contoso.com',
        tid: '72f988bf-0000-0000-0000-000000000000',
        oid: '0b3c5e2a-0000-0000-0000-000000000000',
        name: 'Dev Eloper',
        exp: Math.floor(start / 1000) + 60 * 60,
      }),
    );
    break;
  case 'not-signed-in':
    fail("ERROR: Please run 'az login' to setup account.\n");
    break;
  default:
    fail(`ERROR: fake-az.mjs has no behavior '${behavior}'\n`);
}
