// Writes the Apache Arrow IPC streams under ../fixtures, shaped like Power BI's
// `executeDaxQueries` answers: every result column dictionary-encoded with Int8 keys, LZ4-frame
// compressed buffers, and the in-band DAX error table.
//
// node make-fixtures.mjs [<node_modules with apache-arrow and lz4js>]
// The default is RAYFIN_SDK_NODE_MODULES, else the reference app's node_modules (the Rayfin SDK
// 1.36.2 install) in fabric-apps next to this repository.
import { mkdirSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const nodeModules =
  process.argv[2] ??
  process.env.RAYFIN_SDK_NODE_MODULES ??
  resolve(here, '../../../../../fabric-apps/apps/explorer/node_modules');
const arrow = await import(pathToFileURL(join(nodeModules, 'apache-arrow/Arrow.node.mjs')).href);
const lz4 = createRequire(join(nodeModules, 'noop.js'))('lz4js');

const {
  Bool,
  CompressionType,
  DateMillisecond,
  Dictionary,
  Field,
  Float64,
  Int64,
  Int8,
  RecordBatchStreamWriter,
  Schema,
  Table,
  Utf8,
  compressionRegistry,
  vectorFromArray,
} = arrow;

compressionRegistry.set(CompressionType.LZ4_FRAME, {
  encode: (data) => lz4.compress(data),
  decode: (data) => lz4.decompress(data),
});

const dictionary = (valueType) => new Dictionary(valueType, new Int8());

function table(columns, metadata) {
  const vectors = Object.fromEntries(columns.map(([name, type, values]) => [name, vectorFromArray(values, type)]));
  const fields = columns.map(([name, type]) => new Field(name, type, true));
  return new Table(new Schema(fields, metadata), vectors);
}

function write(name, t, compressed = true) {
  const writer = RecordBatchStreamWriter.writeAll(
    t,
    compressed ? { compressionType: CompressionType.LZ4_FRAME } : {},
  );
  const bytes = writer.toUint8Array(true);
  const out = join(here, '..', 'fixtures', `${name}.arrows`);
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(out, bytes);
  console.log(`${name}.arrows: ${bytes.length} bytes`);
}

// EVALUATE ROW("x", 1)
write('row', table([['[x]', dictionary(new Int64()), [1n]]]));

// A typed result with a null in every column.
write(
  'typed',
  table([
    ['Sales[Region]', dictionary(new Utf8()), ['West', 'East', null]],
    ['Sales[OrderDate]', dictionary(new DateMillisecond()), [Date.UTC(2024, 7, 2), Date.UTC(2023, 8, 27, 1, 12, 12), null]],
    ['[Total]', dictionary(new Float64()), [1713.98, 0.5, null]],
    ['[Orders]', dictionary(new Int64()), [12n, 3n, null]],
    ['[HasProduct]', new Bool(), [true, false, null]],
  ]),
);

// An Int64 the SDK cannot represent as a number: it reports `overflow`.
write('int64-overflow', table([['[big]', dictionary(new Int64()), [2n ** 60n]]]));

// A compile error: the 11-column error table, with Analysis Services' markup in the message.
write(
  'dax-error',
  table(
    [
      ['ErrorCode', new Utf8(), ['3241803779']],
      ['ErrorMessage', new Utf8(), ["Query (1, 10) Cannot find table '<oii>Nope</oii>'."]],
      ['ErrorDescription', new Utf8(), ["Cannot find table '%{oii_table/}'."]],
      ['ErrorType', new Int64(), [1n]],
      ['IsPrimaryErrorCode', new Bool(), [true]],
      ['StartLine', new Int64(), [1n]],
      ['StartColumn', new Int64(), [10n]],
      ['EndLine', new Int64(), [1n]],
      ['EndColumn', new Int64(), [14n]],
      ['LineOffset', new Int64(), [0n]],
      ['TextLength', new Int64(), [4n]],
    ],
    new Map([
      ['IsError', 'true'],
      ['IsAssociatedWithCurrentResultSet', 'false'],
    ]),
  ),
);
