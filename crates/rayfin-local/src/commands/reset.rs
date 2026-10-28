//! `rayfin-local reset`: delete the project's `.rayfin-local/` folder.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use rl_project::{LocalState, Project};

use crate::ui;

pub fn run(dir: &Path, yes: bool) -> anyhow::Result<ExitCode> {
    let project = Project::discover(dir)?;
    let state_dir = project.state_dir();
    if !state_dir.exists() {
        ui::status(format_args!(
            "Nothing to reset: {} does not exist.",
            state_dir.display()
        ));
        return Ok(ExitCode::SUCCESS);
    }
    if !yes
        && !confirm(&format!(
            "Delete {} (the database, users, signing keys and stored files)? [y/N] ",
            state_dir.display()
        ))?
    {
        ui::status("Cancelled; nothing was deleted.");
        return Ok(ExitCode::SUCCESS);
    }
    if LocalState::reset(&project)? {
        ui::step(format_args!("Deleted {}", state_dir.display()));
    } else {
        ui::status(format_args!(
            "Nothing to reset: {} does not exist.",
            state_dir.display()
        ));
    }
    Ok(ExitCode::SUCCESS)
}

/// Asks on stderr and reads the answer from stdin; anything but `y`/`yes` (or end of input) is no.
fn confirm(question: &str) -> anyhow::Result<bool> {
    let mut stderr = io::stderr().lock();
    stderr.write_all(question.as_bytes())?;
    stderr.flush()?;
    drop(stderr);
    let mut answer = String::new();
    let stdin = io::stdin();
    stdin.lock().read_line(&mut answer)?;
    if !stdin.is_terminal() {
        // A piped answer is not echoed: end the question's line.
        eprintln!();
    }
    Ok(is_yes(&answer))
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
