//! Finding and stopping processes that listen on TCP ports.
//!
//! Uses `lsof` and `ps`, which ship with macOS and nearly every Linux distro.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use crate::{
    demo,
    fmt::{self, Paint},
};

/// How long a process gets to shut down after SIGTERM before it gets SIGKILL.
const GRACE: Duration = Duration::from_secs(3);

/// A process listening on a TCP port.
#[derive(Debug, Clone, PartialEq)]
pub struct Listener {
    pub port: u16,
    pub pid: u32,
    pub name: String,
    pub addresses: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub command: String,
    /// Seconds since the process started.
    pub uptime: Option<u64>,
    /// Resident memory in bytes.
    pub memory: Option<u64>,
    /// The Docker container publishing this port, when the listener is Docker's proxy.
    pub container: Option<String>,
}

impl Listener {
    /// Daemons and GUI apps run from `/`; dev servers run from a project folder.
    pub fn is_dev(&self) -> bool {
        self.cwd.as_deref().is_some_and(|cwd| cwd != Path::new("/"))
    }

    /// The project folder, left blank for a container: the process is Docker's own.
    pub fn folder(&self) -> String {
        match (&self.container, &self.cwd) {
            (None, Some(cwd)) => fmt::path(cwd),
            _ => String::new(),
        }
    }

    /// Uptime and memory, left blank for a container: they would be Docker's own.
    pub fn stats(&self) -> (String, String) {
        if self.container.is_some() {
            return (String::new(), String::new());
        }
        (
            self.uptime.map(fmt::duration).unwrap_or_default(),
            self.memory.map(fmt::size).unwrap_or_default(),
        )
    }

    /// The process name, or the container's for a port Docker publishes.
    pub fn label(&self) -> String {
        match &self.container {
            Some(container) => format!("{container} (docker)"),
            None => self.name.clone(),
        }
    }

    /// Whether this is the process Docker publishes container ports through
    /// (Docker Desktop's backend, docker-proxy, OrbStack, Lima).
    fn is_docker(&self) -> bool {
        let name = self.name.to_lowercase();
        ["docker", "vpnkit", "orbstack", "limactl"]
            .iter()
            .any(|engine| name.contains(engine))
    }
}

/// How a process went away.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    /// Exited after SIGTERM (or was already gone).
    Stopped,
    /// Needed SIGKILL.
    Forced,
}

/// All TCP listeners visible to the current user, sorted by port.
pub fn list() -> Result<Vec<Listener>> {
    if demo::enabled() {
        return Ok(demo::listeners());
    }
    let mut listeners = parse_lsof(&run(
        "lsof",
        &["-nP", "+c", "0", "-iTCP", "-sTCP:LISTEN", "-Fpcn"],
    )?);
    if listeners.is_empty() {
        return Ok(listeners);
    }

    let mut pids: Vec<u32> = listeners.iter().map(|l| l.pid).collect();
    pids.sort_unstable();
    pids.dedup();
    let pids = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let cwds = parse_cwds(&run("lsof", &["-a", "-d", "cwd", "-p", &pids, "-Fpn"])?);
    let stats = parse_ps(&run(
        "ps",
        &["-o", "pid=,etime=,rss=,command=", "-p", &pids],
    )?);

    let containers = if listeners.iter().any(Listener::is_docker) {
        // Without Docker installed the ports still list, just without container names.
        run("docker", &["ps", "--format", "{{.Names}}\t{{.Ports}}"])
            .map(|output| parse_docker_ps(&output))
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    for listener in &mut listeners {
        listener.cwd = cwds.get(&listener.pid).cloned();
        if let Some(stats) = stats.get(&listener.pid) {
            listener.uptime = stats.uptime;
            listener.memory = Some(stats.memory);
            listener.command.clone_from(&stats.command);
        }
        if listener.is_docker() {
            listener.container = containers.get(&listener.port).cloned();
        }
    }
    Ok(listeners)
}

/// Stops whatever holds a port. A port Docker publishes is freed by stopping its
/// container: killing Docker's own process would take every container down with it.
pub fn stop_listener(listener: &Listener, force: bool) -> Result<Outcome> {
    if demo::enabled() || !listener.is_docker() {
        return stop(listener.pid, force);
    }
    let Some(container) = &listener.container else {
        bail!("held by Docker, but no running container publishes this port");
    };
    let output = Command::new("docker")
        .args(["stop", container])
        .output()
        .context("could not run `docker`")?;
    if !output.status.success() {
        bail!(
            "docker stop {container} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(Outcome::Stopped)
}

/// Stops `pid`: SIGTERM, then SIGKILL if it's still around after a grace period.
/// With `force`, goes straight to SIGKILL.
pub fn stop(pid: u32, force: bool) -> Result<Outcome> {
    if pid <= 1 || pid == std::process::id() {
        bail!("refusing to stop pid {pid}");
    }
    if demo::enabled() {
        return Ok(demo::stop(pid, force));
    }
    if !force && (!signal(pid, libc::SIGTERM)? || wait_for_exit(pid, GRACE)) {
        return Ok(Outcome::Stopped);
    }
    if !signal(pid, libc::SIGKILL)? {
        return Ok(Outcome::Stopped);
    }
    if wait_for_exit(pid, Duration::from_secs(2)) {
        Ok(Outcome::Forced)
    } else {
        bail!("pid {pid} is still running after SIGKILL")
    }
}

/// Sends `sig` to `pid`. Returns `false` if the process no longer exists.
fn signal(pid: u32, sig: libc::c_int) -> Result<bool> {
    // SAFETY: kill(2) has no memory-safety preconditions.
    if unsafe { libc::kill(pid as libc::pid_t, sig) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(err).with_context(|| format!("could not signal pid {pid}"))
}

fn is_running(pid: u32) -> bool {
    // Signal 0 checks for existence without sending anything.
    signal(pid, 0).unwrap_or(true)
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_running(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !is_running(pid)
}

/// `tidyup ports`
pub fn print(all: bool) -> Result<()> {
    let listeners = list()?;
    let (shown, hidden): (Vec<_>, Vec<_>) = listeners.iter().partition(|l| all || l.is_dev());

    if shown.is_empty() {
        println!("Nothing is listening.");
    } else {
        let name_width = shown
            .iter()
            .map(|l| l.label().len())
            .max()
            .unwrap_or(0)
            .clamp(7, 32);
        println!(
            "{}",
            format!(
                "{:<6} {:<name_width$} {:>7} {:>8} {:>8}  {}",
                "PORT", "PROCESS", "PID", "UPTIME", "MEM", "FOLDER"
            )
            .dim()
        );
        for l in &shown {
            println!(
                "{} {:<name_width$} {:>7} {:>8} {:>8}  {}",
                format!("{:<6}", l.port).cyan().bold(),
                truncate(&l.label(), name_width),
                l.pid,
                l.stats().0,
                l.stats().1,
                l.folder().dim(),
            );
        }
    }

    if !hidden.is_empty() {
        let plural = if hidden.len() == 1 { "" } else { "es" };
        println!(
            "{}",
            format!(
                "{} system process{plural} hidden, show with --all",
                hidden.len()
            )
            .dim()
        );
    }
    Ok(())
}

/// `tidyup kill <ports>`
pub fn kill(ports: &[u16], force: bool) -> Result<()> {
    let listeners = list()?;
    let mut failed = false;

    for &port in ports {
        let holders: Vec<_> = listeners.iter().filter(|l| l.port == port).collect();
        if holders.is_empty() {
            println!("{} :{port} nothing listening", "·".dim());
        }
        for l in holders {
            // A container's pid would be Docker's own process, so it's left out.
            let who = match l.container {
                Some(_) => l.label(),
                None => format!("{} (pid {})", l.label(), l.pid),
            };
            match stop_listener(l, force) {
                Ok(Outcome::Stopped) => println!("{} :{port} stopped {who}", "✔".green()),
                Ok(Outcome::Forced) => println!("{} :{port} force-killed {who}", "✔".yellow()),
                Err(err) => {
                    failed = true;
                    eprintln!("{} :{port} {who}: {err:#}", "✘".red());
                }
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut short: String = text.chars().take(width - 1).collect();
    short.push('…');
    short
}

/// Runs a command and returns its stdout. Exit status is ignored: lsof exits 1 when nothing matches.
fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("could not run `{program}`, is it installed?"))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Iterates `lsof -F` output as `(field tag, value)` pairs.
fn lsof_fields(output: &str) -> impl Iterator<Item = (char, &str)> {
    output.lines().filter_map(|line| {
        let tag = line.chars().next()?;
        Some((tag, &line[tag.len_utf8()..]))
    })
}

fn parse_lsof(output: &str) -> Vec<Listener> {
    let mut found: BTreeMap<(u16, u32), Listener> = BTreeMap::new();
    let (mut pid, mut name) = (0, String::new());

    for (tag, value) in lsof_fields(output) {
        match tag {
            'p' => pid = value.parse().unwrap_or(0),
            'c' => name = value.to_string(),
            'n' => {
                let Some((address, port)) = value.rsplit_once(':') else {
                    continue;
                };
                let Ok(port) = port.parse() else { continue };
                let listener = found.entry((port, pid)).or_insert_with(|| Listener {
                    port,
                    pid,
                    name: name.clone(),
                    addresses: Vec::new(),
                    cwd: None,
                    command: name.clone(),
                    uptime: None,
                    memory: None,
                    container: None,
                });
                if !listener.addresses.iter().any(|a| a == address) {
                    listener.addresses.push(address.to_string());
                }
            }
            _ => {}
        }
    }
    found.into_values().collect()
}

fn parse_cwds(output: &str) -> HashMap<u32, PathBuf> {
    let mut cwds = HashMap::new();
    let mut pid = 0;
    for (tag, value) in lsof_fields(output) {
        match tag {
            'p' => pid = value.parse().unwrap_or(0),
            'n' => {
                cwds.insert(pid, PathBuf::from(value));
            }
            _ => {}
        }
    }
    cwds
}

/// Maps published host ports to container names, from
/// `docker ps --format '{{.Names}}\t{{.Ports}}'`.
fn parse_docker_ps(output: &str) -> HashMap<u16, String> {
    let mut containers = HashMap::new();
    for line in output.lines() {
        let Some((name, mappings)) = line.split_once('\t') else {
            continue;
        };
        // e.g. "0.0.0.0:5435->5432/tcp, [::]:5435->5432/tcp, 0.0.0.0:8000-8001->8000-8001/tcp"
        for mapping in mappings.split(", ") {
            let Some((host, _)) = mapping.split_once("->") else {
                continue;
            };
            let Some((_, ports)) = host.rsplit_once(':') else {
                continue;
            };
            let (first, last) = ports.split_once('-').unwrap_or((ports, ports));
            let (Ok(first), Ok(last)) = (first.parse::<u16>(), last.parse::<u16>()) else {
                continue;
            };
            for port in first..=last {
                containers.insert(port, name.to_string());
            }
        }
    }
    containers
}

struct Stats {
    uptime: Option<u64>,
    memory: u64,
    command: String,
}

/// Parses `ps -o pid=,etime=,rss=,command=`.
fn parse_ps(output: &str) -> HashMap<u32, Stats> {
    output
        .lines()
        .filter_map(|line| {
            let (pid, rest) = next_word(line);
            let (etime, rest) = next_word(rest);
            let (rss, command) = next_word(rest);
            let stats = Stats {
                uptime: parse_etime(etime),
                memory: rss.parse::<u64>().ok()? * 1024,
                command: command.to_string(),
            };
            Some((pid.parse().ok()?, stats))
        })
        .collect()
}

fn next_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    match text.find(char::is_whitespace) {
        Some(end) => (&text[..end], text[end..].trim_start()),
        None => (text, ""),
    }
}

/// Parses ps elapsed time, `[[dd-]hh:]mm:ss`, into seconds.
fn parse_etime(etime: &str) -> Option<u64> {
    let (days, clock) = match etime.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, etime),
    };
    let mut secs = 0;
    for part in clock.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(days * 86_400 + secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_and_merges_ipv4_and_ipv6() {
        let output = "p123\ncnode\nf20\nn*:3000\nf21\nn[::1]:3000\nf22\nn127.0.0.1:9229\np456\ncpostgres\nf5\nn127.0.0.1:5432\n";
        let listeners = parse_lsof(output);
        let summary: Vec<_> = listeners
            .iter()
            .map(|l| (l.port, l.pid, l.name.as_str(), l.addresses.len()))
            .collect();
        assert_eq!(
            summary,
            [
                (3000, 123, "node", 2),
                (5432, 456, "postgres", 1),
                (9229, 123, "node", 1)
            ]
        );
    }

    #[test]
    fn parses_cwds() {
        let cwds = parse_cwds("p123\nfcwd\nn/Users/me/app\np456\nfcwd\nn/\n");
        assert_eq!(cwds[&123], PathBuf::from("/Users/me/app"));
        assert_eq!(cwds[&456], PathBuf::from("/"));
    }

    #[test]
    fn parses_ps() {
        let stats = parse_ps("  123    01:02:03  20480 node /app/node_modules/.bin/next dev\n");
        let node = &stats[&123];
        assert_eq!(node.uptime, Some(3723));
        assert_eq!(node.memory, 20480 * 1024);
        assert_eq!(node.command, "node /app/node_modules/.bin/next dev");
    }

    #[test]
    fn maps_docker_ports_to_containers() {
        let output = "acme-db\t0.0.0.0:5435->5432/tcp, [::]:5435->5432/tcp\n\
                      cache\t0.0.0.0:6380->6379/tcp\n\
                      worker\t\n\
                      api\t0.0.0.0:8000-8001->8000-8001/tcp, 9000/tcp\n";
        let containers = parse_docker_ps(output);
        assert_eq!(containers[&5435], "acme-db");
        assert_eq!(containers[&6380], "cache");
        assert_eq!(containers[&8000], "api");
        assert_eq!(containers[&8001], "api");
        assert_eq!(containers.len(), 4);
    }

    #[test]
    fn parses_etime() {
        assert_eq!(parse_etime("00:05"), Some(5));
        assert_eq!(parse_etime("12:34"), Some(754));
        assert_eq!(
            parse_etime("2-03:04:05"),
            Some(2 * 86_400 + 3 * 3600 + 4 * 60 + 5)
        );
        assert_eq!(parse_etime("garbage"), None);
    }

    #[test]
    fn system_processes_are_not_dev() {
        let mut listener = parse_lsof("p1\ncx\nn*:80\n").remove(0);
        assert!(!listener.is_dev());
        listener.cwd = Some("/".into());
        assert!(!listener.is_dev());
        listener.cwd = Some("/Users/me/app".into());
        assert!(listener.is_dev());
    }
}
