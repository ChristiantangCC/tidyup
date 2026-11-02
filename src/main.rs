mod caches;
mod clean;
mod demo;
mod fmt;
mod ports;
mod progress;
mod receipt;
mod strays;
mod tui;
mod up;

use std::{env, path::PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};

/// Free up ports, disk space and CPU from your dev projects.
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
        /// Also list global caches: Xcode, npm, pnpm, Yarn, Cargo, Go, Gradle, pip, Homebrew, Docker, …
        #[arg(short, long)]
        caches: bool,
        /// Move folders to the Trash instead of deleting them
        #[arg(long)]
        trash: bool,
        /// List what would be deleted, delete nothing
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Delete everything found without asking
        #[arg(short, long, conflicts_with = "dry_run")]
        yes: bool,
    },
    /// Find leftover dev processes (orphaned build workers, watchers, dev servers) and stop them
    Strays {
        /// Pick which to stop, then stop them
        #[arg(short, long)]
        stop: bool,
        /// With --stop: stop everything listed without asking
        #[arg(short, long, requires = "stop")]
        yes: bool,
        /// With --stop: send SIGKILL straight away instead of SIGTERM first
        #[arg(short, long, requires = "stop")]
        force: bool,
        /// Only groups using at least this much CPU, in % of one core (100 = a full core)
        #[arg(long, value_name = "PERCENT")]
        cpu_above: Option<f32>,
        /// Only groups running at least this long: 45s, 30m, 2h, 1d (a plain number is minutes)
        #[arg(long, value_name = "AGE", value_parser = parse_age)]
        older_than: Option<u64>,
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
            caches,
            trash,
            dry_run,
            yes,
        }) => clean::run(clean::Options {
            root: folder(path)?,
            older_than,
            caches,
            trash,
            dry_run,
            yes,
        }),
        Some(Command::Strays {
            stop,
            yes,
            force,
            cpu_above,
            older_than,
        }) => strays::run_cli(strays::Options {
            stop,
            yes,
            force,
            cpu_above,
            older_than,
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

/// Parses an age like `45s`, `30m`, `2h` or `1d` into seconds; a plain number is minutes.
fn parse_age(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, "m"),
    };
    let number: u64 = number
        .parse()
        .map_err(|_| format!("'{text}' is not an age like 30m, 2h or 1d"))?;
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("'{text}': use s, m, h or d, e.g. 30m")),
    };
    Ok(number * unit)
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

#[cfg(test)]
mod tests {
    use super::parse_age;

    #[test]
    fn parses_ages() {
        assert_eq!(parse_age("45s"), Ok(45));
        assert_eq!(parse_age("30m"), Ok(1800));
        assert_eq!(parse_age("30"), Ok(1800));
        assert_eq!(parse_age("2h"), Ok(7200));
        assert_eq!(parse_age("1d"), Ok(86_400));
        assert!(parse_age("2w").is_err());
        assert!(parse_age("soon").is_err());
    }
}
