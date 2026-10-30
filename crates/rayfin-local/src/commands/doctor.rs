//! `rayfin-local doctor`: checks what `serve` needs, one line per check (`--json`: one object
//! with every check).

use std::fmt::Display;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use rl_node::NodeRuntime;
use rl_project::{LocalState, Project};
use rl_tokens::{TokenBroker, TokenBrokerConfig};
use serde::Serialize;
use serde_json::json;

use crate::integrations;
use crate::ports::DEFAULT_API_PORT;
use crate::ui::{Color, Painter};

const DEFAULT_HOST: &str = "127.0.0.1";

pub async fn run(dir: &Path, json: bool) -> anyhow::Result<ExitCode> {
    let mut report = Report::new(json);
    let node = match NodeRuntime::discover().await {
        Ok(node) => {
            report.check(
                "node",
                Status::Ok,
                format_args!("Node.js {} ({})", node.version, node.path.display()),
                None,
            );
            Some(node)
        }
        Err(err) => {
            report.check("node", Status::Fail, err, None);
            None
        }
    };

    let project = match Project::discover(dir) {
        Ok(project) => {
            report.check(
                "project",
                Status::Ok,
                format_args!(
                    "Project {} ({})",
                    project.display_name(),
                    project.root().display()
                ),
                None,
            );
            for warning in project.warnings() {
                report.check("project", Status::Warn, warning, None);
            }
            Some(project)
        }
        Err(err) => {
            report.check("project", Status::Fail, err, None);
            None
        }
    };
    let state = project
        .as_ref()
        .and_then(|project| check_packages_and_state(&mut report, project));

    check_sign_in(&mut report, project.as_ref(), state.as_ref(), node.as_ref()).await;
    check_ports(&mut report, project.as_ref(), state.as_ref());
    Ok(report.finish())
}

fn check_packages_and_state(report: &mut Report, project: &Project) -> Option<LocalState> {
    let installed = project.check_installed();
    match &installed {
        Ok(()) => report.check(
            "packages",
            Status::Ok,
            format_args!(
                "Rayfin SDK {} installed",
                project.sdk_version().unwrap_or("(unknown version)")
            ),
            None,
        ),
        Err(err) => report.check("packages", Status::Fail, err, None),
    }
    match (project.sdk_version_warning(), project.sdk_version()) {
        (Some(warning), _) if installed.is_ok() => {
            report.check("sdk", Status::Warn, warning, None);
        }
        (Some(warning), _) => report.record("sdk", Status::Warn, warning, None),
        (None, Some(version)) => report.record(
            "sdk",
            Status::Ok,
            format!(
                "Rayfin SDK {version} is supported ({})",
                rl_project::supported_sdk_range()
            ),
            None,
        ),
        (None, None) => report.record(
            "sdk",
            Status::Warn,
            "No @microsoft/rayfin-* package is declared or installed, so the Rayfin SDK version \
             is unknown."
                .to_owned(),
            None,
        ),
    }
    match LocalState::load_or_create(project) {
        Ok(state) => {
            let message = format!("Local state in {}", state.dir().display());
            report.record("state", Status::Ok, message, None);
            Some(state)
        }
        Err(err) => {
            report.check("state", Status::Fail, err, None);
            None
        }
    }
}

async fn check_sign_in(
    report: &mut Report,
    project: Option<&Project>,
    state: Option<&LocalState>,
    node: Option<&NodeRuntime>,
) {
    let broker = match (project, state) {
        (Some(project), Some(state)) => {
            match integrations::token_broker(project, state, node, None, None) {
                Ok(broker) => broker,
                Err(err) => {
                    report.check("azure", Status::Fail, err, None);
                    return;
                }
            }
        }
        _ => TokenBroker::new(TokenBrokerConfig::default()),
    };
    match broker.whoami().await {
        Ok(identity) => report.check(
            "azure",
            Status::Ok,
            format_args!(
                "Signed in to Azure as {} (tenant {})",
                identity.upn, identity.tenant_id
            ),
            None,
        ),
        Err(err) => {
            let hint = err
                .hint
                .as_deref()
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default();
            report.check(
                "azure",
                Status::Warn,
                format_args!(
                    "Not signed in to Azure: {}.{hint} Connectors and functions that use Azure \
                     tokens need it.",
                    err.message.trim_end_matches('.')
                ),
                err.hint,
            );
        }
    }
}

fn check_ports(report: &mut Report, project: Option<&Project>, state: Option<&LocalState>) {
    let local = project.map(Project::local_config);
    let host = local
        .and_then(|local| local.server.host.clone())
        .unwrap_or_else(|| DEFAULT_HOST.to_owned());
    let api_port = local
        .and_then(|local| local.server.port)
        .or_else(|| state.and_then(|state| state.data().api_port))
        .unwrap_or(DEFAULT_API_PORT);
    let frontend_port = project.map_or(rl_project::DEFAULT_FRONTEND_PORT, |project| {
        project.preferred_frontend_port()
    });
    for (id, what, port) in [
        ("port-backend", "backend", api_port),
        ("port-frontend", "frontend", frontend_port),
    ] {
        if rl_server::is_port_free(&host, port) {
            report.check(
                id,
                Status::Ok,
                format_args!("Port {port} ({what}) is free"),
                None,
            );
        } else {
            report.check(
                id,
                Status::Warn,
                format_args!(
                    "Port {port} ({what}) is in use; `rayfin-local serve` will use the next free \
                     one unless the port is set with a flag or in rayfin-local.toml"
                ),
                None,
            );
        }
    }
}

/// How a check went; a check with several findings has the worst one's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Fail,
}

/// One check, as `--json` prints it.
#[derive(Debug, PartialEq, Serialize)]
struct Check {
    id: &'static str,
    status: Status,
    message: String,
    hint: Option<String>,
}

struct Report {
    json: bool,
    painter: Painter,
    failures: usize,
    warnings: usize,
    checks: Vec<Check>,
}

impl Report {
    fn new(json: bool) -> Self {
        Self {
            json,
            painter: Painter::stdout(),
            failures: 0,
            warnings: 0,
            checks: Vec::new(),
        }
    }

    /// A finding: a line (`✓`, `!` or `✗`), and a check of `--json`.
    fn check(
        &mut self,
        id: &'static str,
        status: Status,
        message: impl Display,
        hint: Option<String>,
    ) {
        let message = message.to_string();
        let (mark, color) = match status {
            Status::Ok => ("✓", Color::Green),
            Status::Warn => {
                self.warnings += 1;
                ("!", Color::Yellow)
            }
            Status::Fail => {
                self.failures += 1;
                ("✗", Color::Red)
            }
        };
        if !self.json {
            println!("{} {message}", self.painter.paint(color, mark));
        }
        self.record(id, status, message, hint);
    }

    /// A check of `--json` without a line of its own. A further finding of a check gives it the
    /// worse status and adds its message to the hint.
    fn record(&mut self, id: &'static str, status: Status, message: String, hint: Option<String>) {
        match self.checks.iter_mut().find(|check| check.id == id) {
            Some(check) => {
                check.status = check.status.max(status);
                let hints = check.hint.get_or_insert_with(String::new);
                if !hints.is_empty() {
                    hints.push('\n');
                }
                hints.push_str(&message);
            }
            None => self.checks.push(Check {
                id,
                status,
                message,
                hint,
            }),
        }
    }

    fn finish(self) -> ExitCode {
        if self.json {
            let ok = self.checks.iter().all(|check| check.status != Status::Fail);
            let body = json!({ "ok": ok, "checks": self.checks });
            let _ = writeln!(std::io::stdout(), "{body}");
            return if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        println!();
        match (self.failures, self.warnings) {
            (0, 0) => println!("Everything looks good."),
            (0, warnings) => println!("Ready to serve, with {warnings} warning(s)."),
            (failures, _) => println!("{failures} problem(s) to fix before `rayfin-local serve`."),
        }
        if self.failures == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn further_findings_of_a_check_worsen_it_and_join_its_hint() {
        let mut report = Report::new(true);
        report.check("project", Status::Ok, "Project app (C:\\app)", None);
        report.check("project", Status::Warn, "unknown setting `x`", None);
        report.check("project", Status::Warn, "unknown setting `y`", None);
        report.record("sdk", Status::Ok, "Rayfin SDK 1.36.2".to_owned(), None);
        report.check("state", Status::Fail, "corrupt state.json", None);
        assert_eq!(
            report.checks,
            [
                Check {
                    id: "project",
                    status: Status::Warn,
                    message: "Project app (C:\\app)".to_owned(),
                    hint: Some("unknown setting `x`\nunknown setting `y`".to_owned()),
                },
                Check {
                    id: "sdk",
                    status: Status::Ok,
                    message: "Rayfin SDK 1.36.2".to_owned(),
                    hint: None,
                },
                Check {
                    id: "state",
                    status: Status::Fail,
                    message: "corrupt state.json".to_owned(),
                    hint: None,
                },
            ]
        );
    }
}
