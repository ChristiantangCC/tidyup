mod clean;
mod fmt;
mod ports;
mod progress;
mod tui;
mod up;

use std::{env, path::PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};

/// Free up ports and disk space from your dev projects.
///
/// Run without arguments for the interactive dashboard.
#[derive(Parser)]
#[command(name = "tidyup", version, about, long_about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List processes listening on TCP ports
    #[command(visible_alias = "ls")]
    Ports {
        /// Include system processes (anything running from /)
        #[arg(short, long)]
        all: bool,
    },
    /// Stop whatever is listening on the given ports (also: `tidyup 3000`)
    Kill {
        #[arg(required = true)]
        ports: Vec<u16>,
        /// Send SIGKILL straight away instead of SIGTERM first
        #[arg(short, long)]
        force: bool,
    },
    /// Find dependency and build folders (node_modules, target, …) to delete
    Clean {
        /// Folder to scan [default: current folder]
        path: Option<PathBuf>,
        /// Only include projects untouched for this many days
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
        /// List what would be deleted, delete nothing
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Delete everything found without asking
        #[arg(short, long, conflicts_with = "dry_run")]
        yes: bool,
    },
    /// Start the services in tidyup.toml, freeing their ports first
    Up {
        /// Only start these services
        services: Vec<String>,
        /// Create a tidyup.toml in the current folder
        #[arg(long, conflicts_with = "services")]
        init: bool,
    },
    /// Open the interactive dashboard (the default)
    Ui {
        /// Folder the Clean tab scans [default: current folder]
        path: Option<PathBuf>,
    },
    #[command(external_subcommand)]
    Shorthand(Vec<String>),
}

fn main() -> Result<()> {
    match Cli::parse().command {
        None => tui::run(folder(None)?),
        Some(Command::Ui { path }) => tui::run(folder(path)?),
        Some(Command::Ports { all }) => ports::print(all),
        Some(Command::Kill { ports, force }) => ports::kill(&ports, force),
        Some(Command::Clean {
            path,
            older_than,
            dry_run,
            yes,
        }) => clean::run(clean::Options {
            root: folder(path)?,
            older_than,
            dry_run,
            yes,
        }),
        Some(Command::Up { init: true, .. }) => up::init(),
        Some(Command::Up { services, .. }) => up::run(&services),
        Some(Command::Shorthand(args)) => {
            let ports = args
                .iter()
                .map(|arg| arg.parse())
                .collect::<Result<Vec<u16>, _>>()
                .map_err(|_| anyhow!("unknown command '{}' (see `tidyup --help`)", args[0]))?;
            ports::kill(&ports, false)
        }
    }
}

/// Resolves an optional folder argument to an absolute path, defaulting to the current folder.
fn folder(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match path {
        Some(path) => path,
        None => env::current_dir().context("could not read the current folder")?,
    };
    if !path.is_dir() {
        bail!("{} is not a folder", path.display());
    }
    path.canonicalize()
        .with_context(|| format!("could not open {}", path.display()))
}
