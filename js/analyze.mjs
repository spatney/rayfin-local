// analyze.mjs: compile a Rayfin project's `rayfin/data` (and `rayfin/storage`) entities and run
// the project's own `@microsoft/rayfin-core` analyzer over them, the way `rayfin dev db apply`
// (CLI 1.36.2, utils/dab-config-generator.js and utils/storage-config-generator.js) does.
//
// One-shot helper (js/README.md): `node analyze.mjs --input <in.json> --output <out.json>`.
//
// Input:
//   projectRoot   absolute project directory (also the working directory)
//   dataDir       the entity sources, normally <projectRoot>/rayfin/data
//   storageDir    optional; the `@blob` sources, default <dataDir>/../storage
//   tsconfigPath  rayfin/tsconfig.json, or null to compile with built-in defaults
//   outDir        absolute folder for the compiled JavaScript; the only place this helper writes
//                 (besides the --output file). It is emptied first.
//   dialect       "mssql" | "postgresql"
//   storage       whether to analyze the storage folders too
//   verbose       optional; echo the analyzer's log to stderr
//
// Result (all JSON-safe; the Rust side is rl_model::AppModel::from_analysis):
//   coreVersion, typescriptVersion, dialect, featureFlags (RAYFIN_FEATURE_FLAGS, which changes
//   some analyzer output), entities (SchemaAnalyzer.analyzeEntities(), functions/RegExps/Dates
//   made JSON-safe), config (ConfigGenerator.generateConfig(): the DAB config with x-schema,
//   verbatim), policies (one record per database policy: entity, role, index into that role's
//   action list, action, text, and `check` = serializeCheckToAst(expr) or `checkError`), storage
//   ({folders, config} from analyzeStorageFolders()/StorageConfigGenerator, or null), sources
//   (project-relative module paths) and diagnostics.

import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { register } from 'node:module';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// Compiled file → its TypeScript source, for reporting.
const compiledSources = new Map();

const args = parseArgs(process.argv.slice(2));
let outcome;
try {
  const input = JSON.parse(readFileSync(args.input, 'utf8'));
  outcome = { ok: true, result: await analyze(input) };
} catch (error) {
  outcome = {
    ok: false,
    error: {
      message: error instanceof Error ? error.message : String(error),
      stack: error instanceof Error ? (error.stack ?? '') : '',
    },
  };
}
writeFileSync(args.output, JSON.stringify(outcome));
// User modules may leave timers or handles open; the analysis is complete.
process.exit(0);

function parseArgs(argv) {
  const parsed = {};
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--input') parsed.input = argv[(i += 1)];
    else if (argv[i] === '--output') parsed.output = argv[(i += 1)];
  }
  if (!parsed.input || !parsed.output) {
    process.stderr.write('usage: node analyze.mjs --input <file> --output <file>\n');
    process.exit(2);
  }
  return parsed;
}

async function analyze(input) {
  const projectRoot = path.resolve(required(input, 'projectRoot'));
  const dataDir = path.resolve(projectRoot, required(input, 'dataDir'));
  const storageDir = path.resolve(projectRoot, input.storageDir ?? path.join(dataDir, '..', 'storage'));
  const outDir = path.resolve(required(input, 'outDir'));
  const dialect = required(input, 'dialect');
  const tsconfigPath = input.tsconfigPath ? path.resolve(projectRoot, input.tsconfigPath) : null;
  const log = input.verbose ? (message = '') => process.stderr.write(`${message}\n`) : () => {};
  const diagnostics = [];

  installProjectResolution(projectRoot);
  const ts = (await import('typescript')).default;
  const core = await import('@microsoft/rayfin-core');
  const coreSchema = await import('@microsoft/rayfin-core/schema');
  const analysis = await import('@microsoft/rayfin-core/analysis');

  if (!analysis.isDialectSupported(dialect)) {
    throw new Error(`Dialect '${dialect}' is not supported (use mssql or postgresql in rayfin.yml).`);
  }

  // Strategy 1 of the CLI: a data service package with exports/main is imported as built.
  const serviceRoot = path.dirname(path.dirname(dataDir));
  const packageEntries = resolvePackageExports(serviceRoot);
  if (!packageEntries && !existsSync(dataDir)) {
    throw new Error(
      `The entity folder ${rel(projectRoot, dataDir)} does not exist. Create it, or disable ` +
        `services.data in rayfin/rayfin.yml.`,
    );
  }
  const storageExists = input.storage === true && existsSync(storageDir);
  if (input.storage === true && !storageExists) {
    diagnostics.push(warning(`Storage is enabled but ${rel(projectRoot, storageDir)} does not exist; no folders.`));
  }

  const entries = { data: [], storage: [] };
  let inputs = [];
  if (!packageEntries || storageExists) {
    const compiled = compile(ts, {
      projectRoot,
      tsconfigPath,
      outDir,
      entryDirs: [...(packageEntries ? [] : [dataDir]), ...(storageExists ? [storageDir] : [])],
      diagnostics,
    });
    inputs = compiled.inputs;
    if (!packageEntries) entries.data = compiled.entries.get(dataDir) ?? [];
    if (storageExists) entries.storage = compiled.entries.get(storageDir) ?? [];
  }
  if (packageEntries) {
    entries.data = packageEntries.map((entry) => path.resolve(serviceRoot, entry));
    for (const entry of entries.data) {
      if (!existsSync(entry)) {
        throw new Error(
          `The data package entry ${rel(projectRoot, entry)} (package.json exports/main) does not ` +
            `exist. Run the data build command first (e.g. npm run build).`,
        );
      }
    }
    diagnostics.push(warning(`Loaded entities from the data package exports: ${packageEntries.join(', ')}.`));
  } else if (entries.data.length === 0) {
    throw new Error(`No entity modules were compiled from ${rel(projectRoot, dataDir)}.`);
  }

  const foundEntities = {};
  for (const file of entries.data) {
    collect(await import(pathToFileURL(file).href), foundEntities, coreSchema.isRayfinEntity);
  }
  const entityClasses = Object.values(foundEntities);
  const analyzer = new analysis.SchemaAnalyzer(entityClasses, dialect, { log });
  const entities = analyzer.analyzeEntities();
  const config = new analysis.ConfigGenerator(dialect).generateConfig(entities);
  // Serialize before anything else touches the analyzer's objects, so `config` stays verbatim.
  const configJson = JSON.parse(JSON.stringify(config));
  const policies = collectPolicies(core, entityClasses, entities);

  let storage = null;
  if (input.storage === true) {
    const foundFolders = {};
    for (const file of entries.storage) {
      collect(await import(pathToFileURL(file).href), foundFolders, coreSchema.isRayfinStorageFolder);
    }
    // The CLI analyzes storage folders with the mssql dialect, whatever services.data says.
    const folders = new analysis.SchemaAnalyzer(Object.values(foundFolders), 'mssql', { log })
      .analyzeStorageFolders();
    const storageConfig =
      folders.length > 0
        ? new analysis.StorageConfigGenerator().generateConfig(folders)
        : { schemaVersion: analysis.STORAGE_CONFIG_SCHEMA_VERSION, folders: [] };
    storage = { folders: toJson(folders), config: JSON.parse(JSON.stringify(storageConfig)) };
  }

  return {
    coreVersion: packageVersion('@microsoft/rayfin-core', projectRoot),
    typescriptVersion: ts.version,
    dialect,
    featureFlags: process.env.RAYFIN_FEATURE_FLAGS ?? null,
    entities: toJson(entities),
    config: configJson,
    policies,
    storage,
    sources: {
      data: entries.data.map((file) => rel(projectRoot, sourceOf(file))),
      storage: entries.storage.map((file) => rel(projectRoot, sourceOf(file))),
      inputs: inputs.map((file) => rel(projectRoot, file)),
    },
    diagnostics,
  };
}

function required(input, key) {
  const value = input?.[key];
  if (typeof value !== 'string' || value.length === 0) {
    throw new Error(`analyze.mjs: input.${key} is required`);
  }
  return value;
}

function warning(message) {
  return { level: 'warning', message };
}

function rel(root, file) {
  const relative = path.relative(root, file);
  return (relative.startsWith('..') || path.isAbsolute(relative) ? file : relative).split(path.sep).join('/');
}

function isInside(dir, file) {
  const relative = path.relative(dir, file);
  return relative.length > 0 && !relative.startsWith('..') && !path.isAbsolute(relative);
}

/**
 * Makes bare specifiers that cannot be resolved from the importing module resolve from the
 * project instead: this helper and the compiled entities may live outside the project (tests,
 * read-only projects), yet must load the project's own `typescript` and `@microsoft/rayfin-core`.
 * Resolution that already works (the normal case under <project>/.rayfin-local) is untouched.
 */
function installProjectResolution(projectRoot) {
  const hooks = `
    let parents = [];
    export function initialize(data) { parents = data.parents; }
    const isBare = (s) => !s.startsWith('.') && !s.startsWith('/') && !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(s);
    export async function resolve(specifier, context, nextResolve) {
      try {
        return await nextResolve(specifier, context);
      } catch (error) {
        if (!isBare(specifier) || error?.code !== 'ERR_MODULE_NOT_FOUND') throw error;
        for (const parentURL of parents) {
          try { return await nextResolve(specifier, { ...context, parentURL }); } catch {}
        }
        throw error;
      }
    }`;
  register(`data:text/javascript,${encodeURIComponent(hooks)}`, import.meta.url, {
    data: { parents: [pathToFileURL(path.join(projectRoot, 'package.json')).href] },
  });
}

/** The version of the installed package `name`, as the project resolves it. */
function packageVersion(name, projectRoot) {
  const versionAbove = (start) => {
    for (let dir = start; ; dir = path.dirname(dir)) {
      const manifest = path.join(dir, 'package.json');
      if (existsSync(manifest)) {
        const pkg = JSON.parse(readFileSync(manifest, 'utf8'));
        if (pkg.name === name) return pkg.version ?? null;
      }
      if (path.dirname(dir) === dir) return null;
    }
  };
  try {
    return versionAbove(path.dirname(fileURLToPath(import.meta.resolve(name))));
  } catch {
    for (let dir = projectRoot; ; dir = path.dirname(dir)) {
      const manifest = path.join(dir, 'node_modules', ...name.split('/'), 'package.json');
      if (existsSync(manifest)) return JSON.parse(readFileSync(manifest, 'utf8')).version ?? null;
      if (path.dirname(dir) === dir) return null;
    }
  }
}

/** Same rules as the CLI's `resolvePackageExports`: a `.js` exports entry, else `main`. */
function resolvePackageExports(serviceRoot) {
  const manifest = path.join(serviceRoot, 'package.json');
  if (!existsSync(manifest)) return null;
  let pkg;
  try {
    pkg = JSON.parse(readFileSync(manifest, 'utf8'));
  } catch {
    return null;
  }
  const fromExports = (entry) => {
    if (typeof entry === 'string') return entry.endsWith('.js') ? [entry] : [];
    if (entry && typeof entry === 'object' && !Array.isArray(entry)) {
      if ('.' in entry) return fromExports(entry['.']);
      for (const key of ['import', 'require', 'default']) {
        if (typeof entry[key] === 'string' && entry[key].endsWith('.js')) return [entry[key]];
      }
    }
    return [];
  };
  if (pkg.exports) {
    const resolved = fromExports(pkg.exports);
    if (resolved.length > 0) return resolved;
  }
  return typeof pkg.main === 'string' ? [pkg.main] : null;
}

/** The CLI's `collectEntities`: exported classes, and classes inside exported arrays. */
function collect(mod, target, isWanted) {
  for (const [key, value] of Object.entries(mod)) {
    if (value && isWanted(value)) {
      target[key] = value;
    } else if (value && Array.isArray(value)) {
      for (const item of value) {
        if (isWanted(item)) target[item.name] = item;
      }
    }
  }
}

function sourceOf(file) {
  return compiledSources.get(file) ?? file;
}

/**
 * Compiles the rayfin TypeScript project like `tsc --build rayfin/tsconfig.json --force`, but
 * in process, with the output redirected to `outDir` and nothing else written: no declarations,
 * no build info, and a writeFile that refuses any path outside `outDir`. Returns, per entry
 * folder, the compiled `.js` of the folder's top-level modules (the CLI imports
 * `.temp/compiled/data/*.js`), sorted.
 */
function compile(ts, { projectRoot, tsconfigPath, outDir, entryDirs, diagnostics }) {
  const configDir = tsconfigPath ? path.dirname(tsconfigPath) : path.join(projectRoot, 'rayfin');
  let options;
  let rootNames;
  let projectReferences;
  let configErrors = [];
  if (tsconfigPath) {
    const unrecoverable = [];
    const parsed = ts.getParsedCommandLineOfConfigFile(tsconfigPath, undefined, {
      ...ts.sys,
      onUnRecoverableConfigFileDiagnostic: (diagnostic) => unrecoverable.push(diagnostic),
    });
    if (!parsed) throw compileError(ts, projectRoot, unrecoverable);
    options = { ...parsed.options };
    rootNames = [...parsed.fileNames];
    projectReferences = parsed.projectReferences;
    configErrors = parsed.errors;
    // A composite project's sources are laid out relative to its tsconfig folder.
    if (options.composite && options.rootDir === undefined) options.rootDir = configDir;
  } else {
    options = {
      target: ts.ScriptTarget.ES2022,
      module: ts.ModuleKind.NodeNext,
      moduleResolution: ts.ModuleResolutionKind.NodeNext,
      lib: ['lib.es2022.d.ts', 'lib.esnext.decorators.d.ts'],
      useDefineForClassFields: true,
      skipLibCheck: true,
      rootDir: configDir,
    };
    rootNames = [];
    diagnostics.push(warning('No rayfin/tsconfig.json; compiled rayfin with default TypeScript options.'));
  }
  Object.assign(options, {
    outDir,
    noEmit: false,
    composite: false,
    incremental: false,
    declaration: false,
    declarationMap: false,
    emitDeclarationOnly: false,
  });
  for (const key of ['tsBuildInfoFile', 'declarationDir', 'outFile', 'out']) delete options[key];

  const included = new Set(rootNames.map((file) => path.resolve(file)));
  for (const dir of entryDirs) {
    for (const file of listSources(dir)) {
      if (!included.has(file)) {
        included.add(file);
        rootNames.push(file);
        if (tsconfigPath) {
          diagnostics.push(warning(`${rel(projectRoot, file)} is not included by ${rel(projectRoot, tsconfigPath)}; compiled anyway.`));
        }
      }
    }
  }
  // "No inputs were found" is moot once the entry folders are added.
  configErrors = configErrors.filter((d) => d.code !== 18003);

  // Inside the project, only rayfin-local's own state folder may be written.
  if (
    outDir === path.parse(outDir).root ||
    outDir === projectRoot ||
    isInside(outDir, projectRoot) ||
    (isInside(projectRoot, outDir) && !isInside(path.join(projectRoot, '.rayfin-local'), outDir))
  ) {
    throw new Error(
      `analyze.mjs: refusing to use ${outDir} as the output folder; inside the project it must be ` +
        `under .rayfin-local/.`,
    );
  }
  for (const file of included) {
    if (isInside(outDir, file)) {
      throw new Error(`analyze.mjs: the output folder ${outDir} contains project sources (${file}).`);
    }
  }

  const program = ts.createProgram({
    rootNames,
    options,
    projectReferences,
    configFileParsingDiagnostics: configErrors,
  });
  const errors = ts.getPreEmitDiagnostics(program).filter((d) => d.category === ts.DiagnosticCategory.Error);
  if (errors.length > 0) throw compileError(ts, projectRoot, errors);

  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });
  const outputs = new Map(); // source file → emitted .js files
  const formats = new Map(); // output folder → "module" | "commonjs"
  const emit = program.emit(undefined, (fileName, text, writeByteOrderMark, _onError, sourceFiles) => {
    const target = path.resolve(fileName);
    if (!isInside(outDir, target)) {
      throw new Error(`analyze.mjs: refusing to write ${target}, which is outside ${outDir}.`);
    }
    mkdirSync(path.dirname(target), { recursive: true });
    writeFileSync(target, writeByteOrderMark ? `\uFEFF${text}` : text);
    if (!target.endsWith('.js')) return;
    for (const sourceFile of sourceFiles ?? []) {
      const source = path.resolve(sourceFile.fileName);
      outputs.set(source, [...(outputs.get(source) ?? []), target]);
      compiledSources.set(target, source);
      const format = isEsm(ts, program, options, sourceFile) ? 'module' : 'commonjs';
      const dir = path.dirname(target);
      if (formats.has(dir) && formats.get(dir) !== format) {
        throw new Error(`analyze.mjs: ${dir} would hold both ES and CommonJS modules.`);
      }
      formats.set(dir, format);
    }
  });
  const emitErrors = emit.diagnostics.filter((d) => d.category === ts.DiagnosticCategory.Error);
  if (emitErrors.length > 0) throw compileError(ts, projectRoot, emitErrors);
  // Node picks the module format of a `.js` file from the nearest package.json; outDir is
  // outside the project's package scope, so state the format TypeScript emitted.
  for (const [dir, type] of formats) {
    writeFileSync(path.join(dir, 'package.json'), `${JSON.stringify({ type })}\n`);
  }
  if (!formats.has(outDir)) {
    const type = formats.size > 0 ? [...formats.values()][0] : 'module';
    writeFileSync(path.join(outDir, 'package.json'), `${JSON.stringify({ type })}\n`);
  }

  const entries = new Map();
  for (const dir of entryDirs) {
    const files = [];
    for (const [source, emitted] of outputs) {
      if (path.dirname(source) === dir) files.push(...emitted);
    }
    entries.set(dir, files.sort((a, b) => a.localeCompare(b)));
  }
  const inputs = program
    .getSourceFiles()
    .map((sourceFile) => path.resolve(sourceFile.fileName))
    .filter((file) => isInside(projectRoot, file) && !file.split(path.sep).includes('node_modules'))
    .sort((a, b) => a.localeCompare(b));
  return { entries, inputs };
}

/** TypeScript and JavaScript sources the CLI's glob would reach once compiled. */
function listSources(dir) {
  if (!existsSync(dir)) return [];
  return readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isFile() && /\.(ts|tsx|mts|cts)$/.test(entry.name) && !/\.d\.[cm]?ts$/.test(entry.name))
    .map((entry) => path.join(dir, entry.name))
    .sort((a, b) => a.localeCompare(b));
}

function isEsm(ts, program, options, sourceFile) {
  const kind = ts.ModuleKind;
  const format =
    typeof program.getEmitModuleFormatOfFile === 'function'
      ? program.getEmitModuleFormatOfFile(sourceFile)
      : (sourceFile.impliedNodeFormat ??
        options.module ??
        (options.target >= ts.ScriptTarget.ES2015 ? kind.ES2015 : kind.CommonJS));
  return [kind.ES2015, kind.ES2020, kind.ES2022, kind.ESNext, kind.Preserve].includes(format);
}

function compileError(ts, projectRoot, list) {
  const lines = list.map((diagnostic) => {
    const message = ts.flattenDiagnosticMessageText(diagnostic.messageText, '\n  ');
    if (diagnostic.file && diagnostic.start !== undefined) {
      const { line, character } = diagnostic.file.getLineAndCharacterOfPosition(diagnostic.start);
      return `${rel(projectRoot, diagnostic.file.fileName)}:${line + 1}:${character + 1} - error TS${diagnostic.code}: ${message}`;
    }
    return `error TS${diagnostic.code}: ${message}`;
  });
  return new Error(`TypeScript compilation of rayfin failed:\n${lines.join('\n')}`);
}

/**
 * Evaluates every `@role` policy lambda once more, exactly as `SchemaAnalyzer.aggregatePermissions`
 * does, to get the expression tree behind each `policy.database` string, and serializes it with
 * rayfin-core's `serializeCheckToAst` (the analyzer only does that for storage policies).
 * `index` is the action's position in the role's list, in both `entities[].permissions` and the
 * DAB config.
 */
function collectPolicies(core, entityClasses, entities) {
  const order = { create: 1, read: 2, update: 3, delete: 4, execute: 5, '*': 99 };
  const records = [];
  for (const entityClass of entityClasses) {
    const meta = core.getEntityMetadata(entityClass);
    const analyzed = entities.find((entity) => entity.name === meta.name);
    if (!analyzed || !meta.roles || meta.roles.length === 0) continue;
    const byRole = new Map();
    for (const decl of meta.roles) {
      byRole.set(decl.role, [...(byRole.get(decl.role) ?? []), decl]);
    }
    for (const role of [...byRole.keys()].sort()) {
      let index = 0;
      for (const decl of byRole.get(role)) {
        for (const action of [...decl.actions].sort((a, b) => order[a] - order[b])) {
          if (decl.policy) {
            const expr = decl.policy.check(core.claims, core.createItemProxy());
            const text = expr.toString();
            const entry = analyzed.permissions[role]?.[index];
            if (entry?.action !== action || entry?.policy?.database !== text) {
              throw new Error(
                `analyze.mjs: the policy of ${meta.name} (${role}, ${action}) does not match the ` +
                  `analyzer's permissions; this @microsoft/rayfin-core version is not supported.`,
              );
            }
            const record = { entity: meta.name, role, index, action, text };
            try {
              record.check = core.serializeCheckToAst(expr);
            } catch (error) {
              record.checkError = error instanceof Error ? error.message : String(error);
            }
            records.push(record);
          }
          index += 1;
        }
      }
    }
  }
  return records;
}

/** A JSON-safe deep copy of analyzer output (relationship targets, regexes, dates, functions). */
function toJson(value, seen = new Set()) {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
  if (typeof value === 'number') return Number.isFinite(value) ? value : String(value);
  if (typeof value === 'bigint') return value.toString();
  if (typeof value === 'undefined' || typeof value === 'symbol') return undefined;
  if (typeof value === 'function') return { $function: value.name || null };
  if (value instanceof Date) return value.toISOString();
  if (value instanceof RegExp) return { $regex: value.source, flags: value.flags };
  if (seen.has(value)) return { $circular: true };
  seen.add(value);
  let copy;
  if (Array.isArray(value)) {
    copy = value.map((item) => toJson(item, seen) ?? null);
  } else {
    copy = {};
    for (const [key, item] of Object.entries(value)) {
      if (key === 'relationship' && item && typeof item.target === 'function') {
        copy[key] = { ...toJson({ ...item, target: undefined }, seen), target: targetName(item.target) };
        continue;
      }
      const json = toJson(item, seen);
      if (json !== undefined) copy[key] = json;
    }
  }
  seen.delete(value);
  return copy;
}

function targetName(resolver) {
  try {
    const target = resolver();
    return typeof target === 'function' && target.name ? target.name : null;
  } catch {
    return null;
  }
}
