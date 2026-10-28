//! Building and watching a functions package with real processes (Node, npm and the shell).

use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use rl_functions::builder::{self, BuildConfig, BuildError, FunctionsWatch, WatchEvent, WatchMode};
use rl_node::{LineSink, NodeRuntime, Stream};
use tokio::time::timeout;

type Lines = Arc<Mutex<Vec<(String, String)>>>;

/// Whether Node runs here; the tests skip (with a note) when it does not.
async fn has_node() -> bool {
    match NodeRuntime::discover().await {
        Ok(_) => true,
        Err(error) => {
            eprintln!("skipped: {error}");
            false
        }
    }
}

fn recording_sink() -> (LineSink, Lines) {
    let lines = Lines::default();
    let recorded = lines.clone();
    let sink: LineSink = Arc::new(move |label: &str, _stream: Stream, line: &str| {
        eprintln!("[{label}] {line}");
        recorded
            .lock()
            .unwrap()
            .push((label.to_owned(), line.to_owned()));
    });
    (sink, lines)
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// A functions package whose `main` is `dist/index.js`, with these scripts.
fn package(dir: &Path, scripts: serde_json::Value) {
    let package = serde_json::json!({
        "name": "fns",
        "private": true,
        "type": "module",
        "main": "dist/index.js",
        "scripts": scripts,
    });
    write(&dir.join("package.json"), &package.to_string());
    write(&dir.join("src").join("function_app.ts"), "export {};\n");
}

/// A build script that compiles `dist/index.js` (counting its builds) and reports it.
const BUILD_SCRIPT: &str = "import fs from 'node:fs';\n\
    fs.mkdirSync('dist', { recursive: true });\n\
    const count = fs.existsSync('dist/count') ? Number(fs.readFileSync('dist/count', 'utf8')) + 1 : 1;\n\
    fs.writeFileSync('dist/count', String(count));\n\
    fs.writeFileSync('dist/index.js', `export const build = ${count};\\n`);\n\
    console.log(`built ${count}`);\n";

fn config(dir: &Path, command: &str, sink: LineSink) -> BuildConfig {
    BuildConfig {
        functions_dir: dir.to_owned(),
        build_command: Some(command.into()),
        sink,
    }
}

/// Waits for `wanted`, skipping other events.
async fn expect_event(watch: &mut FunctionsWatch, wanted: WatchEvent) {
    let found = timeout(Duration::from_secs(20), async {
        while let Some(event) = watch.next().await {
            if event == wanted {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(found, Ok(true), "no {wanted:?}");
}

/// Lets the file system watcher settle before the test changes files.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn a_build_runs_the_command_in_the_functions_folder() {
    if !has_node().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    package(dir.path(), serde_json::json!({}));
    write(&dir.path().join("build.mjs"), BUILD_SCRIPT);
    let (sink, lines) = recording_sink();

    builder::build(&config(dir.path(), "node build.mjs", sink))
        .await
        .unwrap();
    assert!(
        lines
            .lock()
            .unwrap()
            .contains(&("build".to_owned(), "built 1".to_owned()))
    );
    assert_eq!(
        builder::entry_point(dir.path()).unwrap(),
        dir.path().join("dist").join("index.js")
    );
}

#[tokio::test]
async fn a_failed_build_says_what_to_do() {
    if !has_node().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    package(dir.path(), serde_json::json!({}));
    write(
        &dir.path().join("fail.mjs"),
        "console.error(\"src/function_app.ts(1,1): error TS2304: Cannot find name 'x'.\");\n\
         process.exit(3);\n",
    );
    let (sink, lines) = recording_sink();

    let error = builder::build(&config(dir.path(), "node fail.mjs", sink))
        .await
        .unwrap_err();
    assert!(matches!(error, BuildError::Failed { .. }), "{error}");
    let message = error.to_string();
    assert!(message.starts_with("functions build failed"), "{message}");
    assert!(
        message.contains(&format!("run `node fail.mjs` in {}", dir.path().display())),
        "{message}"
    );
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|(label, line)| label == "build" && line.contains("TS2304"))
    );
}

#[tokio::test]
async fn output_changes_are_reported() {
    if !has_node().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    package(dir.path(), serde_json::json!({}));
    write(&dir.path().join("dist").join("index.js"), "export {};\n");
    let (sink, _) = recording_sink();

    let mut watch = builder::watch(&config(dir.path(), "node build.mjs", sink)).unwrap();
    assert_eq!(watch.mode(), WatchMode::Rebuild);
    assert_eq!(watch.output_dirs(), [dir.path().join("dist")]);
    settle().await;

    write(
        &dir.path().join("dist").join("index.js"),
        "export const changed = true;\n",
    );
    expect_event(&mut watch, WatchEvent::OutputChanged).await;

    // Source maps, declarations and build info do not change what runs.
    settle().await;
    while watch.try_next().is_some() {}
    write(&dir.path().join("dist").join("index.js.map"), "{}");
    write(&dir.path().join("dist").join("index.d.ts"), "export {};\n");
    write(&dir.path().join("dist").join("tsconfig.tsbuildinfo"), "{}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(watch.try_next(), None);
}

#[tokio::test]
async fn without_build_watch_source_changes_rebuild() {
    if !has_node().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    package(dir.path(), serde_json::json!({ "build": "node build.mjs" }));
    write(&dir.path().join("build.mjs"), BUILD_SCRIPT);
    let (sink, lines) = recording_sink();
    let config = config(dir.path(), "node build.mjs", sink);
    builder::build(&config).await.unwrap();

    let mut watch = builder::watch(&config).unwrap();
    assert_eq!(watch.mode(), WatchMode::Rebuild);
    settle().await;

    write(
        &dir.path().join("src").join("function_app.ts"),
        "export const changed = true;\n",
    );
    expect_event(&mut watch, WatchEvent::SourceChanged).await;
    expect_event(&mut watch, WatchEvent::OutputChanged).await;
    assert_eq!(
        fs::read_to_string(dir.path().join("dist").join("index.js")).unwrap(),
        "export const build = 2;\n"
    );
    assert!(
        lines
            .lock()
            .unwrap()
            .contains(&("build".to_owned(), "built 2".to_owned()))
    );
}

#[tokio::test]
async fn build_watch_runs_until_stopped() {
    if !has_node().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    package(
        dir.path(),
        serde_json::json!({ "build:watch": "node watch.mjs" }),
    );
    write(
        &dir.path().join("watch.mjs"),
        "import fs from 'node:fs';\n\
         fs.mkdirSync('dist', { recursive: true });\n\
         let beats = 0;\n\
         setInterval(() => fs.writeFileSync('dist/heartbeat', String(beats++)), 100);\n\
         console.log('watching');\n",
    );
    let (sink, lines) = recording_sink();

    let mut watch = builder::watch(&config(dir.path(), "node build.mjs", sink)).unwrap();
    assert_eq!(watch.mode(), WatchMode::Script);
    expect_event(&mut watch, WatchEvent::OutputChanged).await;
    assert!(
        lines
            .lock()
            .unwrap()
            .contains(&("build".to_owned(), "watching".to_owned()))
    );

    watch.stop().await;
    let heartbeat = dir.path().join("dist").join("heartbeat");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let last = fs::read_to_string(&heartbeat).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        fs::read_to_string(&heartbeat).unwrap(),
        last,
        "build:watch still runs"
    );
}

#[tokio::test]
async fn build_watch_exiting_is_reported() {
    if !has_node().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    package(
        dir.path(),
        serde_json::json!({ "build:watch": "node exit.mjs" }),
    );
    write(&dir.path().join("exit.mjs"), "process.exit(2);\n");
    let (sink, _) = recording_sink();

    let mut watch = builder::watch(&config(dir.path(), "node build.mjs", sink)).unwrap();
    let event = timeout(Duration::from_secs(20), watch.next())
        .await
        .unwrap();
    match event {
        Some(WatchEvent::BuildWatchExited(Some(status))) => assert!(!status.success()),
        other => panic!("unexpected {other:?}"),
    }
}
