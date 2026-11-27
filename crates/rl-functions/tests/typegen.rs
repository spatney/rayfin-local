//! The functions' generated types with `js/functions-types.mjs` on Node: the Rayfin CLI that the
//! functions package resolves generates them, and `runtimemetadata.json` keeps what rayfin-local
//! wrote. A stand-in CLI plays the CLI. Skipped (with a note) without Node.js.

use std::{fs, path::Path, sync::Arc};

use rl_functions::typegen::{self, TypegenError};
use rl_node::{LineSink, NodeRuntime};

async fn node() -> Option<NodeRuntime> {
    match NodeRuntime::discover().await {
        Ok(node) => Some(node),
        Err(error) => {
            eprintln!("skipped: {error}");
            None
        }
    }
}

fn sink() -> LineSink {
    Arc::new(|label, _stream, line| eprintln!("[{label}] {line}"))
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

/// A project with a functions package in `rayfin/functions` and rayfin-local's metadata.
fn project() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let functions = dir.path().join("rayfin").join("functions");
    write(&functions.join("package.json"), r#"{"name": "fns"}"#);
    write(&functions.join("runtimemetadata.json"), "rayfin-local's");
    write(&functions.join("src/secrets.generated.ts"), "export {};\n");
    (dir, functions)
}

/// A Rayfin CLI 9.9.9 at the project root whose type generator is `generator`.
fn stand_in_cli(root: &Path, generator: &str) {
    let cli = root.join("node_modules/@microsoft/rayfin-cli");
    write(
        &cli.join("package.json"),
        r#"{"name": "@microsoft/rayfin-cli", "version": "9.9.9", "type": "module"}"#,
    );
    write(
        &cli.join("dist/utils/functions-types-generator.js"),
        generator,
    );
}

#[tokio::test]
async fn the_cli_generates_the_types_and_the_metadata_stays() {
    let Some(node) = node().await else {
        return;
    };
    let (dir, functions) = project();
    stand_in_cli(
        dir.path(),
        r#"
import fs from 'node:fs';
import path from 'node:path';
export async function generateFunctionsTypes(dir) {
  fs.writeFileSync(path.join(dir, 'runtimemetadata.json'), "the CLI's");
  fs.writeFileSync(path.join(dir, 'src', 'types.ts'), `// for ${JSON.stringify(dir)}\n`);
  fs.writeFileSync(path.join(dir, 'src', 'secrets.generated.ts'), 'export {};\n');
}
"#,
    );

    let cache = dir.path().join("cache");
    let generated = typegen::generate(&node, &functions, &cache, sink())
        .await
        .unwrap();
    assert_eq!(generated.cli_version, "9.9.9");
    assert_eq!(generated.written, ["src/types.ts"], "secrets are unchanged");
    let types = read(&functions.join("src/types.ts"));
    let dir_text = serde_json::to_string(&functions).unwrap();
    assert!(types.contains(&dir_text), "given another folder: {types}");
    assert_eq!(
        read(&functions.join("runtimemetadata.json")),
        "rayfin-local's"
    );

    let again = typegen::generate(&node, &functions, &cache, sink())
        .await
        .unwrap();
    assert!(again.written.is_empty(), "{again:?}");
}

#[tokio::test]
async fn a_failed_generation_says_why_and_the_metadata_stays() {
    let Some(node) = node().await else {
        return;
    };
    let (dir, functions) = project();
    let cache = dir.path().join("cache");
    let error = typegen::generate(&node, &functions, &cache, sink())
        .await
        .unwrap_err();
    assert!(matches!(error, TypegenError::Generate { .. }), "{error:?}");
    let message = error.to_string();
    assert!(
        message.contains("@microsoft/rayfin-cli is not installed. Run `npm install`"),
        "{message}"
    );
    assert!(message.contains("src/types.ts"), "{message}");

    stand_in_cli(
        dir.path(),
        r#"
import fs from 'node:fs';
import path from 'node:path';
export async function generateFunctionsTypes(dir) {
  fs.writeFileSync(path.join(dir, 'runtimemetadata.json'), "the CLI's");
  throw new Error('the Functions worker is too old: run `npm install` in the functions');
}
"#,
    );
    let error = typegen::generate(&node, &functions, &cache, sink())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("the Functions worker is too old"),
        "{error}"
    );
    assert_eq!(
        read(&functions.join("runtimemetadata.json")),
        "rayfin-local's"
    );
    assert!(!functions.join("src/types.ts").exists());

    stand_in_cli(dir.path(), "export const somethingElse = 1;\n");
    let error = typegen::generate(&node, &functions, &cache, sink())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("@microsoft/rayfin-cli 9.9.9 has no type generator for functions"),
        "{error}"
    );
}

#[tokio::test]
async fn before_a_build_only_the_secrets_are_generated() {
    let Some(node) = node().await else {
        return;
    };
    let (dir, functions) = project();
    let cache = dir.path().join("cache");
    stand_in_cli(
        dir.path(),
        r#"
import fs from 'node:fs';
import path from 'node:path';
export async function generateFunctionsTypes(dir) {
  fs.writeFileSync(path.join(dir, 'src', 'types.ts'), 'export type AppFunctionsSchema = {};\n');
}
"#,
    );
    write(
        &dir.path()
            .join("node_modules/@microsoft/rayfin-cli/dist/utils/secrets-types-generator.js"),
        r#"
import fs from 'node:fs';
import path from 'node:path';
export function ensureSecretsTypes(dir, onWarn) {
  if (fs.existsSync(path.join(dir, 'broken'))) return onWarn('rayfin.yml declares no secrets list');
  fs.writeFileSync(path.join(dir, 'src', 'secrets.generated.ts'), '// API_KEY\nexport {};\n');
}
"#,
    );

    let generated = typegen::generate_secrets(&node, &functions, &cache, sink())
        .await
        .unwrap();
    assert_eq!(generated.written, ["src/secrets.generated.ts"]);
    assert!(read(&functions.join("src/secrets.generated.ts")).contains("API_KEY"));
    assert!(!functions.join("src/types.ts").exists(), "only the secrets");

    write(&functions.join("broken"), "");
    let error = typegen::generate_secrets(&node, &functions, &cache, sink())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("rayfin.yml declares no secrets list"),
        "{error}"
    );
}
