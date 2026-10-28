//! `rayfin-local`: run a Rayfin app and its whole backend on this machine.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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
    /// Interface to listen on. Anything but loopback exposes your Azure identity to the network.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Where tokens for Fabric and Azure resources come from.
    #[arg(long, value_enum, default_value_t = TokenSourceArg::Auto)]
    token_source: TokenSourceArg,
    /// Entra tenant for tokens (default: your az/Rayfin login's tenant).
    #[arg(long)]
    tenant: Option<String>,
    /// Apply destructive schema changes (dropped or retyped fields lose their data).
    #[arg(long)]
    force_schema: bool,
    /// Start from an empty database.
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _ = (&cli.project, cli.verbose);
    match cli.command {
        Command::Serve(args) => {
            let _ = (
                args.port,
                args.frontend_port,
                args.static_build,
                args.no_frontend,
                args.no_functions,
                args.host,
                args.token_source,
                args.tenant,
                args.force_schema,
                args.reset,
                args.inspect,
                args.open,
            );
            anyhow::bail!("`serve` is not implemented yet")
        }
        Command::Reset { yes } => {
            let _ = yes;
            anyhow::bail!("`reset` is not implemented yet")
        }
        Command::Doctor => anyhow::bail!("`doctor` is not implemented yet"),
        Command::Schema => anyhow::bail!("`schema` is not implemented yet"),
    }
}
