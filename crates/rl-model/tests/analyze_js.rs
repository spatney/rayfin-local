//! Runs the embedded `js/analyze.mjs` with Node.js on the `basic` fixture project and compares
//! its result with the golden analysis. Skipped (with a note) without Node.js or without the
//! fixture's node_modules (`npm install` in tests/fixtures/apps/basic).

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use rl_model::{ANALYZE_JS, AnalyzeInput, AppModel, Dialect};
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
fn run(node: &OsString, project: &Path, input: &AnalyzeInput, scratch: &Path) -> Value {
    let _ = fs::remove_dir_all(scratch);
    fs::create_dir_all(scratch).unwrap();
    let script = scratch.join("analyze.mjs");
    fs::write(&script, ANALYZE_JS).unwrap();
    let input_file = scratch.join("input.json");
    let output_file = scratch.join("output.json");
    fs::write(&input_file, input.to_json().to_string()).unwrap();
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
    let output = run(&node, &project, &input, &scratch);
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
    let output = run(&node, &project, &input, &scratch);
    assert_eq!(output["ok"], false, "{output}");
    let message = output["error"]["message"].as_str().unwrap();
    assert!(message.contains(".rayfin-local"), "{message}");
    assert!(!input.out_dir.exists());
}
