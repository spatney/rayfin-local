//! `rayfin-local doctor`: checks what `serve` needs, one line per check.

use std::fmt::Display;
use std::path::Path;
use std::process::ExitCode;

use rl_node::NodeRuntime;
use rl_project::{LocalState, Project};
use rl_tokens::{TokenBroker, TokenBrokerConfig};

use crate::integrations;
use crate::ports::DEFAULT_API_PORT;
use crate::ui::{Color, Painter};

const DEFAULT_HOST: &str = "127.0.0.1";

pub async fn run(dir: &Path) -> anyhow::Result<ExitCode> {
    let mut report = Report::new();
    let node = match NodeRuntime::discover().await {
        Ok(node) => {
            report.ok(format_args!(
                "Node.js {} ({})",
                node.version,
                node.path.display()
            ));
            Some(node)
        }
        Err(err) => {
            report.fail(err);
            None
        }
    };

    let project = match Project::discover(dir) {
        Ok(project) => {
            report.ok(format_args!(
                "Project {} ({})",
                project.display_name(),
                project.root().display()
            ));
            for warning in project.warnings() {
                report.warn(warning);
            }
            Some(project)
        }
        Err(err) => {
            report.fail(err);
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
    match project.check_installed() {
        Ok(()) => {
            report.ok(format_args!(
                "Rayfin SDK {} installed",
                project.sdk_version().unwrap_or("(unknown version)")
            ));
            if let Some(warning) = project.sdk_version_warning() {
                report.warn(warning);
            }
        }
        Err(err) => report.fail(err),
    }
    match LocalState::load_or_create(project) {
        Ok(state) => Some(state),
        Err(err) => {
            report.fail(err);
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
                    report.fail(err);
                    return;
                }
            }
        }
        _ => TokenBroker::new(TokenBrokerConfig::default()),
    };
    match broker.whoami().await {
        Ok(identity) => report.ok(format_args!(
            "Signed in to Azure as {} (tenant {})",
            identity.upn, identity.tenant_id
        )),
        Err(err) => {
            let hint = err.hint.map(|hint| format!(" {hint}")).unwrap_or_default();
            report.warn(format_args!(
                "Not signed in to Azure: {}.{hint} Connectors and functions that use Azure \
                 tokens need it.",
                err.message.trim_end_matches('.')
            ));
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
    for (what, port) in [("backend", api_port), ("frontend", frontend_port)] {
        if rl_server::is_port_free(&host, port) {
            report.ok(format_args!("Port {port} ({what}) is free"));
        } else {
            report.warn(format_args!(
                "Port {port} ({what}) is in use; `rayfin-local serve` will use the next free one \
                 unless the port is set with a flag or in rayfin-local.toml"
            ));
        }
    }
}

struct Report {
    painter: Painter,
    failures: usize,
    warnings: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            painter: Painter::stdout(),
            failures: 0,
            warnings: 0,
        }
    }

    fn ok(&mut self, message: impl Display) {
        println!("{} {message}", self.painter.paint(Color::Green, "✓"));
    }

    fn warn(&mut self, message: impl Display) {
        self.warnings += 1;
        println!("{} {message}", self.painter.paint(Color::Yellow, "!"));
    }

    fn fail(&mut self, message: impl Display) {
        self.failures += 1;
        println!("{} {message}", self.painter.paint(Color::Red, "✗"));
    }

    fn finish(self) -> ExitCode {
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
