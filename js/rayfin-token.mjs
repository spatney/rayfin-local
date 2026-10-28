// Mints an access token with the developer's Rayfin CLI login, for rl-tokens' Rayfin CLI provider
// (`--token-source rayfin`, and the fallback when the Azure CLI can't get a token).
//
// It uses the project's own `@microsoft/rayfin-cli` (a devDependency of Rayfin projects):
// `getAuthenticatedToken(scopes, { tenantId })` from `@microsoft/rayfin-cli/auth` serves the token
// from the CLI's MSAL cache (`rayfin login`). When the cache can't serve the scope silently, the
// CLI signs in interactively: a browser window, or a device code printed to the log in headless
// environments. rl-tokens waits up to 180 s for that.
//
// Protocol (`rl_node::run_helper`): `node rayfin-token.mjs --input <in.json> --output <out.json>`,
// run with the project root as the working directory.
// Input: `{ scopes: string[], tenantId?: string }`.
// Result: `{ token, expiresOnTimestamp, tenantId? }`, `expiresOnTimestamp` in ms since the epoch.
// The token only goes to the output file; never print it.

import fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const PACKAGE = '@microsoft/rayfin-cli';
// rl-tokens recognizes this message (src/rayfin_cli.rs).
const NOT_INSTALLED = `${PACKAGE} is not installed`;
const FABRIC_SCOPE = 'https://api.fabric.microsoft.com/.default';

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--input') args.input = argv[++i];
    else if (argv[i] === '--output') args.output = argv[++i];
  }
  if (!args.input || !args.output) {
    throw new Error('usage: node rayfin-token.mjs --input <file> --output <file>');
  }
  return args;
}

/** The project's package directory, searched the way Node resolves it from the project. */
function findPackage(projectRoot) {
  const require = createRequire(path.join(projectRoot, 'package.json'));
  for (const dir of require.resolve.paths(PACKAGE) ?? []) {
    const candidate = path.join(dir, PACKAGE);
    if (fs.existsSync(path.join(candidate, 'package.json'))) return candidate;
  }
  return undefined;
}

/** The file a package `exports` entry maps to for `import` from Node. */
function exportTarget(entry) {
  if (typeof entry === 'string') return entry;
  if (entry && typeof entry === 'object') {
    for (const condition of ['node', 'import', 'default']) {
      const target = exportTarget(entry[condition]);
      if (target) return target;
    }
  }
  return undefined;
}

async function loadAuth(projectRoot) {
  const packageDir = findPackage(projectRoot);
  if (!packageDir) throw new Error(`${NOT_INSTALLED} in ${projectRoot}`);
  const manifest = JSON.parse(fs.readFileSync(path.join(packageDir, 'package.json'), 'utf8'));
  const target = exportTarget(manifest.exports?.['./auth']) ?? './dist/auth/index.js';
  const auth = await import(pathToFileURL(path.join(packageDir, target)).href);
  if (typeof auth.getAuthenticatedToken !== 'function') {
    throw new Error(
      `${PACKAGE} ${manifest.version ?? ''} has no getAuthenticatedToken in ${PACKAGE}/auth; ` +
        'rayfin-local supports Rayfin 1.36',
    );
  }
  return auth;
}

async function mint({ scopes, tenantId }) {
  if (!Array.isArray(scopes) || scopes.length === 0) throw new Error('no scopes were requested');
  const auth = await loadAuth(process.cwd());
  const options = tenantId ? { tenantId } : {};
  const wanted = scopes.join(' ');

  // The CLI returns RAYFIN_TOKEN (set in CI and sandboxes) for any scope, but it holds a Fabric
  // token.
  const fabricScope = typeof auth.getFabricScope === 'function' ? auth.getFabricScope() : FABRIC_SCOPE;
  if (process.env.RAYFIN_TOKEN && scopes.some((scope) => scope !== fabricScope)) {
    console.log(`Ignoring RAYFIN_TOKEN for ${wanted}: it holds a Fabric token.`);
    delete process.env.RAYFIN_TOKEN;
  }

  let result;
  if (typeof auth.ensureAuthenticated === 'function') {
    // Silently first, so the log can say when a sign-in is about to start.
    try {
      result = await auth.ensureAuthenticated(scopes, { ...options, silent: true });
    } catch {
      console.log(
        `The Rayfin CLI's cached sign-in can't serve ${wanted}: signing in interactively ` +
          '(a browser window opens; rayfin-local waits up to 180 s).',
      );
    }
  } else {
    console.log(`Getting a token for ${wanted} with the Rayfin CLI (it may open a browser to sign in).`);
  }
  result ??= await auth.getAuthenticatedToken(scopes, options);
  if (!result?.token) throw new Error('the Rayfin CLI returned no token');
  return {
    token: result.token,
    expiresOnTimestamp: result.expiresOnTimestamp,
    tenantId: result.tenantId,
  };
}

const args = parseArgs(process.argv.slice(2));
let response;
try {
  const input = JSON.parse(fs.readFileSync(args.input, 'utf8'));
  response = { ok: true, result: await mint(input) };
} catch (error) {
  response = { ok: false, error: { message: error?.message ?? String(error), stack: error?.stack } };
}
// The answer carries an access token: readable by the current user only (where modes apply).
fs.writeFileSync(args.output, JSON.stringify(response), { mode: 0o600 });
// MSAL may leave handles open (its sign-in redirect server, cache locks); the answer is written.
setTimeout(() => process.exit(0), 200).unref();
