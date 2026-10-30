//! `rayfin-local`: run a Rayfin app and its whole backend on this machine.

mod analysis;
mod commands;
mod connectors;
mod integrations;
mod ports;
mod reload;
mod shutdown;
mod ui;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rl_tokens::TokenMode;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

use crate::commands::serve::ServeOptions;

#[derive(Parser)]
#[command(
    name = "rayfin-local",
    version,
    about = "Run a Rayfin app and its whole backend locally: database, data API, auth, functions, \
             connectors and storage"
)]
struct Cli {
    /// The Rayfin project to use (default: the current directory or the nearest parent with
    /// rayfin/rayfin.yml).
    #[arg(short = 'C', long = "project", global = true, value_name = "DIR")]
    project: Option<PathBuf>,

    /// Show detailed logs.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the app with a local backend.
    Serve(ServeArgs),
    /// Delete the local state (.rayfin-local): database, users, keys and stored files.
    Reset {
        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
    /// Check Node.js, installed packages, az/Rayfin sign-in and ports.
    Doctor,
    /// Print the app model and the SQLite schema derived from rayfin/data.
    Schema,
    /// List, add and delete the app's local users, and reset their passwords.
    #[command(subcommand)]
    Users(commands::users::UsersCommand),
    /// Set, list and delete the local values of the secrets declared in rayfin.yml.
    #[command(subcommand)]
    Secret(commands::secret::SecretCommand),
}

#[derive(Args)]
struct ServeArgs {
    /// Backend port (default: persisted per project, 7171 when free).
    #[arg(long)]
    port: Option<u16>,
    /// Frontend port (default: RAYFIN_PUBLIC_FRONTEND_PORT from rayfin/.env, else 5173).
    #[arg(long)]
    frontend_port: Option<u16>,
    /// Build the app once and serve it from the backend instead of running the Vite dev server.
    #[arg(long = "static")]
    static_build: bool,
    /// Run only the backend.
    #[arg(long)]
    no_frontend: bool,
    /// Do not build or run the app's functions.
    #[arg(long)]
    no_functions: bool,
    /// Interface to listen on (default: `[server] host` in rayfin-local.toml, else 127.0.0.1).
    /// Anything but loopback exposes your Azure identity to the network.
    #[arg(long, value_name = "HOST")]
    host: Option<String>,
    /// Where tokens for Fabric and Azure resources come from (default: `[tokens] source` in
    /// rayfin-local.toml, else auto).
    #[arg(long, value_enum)]
    token_source: Option<TokenSourceArg>,
    /// Entra tenant for tokens (default: your az/Rayfin login's tenant).
    #[arg(long)]
    tenant: Option<String>,
    /// Apply destructive schema changes (dropped or retyped fields lose their data).
    #[arg(long)]
    force_schema: bool,
    /// Start from an empty database: delete the users, entity data and stored files (keeps the
    /// ports, publishable key, keys, secrets and caches in .rayfin-local).
    #[arg(long)]
    reset: bool,
    /// Start the function host with the Node inspector (default port 9229).
    #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "9229")]
    inspect: Option<u16>,
    /// Open the app in the browser once it is ready.
    #[arg(long)]
    open: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum TokenSourceArg {
    /// The Azure CLI, then the Rayfin CLI's login.
    Auto,
    /// Only `az account get-access-token`.
    Az,
    /// Only the Rayfin CLI's login (`rayfin login`).
    Rayfin,
}

impl From<TokenSourceArg> for TokenMode {
    fn from(arg: TokenSourceArg) -> Self {
        match arg {
            TokenSourceArg::Auto => TokenMode::Auto,
            TokenSourceArg::Az => TokenMode::Az,
            TokenSourceArg::Rayfin => TokenMode::Rayfin,
        }
    }
}

impl ServeArgs {
    fn into_options(self, verbose: bool) -> ServeOptions {
        ServeOptions {
            port: self.port,
            frontend_port: self.frontend_port,
            static_build: self.static_build,
            no_frontend: self.no_frontend,
            no_functions: self.no_functions,
            host: self.host,
            token_source: self.token_source.map(TokenMode::from),
            tenant: self.tenant,
            force_schema: self.force_schema,
            reset: self.reset,
            inspect: self.inspect,
            open: self.open,
            verbose,
        }
    }
}

/// The crates whose logs `--verbose` shows at debug level.
const OWN_CRATES: [&str; 13] = [
    "rayfin_local",
    "rl_project",
    "rl_node",
    "rl_model",
    "rl_db",
    "rl_graphql",
    "rl_auth",
    "rl_tokens",
    "rl_functions",
    "rl_connectors",
    "rl_storage",
    "rl_frontend",
    "rl_server",
];

fn main() -> ExitCode {
    let cli = Cli::parse();
    ui::init();
    init_tracing(cli.verbose);
    let _ = rustls::crypto::ring::default_provider().install_default();

    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            ui::error(&err);
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    rl_node::init_process_supervision()?;
    let dir = project_dir(cli.project.as_deref())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async move {
        match cli.command {
            Command::Serve(args) => {
                commands::serve::run(&dir, args.into_options(cli.verbose)).await
            }
            Command::Reset { yes } => commands::reset::run(&dir, yes),
            Command::Doctor => commands::doctor::run(&dir).await,
            Command::Schema => commands::schema::run(&dir, cli.verbose).await,
            Command::Users(command) => commands::users::run(&dir, command).await,
            Command::Secret(command) => commands::secret::run(&dir, command),
        }
    });
    // Don't let a blocking task that is still running (a database call, a child's output
    // reader) hold up the exit.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

/// `-C` (relative to the current directory), else the current directory.
fn project_dir(project: Option<&Path>) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()
        .map_err(|err| anyhow::anyhow!("Could not read the current directory: {err}"))?;
    Ok(match project {
        Some(dir) => cwd.join(dir),
        None => cwd,
    })
}

/// Logs go to stderr in the style of the status lines (see [`LogFormat`]): `info` for
/// rayfin-local, `warn` for libraries; `--verbose` shows rayfin-local's debug logs, and
/// `RUST_LOG` replaces the defaults.
fn init_tracing(verbose: bool) {
    let rust_log = std::env::var("RUST_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let mut directives = rust_log.unwrap_or_else(|| {
        let own = OWN_CRATES.map(|krate| format!("{krate}=info")).join(",");
        format!("warn,{own}")
    });
    if verbose {
        for krate in OWN_CRATES {
            directives.push_str(&format!(",{krate}=debug"));
        }
    }
    let filter = EnvFilter::try_new(&directives).unwrap_or_else(|err| {
        ui::warn(format_args!("ignoring RUST_LOG: {err}"));
        EnvFilter::new("warn,rayfin_local=info")
    });
    tracing_subscriber::fmt()
        .with_ansi(ui::stderr_colors())
        .event_format(LogFormat)
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
}

/// Log lines that look like the status lines around them: info as a plain line, warnings and
/// errors as [`ui::warn`] and [`ui::error`] print them, and the other levels prefixed with their
/// level and spans.
struct LogFormat;

impl<S, N> FormatEvent<S, N> for LogFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let painter = ui::Painter::new(writer.has_ansi_escapes());
        match *event.metadata().level() {
            Level::ERROR => write!(writer, "{} ", painter.paint(ui::Color::Red, "error:"))?,
            Level::WARN => write!(writer, "{} ", painter.paint(ui::Color::Yellow, "warning:"))?,
            Level::INFO => {}
            level => {
                let label = format!("{}:", level.as_str().to_ascii_lowercase());
                write!(writer, "{} ", painter.paint(ui::Color::Dim, label))?;
                for span in ctx
                    .event_scope()
                    .into_iter()
                    .flat_map(|scope| scope.from_root())
                {
                    write!(writer, "{}", span.name())?;
                    let extensions = span.extensions();
                    if let Some(fields) = extensions.get::<FormattedFields<N>>()
                        && !fields.is_empty()
                    {
                        write!(writer, "{{{fields}}}")?;
                    }
                    write!(writer, ": ")?;
                }
            }
        }
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn logs_look_like_the_status_lines() {
        let output = Output::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(LogFormat)
            .with_writer(move || writer.clone())
            .with_max_level(Level::DEBUG)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("building the functions");
            tracing::warn!(port = 7171, "port in use");
            let _request = tracing::debug_span!("http", status = 200).entered();
            tracing::debug!("answered");
        });
        let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            text,
            "building the functions\nwarning: port in use port=7171\ndebug: http{status=200}: \
             answered\n"
        );
    }
}
