//! `rayfin-local backup`: a consistent snapshot of the state folder, safe while `serve` runs
//! (see [`crate::snapshot`]).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::json;

use crate::snapshot;
use crate::ui;

pub fn run(location: &super::Location, out: &Path, json: bool) -> anyhow::Result<ExitCode> {
    match backup(location, out) {
        Ok((out, summary)) if json => {
            let line = json!({
                "type": "backup",
                "v": 1,
                "out": out,
                "files": summary.files,
                "bytes": summary.bytes,
            });
            let _ = writeln!(std::io::stdout(), "{line}");
            Ok(ExitCode::SUCCESS)
        }
        Ok((out, summary)) => {
            ui::step(format_args!(
                "Backed up the local state to {} ({} files, {} bytes)",
                out.display(),
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

fn backup(location: &super::Location, out: &Path) -> anyhow::Result<(PathBuf, snapshot::Summary)> {
    let state_dir = location.resolve_state_dir()?;
    let out = std::path::absolute(out)?;
    let summary = snapshot::backup(&state_dir, &out).map_err(super::snapshot_error)?;
    Ok((out, summary))
}
