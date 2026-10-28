//! Process supervision and helper protocol, against the real `node`.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use rl_node::{
    Helper, LineSink, ManagedChild, NodeError, NodeRuntime, ProcessSpec, Stream,
    materialize_bundle, run_helper, run_to_completion,
};
use serde_json::json;

type Lines = Arc<Mutex<Vec<(String, Stream, String)>>>;

fn collector() -> (LineSink, Lines) {
    let lines: Lines = Arc::default();
    let sink_lines = Arc::clone(&lines);
    let sink: LineSink = Arc::new(move |label, stream, line| {
        sink_lines
            .lock()
            .unwrap()
            .push((label.to_owned(), stream, line.to_owned()));
    });
    (sink, lines)
}

fn texts(lines: &Lines) -> Vec<String> {
    lines.lock().unwrap().iter().map(|l| l.2.clone()).collect()
}

async fn node() -> NodeRuntime {
    NodeRuntime::discover().await.expect("node on PATH")
}

#[tokio::test]
async fn discover_finds_a_supported_node() {
    let node = node().await;
    assert!(node.major >= rl_node::MIN_NODE_MAJOR);
    assert!(node.version.starts_with(&node.major.to_string()));
}

#[test]
fn helpers_are_content_addressed_and_written_once() {
    let dir = tempfile::tempdir().unwrap();
    let first = Helper::new("hello", "console.log(1)\n");
    let path = first.materialize(dir.path()).unwrap();
    assert_eq!(path, first.materialize(dir.path()).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), first.source);
    assert_eq!(path.extension().unwrap(), "mjs");
    assert!(path.starts_with(dir.path().join("js")));

    let changed = Helper::new("hello", "console.log(2)\n");
    assert_ne!(path, changed.materialize(dir.path()).unwrap());
}

#[test]
fn bundles_keep_exact_names_side_by_side() {
    let dir = tempfile::tempdir().unwrap();
    let helpers = [
        Helper::new("main.mjs", "import './shim.cjs';"),
        Helper::new("shim.cjs", "module.exports = 1;"),
    ];
    let bundle = materialize_bundle(dir.path(), "host", &helpers).unwrap();
    assert!(bundle.join("main.mjs").is_file());
    assert!(bundle.join("shim.cjs").is_file());
    assert_eq!(
        bundle,
        materialize_bundle(dir.path(), "host", &helpers).unwrap()
    );
}

const ECHO_HELPER: &str = r#"
import { readFileSync, writeFileSync } from 'node:fs';
const args = process.argv.slice(2);
const input = JSON.parse(readFileSync(args[args.indexOf('--input') + 1], 'utf8'));
console.log('log from helper');
const output = args[args.indexOf('--output') + 1];
if (input.fail) {
  writeFileSync(output, JSON.stringify({ ok: false, error: { message: 'it failed: ' + input.fail, stack: 'Error: x' } }));
} else if (input.crash) {
  console.error('boom detail');
  process.exit(3);
} else {
  writeFileSync(output, JSON.stringify({ ok: true, result: { doubled: input.n * 2 } }));
}
"#;

#[derive(serde::Deserialize, Debug, PartialEq)]
struct Doubled {
    doubled: i64,
}

#[tokio::test]
async fn run_helper_returns_the_result_and_streams_logs() {
    let node = node().await;
    let dir = tempfile::tempdir().unwrap();
    let helper = Helper::new("echo", ECHO_HELPER);
    let (sink, lines) = collector();
    let result: Doubled = run_helper(
        &node,
        &helper,
        dir.path(),
        dir.path(),
        &json!({"n": 21}),
        sink,
    )
    .await
    .unwrap();
    assert_eq!(result, Doubled { doubled: 42 });
    assert!(lines.lock().unwrap().iter().any(|(label, stream, line)| {
        label == "echo" && *stream == Stream::Stdout && line == "log from helper"
    }));
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("tmp")).unwrap().collect();
    assert!(leftovers.is_empty(), "temp files are removed");
}

#[tokio::test]
async fn run_helper_reports_a_failure_envelope() {
    let node = node().await;
    let dir = tempfile::tempdir().unwrap();
    let helper = Helper::new("echo", ECHO_HELPER);
    let (sink, _) = collector();
    let err = run_helper::<Doubled>(
        &node,
        &helper,
        dir.path(),
        dir.path(),
        &json!({"fail": "x"}),
        sink,
    )
    .await
    .unwrap_err();
    match err {
        NodeError::Helper { helper, message } => {
            assert_eq!(helper, "echo");
            assert_eq!(message, "it failed: x");
        }
        other => panic!("unexpected error {other:?}"),
    }
}

#[tokio::test]
async fn run_helper_reports_a_crash_with_its_stderr() {
    let node = node().await;
    let dir = tempfile::tempdir().unwrap();
    let helper = Helper::new("echo", ECHO_HELPER);
    let (sink, _) = collector();
    let err = run_helper::<Doubled>(
        &node,
        &helper,
        dir.path(),
        dir.path(),
        &json!({"crash": true}),
        sink,
    )
    .await
    .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("boom detail"), "{message}");
    assert!(message.contains("without writing a result"), "{message}");
}

#[tokio::test]
async fn run_to_completion_fails_on_a_non_zero_exit() {
    let node = node().await;
    let dir = tempfile::tempdir().unwrap();
    let (sink, _) = collector();
    let spec = ProcessSpec::new("exit", &node.path, dir.path())
        .args(["-e", "process.exit(2)"])
        .sink(sink);
    match run_to_completion(spec).await.unwrap_err() {
        NodeError::Failed { label, status } => {
            assert_eq!(label, "exit");
            assert_eq!(status.code(), Some(2));
        }
        other => panic!("unexpected error {other:?}"),
    }
}

#[tokio::test]
async fn shell_commands_run_and_stream_output() {
    let dir = tempfile::tempdir().unwrap();
    let (sink, lines) = collector();
    run_to_completion(ProcessSpec::shell("sh", "echo hello", dir.path()).sink(sink))
        .await
        .unwrap();
    assert_eq!(texts(&lines), vec!["hello".to_owned()]);
}

#[tokio::test]
async fn env_clear_keeps_path_and_sets_variables() {
    let node = node().await;
    let dir = tempfile::tempdir().unwrap();
    let (sink, lines) = collector();
    let mut spec = ProcessSpec::new("env", &node.path, dir.path())
        .args([
            "-e",
            "console.log(JSON.stringify(Object.keys(process.env)))",
        ])
        .env("RL_TEST_VALUE", "1")
        .sink(sink);
    spec.env_clear = true;
    run_to_completion(spec).await.unwrap();
    let keys: Vec<String> = serde_json::from_str(&texts(&lines)[0]).unwrap();
    let has = |name: &str| keys.iter().any(|k| k.eq_ignore_ascii_case(name));
    assert!(has("RL_TEST_VALUE"));
    assert!(has("PATH"));
    assert!(
        !has("CARGO_MANIFEST_DIR"),
        "inherited variables are cleared"
    );
}

#[tokio::test]
async fn long_lines_and_crlf_are_handled() {
    let node = node().await;
    let dir = tempfile::tempdir().unwrap();
    let (sink, lines) = collector();
    let spec = ProcessSpec::new("lines", &node.path, dir.path())
        .args([
            "-e",
            "process.stdout.write('a\\r\\nb\\n' + 'x'.repeat(70000) + '\\nlast')",
        ])
        .sink(sink);
    run_to_completion(spec).await.unwrap();
    let texts = texts(&lines);
    assert_eq!(texts[0], "a");
    assert_eq!(texts[1], "b");
    assert_eq!(texts.iter().map(String::len).sum::<usize>(), 2 + 70000 + 4);
    assert_eq!(texts.last().unwrap(), "last");
}

/// Starts `node` that spawns a grandchild and prints its pid; returns the child and that pid.
async fn spawn_tree(node: &NodeRuntime, dir: &std::path::Path) -> (ManagedChild, u32) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
    let sink: LineSink = Arc::new(move |_, _, line| {
        if let Some(pid) = line.strip_prefix("grandchild ") {
            let _ = tx.send(pid.trim().parse().unwrap());
        }
    });
    let script = "const { spawn } = require('node:child_process');\
        const c = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });\
        console.log('grandchild ' + c.pid);\
        setInterval(() => {}, 1000);";
    let child = ManagedChild::spawn(
        ProcessSpec::new("tree", &node.path, dir)
            .args(["-e", script])
            .sink(sink),
    )
    .unwrap();
    let pid = tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("grandchild started")
        .unwrap();
    assert!(is_alive(pid));
    (child, pid)
}

async fn assert_gone(pid: u32) {
    for _ in 0..100 {
        if !is_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("process {pid} is still running");
}

#[tokio::test]
async fn kill_ends_the_whole_tree() {
    let node = node().await;
    let dir = tempfile::tempdir().unwrap();
    let (mut child, grandchild) = spawn_tree(&node, dir.path()).await;
    let parent = child.pid().unwrap();
    child.kill(Duration::from_millis(500)).await.unwrap();
    assert_gone(parent).await;
    assert_gone(grandchild).await;
    assert!(child.wait().await.is_ok());
}

#[tokio::test]
async fn dropping_the_handle_ends_the_whole_tree() {
    let node = node().await;
    let dir = tempfile::tempdir().unwrap();
    let (child, grandchild) = spawn_tree(&node, dir.path()).await;
    let parent = child.pid().unwrap();
    drop(child);
    assert_gone(parent).await;
    assert_gone(grandchild).await;
}

#[cfg(windows)]
fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    // SAFETY: plain FFI; the handle is closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
