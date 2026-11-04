//! `--state-dir` and `RAYFIN_LOCAL_STATE_DIR` of the built binary: the state folder moves out of
//! the project, and nothing is written to `<project>/.rayfin-local`.
//!
//! The `serve` case needs Node.js and the fixture's npm packages (`npm ci` in
//! `tests/fixtures/apps/basic`); without them it prints a note and passes.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

const RAYFIN_YML: &str = "id: app\nname: App\nversion: 1.0.0\nservices:\n  auth:\n    \
                          enabled: true\nsecrets:\n  - name: API_KEY\n";

fn rayfin_local() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rayfin-local"));
    command.env_remove("RAYFIN_LOCAL_STATE_DIR");
    command
}

/// A temporary folder holding a project (`app/`) and room for state folders next to it.
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("app");
    std::fs::create_dir_all(project.join("rayfin")).unwrap();
    std::fs::write(project.join("rayfin").join("rayfin.yml"), RAYFIN_YML).unwrap();
    (dir, project)
}

fn succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_state_dir_option_moves_the_database_and_secrets_out_of_the_project() {
    let (dir, project) = workspace();
    let state = dir.path().join("state");
    let output = rayfin_local()
        .args(["users", "add", "ana@contoso.com", "--password", "pass-1"])
        .arg("--state-dir")
        .arg(&state)
        .arg("-C")
        .arg(&project)
        .output()
        .unwrap();
    succeeded(&output);
    let output = rayfin_local()
        .args(["secret", "set", "API_KEY", "s3cret", "-C"])
        .arg(&project)
        .arg("--state-dir")
        .arg(&state)
        .output()
        .unwrap();
    succeeded(&output);

    assert!(state.join("db.sqlite").is_file());
    assert!(state.join("secrets.json").is_file());
    assert!(state.join("state.json").is_file());
    assert!(!project.join(".rayfin-local").exists());

    let listed = rayfin_local()
        .args(["users", "list", "--state-dir"])
        .arg(&state)
        .arg("-C")
        .arg(&project)
        .output()
        .unwrap();
    succeeded(&listed);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("ana@contoso.com"));
}

#[test]
fn the_environment_variable_is_relative_to_the_current_directory_and_the_option_wins() {
    let (dir, project) = workspace();
    let output = rayfin_local()
        .args(["secret", "set", "API_KEY", "from-env", "-C"])
        .arg(&project)
        .env("RAYFIN_LOCAL_STATE_DIR", "env-state")
        .current_dir(dir.path())
        .output()
        .unwrap();
    succeeded(&output);
    assert!(dir.path().join("env-state").join("secrets.json").is_file());
    assert!(!project.join(".rayfin-local").exists());

    let output = rayfin_local()
        .args([
            "secret",
            "set",
            "API_KEY",
            "from-option",
            "--state-dir",
            "option-state",
        ])
        .arg("-C")
        .arg(&project)
        .env("RAYFIN_LOCAL_STATE_DIR", "env-state")
        .current_dir(dir.path())
        .output()
        .unwrap();
    succeeded(&output);
    let secrets = |folder: &str| -> Value {
        let path = dir.path().join(folder).join("secrets.json");
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    };
    assert_eq!(secrets("option-state")["API_KEY"], "from-option");
    assert_eq!(secrets("env-state")["API_KEY"], "from-env");
}

#[test]
fn a_state_dir_that_would_hold_the_project_is_refused() {
    let (dir, project) = workspace();
    for holding in [&project, &dir.path().to_owned()] {
        let output = rayfin_local()
            .args(["secret", "set", "API_KEY", "value", "--state-dir"])
            .arg(holding)
            .arg("-C")
            .arg(&project)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--state-dir"), "{stderr}");
    }
    assert!(!project.join(".rayfin-local").exists());
    assert!(!project.join("secrets.json").exists());
}

/// `tests/fixtures/apps/basic`, when its packages and Node.js are installed.
fn fixture() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/apps/basic");
    let installed = ["@microsoft/rayfin-core", "vite"].iter().all(|package| {
        let manifest = dir.join("node_modules").join(package).join("package.json");
        manifest.is_file()
    });
    if !installed {
        eprintln!(
            "skipped: the fixture's packages are not installed (run `npm ci` in {})",
            dir.display()
        );
        return None;
    }
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: Node.js is not on PATH");
        return None;
    }
    Some(dir)
}

#[test]
fn serve_keeps_its_whole_state_in_the_state_dir() {
    let Some(project) = fixture() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut child = rayfin_local()
        .args([
            "serve",
            "--json",
            "--no-frontend",
            "--no-functions",
            "--port",
            "0",
        ])
        .arg("--state-dir")
        .arg(&state)
        .arg("-C")
        .arg(&project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (events_tx, events) = mpsc::channel::<Value>();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(event) = serde_json::from_str(&line)
                && events_tx.send(event).is_err()
            {
                break;
            }
        }
    });
    let ready = loop {
        let event = events
            .recv_timeout(Duration::from_secs(300))
            .expect("no ready event");
        assert_ne!(event["type"], "stopped", "{event}");
        if event["type"] == "ready" {
            break event;
        }
    };
    assert_eq!(
        Path::new(ready["dataDir"].as_str().unwrap()),
        state.as_path()
    );
    for file in ["db.sqlite", "state.json", "serve.json", "serve.lock"] {
        assert!(state.join(file).is_file(), "{file}");
    }
    for folder in ["keys", "storage", "cache"] {
        assert!(state.join(folder).is_dir(), "{folder}");
    }
    assert!(state.join("cache").join("compiled").is_dir());

    drop(child.stdin.take());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
}
