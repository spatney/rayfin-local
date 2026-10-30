//! The host contract's error codes: classifying what made `serve` fail.

use std::error::Error as StdError;
use std::fmt;
use std::path::Path;

use rl_db::{DbError, SchemaError};
use rl_frontend::FrontendError;
use rl_functions::FunctionsError;
use rl_node::{MIN_NODE_MAJOR, NodeError};
use rl_project::{PackageJson, ProjectError};
use serde_json::{Value, json};

use crate::events::{ErrorCode, ErrorEvent, Event, no_details};
use crate::ui;

/// A failure as hosts see it: an `error` event's code, message, hint and details.
#[derive(Clone, Debug, PartialEq)]
pub struct HostError {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
    pub details: Value,
}

impl HostError {
    /// The code an error in `err`'s chain was tagged with ([`Coded`]), else the code of the
    /// first error of a type that has one, else `internal`.
    pub fn classify(err: &anyhow::Error) -> Self {
        let (code, details) = err
            .chain()
            .find_map(|cause| {
                cause
                    .downcast_ref::<Coded>()
                    .map(|coded| (coded.code, coded.details.clone()))
            })
            .or_else(|| err.chain().find_map(known))
            .unwrap_or((ErrorCode::Internal, no_details()));
        Self {
            code,
            message: ui::error_lines(err).join("\n"),
            hint: hint(code, &details),
            details,
        }
    }

    /// The `error` event.
    pub fn event(self, fatal: bool) -> Event {
        Event::Error(ErrorEvent {
            code: self.code,
            message: self.message,
            hint: self.hint,
            fatal,
            details: self.details,
        })
    }
}

/// The code and details of an error type that has them.
fn known(cause: &(dyn StdError + 'static)) -> Option<(ErrorCode, Value)> {
    if let Some(err) = cause.downcast_ref::<ProjectError>() {
        return Some(match err {
            ProjectError::NotFound { .. } => (ErrorCode::ProjectNotFound, no_details()),
            ProjectError::NotInstalled { install_dir, .. } => (
                ErrorCode::PackagesMissing,
                json!({ "dir": install_dir.display().to_string() }),
            ),
            ProjectError::AlreadyServing { info } => (
                ErrorCode::AlreadyServing,
                json!({
                    "pid": info.as_ref().map(|info| info.pid),
                    "appUrl": info.as_ref().and_then(|info| info.app_url.clone()),
                    "apiUrl": info.as_ref().and_then(|info| info.api_url.clone()),
                }),
            ),
            ProjectError::Read { .. } | ProjectError::Write { .. } | ProjectError::Reset { .. } => {
                (ErrorCode::Internal, no_details())
            }
            _ => (ErrorCode::ConfigInvalid, no_details()),
        });
    }
    if let Some(err) = cause.downcast_ref::<NodeError>() {
        return match err {
            NodeError::NotFound { .. } => Some((ErrorCode::NodeMissing, no_details())),
            NodeError::TooOld { found, .. } => {
                Some((ErrorCode::NodeTooOld, json!({ "version": found })))
            }
            _ => None,
        };
    }
    if let Some(err) = cause.downcast_ref::<SchemaError>() {
        return Some(match err {
            SchemaError::DataLoss { losses } => (
                ErrorCode::SchemaRefused,
                json!({ "losses": losses.iter().map(ToString::to_string).collect::<Vec<_>>() }),
            ),
            _ => (ErrorCode::SchemaFailed, no_details()),
        });
    }
    if cause.is::<DbError>() {
        return Some((ErrorCode::SchemaFailed, no_details()));
    }
    if let Some(err) = cause.downcast_ref::<FrontendError>() {
        return Some(match err {
            FrontendError::PortInUse { port } => (ErrorCode::PortInUse, json!({ "port": port })),
            FrontendError::ViteNotInstalled { dir, .. } if declares_vite(dir) => (
                ErrorCode::PackagesMissing,
                json!({ "dir": dir.display().to_string() }),
            ),
            FrontendError::MissingFrontendDir(_) => (ErrorCode::ConfigInvalid, no_details()),
            FrontendError::BuildFailed { .. } | FrontendError::MissingBuildOutput { .. } => {
                (ErrorCode::Internal, no_details())
            }
            _ => (ErrorCode::ViteFailed, no_details()),
        });
    }
    if cause.is::<FunctionsError>() {
        return Some((ErrorCode::ConfigInvalid, no_details()));
    }
    None
}

/// Whether the app in `dir` declares `vite`: then installing its packages brings Vite.
fn declares_vite(dir: &Path) -> bool {
    PackageJson::load_dir(dir)
        .ok()
        .flatten()
        .is_some_and(|package| package.dependency("vite").is_some())
}

/// What a host can suggest for `code`.
fn hint(code: ErrorCode, details: &Value) -> Option<String> {
    let install_node = format!(
        "Install Node.js {MIN_NODE_MAJOR} or later from https://nodejs.org, or set \
         RAYFIN_LOCAL_NODE to a node executable."
    );
    Some(match code {
        ErrorCode::NodeMissing | ErrorCode::NodeTooOld => install_node,
        ErrorCode::ProjectNotFound => {
            "Run it in a Rayfin project (a folder with rayfin/rayfin.yml), or pass -C <dir>."
                .to_owned()
        }
        ErrorCode::PackagesMissing => match details["dir"].as_str() {
            Some(dir) => format!("Run `npm install` in {dir}."),
            None => "Run `npm install` in the project.".to_owned(),
        },
        ErrorCode::SchemaRefused => {
            "Restart with --force-schema to apply it, or --reset to start over.".to_owned()
        }
        ErrorCode::PortInUse => match details["port"].as_u64() {
            Some(port) => {
                format!("Stop the program that uses port {port}, or choose another port.")
            }
            None => "Stop the program that uses the port, or choose another port.".to_owned(),
        },
        ErrorCode::AlreadyServing => {
            "Use the running `rayfin-local serve`, or stop it first.".to_owned()
        }
        ErrorCode::ConfigInvalid
        | ErrorCode::AnalysisFailed
        | ErrorCode::SchemaFailed
        | ErrorCode::ViteFailed
        | ErrorCode::Internal => return None,
    })
}

/// An error tagged with the code hosts get for it ([`HostError::classify`]). It shows as the
/// error it wraps, with the same causes, so the human output does not change.
#[derive(Debug)]
pub struct Coded {
    code: ErrorCode,
    details: Value,
    error: anyhow::Error,
}

impl fmt::Display for Coded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl StdError for Coded {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.error.source()
    }
}

/// `error`, tagged with `code` and `details`.
pub fn coded(code: ErrorCode, details: Value, error: impl Into<anyhow::Error>) -> anyhow::Error {
    anyhow::Error::new(Coded {
        code,
        details,
        error: error.into(),
    })
}

/// Tags the error of a result with a code ([`Coded`]).
pub trait WithCode<T> {
    fn with_code(self, code: ErrorCode) -> anyhow::Result<T>;
}

impl<T, E: Into<anyhow::Error>> WithCode<T> for Result<T, E> {
    fn with_code(self, code: ErrorCode) -> anyhow::Result<T> {
        self.map_err(|err| coded(code, no_details(), err))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::{Context as _, anyhow};
    use rl_db::{DataLoss, LossReason};
    use rl_project::ServeInfo;

    use super::*;

    fn classify(err: anyhow::Error) -> (ErrorCode, Value) {
        let host = HostError::classify(&err);
        (host.code, host.details)
    }

    #[test]
    fn typed_errors_have_codes_and_details() {
        let not_installed = ProjectError::NotInstalled {
            package: "@microsoft/rayfin-core".into(),
            searched_from: PathBuf::from("app"),
            install_dir: PathBuf::from("app"),
        };
        assert_eq!(
            classify(anyhow::Error::new(not_installed)),
            (ErrorCode::PackagesMissing, json!({ "dir": "app" }))
        );
        let serving = ProjectError::AlreadyServing {
            info: Some(ServeInfo {
                pid: 42,
                app_url: Some("http://localhost:5173/".into()),
                api_url: None,
            }),
        };
        assert_eq!(
            classify(anyhow::Error::new(serving)),
            (
                ErrorCode::AlreadyServing,
                json!({ "pid": 42, "appUrl": "http://localhost:5173/", "apiUrl": null })
            )
        );
        let missing = ProjectError::NotFound {
            start: PathBuf::from("somewhere"),
        };
        assert_eq!(
            classify(anyhow::Error::new(missing)).0,
            ErrorCode::ProjectNotFound
        );
        let invalid = ProjectError::InvalidLocalConfig {
            path: PathBuf::from("rayfin-local.toml"),
            line: Some(1),
            column: None,
            message: "unknown field".into(),
        };
        assert_eq!(
            classify(anyhow::Error::new(invalid)).0,
            ErrorCode::ConfigInvalid
        );

        let too_old = NodeError::TooOld {
            found: "18.20.0".into(),
            min: MIN_NODE_MAJOR,
        };
        assert_eq!(
            classify(anyhow::Error::new(too_old)),
            (ErrorCode::NodeTooOld, json!({ "version": "18.20.0" }))
        );
        let no_node = NodeError::NotFound {
            min: MIN_NODE_MAJOR,
        };
        assert_eq!(
            classify(anyhow::Error::new(no_node)).0,
            ErrorCode::NodeMissing
        );

        let port = FrontendError::PortInUse { port: 5173 };
        assert_eq!(
            classify(anyhow::Error::new(port).context("Could not start the app's Vite dev server")),
            (ErrorCode::PortInUse, json!({ "port": 5173 }))
        );
        let vite = FrontendError::Protocol("closed".into());
        assert_eq!(classify(anyhow::Error::new(vite)).0, ErrorCode::ViteFailed);
        assert_eq!(
            classify(anyhow!("something else")),
            (ErrorCode::Internal, json!({}))
        );
    }

    #[test]
    fn a_refused_schema_lists_the_losses() {
        let loss = DataLoss {
            table: "Note".into(),
            column: Some("body".into()),
            rows: 3,
            reason: LossReason::DropColumn,
        };
        let refused = SchemaError::DataLoss {
            losses: vec![loss.clone()],
        };
        let err = anyhow::Error::new(refused)
            .context("Could not update the database to the app's entities");
        let host = HostError::classify(&err);
        assert_eq!(host.code, ErrorCode::SchemaRefused);
        assert_eq!(host.details, json!({ "losses": [loss.to_string()] }));
        assert!(host.hint.unwrap().contains("--force-schema"));
        assert!(
            host.message
                .starts_with("Could not update the database to the app's entities\n"),
            "{}",
            host.message
        );
    }

    #[test]
    fn tagged_errors_keep_their_code_and_their_text() {
        let untagged = || {
            anyhow!("the analysis helper failed")
                .context("Could not analyze the entities in rayfin/data")
        };
        let tagged: anyhow::Result<()> = Err(untagged());
        let tagged = tagged
            .with_code(ErrorCode::AnalysisFailed)
            .context("outer")
            .unwrap_err();
        assert_eq!(
            classify(tagged.context("more")).0,
            ErrorCode::AnalysisFailed
        );

        let tagged = coded(ErrorCode::PortInUse, json!({ "port": 7171 }), untagged());
        assert_eq!(ui::error_lines(&tagged), ui::error_lines(&untagged()));
        assert_eq!(tagged.to_string(), untagged().to_string());
        assert_eq!(
            classify(tagged),
            (ErrorCode::PortInUse, json!({ "port": 7171 }))
        );
    }
}
