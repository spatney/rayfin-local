//! `rayfin-local backup` and `rayfin-local restore` of the built binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const RAYFIN_YML: &str = "id: app\nname: App\nversion: 1.0.0\nservices:\n  auth:\n    \
                          enabled: true\nsecrets:\n  - name: API_KEY\n";

fn rayfin_local() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rayfin-local"));
    command.env_remove("RAYFIN_LOCAL_STATE_DIR");
    command
}

/// A temporary folder holding a project (`app/`) whose state folder (`state/`) has a user, a
/// secret and a stored file.
fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("app");
    std::fs::create_dir_all(project.join("rayfin")).unwrap();
    std::fs::write(project.join("rayfin").join("rayfin.yml"), RAYFIN_YML).unwrap();
    let state = dir.path().join("state");
    succeeded(&run(
        &project,
        &state,
        &["users", "add", "ana@contoso.com", "--password", "pass-1"],
    ));
    succeeded(&run(
        &project,
        &state,
        &["secret", "set", "API_KEY", "s3cret"],
    ));
    std::fs::create_dir_all(state.join("storage").join("photos")).unwrap();
    std::fs::write(state.join("storage").join("photos").join("a.txt"), "photo").unwrap();
    std::fs::create_dir_all(state.join("cache")).unwrap();
    std::fs::write(state.join("cache").join("compiled.json"), "{}").unwrap();
    (dir, project, state)
}

fn run(project: &Path, state: &Path, args: &[&str]) -> Output {
    rayfin_local()
        .args(args)
        .arg("-C")
        .arg(project)
        .arg("--state-dir")
        .arg(state)
        .output()
        .unwrap()
}

fn succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The one JSON line a `--json` command printed.
fn json_line(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout:\n{stdout}");
    serde_json::from_str(lines[0]).unwrap()
}

fn failed_with(output: &Output, code: &str) {
    assert!(!output.status.success());
    let line = json_line(output);
    assert_eq!(line["type"], "error", "{line}");
    assert_eq!(line["code"], code, "{line}");
    assert!(line["message"].as_str().is_some_and(|m| !m.is_empty()));
}

fn sha256(path: &Path) -> String {
    hex::encode(ring::digest::digest(
        &ring::digest::SHA256,
        &std::fs::read(path).unwrap(),
    ))
}

/// The files under `dir`, as sorted `/`-separated relative paths.
fn files(dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let relative = entry.path().strip_prefix(dir).unwrap();
            let parts: Vec<_> = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            parts.join("/")
        })
        .collect();
    files.sort();
    files
}

#[test]
fn a_backup_taken_while_the_database_is_written_restores_into_a_new_state_folder() {
    let (dir, project, state) = workspace();
    // A writer in WAL mode holds the database open with rows not checkpointed yet.
    let writer = rl_db::Db::open(&state.join("db.sqlite")).unwrap();
    writer
        .call_blocking(|c| {
            c.execute_batch(
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);
                 INSERT INTO notes (body) VALUES ('one'), ('two'), ('three');",
            )
        })
        .unwrap();
    assert!(
        std::fs::metadata(state.join("db.sqlite-wal"))
            .unwrap()
            .len()
            > 0
    );

    let snapshot = dir.path().join("snapshot");
    let output = run(
        &project,
        &state,
        &["backup", "--json", "--out", snapshot.to_str().unwrap()],
    );
    succeeded(&output);
    let line = json_line(&output);
    assert_eq!(line["type"], "backup");
    assert_eq!(line["v"], 1);
    assert_eq!(Path::new(line["out"].as_str().unwrap()), snapshot.as_path());

    let manifest: Value =
        serde_json::from_slice(&std::fs::read(snapshot.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["v"], 1);
    assert_eq!(manifest["rayfinLocal"], env!("CARGO_PKG_VERSION"));
    assert!(manifest["createdAt"].as_str().unwrap().ends_with('Z'));
    let listed: Vec<String> = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap().to_owned())
        .collect();
    let mut on_disk = files(&snapshot);
    on_disk.retain(|path| path != "manifest.json");
    assert_eq!(listed, on_disk);
    for path in [
        "db.sqlite",
        "secrets.json",
        "state.json",
        "storage/photos/a.txt",
    ] {
        assert!(listed.iter().any(|p| p == path), "{path} in {listed:?}");
    }
    assert!(listed.iter().any(|p| p.starts_with("keys/")), "{listed:?}");
    assert!(
        !listed.iter().any(|p| p.starts_with("cache/")
            || p.starts_with("serve.")
            || p.ends_with("-wal")
            || p.ends_with("-shm")),
        "{listed:?}"
    );
    let mut bytes = 0;
    for file in manifest["files"].as_array().unwrap() {
        let path = snapshot.join(file["path"].as_str().unwrap());
        assert_eq!(file["size"], std::fs::metadata(&path).unwrap().len());
        assert_eq!(file["sha256"], sha256(&path));
        bytes += file["size"].as_u64().unwrap();
    }
    assert_eq!(line["files"], listed.len());
    assert_eq!(line["bytes"], bytes);

    // Opened elsewhere: opening a database file switches it to WAL, which rewrites its header.
    let check = dir.path().join("check.sqlite");
    std::fs::copy(snapshot.join("db.sqlite"), &check).unwrap();
    let copy = rl_db::Db::open(&check).unwrap();
    let count: i64 = copy
        .call_blocking(|c| c.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(count, 3);
    drop((copy, writer));

    let restored = dir.path().join("restored");
    let output = rayfin_local()
        .args(["restore", "--json", "--from"])
        .arg(&snapshot)
        .arg("--state-dir")
        .arg(&restored)
        .arg("-C")
        .arg(&project)
        .output()
        .unwrap();
    succeeded(&output);
    let line = json_line(&output);
    assert_eq!(line["type"], "restore");
    assert_eq!(line["v"], 1);
    assert_eq!(line["files"], listed.len());
    assert_eq!(line["bytes"], bytes);
    for path in &listed {
        if path != "db.sqlite" {
            assert_eq!(
                sha256(&restored.join(path)),
                sha256(&state.join(path)),
                "{path}"
            );
        }
    }
    assert!(!restored.join("cache").exists());
    let siblings: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("restore-"))
        .collect();
    assert!(siblings.is_empty(), "{siblings:?}");

    let listed_users = run(&project, &restored, &["users", "list"]);
    succeeded(&listed_users);
    assert!(String::from_utf8_lossy(&listed_users.stdout).contains("ana@contoso.com"));
}

#[test]
fn a_restore_refuses_a_tampered_snapshot_and_writes_nothing() {
    let (dir, project, state) = workspace();
    let snapshot = dir.path().join("snapshot");
    succeeded(&run(
        &project,
        &state,
        &["backup", "--out", snapshot.to_str().unwrap()],
    ));
    std::fs::write(
        snapshot.join("storage").join("photos").join("a.txt"),
        "PHOTO",
    )
    .unwrap();

    let restored = dir.path().join("restored");
    let output = run(
        &project,
        &restored,
        &["restore", "--json", "--from", snapshot.to_str().unwrap()],
    );
    failed_with(&output, "snapshot-invalid");
    assert!(!restored.exists());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("restore-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    std::fs::write(
        snapshot.join("storage").join("photos").join("a.txt"),
        "photo",
    )
    .unwrap();
    std::fs::remove_file(snapshot.join("secrets.json")).unwrap();
    let output = run(
        &project,
        &restored,
        &["restore", "--json", "--from", snapshot.to_str().unwrap()],
    );
    failed_with(&output, "snapshot-invalid");
    assert!(!restored.exists());
}

#[test]
fn a_backup_refuses_a_folder_that_is_not_empty_and_a_missing_database() {
    let (dir, project, state) = workspace();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("keep.txt"), "mine").unwrap();
    let output = run(
        &project,
        &state,
        &["backup", "--json", "--out", out.to_str().unwrap()],
    );
    failed_with(&output, "out-not-empty");
    assert_eq!(files(&out), ["keep.txt"]);

    let empty = dir.path().join("empty-state");
    let output = run(
        &project,
        &empty,
        &[
            "backup",
            "--json",
            "--out",
            dir.path().join("out2").to_str().unwrap(),
        ],
    );
    failed_with(&output, "state-missing");
    assert!(!dir.path().join("out2").exists());

    let inside = state.join("storage").join("backup");
    let output = run(
        &project,
        &state,
        &["backup", "--json", "--out", inside.to_str().unwrap()],
    );
    failed_with(&output, "out-invalid");
    assert!(!inside.exists());

    // Without --json the failure is a message on stderr.
    let output = run(
        &project,
        &state,
        &["backup", "--out", out.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not empty"));
}

#[test]
fn a_restore_refuses_a_state_folder_that_has_state_or_is_being_served() {
    let (dir, project, state) = workspace();
    let snapshot = dir.path().join("snapshot");
    succeeded(&run(
        &project,
        &state,
        &["backup", "--out", snapshot.to_str().unwrap()],
    ));
    let before = files(&state);
    let output = run(
        &project,
        &state,
        &["restore", "--json", "--from", snapshot.to_str().unwrap()],
    );
    failed_with(&output, "state-exists");
    assert_eq!(files(&state), before);

    let served = dir.path().join("served");
    let lock = rl_project::LocalState::lock_dir(&served).unwrap();
    let output = run(
        &project,
        &served,
        &["restore", "--json", "--from", snapshot.to_str().unwrap()],
    );
    failed_with(&output, "already-serving");
    assert!(!served.join("db.sqlite").exists());
    drop(lock);

    let output = run(
        &project,
        &served,
        &["restore", "--json", "--from", snapshot.to_str().unwrap()],
    );
    succeeded(&output);
    assert!(served.join("db.sqlite").is_file());
}

#[test]
fn backup_and_restore_use_the_projects_own_state_folder_by_default() {
    let (dir, project, state) = workspace();
    let snapshot = dir.path().join("snapshot");
    succeeded(&run(
        &project,
        &state,
        &["backup", "--out", snapshot.to_str().unwrap()],
    ));
    let output = rayfin_local()
        .args(["restore", "--from"])
        .arg(&snapshot)
        .arg("-C")
        .arg(&project)
        .output()
        .unwrap();
    succeeded(&output);
    assert!(project.join(".rayfin-local").join("db.sqlite").is_file());
    assert_eq!(
        std::fs::read_to_string(project.join(".rayfin-local").join(".gitignore")).unwrap(),
        "*\n"
    );
    let output = rayfin_local()
        .args(["users", "list", "-C"])
        .arg(&project)
        .output()
        .unwrap();
    succeeded(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("ana@contoso.com"));
}
