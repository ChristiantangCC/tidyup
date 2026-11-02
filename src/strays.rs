//! Finding and stopping leftover dev processes: build workers, watchers and dev
//! servers whose parent is gone, still burning CPU long after you stopped caring.
//!
//! A cancelled `next build` can leave a dozen workers spinning for hours. They
//! listen on no port, so `tidyup ports` never shows them; this does.
//!
//! A process counts as a stray when all of these hold:
//! - it belongs to you;
//! - its parent is gone (it was re-parented to `launchd`/`init`, or to a
//!   `systemd --user` subreaper on Linux), or its parent is itself a stray;
//! - its command line is clearly dev tooling (see [`classify`]);
//! - it isn't an app bundle or a globally installed tool.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::IsTerminal,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use dialoguer::{Confirm, MultiSelect, theme::ColorfulTheme};
use rayon::prelude::*;

use crate::{
    demo,
    fmt::{self, Paint},
    ports::{self, Outcome, next_word, parse_cwds, parse_etime, run},
    receipt,
};

/// Above this much CPU (summed over the group, in % of one core) for at least
/// [`HOT_AFTER`] seconds, a group is flagged as the likely cause of a loud fan.
const HOT_CPU: f32 = 50.0;
const HOT_AFTER: u64 = 10 * 60;

/// One row of `ps`.
#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub uid: u32,
    /// % of one core, as `ps` reports it.
    pub cpu: f32,
    /// Resident memory in bytes.
    pub memory: u64,
    /// Seconds since the process started.
    pub uptime: u64,
    pub command: String,
}

/// Leftover processes from one build or tool run, stopped together.
#[derive(Debug, Clone, PartialEq)]
pub struct Stray {
    /// What most of them are, e.g. `Next.js build worker`.
    pub kind: &'static str,
    /// How many of [`Stray::pids`] are of that kind; the rest are other
    /// processes of the same run (a build's `esbuild` service, its `pnpm` parent).
    pub kind_count: usize,
    /// The project they ran for, when it can be told from the command or folder.
    pub project: Option<PathBuf>,
    /// The project folder no longer exists (a deleted checkout or temp folder).
    pub project_gone: bool,
    pub pids: Vec<u32>,
    /// Summed CPU, in % of one core.
    pub cpu: f32,
    /// Summed resident memory in bytes.
    pub memory: u64,
    /// The longest-running process's uptime, in seconds.
    pub uptime: u64,
}

impl Stray {
    /// `11 × Next.js build worker`, `2 × Next.js build worker +1`, or just the
    /// kind for a single process.
    pub fn label(&self) -> String {
        let others = self.pids.len() - self.kind_count;
        let main = match self.kind_count {
            1 => self.kind.to_string(),
            n => format!("{n} × {}", self.kind),
        };
        match others {
            0 => main,
            n => format!("{main} +{n}"),
        }
    }

    pub fn folder(&self) -> String {
        match &self.project {
            Some(project) if self.project_gone => format!("{} (deleted)", fmt::path(project)),
            Some(project) => fmt::path(project),
            None => String::new(),
        }
    }

    /// `3.5 cores` or `35%`.
    pub fn cpu_text(&self) -> String {
        cpu_text(self.cpu)
    }

    /// Busy for a while: probably what's spinning your fan.
    pub fn is_hot(&self) -> bool {
        self.cpu >= HOT_CPU && self.uptime >= HOT_AFTER
    }
}

/// How stopping a group went.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StopReport {
    pub stopped: usize,
    pub forced: usize,
    pub failed: Vec<String>,
}

/// All strays visible to the current user, hottest first.
pub fn list() -> Result<Vec<Stray>> {
    if demo::enabled() {
        return Ok(demo::strays());
    }
    let procs = parse_ps(&run(
        "ps",
        &["-Ao", "pid=,ppid=,pgid=,uid=,pcpu=,rss=,etime=,args="],
    )?);
    // SAFETY: getuid(2) has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let candidates = find(&procs, uid);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let pids = candidates
        .iter()
        .map(|p| p.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let cwds = parse_cwds(&run("lsof", &["-a", "-d", "cwd", "-p", &pids, "-Fpn"])?);
    Ok(group(&candidates, &cwds, |path| path.exists()))
}

/// Stops every process of a group: SIGTERM, then SIGKILL after a grace period
/// (build workers often ignore SIGTERM). With `force`, SIGKILL straight away.
pub fn stop(stray: &Stray, force: bool) -> StopReport {
    let results: Vec<(u32, Result<Outcome>)> = stray
        .pids
        .par_iter()
        .map(|&pid| (pid, ports::stop(pid, force)))
        .collect();
    let mut report = StopReport::default();
    for (pid, result) in results {
        match result {
            Ok(Outcome::Stopped) => report.stopped += 1,
            Ok(Outcome::Forced) => report.forced += 1,
            Err(err) => report.failed.push(format!("pid {pid}: {err:#}")),
        }
    }
    report
}

/// Options for `tidyup strays`.
pub struct Options {
    pub stop: bool,
    pub yes: bool,
    pub force: bool,
    /// Only groups using at least this much CPU, in % of one core.
    pub cpu_above: Option<f32>,
    /// Only groups running for at least this many seconds.
    pub older_than: Option<u64>,
}

/// `tidyup strays`
pub fn run_cli(options: Options) -> Result<()> {
    let Options {
        stop: stopping,
        yes,
        force,
        cpu_above,
        older_than,
    } = options;
    if stopping && !yes && !std::io::stdin().is_terminal() {
        bail!("not running in a terminal: pass --yes to stop everything listed");
    }

    let mut strays = list()?;
    strays.retain(|s| {
        cpu_above.is_none_or(|min| s.cpu >= min) && older_than.is_none_or(|min| s.uptime >= min)
    });
    if strays.is_empty() {
        println!("No leftover dev processes.");
        return Ok(());
    }

    let lines: Vec<String> = strays.iter().map(line).collect();
    let summary = summary(&strays);

    let chosen: Vec<&Stray> = if !stopping || yes {
        println!(
            "{}",
            format!(
                "  {:>10} {:>8} {:>8}  {:<28} {}",
                "CPU", "MEM", "RUNNING", "WHAT", "PROJECT"
            )
            .dim()
        );
        for line in &lines {
            println!("{line}");
        }
        println!("{}", summary.as_str().bold());
        if !stopping {
            println!("{}", "Stop them with `tidyup strays --stop`.".dim());
            return Ok(());
        }
        strays.iter().collect()
    } else {
        let picked = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Found {summary}. Space to pick, a for all, enter to continue"
            ))
            .items(&lines)
            .defaults(&strays.iter().map(Stray::is_hot).collect::<Vec<_>>())
            .max_length(20)
            .report(false)
            .interact_opt()?;
        let Some(picked) = picked else { return Ok(()) };
        picked.into_iter().map(|i| &strays[i]).collect()
    };

    if chosen.is_empty() {
        println!("Nothing picked.");
        return Ok(());
    }
    let count: usize = chosen.iter().map(|s| s.pids.len()).sum();
    if !yes
        && !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Stop {count} process{}?",
                if count == 1 { "" } else { "es" }
            ))
            .default(true)
            .interact()?
    {
        return Ok(());
    }

    let mut failed = false;
    let mut stopped: Vec<&Stray> = Vec::new();
    let mut forced = 0;
    for stray in chosen {
        let report = stop(stray, force);
        forced += report.forced;
        let what = format!("{} {}", stray.label(), stray.folder().dim());
        if report.failed.is_empty() {
            stopped.push(stray);
            let (mark, verb) = if report.forced > 0 {
                ("✔".yellow(), "force-killed")
            } else {
                ("✔".green(), "stopped")
            };
            println!("{mark} {verb} {what}");
        } else {
            failed = true;
            eprintln!("{} {what}: {}", "✘".red(), report.failed.join("; "));
        }
    }
    receipt::print(&receipt::strays(&stopped, forced));
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn line(stray: &Stray) -> String {
    let flag = if stray.is_hot() {
        "●".red()
    } else {
        " ".to_string()
    };
    format!(
        "{flag} {:>10} {:>8} {:>8}  {:<28} {}",
        stray.cpu_text(),
        fmt::size(stray.memory),
        fmt::duration(stray.uptime),
        ports::truncate(&stray.label(), 28),
        stray.folder().dim(),
    )
}

/// CPU in % of one core as text: `3.5 cores` from a full core up, else `35%`.
fn cpu_text(cpu: f32) -> String {
    if cpu >= 100.0 {
        format!("{:.1} cores", cpu / 100.0)
    } else {
        format!("{cpu:.0}%")
    }
}

/// `14 processes in 2 groups, using 3.6 cores`
pub fn summary(strays: &[Stray]) -> String {
    let processes: usize = strays.iter().map(|s| s.pids.len()).sum();
    let cpu: f32 = strays.iter().map(|s| s.cpu).sum();
    format!(
        "{processes} process{} in {} group{}, using {}",
        if processes == 1 { "" } else { "es" },
        strays.len(),
        if strays.len() == 1 { "" } else { "s" },
        cpu_text(cpu)
    )
}

/// Parses `ps -Ao pid=,ppid=,pgid=,uid=,pcpu=,rss=,etime=,args=`.
pub fn parse_ps(output: &str) -> Vec<Proc> {
    output
        .lines()
        .filter_map(|line| {
            let (pid, rest) = next_word(line);
            let (ppid, rest) = next_word(rest);
            let (pgid, rest) = next_word(rest);
            let (uid, rest) = next_word(rest);
            let (cpu, rest) = next_word(rest);
            let (rss, rest) = next_word(rest);
            let (etime, command) = next_word(rest);
            Some(Proc {
                pid: pid.parse().ok()?,
                ppid: ppid.parse().ok()?,
                pgid: pgid.parse().ok()?,
                uid: uid.parse().ok()?,
                // Some locales print `12,5`.
                cpu: cpu.replace(',', ".").parse().ok()?,
                memory: rss.parse::<u64>().ok()? * 1024,
                uptime: parse_etime(etime)?,
                command: command.to_string(),
            })
            .filter(|p| !p.command.is_empty())
        })
        .collect()
}

/// The strays among `procs`: orphaned dev processes owned by `uid`, plus every
/// process they started.
pub fn find(procs: &[Proc], uid: u32) -> Vec<Proc> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let reapers: HashSet<u32> = procs
        .iter()
        .filter(|p| p.pid == 1 || is_subreaper(&p.command))
        .map(|p| p.pid)
        .chain([1])
        .collect();
    let orphaned = |p: &Proc| reapers.contains(&p.ppid) || !by_pid.contains_key(&p.ppid);

    let mut strays: HashSet<u32> = procs
        .iter()
        .filter(|p| {
            p.uid == uid
                && p.pid != std::process::id()
                && orphaned(p)
                && !is_excluded(&p.command)
                && classify(&p.command).is_some()
        })
        .map(|p| p.pid)
        .collect();

    // Children of a stray are strays too, whatever they run (a stray `pnpm`'s
    // `node`, a build's `esbuild` service). Repeat until nothing new joins.
    loop {
        let before = strays.len();
        for p in procs {
            if p.uid == uid && strays.contains(&p.ppid) {
                strays.insert(p.pid);
            }
        }
        if strays.len() == before {
            break;
        }
    }

    let mut found: Vec<Proc> = strays
        .into_iter()
        .filter_map(|pid| by_pid.get(&pid).map(|p| (*p).clone()))
        .collect();
    found.sort_by_key(|p| p.pid);
    found
}

/// Groups strays that belong together: same process group (one build or tool
/// run), or, failing that, same project and kind. `exists` checks whether a
/// project folder is still there; it is a parameter so tests needn't touch disk.
pub fn group(
    strays: &[Proc],
    cwds: &HashMap<u32, PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Vec<Stray> {
    let mut groups: BTreeMap<(u32, Option<PathBuf>), Vec<&Proc>> = BTreeMap::new();
    for p in strays {
        let project = project_of(&p.command).or_else(|| {
            cwds.get(&p.pid)
                .filter(|cwd| cwd.as_path() != Path::new("/"))
                .cloned()
        });
        groups.entry((p.pgid, project)).or_default().push(p);
    }

    let mut result: Vec<Stray> = groups
        .into_iter()
        .map(|((_, project), procs)| {
            // The group is named after its most common kind (workers outnumber
            // the one parent that launched them).
            let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
            for p in &procs {
                *counts
                    .entry(classify(&p.command).unwrap_or("process"))
                    .or_default() += 1;
            }
            let (kind, kind_count) = counts
                .into_iter()
                .max_by_key(|&(kind, count)| (count, kind != "process"))
                .unwrap_or(("process", procs.len()));
            let project_gone = project.as_deref().is_some_and(|p| !exists(p));
            Stray {
                kind,
                kind_count,
                project,
                project_gone,
                pids: procs.iter().map(|p| p.pid).collect(),
                cpu: procs.iter().map(|p| p.cpu).sum(),
                memory: procs.iter().map(|p| p.memory).sum(),
                uptime: procs.iter().map(|p| p.uptime).max().unwrap_or(0),
            }
        })
        .collect();
    result.sort_by(|a, b| {
        b.is_hot()
            .cmp(&a.is_hot())
            .then(b.cpu.total_cmp(&a.cpu))
            .then(b.uptime.cmp(&a.uptime))
    });
    result
}

/// What dev tool a command line is, if it is one. Order matters: the most
/// specific signatures come first.
pub fn classify(command: &str) -> Option<&'static str> {
    let has = |needle: &str| command.contains(needle);
    let signatures: &[(&dyn Fn() -> bool, &str)] = &[
        (
            &|| has("jest-worker") && has("processChild") && has("/next/"),
            "Next.js build worker",
        ),
        (&|| has("jest-worker") && has("processChild"), "Jest worker"),
        (
            &|| has("next/dist/server") || has("next-server"),
            "Next.js server",
        ),
        (&|| has("next/dist/") || has("/.bin/next"), "Next.js"),
        (&|| has("turbopack"), "Turbopack"),
        (&|| has("vitest"), "Vitest"),
        (&|| has("esbuild") && has("--service"), "esbuild service"),
        (&|| has("webpack"), "webpack"),
        (&|| has("/vite/") || has("/.bin/vite"), "Vite"),
        (&|| has("tsserver"), "TypeScript server"),
        (
            &|| (has("/tsc") || has("typescript/lib/tsc")) && (has("--watch") || has(" -w")),
            "tsc --watch",
        ),
        (&|| has("nodemon"), "nodemon"),
        (&|| has("ts-node"), "ts-node"),
        (&|| has("/tsx/") || has("/.bin/tsx"), "tsx"),
        (&|| has("/jest/") || has("/.bin/jest"), "Jest"),
        (&|| has("eslint"), "ESLint"),
        (&|| has("tailwindcss") || has("postcss"), "Tailwind/PostCSS"),
        (&|| has("rust-analyzer"), "rust-analyzer"),
        (&|| has("cargo-watch") || has("cargo watch"), "cargo watch"),
        (&|| has("/node_modules/"), "Node"),
        (
            &|| has("/target/debug/") || has("/target/release/"),
            "Rust binary",
        ),
        (&|| has("/.venv/") || has("/venv/bin/"), "Python"),
    ];
    signatures
        .iter()
        .find(|(matches, _)| matches())
        .map(|(_, kind)| *kind)
}

/// Processes that look like dev tooling but are installed apps or global tools:
/// leaving those alone is worth missing the odd real stray.
fn is_excluded(command: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "/Applications/",
        "/System/",
        "/Library/",
        "/usr/libexec/",
        "/usr/sbin/",
        "/sbin/",
    ];
    const ANYWHERE: &[&str] = &[
        ".app/Contents/",
        "/opt/homebrew/lib/node_modules/",
        "/usr/local/lib/node_modules/",
        "/usr/lib/node_modules/",
        "/.vscode/extensions/",
        "/.cursor/extensions/",
        "/.npm/_npx/",
    ];
    let program = command.split_whitespace().next().unwrap_or("");
    PREFIXES.iter().any(|prefix| program.starts_with(prefix))
        || ANYWHERE.iter().any(|needle| command.contains(needle))
}

/// `systemd --user` adopts orphans on most Linux desktops instead of pid 1.
fn is_subreaper(command: &str) -> bool {
    command.contains("systemd") && command.contains("--user")
}

/// The project a command runs from, read from the paths in it: everything
/// before `/node_modules/`, `/target/` or `/.venv/`.
pub fn project_of(command: &str) -> Option<PathBuf> {
    command
        .split_whitespace()
        .filter(|word| word.starts_with('/'))
        .find_map(|word| {
            ["/node_modules/", "/target/", "/.venv/", "/venv/"]
                .iter()
                .find_map(|marker| word.find(marker).map(|at| &word[..at]))
        })
        .filter(|project| !project.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 501;
    const WORKER: &str = "/opt/homebrew/bin/node /Users/me/code/shop/node_modules/.pnpm/next@16.4.2/node_modules/next/dist/compiled/jest-worker/processChild.js";

    fn proc(pid: u32, ppid: u32, pgid: u32, command: &str) -> Proc {
        Proc {
            pid,
            ppid,
            pgid,
            uid: ME,
            cpu: 35.0,
            memory: 100 * 1024 * 1024,
            uptime: 5400,
            command: command.to_string(),
        }
    }

    #[test]
    fn parses_ps() {
        let output = "    1     0     1     0   0.0  12000 12-03:04:05 /sbin/launchd\n29937     1 29900   501  35,4 120000    01:31:08 /opt/homebrew/bin/node /x/node_modules/next/dist/compiled/jest-worker/processChild.js\n";
        let procs = parse_ps(output);
        assert_eq!(procs.len(), 2);
        let worker = &procs[1];
        assert_eq!(
            (worker.pid, worker.ppid, worker.pgid, worker.uid),
            (29937, 1, 29900, 501)
        );
        assert!((worker.cpu - 35.4).abs() < 0.01);
        assert_eq!(worker.memory, 120_000 * 1024);
        assert_eq!(worker.uptime, 5468);
        assert!(worker.command.ends_with("processChild.js"));
    }

    #[test]
    fn classifies_dev_tools() {
        assert_eq!(classify(WORKER), Some("Next.js build worker"));
        assert_eq!(
            classify("node /a/node_modules/jest-worker/build/workers/processChild.js"),
            Some("Jest worker")
        );
        assert_eq!(
            classify("/a/node_modules/@esbuild/darwin-arm64/bin/esbuild --service=0.25.0 --ping"),
            Some("esbuild service")
        );
        assert_eq!(
            classify("node /a/node_modules/.bin/tsc --watch"),
            Some("tsc --watch")
        );
        assert_eq!(classify("/a/target/debug/my-server"), Some("Rust binary"));
        assert_eq!(classify("/usr/bin/vim notes.txt"), None);
        assert_eq!(
            classify("/opt/homebrew/opt/postgresql@17/bin/postgres -D x"),
            None
        );
    }

    #[test]
    fn finds_orphaned_workers_and_their_children() {
        let procs = vec![
            proc(1, 0, 1, "/sbin/launchd"),
            proc(10, 1, 10, WORKER),
            proc(11, 1, 10, WORKER),
            proc(12, 10, 10, "/x/esbuild --service"),
            // Still has a living parent (a terminal): not a stray.
            proc(20, 15, 20, WORKER),
            proc(15, 1, 15, "-zsh"),
        ];
        let pids: Vec<u32> = find(&procs, ME).iter().map(|p| p.pid).collect();
        assert_eq!(pids, [10, 11, 12]);
    }

    #[test]
    fn treats_a_missing_parent_and_systemd_user_as_orphaned() {
        let procs = vec![
            proc(1, 0, 1, "/sbin/init"),
            proc(900, 1, 900, "/usr/lib/systemd/systemd --user"),
            proc(30, 900, 30, WORKER),
            // Parent 4242 isn't in the list any more.
            proc(31, 4242, 31, WORKER),
        ];
        let pids: Vec<u32> = find(&procs, ME).iter().map(|p| p.pid).collect();
        assert_eq!(pids, [30, 31]);
    }

    #[test]
    fn leaves_other_users_apps_and_global_tools_alone() {
        let mut other = proc(40, 1, 40, WORKER);
        other.uid = 0;
        let procs = vec![
            other,
            proc(
                41,
                1,
                41,
                "/Applications/Cursor.app/Contents/Resources/app/node_modules/x/server.js",
            ),
            proc(
                42,
                1,
                42,
                "/opt/homebrew/bin/node /opt/homebrew/lib/node_modules/some-cli/daemon.js",
            ),
            proc(43, 1, 43, "/opt/homebrew/opt/redis/bin/redis-server"),
        ];
        assert!(find(&procs, ME).is_empty());
    }

    #[test]
    fn groups_a_build_into_one_row() {
        let procs = vec![
            proc(10, 1, 10, WORKER),
            proc(11, 1, 10, WORKER),
            proc(12, 10, 10, "/x/esbuild --service"),
            proc(
                50,
                1,
                50,
                "node /Users/me/code/api/node_modules/.bin/tsc --watch",
            ),
        ];
        let cwds = HashMap::from([(12, PathBuf::from("/Users/me/code/shop"))]);
        let groups = group(&procs, &cwds, |_| true);
        assert_eq!(groups.len(), 2, "{groups:#?}");
        // The workers and the esbuild service they started: one build, one row.
        let build = groups.iter().find(|g| g.pids.contains(&10)).unwrap();
        assert_eq!(build.kind, "Next.js build worker");
        assert_eq!(build.pids, [10, 11, 12]);
        assert_eq!(
            build.project.as_deref(),
            Some(Path::new("/Users/me/code/shop"))
        );
        assert!((build.cpu - 105.0).abs() < 0.01);
        assert_eq!(build.label(), "2 × Next.js build worker +1");
        let tsc = groups.iter().find(|g| g.pids == [50]).unwrap();
        assert_eq!(tsc.label(), "tsc --watch");
    }

    #[test]
    fn flags_hot_groups_and_deleted_projects_first() {
        let mut idle = proc(
            60,
            1,
            60,
            "node /Users/me/code/api/node_modules/.bin/tsc --watch",
        );
        idle.cpu = 0.5;
        let busy = vec![proc(10, 1, 10, WORKER), proc(11, 1, 10, WORKER)];
        let procs: Vec<Proc> = busy.into_iter().chain([idle]).collect();
        let groups = group(&procs, &HashMap::new(), |p| !p.ends_with("shop"));
        assert!(groups[0].is_hot());
        assert!(groups[0].project_gone);
        assert_eq!(groups[0].folder(), "/Users/me/code/shop (deleted)");
        assert!(!groups[1].is_hot());
        assert_eq!(groups[0].cpu_text(), "70%");
    }

    #[test]
    fn reads_the_project_from_the_command() {
        assert_eq!(
            project_of(WORKER),
            Some(PathBuf::from("/Users/me/code/shop"))
        );
        assert_eq!(
            project_of("/Users/me/code/cli/target/debug/cli serve"),
            Some(PathBuf::from("/Users/me/code/cli"))
        );
        assert_eq!(project_of("node server.js"), None);
    }

    #[test]
    fn summarises() {
        let mut a = group(&[proc(10, 1, 10, WORKER)], &HashMap::new(), |_| true);
        a[0].cpu = 350.0;
        assert_eq!(a[0].cpu_text(), "3.5 cores");
        assert_eq!(summary(&a), "1 process in 1 group, using 3.5 cores");
        a[0].cpu = 2.0;
        assert_eq!(summary(&a), "1 process in 1 group, using 2%");
    }
}
