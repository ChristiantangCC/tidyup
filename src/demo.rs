//! Made-up ports and projects for recording demos: `TIDYUP_DEMO=1 tidyup …`.
//!
//! Nothing is stopped or deleted in demo mode; it only pretends, with realistic timing.

use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime},
};

use crate::{
    clean::{Junk, Source},
    ports::Listener,
    ports::Outcome,
};

const MB: u64 = 1_000_000;
const DAY: u64 = 86_400;

pub fn enabled() -> bool {
    env::var_os("TIDYUP_DEMO").is_some_and(|v| !v.is_empty())
}

/// Pids "stopped" so far are kept in a file, so they stay gone across commands.
fn state_file() -> PathBuf {
    home().join(".tidyup-demo-stopped")
}

fn stopped() -> HashSet<u32> {
    fs::read_to_string(state_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.parse().ok())
        .collect()
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("/Users/you"), PathBuf::from)
}

pub fn listeners() -> Vec<Listener> {
    let code = home().join("code");
    let homebrew = Path::new("/opt/homebrew/var");
    let root = Path::new("/");
    let docker = home().join("Library/Containers/com.docker.docker/Data");
    let stopped = stopped();
    [
        fake(
            3000,
            48213,
            "node",
            &code.join("acme-shop"),
            7980,
            412,
            "next dev",
        ),
        fake(
            4000,
            51022,
            "ruby",
            &code.join("legacy-crm"),
            302_000,
            188,
            "rails server",
        ),
        fake(
            5173,
            48877,
            "node",
            &code.join("dashboard"),
            1520,
            236,
            "vite",
        ),
        fake(
            5432,
            912,
            "postgres",
            &homebrew.join("postgresql@16"),
            640_000,
            64,
            "postgres",
        ),
        fake(
            6379,
            905,
            "redis-server",
            &homebrew.join("db/redis"),
            640_000,
            9,
            "redis-server",
        ),
        fake(
            8080,
            50731,
            "python3",
            &code.join("ml-api"),
            45_100,
            1840,
            "uvicorn main:app",
        ),
        fake(
            8787,
            52004,
            "workerd",
            &code.join("edge-worker"),
            610,
            96,
            "wrangler dev",
        ),
        Listener {
            container: Some("acme-mongo".to_string()),
            ..fake(
                27017,
                975,
                "com.docker.backend",
                &docker,
                640_000,
                143,
                "docker",
            )
        },
        fake(
            5000,
            611,
            "ControlCenter",
            root,
            640_000,
            58,
            "ControlCenter",
        ),
        fake(
            7000,
            611,
            "ControlCenter",
            root,
            640_000,
            58,
            "ControlCenter",
        ),
        fake(49152, 598, "rapportd", root, 640_000, 12, "rapportd"),
    ]
    .into_iter()
    .filter(|l| !stopped.contains(&l.pid))
    .collect()
}

fn fake(
    port: u16,
    pid: u32,
    name: &str,
    cwd: &Path,
    uptime: u64,
    mb: u64,
    command: &str,
) -> Listener {
    Listener {
        port,
        pid,
        name: name.to_string(),
        addresses: vec!["*".to_string()],
        cwd: Some(cwd.to_path_buf()),
        command: command.to_string(),
        uptime: Some(uptime),
        memory: Some(mb * MB),
        container: None,
    }
}

/// Pretends to stop `pid`. The Python API "ignores" SIGTERM to show the force path.
pub fn stop(pid: u32, force: bool) -> Outcome {
    let stubborn = pid == 50731 && !force;
    thread::sleep(Duration::from_millis(if stubborn { 1800 } else { 350 }));
    let mut pids = stopped();
    pids.insert(pid);
    let list: Vec<String> = pids.iter().map(u32::to_string).collect();
    let _ = fs::write(state_file(), list.join("\n"));
    if stubborn || force {
        Outcome::Forced
    } else {
        Outcome::Stopped
    }
}

fn junk(root: &Path) -> Vec<Junk> {
    let fixtures: [(&str, &str, u64, u64); 15] = [
        ("rust-cli/target", "Rust", 4620, 64),
        ("ml-api/.venv", "Python", 2910, 121),
        ("game-jam/target", "Rust", 2240, 190),
        ("acme-shop/node_modules", "Node", 1830, 3),
        ("android-app/build", "Gradle", 1410, 214),
        ("dashboard/node_modules", "Node", 1220, 0),
        ("mobile-app/Pods", "CocoaPods", 1090, 152),
        ("mobile-app/node_modules", "Node", 980, 152),
        ("acme-shop/.next", "Next.js", 940, 3),
        ("old-portfolio/node_modules", "Node", 820, 247),
        ("hackathon-2024/node_modules", "Node", 640, 371),
        ("android-app/.gradle", "Gradle", 600, 214),
        ("edge-worker/node_modules", "Node", 410, 12),
        ("old-portfolio/.next", "Next.js", 310, 247),
        ("legacy-crm/vendor", "Composer", 260, 402),
    ];
    let now = SystemTime::now();
    fixtures
        .into_iter()
        .map(|(path, kind, mb, days)| Junk {
            path: root.join(path),
            kind,
            size: mb * MB,
            last_used: now - Duration::from_secs(days * DAY + 3600),
            source: Source::Project,
        })
        .collect()
}

/// Pretends to walk `root`, reporting folders at a believable pace.
pub fn scan(root: &Path, on_found: impl Fn(Junk)) {
    for junk in junk(root) {
        thread::sleep(Duration::from_millis(90));
        on_found(junk);
    }
}

/// Pretends to delete `junk`, counting its size into `freed` over about a second.
pub fn delete(junk: &Junk, freed: &AtomicU64) {
    let steps = 25;
    let pause = Duration::from_millis(300 + junk.size / (4 * MB)) / steps;
    for _ in 0..steps {
        thread::sleep(pause);
        freed.fetch_add(junk.size / steps as u64, Ordering::Relaxed);
    }
}

/// Made-up global caches for `tidyup clean --caches`.
pub fn caches(on_found: impl Fn(Junk)) {
    let home = home();
    let now = SystemTime::now();
    let fixtures: [(&str, &'static str, u64, Source); 4] = [
        (
            "Library/Developer/Xcode/DerivedData",
            "Xcode",
            18_400,
            Source::Cache,
        ),
        (".npm/_cacache", "npm cache", 3_120, Source::Cache),
        (".cargo/registry/src", "Cargo cache", 2_640, Source::Cache),
        ("", "Docker", 7_610, Source::Docker),
    ];
    for (path, kind, mb, source) in fixtures {
        thread::sleep(Duration::from_millis(90));
        let path = if source == Source::Docker {
            PathBuf::from(crate::caches::DOCKER_LABEL)
        } else {
            home.join(path)
        };
        on_found(Junk {
            path,
            kind,
            size: mb * MB,
            last_used: now - Duration::from_secs(2 * DAY),
            source,
        });
    }
}
