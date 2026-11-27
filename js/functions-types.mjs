// Regenerates the functions' generated TypeScript with the project's own Rayfin CLI, as `rayfin dev`
// does when it starts and whenever the functions' sources change:
// - `src/types.ts`, whose `AppFunctionsSchema` types `client.functions.<name>.invoke(...)` in the
//   frontend;
// - `src/secrets.generated.ts`, the secret names that type `ctx.Secrets`;
// - with SDK 2.0, the dynamic auth claims contract, when the app declares one.
//
// It runs `generateFunctionsTypes(functionsDir)` from the CLI's
// `dist/utils/functions-types-generator.js`, which the 1.36 and 2.0 CLIs have but don't export:
// what it writes depends on the CLI's version, and no CLI command only generates these files.
// That function also writes `runtimemetadata.json`, which rayfin-local generates itself for its
// function host (`udf-metadata.mjs`; the 2.0 CLI writes schema 3.0, which rayfin-local doesn't
// read), so this helper puts that file back as it was.
//
// With `secretsOnly`, it runs only `ensureSecretsTypes(functionsDir)` from
// `dist/utils/secrets-types-generator.js`, as `rayfin dev` does before it builds the functions: a
// build of handlers that read `ctx.Secrets.<NAME>` fails without the secrets' names. The CLI
// passes what goes wrong there to a callback, and the helper fails with it.
//
// Protocol (`rl_node::run_helper`): `node functions-types.mjs --input <in.json> --output <out.json>`,
// run in the functions package.
// Input: `{ functionsDir, secretsOnly? }`.
// Result: `{ cliVersion, written }`: the CLI's version, and the files it changed, relative to the
// functions package with `/` separators (`src/types.ts`, `src/secrets.generated.ts`).

import fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const PACKAGE = '@microsoft/rayfin-cli';
const GENERATOR = 'dist/utils/functions-types-generator.js';
const SECRETS_GENERATOR = 'dist/utils/secrets-types-generator.js';
const METADATA_FILE = 'runtimemetadata.json';
/** The generated files `written` reports. */
const GENERATED = ['src/types.ts', 'src/secrets.generated.ts'];

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--input') args.input = argv[++i];
    else if (argv[i] === '--output') args.output = argv[++i];
  }
  if (!args.input || !args.output) {
    throw new Error('usage: node functions-types.mjs --input <file> --output <file>');
  }
  return args;
}

/** The CLI's package directory, searched the way Node resolves it from the functions package. */
function findCli(functionsDir) {
  const require = createRequire(path.join(functionsDir, 'package.json'));
  for (const dir of require.resolve.paths(PACKAGE) ?? []) {
    const candidate = path.join(dir, PACKAGE);
    if (fs.existsSync(path.join(candidate, 'package.json'))) return candidate;
  }
  return undefined;
}

/** The file's bytes, or `null` when there is no file. */
function read(file) {
  try {
    return fs.readFileSync(file);
  } catch (error) {
    if (error.code === 'ENOENT') return null;
    throw error;
  }
}

/** Puts `content` (`null`: no file) back in `file`, through a sibling temp file, if it changed. */
function restore(file, content) {
  const current = read(file);
  if (content === null) {
    if (current !== null) fs.rmSync(file, { force: true });
    return;
  }
  if (current !== null && current.equals(content)) return;
  const temp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(temp, content);
  try {
    fs.renameSync(temp, file);
  } catch (error) {
    fs.rmSync(temp, { force: true });
    throw error;
  }
}

/** The CLI's function `name` in its module `file`. */
async function cliFunction(cliDir, version, file, name, what) {
  const modulePath = path.join(cliDir, file);
  const module = fs.existsSync(modulePath) ? await import(pathToFileURL(modulePath).href) : {};
  if (typeof module[name] !== 'function') {
    throw new Error(
      `${PACKAGE} ${version} has no ${what} (${name} in ${file}). Install the Rayfin CLI of the ` +
        "app's Rayfin version.",
    );
  }
  return module[name];
}

async function generate(input) {
  if (typeof input?.functionsDir !== 'string' || input.functionsDir.length === 0) {
    throw new Error('input.functionsDir is required');
  }
  const functionsDir = path.resolve(input.functionsDir);
  const cliDir = findCli(functionsDir);
  if (!cliDir) {
    throw new Error(`${PACKAGE} is not installed. Run \`npm install\` in the project.`);
  }
  const { version } = JSON.parse(fs.readFileSync(path.join(cliDir, 'package.json'), 'utf8'));
  const before = GENERATED.map((file) => read(path.join(functionsDir, file)));
  if (input.secretsOnly === true) {
    const ensureSecretsTypes = await cliFunction(
      cliDir,
      version,
      SECRETS_GENERATOR,
      'ensureSecretsTypes',
      'type generator for secrets',
    );
    const problems = [];
    ensureSecretsTypes(functionsDir, (message) => problems.push(message));
    if (problems.length > 0) throw new Error(problems.join('\n'));
  } else {
    const generateFunctionsTypes = await cliFunction(
      cliDir,
      version,
      GENERATOR,
      'generateFunctionsTypes',
      'type generator for functions',
    );
    const metadataPath = path.join(functionsDir, METADATA_FILE);
    const metadata = read(metadataPath);
    try {
      await generateFunctionsTypes(functionsDir);
    } finally {
      restore(metadataPath, metadata);
    }
  }
  const written = GENERATED.filter((file, index) => {
    const after = read(path.join(functionsDir, file));
    return after !== null && !(before[index]?.equals(after) ?? false);
  });
  return { cliVersion: version, written };
}

function errorInfo(error) {
  return error instanceof Error
    ? { message: error.message, stack: error.stack }
    : { message: String(error) };
}

const args = parseArgs(process.argv.slice(2));
let response;
try {
  const input = JSON.parse(fs.readFileSync(args.input, 'utf8'));
  response = { ok: true, result: await generate(input) };
} catch (error) {
  response = { ok: false, error: errorInfo(error) };
}
fs.writeFileSync(args.output, JSON.stringify(response));
