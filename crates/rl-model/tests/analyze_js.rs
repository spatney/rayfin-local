//! Runs the embedded `js/analyze.mjs` with Node.js on the `basic` fixture project and compares
//! its result with the golden analysis. Skipped (with a note) without Node.js or without the
//! fixture's node_modules (`npm install` in tests/fixtures/apps/basic).

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use rl_model::connector::ConnectorsAnalysis;
use rl_model::{
    ANALYZE_JS, AnalyzeInput, AppModel, ConnectorSource, ConnectorsAnalyzeInput, Dialect,
};
use serde_json::Value;

fn fixture_project() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.parent().and_then(Path::parent).unwrap();
    repo.join("tests")
        .join("fixtures")
        .join("apps")
        .join("basic")
}

fn node() -> Option<OsString> {
    let node = std::env::var_os("RAYFIN_LOCAL_NODE").unwrap_or_else(|| "node".into());
    let found = Command::new(&node)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    found.then_some(node)
}

/// Runs the helper in `project` and returns its output document.
fn run(node: &OsString, project: &Path, input: &Value, scratch: &Path) -> Value {
    let _ = fs::remove_dir_all(scratch);
    fs::create_dir_all(scratch).unwrap();
    let script = scratch.join("analyze.mjs");
    fs::write(&script, ANALYZE_JS).unwrap();
    let input_file = scratch.join("input.json");
    let output_file = scratch.join("output.json");
    fs::write(&input_file, input.to_string()).unwrap();
    let status = Command::new(node)
        .arg(&script)
        .arg("--input")
        .arg(&input_file)
        .arg("--output")
        .arg(&output_file)
        .current_dir(project)
        .env_remove("RAYFIN_FEATURE_FLAGS")
        .status()
        .unwrap();
    assert!(status.success(), "analyze.mjs exited with {status}");
    serde_json::from_str(&fs::read_to_string(&output_file).unwrap()).unwrap()
}

fn prerequisites() -> Option<(OsString, PathBuf)> {
    let project = fixture_project();
    if !project
        .join("node_modules")
        .join("@microsoft")
        .join("rayfin-core")
        .is_dir()
    {
        eprintln!("skipped: run `npm install` in {}", project.display());
        return None;
    }
    let Some(node) = node() else {
        eprintln!("skipped: Node.js was not found (set RAYFIN_LOCAL_NODE)");
        return None;
    };
    Some((node, project))
}

#[test]
fn analyze_js_reproduces_the_golden_analysis() {
    let Some((node, project)) = prerequisites() else {
        return;
    };
    // Outside the project, so the helper must resolve the project's packages itself.
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rl-model-analyze-golden");
    let mut input = AnalyzeInput::for_project(&project, Dialect::Mssql, true);
    input.out_dir = scratch.join("compiled");
    let output = run(&node, &project, &input.to_json(), &scratch);
    assert_eq!(output["ok"], true, "{output}");
    let golden: Value = serde_json::from_str(include_str!("fixtures/basic.analysis.json")).unwrap();
    assert!(
        output["result"] == golden,
        "analyze.mjs output differs from tests/fixtures/basic.analysis.json; if the change is \
         intended, regenerate it. Output:\n{}",
        serde_json::to_string_pretty(&output["result"]).unwrap()
    );
    AppModel::from_analysis(&output["result"]).unwrap();
    assert!(
        !project.join("rayfin").join(".temp").exists(),
        "wrote into the project"
    );
}

#[test]
fn analyze_js_writes_inside_a_project_only_under_rayfin_local() {
    let Some((node, project)) = prerequisites() else {
        return;
    };
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rl-model-analyze-outdir");
    let mut input = AnalyzeInput::for_project(&project, Dialect::Mssql, false);
    input.out_dir = project.join("rayfin").join("compiled");
    let output = run(&node, &project, &input.to_json(), &scratch);
    assert_eq!(output["ok"], false, "{output}");
    let message = output["error"]["message"].as_str().unwrap();
    assert!(message.contains(".rayfin-local"), "{message}");
    assert!(!input.out_dir.exists());
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// SDK 1.37 moved `@microsoft/rayfin-core/schema` and `/analysis` to `/_internal/schema` and
/// `/_internal/analysis`, with the same exports. The fixture's model analyzes the same with its
/// rayfin-core exported that way.
#[test]
fn analyze_js_loads_the_analyzer_where_sdk_1_37_moved_it() {
    let Some((node, fixture)) = prerequisites() else {
        return;
    };
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rl-model-analyze-sdk-1-37");
    let project = scratch.join("project");
    let _ = fs::remove_dir_all(&project);
    for folder in ["rayfin/data", "rayfin/storage"] {
        copy_dir(&fixture.join(folder), &project.join(folder));
    }
    for file in [
        "package.json",
        "tsconfig.json",
        "rayfin/rayfin.yml",
        "rayfin/tsconfig.json",
    ] {
        fs::copy(fixture.join(file), project.join(file)).unwrap();
    }
    let modules = fixture.join("node_modules");
    let packages = [
        "@microsoft/rayfin-core",
        "@microsoft/rayfin-lib",
        "@standard-schema/spec",
        "pluralize",
    ];
    for package in packages {
        copy_dir(
            &modules.join(package),
            &project.join("node_modules").join(package),
        );
    }
    let manifest_path = project.join("node_modules/@microsoft/rayfin-core/package.json");
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let exports = manifest["exports"].as_object_mut().unwrap();
    for name in ["schema", "analysis"] {
        let target = exports.remove(&format!("./{name}")).unwrap();
        exports.insert(format!("./_internal/{name}"), target);
    }
    fs::write(&manifest_path, manifest.to_string()).unwrap();
    // The helper imports TypeScript from the project: the fixture's.
    let typescript = project.join("node_modules").join("typescript");
    fs::create_dir_all(&typescript).unwrap();
    fs::write(
        typescript.join("package.json"),
        r#"{"name":"typescript","main":"index.cjs"}"#,
    )
    .unwrap();
    let real = serde_json::to_string(&modules.join("typescript")).unwrap();
    fs::write(
        typescript.join("index.cjs"),
        format!("module.exports = require({real});\n"),
    )
    .unwrap();

    let mut input = AnalyzeInput::for_project(&project, Dialect::Mssql, true);
    input.out_dir = scratch.join("compiled");
    let output = run(&node, &project, &input.to_json(), &scratch.join("run"));
    assert_eq!(output["ok"], true, "{output}");
    let golden: Value = serde_json::from_str(include_str!("fixtures/basic.analysis.json")).unwrap();
    for key in ["coreVersion", "entities", "config", "policies", "storage"] {
        assert!(
            output["result"][key] == golden[key],
            "{key} differs from tests/fixtures/basic.analysis.json: {}",
            output["result"][key]
        );
    }
}

#[test]
fn analyze_js_supports_v2_data_storage_and_connector_metadata() {
    let project = fixture_project().parent().unwrap().join("sdk-v2");
    if !project.join("node_modules/@microsoft/rayfin-core").is_dir() {
        eprintln!("skipped: run `npm ci` in {}", project.display());
        return;
    }
    let Some(node) = node() else {
        eprintln!("skipped: Node.js was not found (set RAYFIN_LOCAL_NODE)");
        return;
    };
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rl-model-analyze-sdk-v2");
    let mut input = AnalyzeInput::for_project(&project, Dialect::Mssql, true);
    input.out_dir = scratch.join("compiled");
    let output = run(&node, &project, &input.to_json(), &scratch.join("data"));
    assert_eq!(output["ok"], true, "{output}");
    let model = AppModel::from_analysis(&output["result"]).unwrap();
    let todo = model
        .entities
        .iter()
        .find(|entity| entity.name == "Todo")
        .unwrap();
    assert_eq!(
        todo.columns
            .iter()
            .find(|column| column.name == "done")
            .unwrap()
            .default,
        Some(false.into())
    );
    let comment = model
        .entities
        .iter()
        .find(|entity| entity.name == "Comment")
        .unwrap();
    assert_eq!(comment.foreign_keys.len(), 1);
    assert!(model.storage.is_some());

    let mut input = ConnectorsAnalyzeInput::for_project(
        &project,
        vec![ConnectorSource {
            name: "inventory".into(),
            dir: project.join("rayfin/connectors/inventory"),
            dialect: "mssql".into(),
        }],
    );
    input.out_dir = scratch.join("compiled-connectors");
    let output = run(
        &node,
        &project,
        &input.to_json(),
        &scratch.join("connectors"),
    );
    assert_eq!(output["ok"], true, "{output}");
    let analysis = ConnectorsAnalysis::from_result(&output["result"]).unwrap();
    let model = analysis.connectors[0].model.as_ref().unwrap();
    let product = &model.entities[0];
    assert_eq!(product.table, "inventory.products");
    assert_eq!(product.primary_key.columns, ["product-id"]);
    assert!(
        product
            .columns
            .iter()
            .any(|column| column.name == "product-title" && column.field == "title")
    );
    let permission = &product.permissions[0].actions[0];
    let fields = permission.fields.as_ref().unwrap();
    for field in ["productId", "title", "quantity"] {
        assert!(fields.allows(field), "{field}: {fields:?}");
    }
    assert!(!fields.allows("ownerId"));
    assert!(!fields.allows("internalTitle"));
    assert_eq!(
        permission.policy.as_ref().unwrap().expr.fields(),
        ["ownerId"]
    );
}

/// The `connectors` fixture app (the `inventory` SQL connector), with its packages installed.
fn connectors_prerequisites() -> Option<(OsString, PathBuf)> {
    let project = fixture_project().parent().unwrap().join("connectors");
    if !project
        .join("node_modules")
        .join("@microsoft")
        .join("rayfin-connectors")
        .is_dir()
    {
        eprintln!("skipped: run `npm install` in {}", project.display());
        return None;
    }
    let Some(node) = node() else {
        eprintln!("skipped: Node.js was not found (set RAYFIN_LOCAL_NODE)");
        return None;
    };
    Some((node, project))
}

#[test]
fn analyze_js_reproduces_the_golden_connector_analysis() {
    let Some((node, project)) = connectors_prerequisites() else {
        return;
    };
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rl-model-analyze-connectors");
    let source = |name: &str| ConnectorSource {
        name: name.to_owned(),
        dir: project.join("rayfin").join("connectors").join(name),
        dialect: "mssql".to_owned(),
    };
    let mut input =
        ConnectorsAnalyzeInput::for_project(&project, vec![source("inventory"), source("missing")]);
    input.out_dir = scratch.join("compiled");
    let output = run(&node, &project, &input.to_json(), &scratch);
    assert_eq!(output["ok"], true, "{output}");
    let connectors = &output["result"]["connectors"];
    let golden: Value =
        serde_json::from_str(include_str!("fixtures/inventory.connector.json")).unwrap();
    assert!(
        connectors[0] == golden,
        "analyze.mjs output differs from tests/fixtures/inventory.connector.json; if the change \
         is intended, regenerate it. Output:\n{}",
        serde_json::to_string_pretty(&connectors[0]).unwrap()
    );
    let message = connectors[1]["error"]["message"].as_str().unwrap();
    assert!(message.contains("rayfin/connectors/missing"), "{message}");
    let analysis = ConnectorsAnalysis::from_result(&output["result"]).unwrap();
    assert!(analysis.connectors[0].model.is_ok());
    assert!(analysis.connectors[1].model.is_err());
    assert!(
        analysis
            .inputs
            .iter()
            .any(|input| input.ends_with("inventory/Product.ts")),
        "{:?}",
        analysis.inputs
    );
}
