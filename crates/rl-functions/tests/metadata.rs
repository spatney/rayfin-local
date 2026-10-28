//! `runtimemetadata.json` generation with `js/udf-metadata.mjs` on Node and TypeScript.
//!
//! The expected files under `tests/fixtures` were produced by the Rayfin CLI 1.36.2 generator on
//! the same sources. Tests skip (with a note) when Node or TypeScript is unavailable: TypeScript
//! comes from `RAYFIN_LOCAL_TYPESCRIPT_DIR` (a folder `typescript` resolves from) or from the
//! explorer app (`RAYFIN_LOCAL_EXPLORER`, else `fabric-apps/apps/explorer` beside this repository).

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use rl_functions::metadata::{self, GenerateInput, MetadataError, RuntimeMetadata};
use rl_node::{LineSink, NodeRuntime};
use rl_tokens::Audience;
use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn explorer() -> Option<PathBuf> {
    let dir = std::env::var_os("RAYFIN_LOCAL_EXPLORER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fabric-apps/apps/explorer")
        });
    dir.join("rayfin/functions/runtimemetadata.json")
        .is_file()
        .then_some(dir)
}

fn typescript_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("RAYFIN_LOCAL_TYPESCRIPT_DIR") {
        return Some(dir.into());
    }
    let functions = explorer()?.join("rayfin").join("functions");
    functions
        .join("node_modules/typescript/package.json")
        .is_file()
        .then_some(functions)
}

async fn node() -> Option<NodeRuntime> {
    match NodeRuntime::discover().await {
        Ok(node) => Some(node),
        Err(error) => {
            eprintln!("skipped: {error}");
            None
        }
    }
}

/// Node and a TypeScript to generate with, or `None` (test skipped).
async fn toolchain() -> Option<(NodeRuntime, PathBuf)> {
    let Some(typescript) = typescript_dir() else {
        eprintln!("skipped: no TypeScript (set RAYFIN_LOCAL_TYPESCRIPT_DIR)");
        return None;
    };
    Some((node().await?, typescript))
}

fn sink() -> LineSink {
    Arc::new(|label, _stream, line| eprintln!("[{label}] {line}"))
}

/// Copies a fixture folder with LF line endings, as a checkout has them.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            let text = fs::read_to_string(entry.path()).unwrap();
            fs::write(target, text.replace("\r\n", "\n")).unwrap();
        }
    }
}

/// The fixture functions package in `dir`, with the UDF package's declarations installed.
fn fixture_app(dir: &Path) -> PathBuf {
    let app = dir.join("functions");
    copy_dir(&fixtures().join("udf-app"), &app);
    copy_dir(
        &fixtures().join("udf-stub"),
        &app.join("node_modules/@microsoft/fabric-user-data-functions"),
    );
    app
}

fn expected(name: &str) -> String {
    fs::read_to_string(fixtures().join(name))
        .unwrap()
        .replace("\r\n", "\n")
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn input(app: &Path, typescript: &Path) -> GenerateInput {
    GenerateInput {
        functions_dir: app.to_owned(),
        resolve_paths: vec![typescript.to_owned()],
        output_path: None,
    }
}

#[tokio::test]
async fn explorer_metadata_matches_the_cli() {
    let Some(explorer) = explorer() else {
        eprintln!("skipped: the explorer app is not checked out (set RAYFIN_LOCAL_EXPLORER)");
        return;
    };
    let Some(node) = node().await else {
        return;
    };
    let functions = explorer.join("rayfin").join("functions");
    // Generated in place, written only to a temporary file: the app is never modified.
    let scratch = tempfile::tempdir().unwrap();
    let output = scratch.path().join("runtimemetadata.json");
    let mut request = GenerateInput::new(&functions);
    request.output_path = Some(output.clone());
    let generated = metadata::generate(&node, &request, &scratch.path().join("cache"), sink())
        .await
        .unwrap();

    let golden = fs::read_to_string(functions.join("runtimemetadata.json")).unwrap();
    assert_eq!(
        json(&generated.json),
        json(&golden),
        "differs from explorer's CLI-generated runtimemetadata.json (or that file is stale: \
         run `rayfin dev` in explorer)"
    );
    assert_eq!(generated.json, golden.replace("\r\n", "\n").trim_end());
    assert_eq!(generated.written, Some(true));
    assert_eq!(fs::read_to_string(&output).unwrap(), generated.json);
    assert!(generated.type_checked);
    assert!(generated.diagnostics.is_empty());
    assert_eq!(
        generated.metadata.audiences("askAnalyst"),
        [Audience::Ado, Audience::AzureAi]
    );
    assert_eq!(
        generated.metadata.audiences("startModelRefresh"),
        [Audience::Fabric]
    );
}

#[tokio::test]
async fn fixture_metadata_matches_the_cli() {
    let Some((node, typescript)) = toolchain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let app = fixture_app(dir.path());
    let generated = metadata::generate(
        &node,
        &input(&app, &typescript),
        &dir.path().join("cache"),
        sink(),
    )
    .await
    .unwrap();

    assert_eq!(generated.json, expected("udf-app.expected.json"));
    assert!(generated.type_checked);
    assert!(generated.diagnostics.is_empty());
    assert_eq!(
        generated.source_files,
        ["src/function_app.ts", "src/more/extra.ts", "src/udf.ts"]
    );
    assert_eq!(generated.written, None);
    assert!(!app.join("runtimemetadata.json").exists());

    let metadata = generated.metadata;
    assert_eq!(metadata.validate(), Ok(()));
    assert_eq!(
        metadata.function_names().collect::<Vec<_>>(),
        [
            "ping",
            "whoAmI",
            "addNote",
            "askAgent",
            "search",
            "filterItems",
            "backlog",
            "legacyContext",
            "lakeFile",
            "namespaced",
            "nothingDeclared",
            "destructured",
            "legacyStyle",
            "syncHandler",
            "arrowNoAsync",
        ]
    );
    assert_eq!(
        metadata.audiences("askAgent"),
        [Audience::Ado, Audience::AzureAi]
    );
    assert_eq!(metadata.audiences("backlog"), [Audience::Ado]);
    assert_eq!(
        metadata.audiences("legacyContext"),
        [Audience::Fabric, Audience::Storage]
    );
    assert_eq!(metadata.audiences("ping"), []);
    let ask = metadata.function("askAgent").unwrap();
    assert!(ask.delegate_parameters[1].is_context());
    assert!(!ask.delegate_parameters[0].is_context());
}

#[tokio::test]
async fn without_tsconfig_audiences_come_from_the_annotation_text() {
    let Some((node, typescript)) = toolchain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let app = fixture_app(dir.path());
    fs::remove_file(app.join("tsconfig.json")).unwrap();
    let generated = metadata::generate(
        &node,
        &input(&app, &typescript),
        &dir.path().join("cache"),
        sink(),
    )
    .await
    .unwrap();

    assert_eq!(
        generated.json,
        expected("udf-app.no-tsconfig.expected.json")
    );
    assert!(!generated.type_checked);
    let warned: Vec<_> = generated
        .diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.function_name.as_str(), diagnostic.code.as_str()))
        .collect();
    assert_eq!(
        warned,
        [
            "addNote",
            "askAgent",
            "backlog",
            "legacyContext",
            "namespaced",
            "legacyStyle"
        ]
        .map(|name| (name, "unverified-context-audience"))
    );
    // An alias cannot be resolved without the checker; the runtime binds nothing for it.
    assert_eq!(generated.metadata.audiences("backlog"), []);
}

#[tokio::test]
async fn ensure_fresh_regenerates_only_when_stale() {
    let Some((node, typescript)) = toolchain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let app = fixture_app(dir.path());
    let cache = dir.path().join("cache");
    let request = input(&app, &typescript);
    let path = metadata::metadata_path(&app);
    let tick = || std::thread::sleep(Duration::from_millis(50));

    let generated = metadata::ensure_fresh_with(&node, &request, &cache, sink())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        expected("udf-app.expected.json")
    );
    assert!(!metadata::is_stale(&app, &path));

    // A current file is used as it is.
    let mut edited: RuntimeMetadata = generated.clone();
    edited.functions.truncate(1);
    tick();
    fs::write(&path, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
    let current = metadata::ensure_fresh_with(&node, &request, &cache, sink())
        .await
        .unwrap();
    assert_eq!(current, edited);

    // A newer source makes it regenerate.
    tick();
    let source = app.join("src").join("udf.ts");
    let touch = || fs::write(&source, fs::read_to_string(&source).unwrap()).unwrap();
    touch();
    assert!(metadata::is_stale(&app, &path));
    let regenerated = metadata::ensure_fresh_with(&node, &request, &cache, sink())
        .await
        .unwrap();
    assert_eq!(regenerated, generated);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        expected("udf-app.expected.json")
    );

    // A file the runtime would reject is regenerated even when it is newer.
    tick();
    fs::write(&path, r#"{"schemaVersion": "1.0", "functions": []}"#).unwrap();
    let repaired = metadata::ensure_fresh_with(&node, &request, &cache, sink())
        .await
        .unwrap();
    assert_eq!(repaired, generated);

    // Same content: the file is left alone, and the stamp in the cache vouches for it until an
    // input changes.
    tick();
    touch();
    let modified = || fs::metadata(&path).unwrap().modified().unwrap();
    let before = modified();
    metadata::ensure_fresh_with(&node, &request, &cache, sink())
        .await
        .unwrap();
    assert_eq!(modified(), before);
    assert!(metadata::is_stale(&app, &path));
    // Without the UDF declarations generating again would change `backlog` (its audience alias
    // no longer resolves), so equal metadata shows that nothing was generated.
    fs::remove_dir_all(app.join("node_modules")).unwrap();
    let vouched = metadata::ensure_fresh_with(&node, &request, &cache, sink())
        .await
        .unwrap();
    assert_eq!(vouched, generated);
    tick();
    touch();
    let unresolved = metadata::ensure_fresh_with(&node, &request, &cache, sink())
        .await
        .unwrap();
    assert_eq!(unresolved.audiences("backlog"), []);
    assert_ne!(
        fs::read_to_string(&path).unwrap(),
        expected("udf-app.expected.json")
    );
}

#[tokio::test]
async fn generation_errors_say_what_to_fix() {
    let Some((node, typescript)) = toolchain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let app = fixture_app(dir.path());
    let cache = dir.path().join("cache");

    fs::remove_dir_all(app.join("src").join("more")).unwrap();
    fs::write(app.join("src").join("function_app.ts"), "export {};\n").unwrap();
    let error = metadata::ensure_fresh_with(&node, &input(&app, &typescript), &cache, sink())
        .await
        .unwrap_err();
    assert!(matches!(error, MetadataError::Generate { .. }), "{error}");
    assert!(
        error.to_string().contains("No udf.func() registrations"),
        "{error}"
    );
    assert!(!metadata::metadata_path(&app).exists());

    fs::remove_dir_all(app.join("src")).unwrap();
    let error = metadata::ensure_fresh_with(&node, &input(&app, &typescript), &cache, sink())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("source directory not found"),
        "{error}"
    );
}
