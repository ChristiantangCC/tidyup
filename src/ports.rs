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

use crate::fmt::{self, Paint};

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
}

impl Listener {
    /// Daemons and GUI apps run from `/`; dev servers run from a project folder.
    pub fn is_dev(&self) -> bool {
        self.cwd.as_deref().is_some_and(|cwd| cwd != Path::new("/"))
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

    for listener in &mut listeners {
        listener.cwd = cwds.get(&listener.pid).cloned();
        if let Some(stats) = stats.get(&listener.pid) {
            listener.uptime = stats.uptime;
            listener.memory = Some(stats.memory);
            listener.command.clone_from(&stats.command);
        }
    }
    Ok(listeners)
}

/// Stops `pid`: SIGTERM, then SIGKILL if it's still around after a grace period.
/// With `force`, goes straight to SIGKILL.
pub fn stop(pid: u32, force: bool) -> Result<Outcome> {
    if pid <= 1 || pid == std::process::id() {
        bail!("refusing to stop pid {pid}");
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
            .map(|l| l.name.len())
            .max()
            .unwrap_or(0)
            .clamp(7, 24);
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
                truncate(&l.name, name_width),
                l.pid,
                l.uptime.map(fmt::duration).unwrap_or_default(),
                l.memory.map(fmt::size).unwrap_or_default(),
                l.cwd.as_deref().map(fmt::path).unwrap_or_default().dim(),
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
            match stop(l.pid, force) {
                Ok(Outcome::Stopped) => {
                    println!("{} :{port} stopped {} (pid {})", "✔".green(), l.name, l.pid)
                }
                Ok(Outcome::Forced) => {
                    println!(
                        "{} :{port} force-killed {} (pid {})",
                        "✔".yellow(),
                        l.name,
                        l.pid
                    )
                }
                Err(err) => {
                    failed = true;
                    eprintln!("{} :{port} {} (pid {}): {err:#}", "✘".red(), l.name, l.pid);
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
