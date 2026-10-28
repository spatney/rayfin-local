// Generates `runtimemetadata.json` for a Rayfin functions package, as `rayfin dev` (Rayfin CLI
// 1.36.2) does in its type generation step.
//
// `@microsoft/fabric-user-data-functions` 1.36 never parses source at runtime: `new
// UserDataFunctions()` reads `<cwd>/runtimemetadata.json` (schema 2.0), which lists every
// `udf.func(...)` registration with its delegate parameters and the audiences declared on its
// `RayfinContext<Schema, Audiences>` annotation. Type arguments are erased by the compiler, so the
// audiences can only come from a build-time pass with the TypeScript type checker.
//
// The analysis follows the CLI's rules exactly, so the output is byte-for-byte what the CLI writes
// for the same source:
// - A TypeScript program from the package's `tsconfig.json`, created with the package's own
//   `typescript`. Without a usable tsconfig, each `src/**/*.ts` file is parsed on its own and the
//   audiences are read from the annotation syntax.
// - Files under `src/`, except declaration files, `types.ts` and `*.test.ts` / `*.spec.ts`, in
//   order of their path relative to the package.
// - Calls `<expr>.func('<name>', <arrow function or function expression>, ...)`.
//
// Protocol (`rl_node::run_helper`): `node udf-metadata.mjs --input <in.json> --output <out.json>`.
// Input: `{ functionsDir, resolvePaths?: string[], outputPath?: string }`.
//   `resolvePaths` are extra directories to resolve `typescript` from, after the functions dir and
//   its ancestors; `outputPath`, when set, is written (atomically, only if its content changes).
// Result: `{ json, metadata, written, diagnostics, typescript: { version, path }, typeChecked,
//   sourceFiles }`, where `json` is the exact file content.

import fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';

const SCHEMA_VERSION = '2.0';
const CONTEXT_TYPE_NAME = 'RayfinContext';
const FABRIC_PARAMETER_TYPES = new Set([
  'FabricContext',
  'FabricSqlConnection',
  'FabricConnection',
  'DataConnection',
  'FabricUdf',
  'FabricLogger',
]);

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--input') args.input = argv[++i];
    else if (argv[i] === '--output') args.output = argv[++i];
  }
  if (!args.input || !args.output) {
    throw new Error('usage: node udf-metadata.mjs --input <file> --output <file>');
  }
  return args;
}

/** The functions package's own compiler, else the closest one up the tree, else the CLI's. */
function loadTypeScript(functionsDir, resolvePaths) {
  const bases = [functionsDir, ...resolvePaths, process.cwd()];
  for (const base of bases) {
    try {
      const require = createRequire(path.join(path.resolve(base), 'package.json'));
      const resolved = require.resolve('typescript');
      return { ts: require(resolved), path: resolved };
    } catch {
      // Not installed here; try the next base.
    }
  }
  for (const base of bases) {
    try {
      const require = createRequire(path.join(path.resolve(base), 'package.json'));
      const cliRequire = createRequire(require.resolve('@microsoft/rayfin-cli/package.json'));
      const resolved = cliRequire.resolve('typescript');
      return { ts: cliRequire(resolved), path: resolved };
    } catch {
      // No Rayfin CLI here either.
    }
  }
  throw new Error(
    `TypeScript was not found for the functions package in ${functionsDir}. ` +
      `Run \`npm install\` in ${functionsDir} (its package.json should list "typescript" in devDependencies).`,
  );
}

/** A program from `<functionsDir>/tsconfig.json`, or null when there is no usable config. */
function createProgram(ts, functionsDir) {
  const tsconfigPath = path.join(functionsDir, 'tsconfig.json');
  if (!fs.existsSync(tsconfigPath)) return null;
  const configFile = ts.readConfigFile(tsconfigPath, ts.sys.readFile);
  if (configFile.error) return null;
  const parsed = ts.parseJsonConfigFileContent(configFile.config, ts.sys, functionsDir);
  if (parsed.errors.length > 0 || parsed.fileNames.length === 0) return null;
  const program = ts.createProgram(parsed.fileNames, parsed.options);
  return { program, checker: program.getTypeChecker() };
}

function isExcludedSourceName(baseName) {
  return baseName === 'types.ts' || /\.(test|spec)\.ts$/.test(baseName);
}

/** `src/**\/*.ts` without dot entries, declaration files, `types.ts` and tests. */
function listSourceFiles(dir, files = []) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name.startsWith('.')) continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      listSourceFiles(full, files);
    } else if (
      entry.isFile() &&
      entry.name.endsWith('.ts') &&
      !entry.name.endsWith('.d.ts') &&
      !isExcludedSourceName(entry.name)
    ) {
      files.push(full);
    }
  }
  return files;
}

function collectSources(ts, functionsDir, srcDir, project) {
  const sources = [];
  if (project) {
    const normalized = srcDir.replace(/\\/g, '/');
    const prefix = normalized.endsWith('/') ? normalized : `${normalized}/`;
    for (const sourceFile of project.program.getSourceFiles()) {
      if (sourceFile.isDeclarationFile) continue;
      const fileName = sourceFile.fileName.replace(/\\/g, '/');
      if (!fileName.startsWith(prefix)) continue;
      if (isExcludedSourceName(fileName.slice(fileName.lastIndexOf('/') + 1))) continue;
      sources.push({ sourceFile, scriptFile: path.relative(functionsDir, sourceFile.fileName) });
    }
  } else {
    for (const file of listSourceFiles(srcDir)) {
      const sourceFile = ts.createSourceFile(
        file,
        fs.readFileSync(file, 'utf8'),
        ts.ScriptTarget.Latest,
        true,
        ts.ScriptKind.TS,
      );
      sources.push({ sourceFile, scriptFile: path.relative(functionsDir, file) });
    }
  }
  sources.sort((a, b) => a.scriptFile.localeCompare(b.scriptFile));
  return sources;
}

function isContextTypeNode(ts, node) {
  if (!ts.isTypeReferenceNode(node)) return false;
  const { typeName } = node;
  return (ts.isQualifiedName(typeName) ? typeName.right.text : typeName.text) === CONTEXT_TYPE_NAME;
}

/** Same test as the worker's `isRayfinContextType`. */
function isContextTypeText(typeText) {
  return (
    typeof typeText === 'string' &&
    (typeText === CONTEXT_TYPE_NAME || typeText.startsWith(`${CONTEXT_TYPE_NAME}<`))
  );
}

/** The `RayfinContext` instantiation behind a parameter type, looking through unions. */
function findContextType(type) {
  const candidates = type.isUnionOrIntersection() ? type.types : [type];
  return (
    candidates.find(
      (candidate) =>
        candidate.getSymbol()?.getName() === CONTEXT_TYPE_NAME ||
        candidate.aliasSymbol?.getName() === CONTEXT_TYPE_NAME,
    ) ?? null
  );
}

/** Audience names in a resolved type: string enum members surface as string literals. */
function audienceNamesFromType(ts, type) {
  const names = [];
  const visit = (current) => {
    if (current.flags & ts.TypeFlags.Never) return;
    if (current.isUnion()) {
      current.types.forEach(visit);
      return;
    }
    if (current.isStringLiteral()) {
      if (!names.includes(current.value)) names.push(current.value);
      return;
    }
    const symbolName = current.getSymbol()?.getName();
    if (symbolName && !names.includes(symbolName)) names.push(symbolName);
  };
  visit(type);
  return names;
}

/** Audience names written literally: `AudienceType.Sql`, `Sql` or `'Sql'`. */
function audienceNamesFromTypeNode(ts, node) {
  const names = [];
  const visit = (current) => {
    if (ts.isUnionTypeNode(current)) {
      current.types.forEach(visit);
    } else if (ts.isParenthesizedTypeNode(current)) {
      visit(current.type);
    } else if (
      ts.isLiteralTypeNode(current) &&
      ts.isStringLiteral(current.literal) &&
      current.literal.text.length > 0
    ) {
      if (!names.includes(current.literal.text)) names.push(current.literal.text);
    } else if (ts.isTypeReferenceNode(current)) {
      const { typeName } = current;
      const name = ts.isQualifiedName(typeName) ? typeName.right.text : typeName.text;
      if (name && name !== 'never' && !names.includes(name)) names.push(name);
    }
  };
  visit(node);
  return names;
}

/** A parameter's declared audiences; null when it is not a `RayfinContext`. */
function parameterAudiences(ts, parameter, checker) {
  if (checker) {
    const contextType = findContextType(checker.getTypeAtLocation(parameter));
    if (contextType) {
      const typeArguments = checker.getTypeArguments(contextType);
      return typeArguments.length < 2 ? [] : audienceNamesFromType(ts, typeArguments[1]);
    }
  }
  const typeNode = parameter.type;
  if (!typeNode || !isContextTypeNode(ts, typeNode)) return null;
  const typeArguments = typeNode.typeArguments;
  if (!typeArguments || typeArguments.length < 2) return [];
  return audienceNamesFromTypeNode(ts, typeArguments[1]);
}

/** Audiences across all parameters, deduplicated and sorted (stable across checker and syntax). */
function handlerAudiences(ts, handler, checker) {
  const audiences = [];
  for (const parameter of handler.parameters) {
    for (const audience of parameterAudiences(ts, parameter, checker) ?? []) {
      if (!audiences.includes(audience)) audiences.push(audience);
    }
  }
  return audiences.sort();
}

function isFabricParameter(typeText, name) {
  if (typeText && FABRIC_PARAMETER_TYPES.has(typeText)) return true;
  if (!name) return false;
  const lower = name.toLowerCase();
  return lower === 'context' || lower.includes('connection') || lower.includes('fabric');
}

function analyzeParameters(handler, sourceFile) {
  return handler.parameters.map((parameter, position) => {
    const name = parameter.name.getText(sourceFile);
    const typeText = parameter.type?.getText(sourceFile);
    const fabric = isFabricParameter(typeText, name);
    return {
      name,
      typeText,
      optional: parameter.questionToken !== undefined,
      hasDefault: parameter.initializer !== undefined,
      position,
      isFabricParameter: fabric,
      fabricParameterType: fabric ? typeText : undefined,
    };
  });
}

function analyzeFuncCall(ts, node, sourceFile, scriptFile, checker, diagnostics) {
  const callee = node.expression;
  if (!ts.isPropertyAccessExpression(callee) || callee.name.text !== 'func') return null;
  if (node.arguments.length < 2) return null;
  const [nameArgument, handler] = node.arguments;
  if (!ts.isStringLiteral(nameArgument)) return null;
  if (!ts.isArrowFunction(handler) && !ts.isFunctionExpression(handler)) return null;

  const functionName = nameArgument.text;
  const contextAudiences = handlerAudiences(ts, handler, checker);
  if (!checker && contextAudiences.length > 0) {
    const { line, character } = sourceFile.getLineAndCharacterOfPosition(
      handler.getStart(sourceFile),
    );
    diagnostics.push({
      code: 'unverified-context-audience',
      severity: 'warning',
      message:
        `Function '${functionName}': context audiences [${contextAudiences.join(', ')}] were read ` +
        'from the annotation text because the functions package has no usable tsconfig.json. ' +
        'Add a tsconfig.json, or declare audiences literally, e.g. RayfinContext<Schema, AudienceType.Sql>.',
      filePath: scriptFile,
      line: line + 1,
      column: character + 1,
      functionName,
    });
  }
  return { functionName, parameters: analyzeParameters(handler, sourceFile), contextAudiences };
}

function analyzeSourceFile(ts, sourceFile, scriptFile, checker, diagnostics) {
  const functions = [];
  const visit = (node) => {
    if (ts.isCallExpression(node)) {
      const fn = analyzeFuncCall(ts, node, sourceFile, scriptFile, checker, diagnostics);
      if (fn) functions.push(fn);
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
  return functions;
}

/**
 * Rewrites a context annotation so its audience argument is the resolved union spelled literally
 * (`RayfinContext<M, SqlAlias>` -> `RayfinContext<M, AudienceType.Sql>`); other type arguments
 * keep their source text.
 */
function normalizeContextAnnotation(ts, typeText, audiences) {
  const prefix = 'type __A = ';
  let sourceFile;
  try {
    sourceFile = ts.createSourceFile(
      '__annotation__.ts',
      `${prefix}${typeText};`,
      ts.ScriptTarget.ES2022,
      true,
    );
  } catch {
    return typeText;
  }
  const alias = sourceFile.statements.find((statement) => ts.isTypeAliasDeclaration(statement));
  if (!alias || !isContextTypeNode(ts, alias.type)) return typeText;
  const typeArguments = alias.type.typeArguments;
  const union =
    audiences.length === 0
      ? 'never'
      : audiences.map((audience) => `AudienceType.${audience}`).join(' | ');
  if (!typeArguments || typeArguments.length === 0) {
    return audiences.length === 0 ? typeText : `${CONTEXT_TYPE_NAME}<Record<string, any>, ${union}>`;
  }
  const local = (position) => position - prefix.length;
  if (typeArguments.length === 1) {
    if (audiences.length === 0) return typeText;
    const schemaEnd = local(typeArguments[0].getEnd());
    return `${typeText.slice(0, schemaEnd)}, ${union}${typeText.slice(schemaEnd)}`;
  }
  const start = local(typeArguments[1].getStart(sourceFile));
  const end = local(typeArguments[1].getEnd());
  return `${typeText.slice(0, start)}${union}${typeText.slice(end)}`;
}

function toRuntimeMetadata(ts, functions) {
  return {
    schemaVersion: SCHEMA_VERSION,
    functions: functions.map((fn) => ({
      functionName: fn.functionName,
      delegateParameters: fn.parameters.map((parameter) => {
        const type = isContextTypeText(parameter.typeText)
          ? normalizeContextAnnotation(ts, parameter.typeText, fn.contextAudiences)
          : parameter.typeText;
        // The worker rejects the whole file when `fabricParameterType` and `type` disagree, so a
        // copy of the annotation is normalized with it.
        const fabricParameterType =
          parameter.fabricParameterType !== undefined &&
          parameter.fabricParameterType === parameter.typeText
            ? type
            : parameter.fabricParameterType;
        return {
          name: parameter.name,
          ...(type === undefined ? {} : { type }),
          optional: parameter.optional,
          hasDefault: parameter.hasDefault,
          position: parameter.position,
          isFabricParameter: parameter.isFabricParameter,
          ...(fabricParameterType === undefined ? {} : { fabricParameterType }),
        };
      }),
      contextAudiences: [...fn.contextAudiences],
    })),
  };
}

/** Writes `content` to `file` through a sibling temp file, unless it already has that content. */
function writeIfChanged(file, content) {
  try {
    if (fs.readFileSync(file, 'utf8') === content) return false;
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  const temp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(temp, content);
  try {
    fs.renameSync(temp, file);
  } catch (error) {
    fs.rmSync(temp, { force: true });
    throw error;
  }
  return true;
}

function generate(input) {
  if (typeof input?.functionsDir !== 'string' || input.functionsDir.length === 0) {
    throw new Error('input.functionsDir is required');
  }
  const functionsDir = path.resolve(input.functionsDir);
  const resolvePaths = Array.isArray(input.resolvePaths) ? input.resolvePaths : [];
  const srcDir = path.join(functionsDir, 'src');
  if (!fs.existsSync(srcDir)) {
    throw new Error(
      `Functions source directory not found: ${srcDir}. Functions are registered with udf.func(...) in src/.`,
    );
  }

  const { ts, path: typescriptPath } = loadTypeScript(functionsDir, resolvePaths);
  const project = createProgram(ts, functionsDir);
  const sources = collectSources(ts, functionsDir, srcDir, project);
  if (sources.length === 0) {
    throw new Error(`No TypeScript source files found in ${srcDir}.`);
  }

  const diagnostics = [];
  const functions = sources.flatMap(({ sourceFile, scriptFile }) =>
    analyzeSourceFile(ts, sourceFile, scriptFile, project?.checker, diagnostics),
  );
  if (functions.length === 0) {
    throw new Error(
      `No udf.func() registrations found in ${srcDir}. ` +
        "Register functions with udf.func('name', async (...) => { ... }).",
    );
  }

  const metadata = toRuntimeMetadata(ts, functions);
  const json = JSON.stringify(metadata, null, 2);
  const written =
    typeof input.outputPath === 'string' && input.outputPath.length > 0
      ? writeIfChanged(path.resolve(input.outputPath), json)
      : null;
  return {
    json,
    metadata,
    written,
    diagnostics,
    typescript: { version: ts.version, path: typescriptPath },
    typeChecked: project !== null,
    sourceFiles: sources.map(({ scriptFile }) => scriptFile.replace(/\\/g, '/')),
  };
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
  response = { ok: true, result: generate(input) };
} catch (error) {
  response = { ok: false, error: errorInfo(error) };
}
fs.writeFileSync(args.output, JSON.stringify(response));
