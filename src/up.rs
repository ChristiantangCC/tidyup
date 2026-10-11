//! `tidyup up`: start a project's dev services from `tidyup.toml`.

use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::style::Color;
use serde::Deserialize;

use crate::{
    fmt::{self, Paint},
    ports,
};

const CONFIG: &str = "tidyup.toml";
const COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Green,
    Color::Blue,
    Color::Red,
];
/// How long services get to shut down after ctrl-c before they're killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default, rename = "service")]
    services: Vec<Service>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Service {
    name: String,
    cmd: String,
    /// Working folder, relative to the config file.
    cwd: Option<PathBuf>,
    /// Port the service listens on; whatever holds it is stopped first.
    port: Option<u16>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

enum Event {
    Exited { name: String, code: Option<i32> },
    Interrupt,
}

/// `tidyup up [services]`
pub fn run(only: &[String]) -> Result<()> {
    let path = find_config()?;
    let root = path.parent().unwrap_or(Path::new("."));
    let config: Config = toml::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("{} is not valid", fmt::path(&path)))?;
    let services = select(config.services, only)?;

    free_ports(&services)?;

    let (tx, rx) = mpsc::channel();
    let interrupt = tx.clone();
    ctrlc::set_handler(move || {
        let _ = interrupt.send(Event::Interrupt);
    })?;

    let width = services.iter().map(|s| s.name.len()).max().unwrap_or(0);
    let mut groups = Vec::new();
    for (i, service) in services.iter().enumerate() {
        let prefix = format!("{:<width$} │ ", service.name)
            .with(COLORS[i % COLORS.len()])
            .to_string();
        match spawn(service, root, prefix, tx.clone()) {
            Ok(pid) => groups.push(pid),
            Err(err) => {
                signal_groups(&groups, libc::SIGTERM);
                return Err(err);
            }
        }
    }

    let mut running = groups.len();
    let mut deadline: Option<Instant> = None;
    while running > 0 {
        let event = match deadline {
            None => rx.recv()?,
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!("{}", "Some services didn't stop, killing them".dim());
                    signal_groups(&groups, libc::SIGKILL);
                    deadline = Some(Instant::now() + Duration::from_secs(3600));
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        match event {
            Event::Exited { name, code } => {
                running -= 1;
                let how = match code {
                    Some(0) => "exited".to_string(),
                    Some(code) => format!("exited with code {code}"),
                    None => "stopped".to_string(),
                };
                eprintln!("{} {how}", name.bold());
            }
            Event::Interrupt if deadline.is_none() => {
                eprintln!("\n{}", "Stopping… (ctrl-c again to force)".dim());
                signal_groups(&groups, libc::SIGTERM);
                deadline = Some(Instant::now() + STOP_TIMEOUT);
            }
            Event::Interrupt => signal_groups(&groups, libc::SIGKILL),
        }
    }
    Ok(())
}

/// `tidyup up --init`
pub fn init() -> Result<()> {
    let path = Path::new(CONFIG);
    if path.exists() {
        bail!("{CONFIG} already exists");
    }
    fs::write(path, template(package_manager()))?;
    println!("Created {CONFIG}. Edit it, then run `tidyup up`.");
    Ok(())
}

fn find_config() -> Result<PathBuf> {
    env::current_dir()?
        .ancestors()
        .map(|dir| dir.join(CONFIG))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            anyhow!("no {CONFIG} here or in any parent folder. Create one with `tidyup up --init`")
        })
}

fn select(services: Vec<Service>, only: &[String]) -> Result<Vec<Service>> {
    if services.is_empty() {
        bail!("{CONFIG} has no [[service]] entries");
    }
    if let Some(unknown) = only
        .iter()
        .find(|name| !services.iter().any(|s| &s.name == *name))
    {
        bail!("{CONFIG} has no service called '{unknown}'");
    }
    Ok(services
        .into_iter()
        .filter(|s| only.is_empty() || only.contains(&s.name))
        .collect())
}

fn free_ports(services: &[Service]) -> Result<()> {
    let wanted: Vec<u16> = services.iter().filter_map(|s| s.port).collect();
    if wanted.is_empty() {
        return Ok(());
    }
    for holder in ports::list()?
        .into_iter()
        .filter(|l| wanted.contains(&l.port))
    {
        ports::stop_listener(&holder, false).with_context(|| {
            format!(
                "port {} is held by {} (pid {})",
                holder.port,
                holder.label(),
                holder.pid
            )
        })?;
        eprintln!(
            "{} freed :{} (stopped {}, pid {})",
            "✔".green(),
            holder.port,
            holder.label(),
            holder.pid
        );
    }
    Ok(())
}

/// Starts a service in its own process group, so stopping it also stops anything it spawned
/// (`npm run dev` → node → esbuild, …). Returns the group id.
fn spawn(service: &Service, root: &Path, prefix: String, events: Sender<Event>) -> Result<u32> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&service.cmd)
        .current_dir(root.join(service.cwd.as_deref().unwrap_or(Path::new("."))))
        .envs(&service.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if io::stdout().is_terminal() {
        // Output goes through a pipe, so tools would otherwise drop their colors.
        command.env("FORCE_COLOR", "1").env("CLICOLOR_FORCE", "1");
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {}", service.name))?;

    if let Some(stdout) = child.stdout.take() {
        forward(stdout, prefix.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward(stderr, prefix);
    }

    let pid = child.id();
    let name = service.name.clone();
    thread::spawn(move || {
        let code = child.wait().ok().and_then(|status| status.code());
        let _ = events.send(Event::Exited { name, code });
    });
    Ok(pid)
}

/// Copies a stream to stdout line by line, with the service's prefix.
fn forward(stream: impl Read + Send + 'static, prefix: String) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
            let text = String::from_utf8_lossy(&line);
            let mut out = io::stdout().lock();
            let _ = write!(out, "{prefix}{text}");
            if !text.ends_with('\n') {
                let _ = writeln!(out);
            }
            line.clear();
        }
    });
}

fn signal_groups(groups: &[u32], signal: libc::c_int) {
    for &group in groups {
        // SAFETY: kill(2) has no memory-safety preconditions. A negative pid targets the group;
        // groups that already exited return ESRCH, which is fine to ignore.
        unsafe { libc::kill(-(group as libc::pid_t), signal) };
    }
}

fn package_manager() -> &'static str {
    [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
    ]
    .into_iter()
    .find(|(lockfile, _)| Path::new(lockfile).exists())
    .map_or("npm", |(_, manager)| manager)
}

fn template(manager: &str) -> String {
    format!(
        r#"# Services started by `tidyup up`. ctrl-c stops them all.
# `tidyup up web` starts only the service called "web".

[[service]]
name = "web"
cmd = "{manager} run dev"
port = 3000                   # optional: whatever holds this port is stopped first

# [[service]]
# name = "worker"
# cmd = "{manager} run worker"
# cwd = "packages/worker"     # optional: relative to this file
# env = {{ LOG_LEVEL = "debug" }}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses() {
        let config: Config = toml::from_str(&template("pnpm")).unwrap();
        assert_eq!(config.services.len(), 1);
        assert_eq!(config.services[0].cmd, "pnpm run dev");
        assert_eq!(config.services[0].port, Some(3000));
    }

    #[test]
    fn selects_services_by_name() {
        let config: Config = toml::from_str(
            "[[service]]\nname = \"a\"\ncmd = \"x\"\n[[service]]\nname = \"b\"\ncmd = \"y\"\n",
        )
        .unwrap();
        let picked = select(config.services, &["b".to_string()]).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, "b");

        let config: Config = toml::from_str("[[service]]\nname = \"a\"\ncmd = \"x\"\n").unwrap();
        assert!(select(config.services, &["nope".to_string()]).is_err());
    }
}
