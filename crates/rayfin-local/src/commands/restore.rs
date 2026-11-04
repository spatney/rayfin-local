//! `rayfin-local restore`: puts a snapshot from `rayfin-local backup` into an empty state folder
//! (see [`crate::snapshot`]).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::json;

use crate::snapshot;
use crate::ui;

pub fn run(location: &super::Location, from: &Path, json: bool) -> anyhow::Result<ExitCode> {
    match restore(location, from) {
        Ok((_, summary)) if json => {
            let line = json!({
                "type": "restore",
                "v": 1,
                "files": summary.files,
                "bytes": summary.bytes,
            });
            let _ = writeln!(std::io::stdout(), "{line}");
            Ok(ExitCode::SUCCESS)
        }
        Ok((state_dir, summary)) => {
            ui::step(format_args!(
                "Restored the local state into {} ({} files, {} bytes)",
                state_dir.display(),
                summary.files,
                summary.bytes
            ));
            Ok(ExitCode::SUCCESS)
        }
        Err(err) if json => {
            super::print_json_error(&err);
            Ok(ExitCode::FAILURE)
        }
        Err(err) => Err(err),
    }
}

fn restore(
    location: &super::Location,
    from: &Path,
) -> anyhow::Result<(PathBuf, snapshot::Summary)> {
    let state_dir = location.resolve_state_dir()?;
    let from = std::path::absolute(from)?;
    let summary = snapshot::restore(&from, &state_dir).map_err(super::snapshot_error)?;
    Ok((state_dir, summary))
}
