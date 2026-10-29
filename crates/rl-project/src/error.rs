use std::path::PathBuf;

/// Everything that can go wrong while loading a Rayfin project or its `.rayfin-local/` state.
///
/// Messages are meant for the developer: they name the file (with line and column when known) and
/// say what to do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProjectError {
    /// No `rayfin/rayfin.yml` in the start directory or any of its parents.
    #[error(
        "No Rayfin project found in {} or its parents: expected rayfin/rayfin.yml. Run \
         rayfin-local inside a Rayfin project, or pass -C <dir>.",
        .start.display()
    )]
    NotFound { start: PathBuf },

    /// A file or directory could not be read.
    #[error("Could not read {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A file or directory could not be created, written or removed.
    #[error("Could not write {}: {source}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `rayfin.yml` is not valid YAML or does not match the Rayfin configuration schema.
    #[error("Invalid {}{}: {message}", .path.display(), at(*.line, *.column))]
    InvalidConfig {
        path: PathBuf,
        line: Option<usize>,
        column: Option<usize>,
        message: String,
    },

    /// A `${VAR}` reference in `rayfin.yml` names a variable that is unset or empty and has no
    /// `${VAR:-default}`.
    #[error(
        "Environment variable '{var}' referenced in rayfin.yml ({context}) is not defined. Set it \
         in {} or in the shell environment.",
        .env_file.display()
    )]
    MissingVariable {
        var: String,
        /// Where the reference is, as a dotted path (for example `connectors[0].config.itemId`).
        context: String,
        env_file: PathBuf,
    },

    /// A service `path` (or `staticHosting.root`/`folder`) in `rayfin.yml` is absolute or leaves
    /// its base directory.
    #[error(
        "Invalid services.{service}.{field} '{value}' in {}: {reason}. Use a relative path \
         inside the project.",
        .path.display()
    )]
    InvalidServicePath {
        path: PathBuf,
        /// `data`, `storage`, `functions` or `staticHosting`.
        service: &'static str,
        /// `path`, or `root`/`folder` for `staticHosting`.
        field: &'static str,
        value: String,
        reason: String,
    },

    /// A `package.json` is not valid JSON.
    #[error("Invalid {}{}: {message}", .path.display(), at(*.line, *.column))]
    InvalidPackageJson {
        path: PathBuf,
        line: Option<usize>,
        column: Option<usize>,
        message: String,
    },

    /// `rayfin-local.toml` is invalid, for example because of an unknown key.
    #[error("Invalid {}{}: {message}", .path.display(), at(*.line, *.column))]
    InvalidLocalConfig {
        path: PathBuf,
        line: Option<usize>,
        column: Option<usize>,
        message: String,
    },

    /// A file in `.rayfin-local/` (`state.json`, `secrets.json`) is corrupt.
    #[error(
        "Invalid {}: {message}. Fix or delete the file, or run `rayfin-local reset`.",
        .path.display()
    )]
    InvalidState { path: PathBuf, message: String },

    /// The `.rayfin-local/` folder could not be deleted.
    #[error(
        "Could not delete {}: {source}. Stop any rayfin-local running for this project (it keeps \
         the database open) and try again.",
        .path.display()
    )]
    Reset {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Another `rayfin-local serve` is running the project.
    #[error("{}", already_serving(.info.as_ref()))]
    AlreadyServing { info: Option<crate::ServeInfo> },

    /// An explicitly requested env file does not exist.
    #[error(
        "Environment file not found: {}. Create it or omit the explicit env file.",
        .path.display()
    )]
    EnvFileNotFound { path: PathBuf },

    /// The project's npm dependencies are not installed.
    #[error(
        "{package} is not installed: there is no node_modules/{package} in {} or its parents. Run \
         `npm install` in {} first.",
        .searched_from.display(),
        .install_dir.display()
    )]
    NotInstalled {
        package: String,
        searched_from: PathBuf,
        install_dir: PathBuf,
    },
}

/// ` (line L, column C)`, or nothing when the position is unknown.
fn at(line: Option<usize>, column: Option<usize>) -> String {
    match (line, column) {
        (Some(line), Some(column)) => format!(" (line {line}, column {column})"),
        (Some(line), None) => format!(" (line {line})"),
        _ => String::new(),
    }
}

fn already_serving(info: Option<&crate::ServeInfo>) -> String {
    let who = info.map_or_else(String::new, |info| {
        let urls: Vec<String> = [("app", &info.app_url), ("API", &info.api_url)]
            .into_iter()
            .filter_map(|(label, url)| url.as_ref().map(|url| format!("{label} {url}")))
            .collect();
        if urls.is_empty() {
            format!(" (process {})", info.pid)
        } else {
            format!(" (process {}: {})", info.pid, urls.join(", "))
        }
    });
    format!(
        "rayfin-local serve is already running this project{who}. Stop it first (Ctrl+C in its \
         terminal)."
    )
}

/// The 1-based line and column of byte `offset` in `text`.
pub(crate) fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rfind('\n')
        .map_or(before, |newline| &before[newline + 1..])
        .chars()
        .count()
        + 1;
    (line, column)
}

/// Removes the ` at line L column C` suffix serde_yaml and serde_json append to their messages
/// (the position is reported separately).
pub(crate) fn strip_position_suffix(message: &str, line: usize, column: usize) -> &str {
    message
        .strip_suffix(&format!(" at line {line} column {column}"))
        .unwrap_or(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_column_counts_from_one() {
        let text = "a = 1\nbé = 2\n";
        assert_eq!(line_column(text, 0), (1, 1));
        assert_eq!(line_column(text, text.find('=').unwrap()), (1, 3));
        assert_eq!(line_column(text, text.rfind('=').unwrap()), (2, 4));
    }
}
