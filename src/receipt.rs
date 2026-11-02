//! The summary printed after a run: what you got back, in disk, ports or CPU.
//!
//! Each function returns the lines to print, so the wording is testable without
//! running anything. Callers print them with [`print`].

use std::{collections::BTreeMap, ffi::CString, path::Path, time::Duration};

use crate::{
    clean::{self, Junk},
    fmt::{self, Paint},
    ports::Listener,
    strays::Stray,
};

/// Width of the label column, so values line up under each other.
const LABEL: usize = 9;

pub fn print(lines: &[String]) {
    for line in lines {
        println!("{line}");
    }
}

/// What `tidyup clean` did.
pub struct Clean<'a> {
    pub root: &'a Path,
    /// Folders that were deleted (or moved to the Trash).
    pub done: Vec<&'a Junk>,
    /// Folders that could not be removed; their errors were printed already.
    pub failed: usize,
    pub elapsed: Duration,
    /// Free space on the scanned disk before and after, when it could be read.
    pub disk: Option<(u64, u64)>,
    pub trash: bool,
}

pub fn clean(run: &Clean) -> Vec<String> {
    let total: u64 = run.done.iter().map(|j| j.size).sum();
    let count = run.done.len();
    let mut lines = vec![String::new()];

    if count == 0 {
        lines.push(format!("  {}", "✘ Nothing could be removed".red().bold()));
        return lines;
    }

    let headline = if run.trash {
        format!("✔ {} moved to the Trash", fmt::size(total))
    } else {
        format!("✔ {} freed", fmt::size(total))
    };
    let detail = format!(
        "{count} folder{} · {}",
        if count == 1 { "" } else { "s" },
        fmt::duration(run.elapsed.as_secs().max(1))
    );
    lines.push(format!("  {}   {}", headline.green().bold(), detail.dim()));
    lines.push(String::new());

    if run.trash {
        lines.push(format!(
            "  {}",
            "Empty the Trash to get the space back.".dim()
        ));
    } else if let Some((before, after)) = run.disk {
        lines.push(row(
            "Disk",
            &format!(
                "{} free {} {}",
                fmt::size(before),
                "→".dim(),
                format!("{} free", fmt::size(after)).green().bold()
            ),
        ));
    }

    if count > 1 {
        let mut biggest: Vec<&&Junk> = run.done.iter().collect();
        biggest.sort_by_key(|j| std::cmp::Reverse(j.size));
        let shown: Vec<String> = biggest
            .iter()
            .take(3)
            .map(|j| clean::display(&j.path, run.root))
            .collect();
        let width = shown.iter().map(|s| s.chars().count()).max().unwrap_or(0);
        for (i, (junk, name)) in biggest.iter().zip(&shown).enumerate() {
            let label = if i == 0 { "Biggest" } else { "" };
            lines.push(row(
                label,
                &format!("{name:<width$}  {}", fmt::size(junk.size).bold()),
            ));
        }

        let by_kind = by_kind(&run.done);
        if by_kind.len() > 1 {
            lines.push(row("By kind", &by_kind.join(" · ")));
        }
    }

    if run.failed > 0 {
        lines.push(row(
            "Skipped",
            &format!(
                "{} folder{} could not be removed (see above)",
                run.failed,
                if run.failed == 1 { "" } else { "s" }
            )
            .yellow(),
        ));
    }
    lines
}

/// `Node 18 GB · Rust 11 GB · Python 6.0 GB · other 6.3 GB`: the three biggest
/// kinds, then the rest together.
fn by_kind(done: &[&Junk]) -> Vec<String> {
    let mut sizes: BTreeMap<&str, u64> = BTreeMap::new();
    for junk in done {
        *sizes.entry(junk.kind).or_default() += junk.size;
    }
    let mut sizes: Vec<(&str, u64)> = sizes.into_iter().collect();
    sizes.sort_by_key(|&(_, size)| std::cmp::Reverse(size));
    let mut parts: Vec<String> = sizes
        .iter()
        .take(3)
        .map(|(kind, size)| format!("{kind} {}", fmt::size(*size)))
        .collect();
    let other: u64 = sizes.iter().skip(3).map(|(_, size)| size).sum();
    if other > 0 {
        parts.push(format!("other {}", fmt::size(other)));
    }
    parts
}

/// What `tidyup kill` freed. Only worth a summary when several ports were asked for.
pub fn ports(asked: usize, freed: &[&Listener]) -> Vec<String> {
    if asked < 2 || freed.is_empty() {
        return Vec::new();
    }
    let mut ports: Vec<u16> = freed.iter().map(|l| l.port).collect();
    ports.sort_unstable();
    ports.dedup();
    let list = ports
        .iter()
        .map(|p| format!(":{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    let headline = format!(
        "✔ {} port{} free",
        ports.len(),
        if ports.len() == 1 { "" } else { "s" }
    );
    let mut lines = vec![
        String::new(),
        format!("  {}   {}", headline.green().bold(), list.dim()),
    ];
    // A container's numbers would be Docker's own process, so they're left out.
    let memory: u64 = freed
        .iter()
        .filter(|l| l.container.is_none())
        .filter_map(|l| l.memory)
        .sum();
    if memory > 0 {
        lines.push(row("Memory", &format!("{} back", fmt::size(memory))));
    }
    lines
}

/// What `tidyup strays --stop` got back, from the groups that stopped cleanly.
pub fn strays(stopped: &[&Stray], forced: usize) -> Vec<String> {
    if stopped.is_empty() {
        return Vec::new();
    }
    let cpu: f32 = stopped.iter().map(|s| s.cpu).sum();
    let memory: u64 = stopped.iter().map(|s| s.memory).sum();
    let processes: usize = stopped.iter().map(|s| s.pids.len()).sum();
    // ps reports CPU averaged over the process's life (Linux) or recent past
    // (macOS), so this is an estimate.
    let cpu_hours: f64 = stopped
        .iter()
        .map(|s| f64::from(s.cpu) / 100.0 * s.uptime as f64 / 3600.0)
        .sum();

    let headline = if cpu >= 1.0 {
        format!("✔ {} back", cpu_text(cpu))
    } else {
        "✔ Stopped".to_string()
    };
    let detail = format!(
        "{processes} process{} · {} group{}",
        if processes == 1 { "" } else { "es" },
        stopped.len(),
        if stopped.len() == 1 { "" } else { "s" }
    );
    let mut lines = vec![
        String::new(),
        format!("  {}   {}", headline.green().bold(), detail.dim()),
    ];
    if memory > 0 {
        lines.push(row("Memory", &format!("{} back", fmt::size(memory))));
    }
    if cpu_hours >= 0.1 {
        lines.push(row(
            "Had used",
            &format!("~{cpu_hours:.1} CPU-hours while running"),
        ));
    }
    if forced > 0 {
        lines.push(row(
            "Forced",
            &format!(
                "{forced} {}",
                "(ignored SIGTERM, normal for build workers)".dim()
            ),
        ));
    }
    lines
}

/// `3.6 cores` from a full core up, else `35% of a core`.
pub fn cpu_text(cpu: f32) -> String {
    if cpu >= 100.0 {
        format!("{:.1} cores", cpu / 100.0)
    } else {
        format!("{cpu:.0}% of a core")
    }
}

fn row(label: &str, value: &str) -> String {
    format!("  {}{value}", format!("{label:<LABEL$}").dim())
}

/// Free space on the disk holding `path`, in bytes.
pub fn disk_free(path: &Path) -> Option<u64> {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs(3) only writes into the zeroed struct we pass and reads a
    // NUL-terminated path, which CString guarantees.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)] // the field types differ by platform
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::SystemTime};

    use super::*;
    use crate::clean::Source;

    const GB: u64 = 1_000_000_000;

    /// Receipts are coloured; tests compare the plain text.
    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                let mut out = String::new();
                let mut chars = line.chars();
                while let Some(c) = chars.next() {
                    if c == '\x1b' {
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() {
                                break;
                            }
                        }
                    } else {
                        out.push(c);
                    }
                }
                out.trim_end().to_string()
            })
            .collect()
    }

    fn junk(path: &str, kind: &'static str, size: u64) -> Junk {
        Junk {
            path: PathBuf::from("/code").join(path),
            kind,
            size,
            last_used: SystemTime::now(),
            source: Source::Project,
        }
    }

    #[test]
    fn summarises_a_clean() {
        let folders = [
            junk("rust-cli/target", "Rust", 46 * GB / 10),
            junk("ml-api/.venv", "Python", 29 * GB / 10),
            junk("shop/node_modules", "Node", 2 * GB),
            junk("app/node_modules", "Node", GB),
            junk("game/Library", "Unity", GB / 2),
            junk("old/Pods", "CocoaPods", GB / 4),
        ];
        let lines = plain(&clean(&Clean {
            root: Path::new("/code"),
            done: folders.iter().collect(),
            failed: 1,
            elapsed: Duration::from_secs(38),
            disk: Some((212 * GB, 254 * GB)),
            trash: false,
        }));
        assert_eq!(
            lines,
            [
                "",
                "  ✔ 11 GB freed   6 folders · 38s",
                "",
                "  Disk     212 GB free → 254 GB free",
                "  Biggest  rust-cli/target    4.6 GB",
                "           ml-api/.venv       2.9 GB",
                "           shop/node_modules  2.0 GB",
                "  By kind  Rust 4.6 GB · Node 3.0 GB · Python 2.9 GB · other 750 MB",
                "  Skipped  1 folder could not be removed (see above)",
            ]
        );
    }

    #[test]
    fn says_to_empty_the_trash_and_skips_details_for_one_folder() {
        let folder = junk("shop/node_modules", "Node", GB);
        let lines = plain(&clean(&Clean {
            root: Path::new("/code"),
            done: vec![&folder],
            failed: 0,
            elapsed: Duration::from_millis(300),
            disk: Some((1, 2)),
            trash: true,
        }));
        assert_eq!(
            lines,
            [
                "",
                "  ✔ 1.0 GB moved to the Trash   1 folder · 1s",
                "",
                "  Empty the Trash to get the space back.",
            ]
        );
    }

    #[test]
    fn summarises_freed_ports_only_for_several() {
        let listener = |port, memory, container: Option<&str>| Listener {
            port,
            pid: 1000 + u32::from(port),
            name: "node".into(),
            addresses: vec![],
            cwd: None,
            command: String::new(),
            uptime: Some(60),
            memory: Some(memory),
            container: container.map(str::to_string),
        };
        let web = listener(3000, 400_000_000, None);
        let api = listener(5173, 212_000_000, None);
        let db = listener(5432, 9_000_000_000, Some("postgres"));
        assert!(ports(1, &[&web]).is_empty());
        assert_eq!(
            plain(&ports(3, &[&api, &web, &db])),
            [
                "",
                "  ✔ 3 ports free   :3000 :5173 :5432",
                "  Memory   612 MB back"
            ]
        );
    }

    #[test]
    fn summarises_stopped_strays() {
        let stray = |cpu: f32, memory: u64, pids: usize| Stray {
            kind: "Next.js build worker",
            kind_count: pids,
            project: None,
            project_gone: false,
            pids: (1..=pids as u32).collect(),
            cpu,
            memory,
            uptime: 5400,
        };
        let build = stray(352.0, 1_340_000_000, 11);
        let esbuild = stray(4.0, 38_000_000, 1);
        assert_eq!(
            plain(&strays(&[&build, &esbuild], 11)),
            [
                "",
                "  ✔ 3.6 cores back   12 processes · 2 groups",
                "  Memory   1.4 GB back",
                "  Had used ~5.3 CPU-hours while running",
                "  Forced   11 (ignored SIGTERM, normal for build workers)",
            ]
        );
        assert!(strays(&[], 0).is_empty());
    }

    #[test]
    fn reads_free_disk_space() {
        assert!(disk_free(Path::new("/")).is_some_and(|free| free > 0));
        assert_eq!(disk_free(Path::new("/does/not/exist")), None);
    }
}
