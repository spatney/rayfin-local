//! Snapshots of the state folder, for `rayfin-local backup` and `rayfin-local restore`.
//!
//! A snapshot is a folder with `db.sqlite` (one self-contained file from SQLite's online backup,
//! so it is consistent while `serve` writes the database), copies of `keys/`, `storage/`,
//! `secrets.json` and `state.json` when the state folder has them, and `manifest.json`, which
//! lists every other file with its size and SHA-256. The caches, `serve.json`, `serve.lock` and
//! the database's `-wal` and `-shm` are never part of it.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use ring::digest::{Context, SHA256};
use rl_project::{LocalState, ProjectError, ServeLock};
use serde::{Deserialize, Serialize};

/// The snapshot's list of files.
pub const MANIFEST_FILE: &str = "manifest.json";
/// The version of [`Manifest`] this rayfin-local writes and reads.
pub const MANIFEST_VERSION: u32 = 1;

const DB_FILE: &str = "db.sqlite";
/// What a snapshot copies besides the database: folders and files of the state folder.
const COPIED: [&str; 4] = ["keys", "storage", "secrets.json", "state.json"];
/// The state folder's entries a restore would replace; it refuses a folder that has any of them.
const RESTORED: [&str; 8] = [
    DB_FILE,
    "db.sqlite-wal",
    "db.sqlite-shm",
    "db.sqlite-journal",
    "keys",
    "storage",
    "secrets.json",
    "state.json",
];

/// `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub v: u32,
    /// The version of rayfin-local that made the snapshot.
    pub rayfin_local: String,
    /// When the snapshot was made (RFC 3339, UTC).
    pub created_at: String,
    /// Every file of the snapshot but the manifest, sorted by path.
    pub files: Vec<ManifestFile>,
}

/// A file of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Relative to the snapshot folder, with `/` between the parts.
    pub path: String,
    /// In bytes.
    pub size: u64,
    /// The SHA-256 of the contents, lowercase hex.
    pub sha256: String,
}

/// How many files a backup or restore wrote, and their total size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub bytes: u64,
}

/// Why a backup or restore failed.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("There is no local state to back up: {path} does not exist.")]
    StateMissing { path: PathBuf },
    #[error("The backup folder {path} is not empty.")]
    OutNotEmpty { path: PathBuf },
    #[error("The backup folder {path} is inside the state folder {state_dir}.")]
    OutInsideState { path: PathBuf, state_dir: PathBuf },
    #[error(
        "The state folder {path} already has local state ({entry}); restore into a new folder, or \
         run `rayfin-local reset` first."
    )]
    StateExists { path: PathBuf, entry: &'static str },
    #[error("The snapshot in {path} is not valid: {reason}")]
    Invalid { path: PathBuf, reason: String },
    #[error("The backup failed: {0}")]
    Backup(String),
    #[error("The restore failed: {0}")]
    Restore(String),
    #[error(transparent)]
    Project(#[from] ProjectError),
}

impl SnapshotError {
    /// The code of the command's JSON error line; `None` for the project's own errors.
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            Self::StateMissing { .. } => "state-missing",
            Self::OutNotEmpty { .. } => "out-not-empty",
            Self::OutInsideState { .. } => "out-invalid",
            Self::StateExists { .. } => "state-exists",
            Self::Invalid { .. } => "snapshot-invalid",
            Self::Backup(_) => "backup-failed",
            Self::Restore(_) => "restore-failed",
            Self::Project(_) => return None,
        })
    }
}

/// Writes a snapshot of the state folder `state_dir` into `out`, which must not exist or be
/// empty. Takes no lock: it runs while `serve` does. On failure, `out` is left as it was.
pub fn backup(state_dir: &Path, out: &Path) -> Result<Summary, SnapshotError> {
    let db = state_dir.join(DB_FILE);
    if !db.is_file() {
        return Err(SnapshotError::StateMissing { path: db });
    }
    let created = prepare_out(out)?;
    let result = check_out_outside(state_dir, out).and_then(|()| write_snapshot(state_dir, out));
    if result.is_err() {
        if created {
            let _ = fs::remove_dir_all(out);
        } else {
            empty_folder(out);
        }
    }
    result
}

/// Creates `out`, or checks the existing folder is empty; returns whether it was created.
fn prepare_out(out: &Path) -> Result<bool, SnapshotError> {
    let not_empty = || SnapshotError::OutNotEmpty {
        path: out.to_path_buf(),
    };
    match fs::metadata(out) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(out).map_err(|err| backup_failed(out, &err))?;
            Ok(true)
        }
        Err(err) => Err(backup_failed(out, &err)),
        Ok(meta) if !meta.is_dir() => Err(not_empty()),
        Ok(_) => {
            let mut entries = fs::read_dir(out).map_err(|err| backup_failed(out, &err))?;
            match entries.next() {
                Some(_) => Err(not_empty()),
                None => Ok(false),
            }
        }
    }
}

/// Refuses an `out` inside the state folder, which `storage/` could contain.
fn check_out_outside(state_dir: &Path, out: &Path) -> Result<(), SnapshotError> {
    let state = fs::canonicalize(state_dir).map_err(|err| backup_failed(state_dir, &err))?;
    let target = fs::canonicalize(out).map_err(|err| backup_failed(out, &err))?;
    if target.starts_with(&state) {
        return Err(SnapshotError::OutInsideState {
            path: out.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
        });
    }
    Ok(())
}

fn write_snapshot(state_dir: &Path, out: &Path) -> Result<Summary, SnapshotError> {
    rl_db::backup_to(&state_dir.join(DB_FILE), &out.join(DB_FILE)).map_err(|err| {
        SnapshotError::Backup(format!("{}: {err}", state_dir.join(DB_FILE).display()))
    })?;
    for name in COPIED {
        let source = state_dir.join(name);
        match fs::symlink_metadata(&source) {
            Ok(meta) if meta.is_dir() => copy_tree(&source, &out.join(name))?,
            Ok(meta) if meta.is_file() => copy_file(&source, &out.join(name))?,
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(backup_failed(&source, &err)),
        }
    }

    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(out).min_depth(1) {
        let entry = entry.map_err(|err| SnapshotError::Backup(err.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(out).unwrap_or(entry.path());
        let path = manifest_path(relative).ok_or_else(|| {
            SnapshotError::Backup(format!(
                "{} has a name that is not valid UTF-8",
                entry.path().display()
            ))
        })?;
        let mut file =
            fs::File::open(entry.path()).map_err(|err| backup_failed(entry.path(), &err))?;
        let (size, sha256) =
            hash(&mut file, &mut io::sink()).map_err(|err| backup_failed(entry.path(), &err))?;
        files.push(ManifestFile { path, size, sha256 });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let summary = Summary {
        files: files.len(),
        bytes: files.iter().map(|file| file.size).sum(),
    };
    let manifest = Manifest {
        v: MANIFEST_VERSION,
        rayfin_local: env!("CARGO_PKG_VERSION").to_owned(),
        created_at: now_rfc3339(),
        files,
    };
    let mut json = serde_json::to_vec_pretty(&manifest)
        .map_err(|err| SnapshotError::Backup(err.to_string()))?;
    json.push(b'\n');
    let path = out.join(MANIFEST_FILE);
    fs::write(&path, json).map_err(|err| backup_failed(&path, &err))?;
    Ok(summary)
}

/// Copies the folder `source` to `dest`. Entries that disappear while it runs (a file `serve`
/// deletes) and anything but files and folders are skipped.
fn copy_tree(source: &Path, dest: &Path) -> Result<(), SnapshotError> {
    fs::create_dir_all(dest).map_err(|err| backup_failed(dest, &err))?;
    for entry in walkdir::WalkDir::new(source).min_depth(1) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) if err.io_error().map(io::Error::kind) == Some(io::ErrorKind::NotFound) => {
                continue;
            }
            Err(err) => return Err(SnapshotError::Backup(err.to_string())),
        };
        let target = dest.join(entry.path().strip_prefix(source).unwrap_or(entry.path()));
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|err| backup_failed(&target, &err))?;
        } else if entry.file_type().is_file() {
            copy_file(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn copy_file(source: &Path, dest: &Path) -> Result<(), SnapshotError> {
    match fs::copy(source, dest) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound && !source.exists() => Ok(()),
        Err(err) => Err(backup_failed(source, &err)),
    }
}

/// Restores the snapshot in `from` into the state folder `state_dir`, holding its `serve` lock.
/// Refused while `serve` runs, when the folder already has state, and when any file of the
/// manifest is missing or differs; then nothing is written. The files are verified into a
/// temporary sibling of the state folder first and moved in afterwards, the database last.
pub fn restore(from: &Path, state_dir: &Path) -> Result<Summary, SnapshotError> {
    let manifest = read_manifest(from)?;
    // An existing folder is checked before the copy, a new one once the copy is verified.
    let mut lock = match state_dir.exists() {
        true => Some(lock_empty_state(state_dir)?),
        false => None,
    };
    if let Some(parent) = state_dir.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| SnapshotError::Restore(format!("{}: {err}", parent.display())))?;
    }
    let stage = stage_dir(state_dir)?;
    let result = stage_files(from, &manifest, &stage).and_then(|()| {
        if lock.is_none() {
            lock = Some(lock_empty_state(state_dir)?);
        }
        move_in(&stage, state_dir)
    });
    let _ = fs::remove_dir_all(&stage);
    drop(lock);
    result?;
    Ok(Summary {
        files: manifest.files.len(),
        bytes: manifest.files.iter().map(|file| file.size).sum(),
    })
}

/// Takes the `serve` lock of `state_dir` (creating the folder) and checks it holds no state.
fn lock_empty_state(state_dir: &Path) -> Result<ServeLock, SnapshotError> {
    // Checked before locking too, so a refused restore leaves no lock file behind.
    refuse_existing_state(state_dir)?;
    let lock = LocalState::lock_dir(state_dir)?;
    refuse_existing_state(state_dir)?;
    Ok(lock)
}

/// Fails if `state_dir` has any of the entries a restore would write.
fn refuse_existing_state(state_dir: &Path) -> Result<(), SnapshotError> {
    match RESTORED
        .into_iter()
        .find(|entry| fs::symlink_metadata(state_dir.join(entry)).is_ok())
    {
        Some(entry) => Err(SnapshotError::StateExists {
            path: state_dir.to_path_buf(),
            entry,
        }),
        None => Ok(()),
    }
}

/// Reads and checks `from`'s manifest: its version, and paths that stay in the parts of the
/// state folder a snapshot holds, with the database among them.
fn read_manifest(from: &Path) -> Result<Manifest, SnapshotError> {
    let invalid = |reason: String| SnapshotError::Invalid {
        path: from.to_path_buf(),
        reason,
    };
    let bytes = fs::read(from.join(MANIFEST_FILE))
        .map_err(|err| invalid(format!("could not read {MANIFEST_FILE}: {err}")))?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|err| invalid(format!("{MANIFEST_FILE}: {err}")))?;
    if manifest.v != MANIFEST_VERSION {
        return Err(invalid(format!(
            "{MANIFEST_FILE} has version {}; this rayfin-local reads version {MANIFEST_VERSION}",
            manifest.v
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    for file in &manifest.files {
        if !allowed_path(&file.path) {
            return Err(invalid(format!("{MANIFEST_FILE} lists {:?}", file.path)));
        }
        if !seen.insert(file.path.as_str()) {
            return Err(invalid(format!(
                "{MANIFEST_FILE} lists {} twice",
                file.path
            )));
        }
        if file.sha256.len() != 64
            || !file
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(invalid(format!(
                "{MANIFEST_FILE}: {} has no valid sha256",
                file.path
            )));
        }
    }
    if !seen.contains(DB_FILE) {
        return Err(invalid(format!("{MANIFEST_FILE} does not list {DB_FILE}")));
    }
    Ok(manifest)
}

/// A manifest path a restore writes: `db.sqlite`, `secrets.json`, `state.json`, or a file under
/// `keys/` or `storage/`, made of plain names.
fn allowed_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let plain = parts.iter().all(|part| {
        !part.is_empty()
            && *part != "."
            && *part != ".."
            && !part.contains(['\\', ':', '\0'])
            && Path::new(part)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
    });
    plain
        && match parts.as_slice() {
            [name] => [DB_FILE, "secrets.json", "state.json"].contains(name),
            [folder, ..] => ["keys", "storage"].contains(folder),
            [] => false,
        }
}

/// A new, empty folder next to `state_dir` (so moving out of it is a rename).
fn stage_dir(state_dir: &Path) -> Result<PathBuf, SnapshotError> {
    let name = state_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let stage = state_dir.with_file_name(format!(".{name}.restore-{}-{nanos}", std::process::id()));
    #[cfg(unix)]
    let created =
        std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700).create(&stage);
    #[cfg(not(unix))]
    let created = fs::DirBuilder::new().create(&stage);
    created.map_err(|err| SnapshotError::Restore(format!("{}: {err}", stage.display())))?;
    Ok(stage)
}

/// Copies every file of the manifest from `from` into `stage`, checking its size and hash.
fn stage_files(from: &Path, manifest: &Manifest, stage: &Path) -> Result<(), SnapshotError> {
    let invalid = |reason: String| SnapshotError::Invalid {
        path: from.to_path_buf(),
        reason,
    };
    let failed =
        |path: &Path, err: io::Error| SnapshotError::Restore(format!("{}: {err}", path.display()));
    for file in &manifest.files {
        let relative: PathBuf = file.path.split('/').collect();
        let source = from.join(&relative);
        if !fs::symlink_metadata(&source).is_ok_and(|meta| meta.is_file()) {
            return Err(invalid(format!("{} is missing", file.path)));
        }
        let mut reader = fs::File::open(&source).map_err(|err| failed(&source, err))?;
        let target = stage.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| failed(parent, err))?;
        }
        let mut writer = fs::File::create(&target).map_err(|err| failed(&target, err))?;
        let (size, sha256) = hash(&mut reader, &mut writer).map_err(|err| failed(&source, err))?;
        writer.sync_all().map_err(|err| failed(&target, err))?;
        if size != file.size {
            return Err(invalid(format!(
                "{} has {size} bytes; the manifest says {}",
                file.path, file.size
            )));
        }
        if sha256 != file.sha256 {
            return Err(invalid(format!("{} does not match its sha256", file.path)));
        }
    }
    Ok(())
}

/// Moves the staged entries into the state folder, the database last; on failure, moves back the
/// ones already moved.
fn move_in(stage: &Path, state_dir: &Path) -> Result<(), SnapshotError> {
    let failed = |err: io::Error| SnapshotError::Restore(format!("{}: {err}", state_dir.display()));
    let mut names: Vec<_> = fs::read_dir(stage)
        .map_err(failed)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<_, _>>()
        .map_err(failed)?;
    names.sort_by_key(|name| name == DB_FILE);
    let mut moved = Vec::new();
    for name in names {
        if let Err(err) = fs::rename(stage.join(&name), state_dir.join(&name)) {
            for name in moved.iter().rev() {
                let _ = fs::rename(state_dir.join(name), stage.join(name));
            }
            return Err(failed(err));
        }
        moved.push(name);
    }
    Ok(())
}

/// Removes everything in `dir`, after a failed backup into a folder that was empty.
fn empty_folder(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let _ = match entry.file_type() {
            Ok(kind) if kind.is_dir() => fs::remove_dir_all(&path),
            _ => fs::remove_file(&path),
        };
    }
}

/// Copies `reader` to `writer`; returns the size and the SHA-256 (lowercase hex).
fn hash(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<(u64, String)> {
    let mut context = Context::new(&SHA256);
    let mut buffer = vec![0; 64 * 1024];
    let mut size = 0;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        context.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
        size += read as u64;
    }
    Ok((size, hex::encode(context.finish())))
}

/// `relative` with `/` between its parts; `None` when a part is not UTF-8.
fn manifest_path(relative: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect();
    Some(parts?.join("/"))
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn backup_failed(path: &Path, err: &io::Error) -> SnapshotError {
    SnapshotError::Backup(format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_lists_only_the_parts_of_the_state_folder_a_restore_writes() {
        for path in [
            "db.sqlite",
            "secrets.json",
            "state.json",
            "keys/a",
            "storage/x/y.txt",
        ] {
            assert!(allowed_path(path), "{path}");
        }
        for path in [
            "",
            "keys",
            "keys/",
            "keys//a",
            "../db.sqlite",
            "keys/../db.sqlite",
            "storage/./a",
            "cache/compiled.json",
            "serve.json",
            "serve.lock",
            "db.sqlite-wal",
            "manifest.json",
            "/keys/a",
            "keys\\a",
            "C:/keys/a",
            "storage/C:a",
        ] {
            assert!(!allowed_path(path), "{path}");
        }
    }

    #[test]
    fn a_manifest_needs_its_version_the_database_and_hex_digests() {
        let dir = tempfile::tempdir().unwrap();
        let digest = "0".repeat(64);
        let read = |manifest: serde_json::Value| {
            fs::write(dir.path().join(MANIFEST_FILE), manifest.to_string()).unwrap();
            read_manifest(dir.path())
        };
        let file = |path: &str, sha256: &str| serde_json::json!({ "path": path, "size": 0, "sha256": sha256 });
        let manifest = |v: u32, files: Vec<serde_json::Value>| {
            serde_json::json!({
                "v": v, "rayfinLocal": "0", "createdAt": "2026-01-01T00:00:00Z", "files": files,
            })
        };

        assert!(read(manifest(1, vec![file(DB_FILE, &digest)])).is_ok());
        for bad in [
            manifest(2, vec![file(DB_FILE, &digest)]),
            manifest(1, vec![file("keys/a", &digest)]),
            manifest(1, vec![file(DB_FILE, &digest), file(DB_FILE, &digest)]),
            manifest(1, vec![file(DB_FILE, &"A".repeat(64))]),
            manifest(1, vec![file(DB_FILE, "00")]),
        ] {
            assert!(
                matches!(read(bad.clone()), Err(SnapshotError::Invalid { .. })),
                "{bad}"
            );
        }
    }
}
