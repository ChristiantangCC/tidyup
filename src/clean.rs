//! Finding folders inside projects that can be regenerated: dependencies, build output, caches.
//!
//! A folder only counts when the project file that creates it sits next to it, so a
//! `target/` or `build/` folder you made yourself is never touched.

use std::{
    collections::HashMap,
    env, fs, io,
    io::IsTerminal,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Result, bail};
use dialoguer::{Confirm, MultiSelect, theme::ColorfulTheme};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::{
    caches, demo,
    fmt::{self, Paint},
    progress::{self, Progress},
};

/// A deletable folder `dir`, recognised by one of `markers` in the same parent folder.
/// A marker is a file name, a path such as `ProjectSettings/ProjectVersion.txt`, or a
/// `*.ext` pattern for project files named after the project (`MyApp.csproj`).
pub struct Rule {
    pub dir: &'static str,
    pub markers: &'static [&'static str],
    pub kind: &'static str,
}

const NODE: &[&str] = &["package.json"];
const GRADLE: &[&str] = &[
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
];
const PYTHON: &[&str] = &["pyproject.toml", "requirements.txt", "setup.py", "Pipfile"];
const DOTNET: &[&str] = &["*.csproj", "*.fsproj", "*.vbproj"];
const CMAKE: &[&str] = &["CMakeLists.txt"];
const UNITY: &[&str] = &["ProjectSettings/ProjectVersion.txt"];
const UNREAL: &[&str] = &["*.uproject"];

pub const RULES: &[Rule] = &[
    Rule {
        dir: "node_modules",
        markers: NODE,
        kind: "Node",
    },
    Rule {
        dir: ".next",
        markers: NODE,
        kind: "Next.js",
    },
    Rule {
        dir: ".nuxt",
        markers: NODE,
        kind: "Nuxt",
    },
    Rule {
        dir: ".svelte-kit",
        markers: NODE,
        kind: "SvelteKit",
    },
    Rule {
        dir: ".turbo",
        markers: NODE,
        kind: "Turborepo",
    },
    Rule {
        dir: ".parcel-cache",
        markers: NODE,
        kind: "Parcel",
    },
    Rule {
        dir: ".angular",
        markers: NODE,
        kind: "Angular",
    },
    Rule {
        dir: "target",
        markers: &["Cargo.toml"],
        kind: "Rust",
    },
    Rule {
        dir: "target",
        markers: &["pom.xml"],
        kind: "Maven",
    },
    Rule {
        dir: "build",
        markers: GRADLE,
        kind: "Gradle",
    },
    Rule {
        dir: ".gradle",
        markers: GRADLE,
        kind: "Gradle",
    },
    Rule {
        dir: ".venv",
        markers: PYTHON,
        kind: "Python",
    },
    Rule {
        dir: "venv",
        markers: PYTHON,
        kind: "Python",
    },
    Rule {
        dir: "Pods",
        markers: &["Podfile"],
        kind: "CocoaPods",
    },
    Rule {
        dir: ".dart_tool",
        markers: &["pubspec.yaml"],
        kind: "Dart",
    },
    Rule {
        dir: "build",
        markers: &["pubspec.yaml"],
        kind: "Flutter",
    },
    Rule {
        dir: "vendor",
        markers: &["composer.json"],
        kind: "Composer",
    },
    Rule {
        dir: "_build",
        markers: &["mix.exs"],
        kind: "Elixir",
    },
    Rule {
        dir: "deps",
        markers: &["mix.exs"],
        kind: "Elixir",
    },
    Rule {
        dir: ".zig-cache",
        markers: &["build.zig"],
        kind: "Zig",
    },
    Rule {
        dir: "zig-out",
        markers: &["build.zig"],
        kind: "Zig",
    },
    Rule {
        dir: "bin",
        markers: DOTNET,
        kind: ".NET",
    },
    Rule {
        dir: "obj",
        markers: DOTNET,
        kind: ".NET",
    },
    Rule {
        dir: ".build",
        markers: &["Package.swift"],
        kind: "Swift",
    },
    Rule {
        dir: "build",
        markers: CMAKE,
        kind: "CMake",
    },
    Rule {
        dir: "cmake-build-debug",
        markers: CMAKE,
        kind: "CMake",
    },
    Rule {
        dir: "cmake-build-release",
        markers: CMAKE,
        kind: "CMake",
    },
    Rule {
        dir: "Library",
        markers: UNITY,
        kind: "Unity",
    },
    Rule {
        dir: "Temp",
        markers: UNITY,
        kind: "Unity",
    },
    Rule {
        dir: "Binaries",
        markers: UNREAL,
        kind: "Unreal",
    },
    Rule {
        dir: "Intermediate",
        markers: UNREAL,
        kind: "Unreal",
    },
    Rule {
        dir: "DerivedDataCache",
        markers: UNREAL,
        kind: "Unreal",
    },
    Rule {
        dir: ".terraform",
        markers: &["*.tf"],
        kind: "Terraform",
    },
    Rule {
        dir: ".stack-work",
        markers: &["stack.yaml"],
        kind: "Haskell",
    },
    Rule {
        dir: "dist-newstyle",
        markers: &["cabal.project", "*.cabal"],
        kind: "Haskell",
    },
    Rule {
        dir: "target",
        markers: &["build.sbt"],
        kind: "Scala",
    },
    Rule {
        dir: ".godot",
        markers: &["project.godot"],
        kind: "Godot",
    },
    Rule {
        dir: ".pixi",
        markers: &["pixi.toml"],
        kind: "Pixi",
    },
    Rule {
        dir: ".nx",
        markers: &["nx.json"],
        kind: "Nx",
    },
];

/// Big folders in the home directory that never contain projects worth scanning.
const HOME_SKIP: &[&str] = &["Library", "Applications", "Movies", "Music", "Pictures"];

/// A deletable folder that was found.
#[derive(Debug, Clone)]
pub struct Junk {
    pub path: PathBuf,
    pub kind: &'static str,
    /// Bytes freed by deleting it.
    pub size: u64,
    /// Last change to the project the folder belongs to.
    pub last_used: SystemTime,
    pub source: Source,
}

/// Where a [`Junk`] came from, which decides how it is removed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    /// A dependency or build folder inside a project.
    Project,
    /// A global cache folder (see `caches`).
    Cache,
    /// Docker's unused images and build cache, removed through `docker` itself.
    Docker,
}

fn rule_for(dir: &Path) -> Option<&'static Rule> {
    let name = dir.file_name()?.to_str()?;
    let parent = dir.parent()?;
    RULES.iter().find(|rule| {
        rule.dir == name && rule.markers.iter().any(|marker| has_marker(parent, marker))
    })
}

fn has_marker(project: &Path, marker: &str) -> bool {
    let Some(extension) = marker.strip_prefix('*') else {
        return project.join(marker).is_file();
    };
    fs::read_dir(project)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry.file_name().to_string_lossy().ends_with(extension)
                && entry.file_type().is_ok_and(|kind| kind.is_file())
        })
}

/// Hidden folders (`.git`, `.vscode`, `.cargo`, …), app bundles and big media folders.
/// Skipping hidden folders also keeps editor extensions' `node_modules` safe.
fn should_skip(dir: &Path, home: Option<&Path>) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    name.starts_with('.')
        || name.ends_with(".app")
        || (dir.parent() == home && HOME_SKIP.contains(&name))
}

/// Walks `root` and returns every deletable folder, without measuring them.
fn find(root: &Path) -> Vec<(PathBuf, &'static Rule)> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut found = Vec::new();
    let mut walk = WalkDir::new(root).into_iter();

    while let Some(entry) = walk.next() {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || !entry.file_type().is_dir() {
            continue;
        }
        if let Some(rule) = rule_for(entry.path()) {
            found.push((entry.into_path(), rule));
            walk.skip_current_dir();
        } else if should_skip(entry.path(), home.as_deref()) {
            walk.skip_current_dir();
        }
    }
    found
}

fn measure(path: PathBuf, rule: &'static Rule) -> Junk {
    Junk {
        size: disk_usage(&path),
        last_used: last_used(&path),
        kind: rule.kind,
        path,
        source: Source::Project,
    }
}

/// Space on disk that deleting `path` gives back. A hard-linked file counts only when all
/// of its links are inside `path` (Cargo links outputs within `target/`); one linked from
/// elsewhere, like pnpm's store, stays on disk after the delete.
pub(crate) fn disk_usage(path: &Path) -> u64 {
    let mut total = 0;
    // (device, inode) → (links seen inside `path`, size)
    let mut linked: HashMap<(u64, u64), (u64, u64)> = HashMap::new();

    for meta in WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
    {
        let size = meta.blocks() * 512;
        if meta.is_dir() || meta.nlink() == 1 {
            total += size;
            continue;
        }
        let (seen, _) = linked.entry((meta.dev(), meta.ino())).or_insert((0, size));
        *seen += 1;
        if *seen == meta.nlink() {
            total += size;
        }
    }
    total
}

/// Newest modification time among the project's other top-level entries.
fn last_used(junk: &Path) -> SystemTime {
    let project = junk.parent().unwrap_or(junk);
    fs::read_dir(project)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path() != junk && entry.file_name() != ".DS_Store")
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Scans `root`, calling `on_found` from worker threads as each folder is measured.
pub fn scan(root: &Path, on_found: impl Fn(Junk) + Sync) {
    if demo::enabled() {
        return demo::scan(root, on_found);
    }
    find(root)
        .into_par_iter()
        .for_each(|(path, rule)| on_found(measure(path, rule)));
}

pub fn delete(junk: &Junk) -> io::Result<()> {
    delete_counting(junk, &AtomicU64::new(0))
}

/// Deletes `path`, adding each removed entry's size to `freed` as it goes. A hard-linked
/// file counts once its last link inside `path` is removed, matching `disk_usage`.
pub fn delete_counting(junk: &Junk, freed: &AtomicU64) -> io::Result<()> {
    if demo::enabled() {
        demo::delete(junk, freed);
        return Ok(());
    }
    if junk.source == Source::Docker {
        caches::prune_docker()?;
        freed.fetch_add(junk.size, Ordering::Relaxed);
        return Ok(());
    }
    remove_tree(&junk.path, freed)
}

/// Deletes a folder tree with its subfolders handled in parallel. Unlinking is limited by
/// filesystem latency rather than CPU, so many deletes in flight beat one at a time.
fn remove_tree(dir: &Path, freed: &AtomicU64) -> io::Result<()> {
    let entries: Vec<fs::DirEntry> = fs::read_dir(dir)?.collect::<io::Result<_>>()?;
    entries.into_par_iter().try_for_each(|entry| {
        let meta = entry.metadata()?;
        if meta.is_dir() {
            return remove_tree(&entry.path(), freed);
        }
        fs::remove_file(entry.path())?;
        if meta.nlink() == 1 {
            freed.fetch_add(meta.blocks() * 512, Ordering::Relaxed);
        }
        Ok(())
    })?;
    let size = fs::symlink_metadata(dir).map_or(0, |meta| meta.blocks() * 512);
    fs::remove_dir(dir)?;
    freed.fetch_add(size, Ordering::Relaxed);
    Ok(())
}

/// Running totals shared with a progress line.
#[derive(Default)]
struct Tally {
    count: AtomicUsize,
    bytes: AtomicU64,
}

/// Displays `path` relative to `root` when it's inside it.
pub fn display(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) => relative.display().to_string(),
        Err(_) => fmt::path(path),
    }
}

pub struct Options {
    pub root: PathBuf,
    pub older_than: Option<u64>,
    /// Also list global caches (Xcode, npm, Cargo, Docker, …).
    pub caches: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// `tidyup clean`
pub fn run(options: Options) -> Result<()> {
    let Options {
        root,
        older_than,
        caches,
        dry_run,
        yes,
    } = options;
    if !dry_run && !yes && !std::io::stdin().is_terminal() {
        bail!("not running in a terminal: pass --dry-run to list or --yes to delete everything");
    }

    let scanned = Arc::new(Tally::default());
    let progress = Progress::start({
        let (scanned, shown) = (scanned.clone(), fmt::path(&root));
        move |spinner| {
            format!(
                "{spinner} Scanning {shown}… {} found, {}",
                scanned.count.load(Ordering::Relaxed),
                fmt::size(scanned.bytes.load(Ordering::Relaxed))
            )
        }
    });
    let found = Mutex::new(Vec::new());
    let collect = |junk: Junk| {
        scanned.count.fetch_add(1, Ordering::Relaxed);
        scanned.bytes.fetch_add(junk.size, Ordering::Relaxed);
        found.lock().unwrap().push(junk);
    };
    if caches {
        rayon::join(|| scan(&root, collect), || caches::scan(collect));
    } else {
        scan(&root, collect);
    }
    drop(progress);
    let mut junk = found.into_inner().unwrap();

    if let Some(days) = older_than {
        let cutoff = SystemTime::now() - Duration::from_secs(days * 86_400);
        // Caches are shared by every project, so a project's age says nothing about them.
        junk.retain(|j| j.source != Source::Project || j.last_used < cutoff);
    }
    junk.retain(|j| j.size > 0);
    junk.sort_by_key(|j| std::cmp::Reverse(j.size));

    if junk.is_empty() {
        println!("Nothing to clean.");
        return Ok(());
    }

    let total: u64 = junk.iter().map(|j| j.size).sum();
    let lines: Vec<String> = junk
        .iter()
        .map(|j| {
            format!(
                "{:>8}  {:>5}  {:<12} {}",
                fmt::size(j.size),
                fmt::age(j.last_used),
                j.kind,
                display(&j.path, &root)
            )
        })
        .collect();
    let summary = format!(
        "{} folder{}, {}",
        junk.len(),
        plural(junk.len()),
        fmt::size(total)
    );

    let chosen: Vec<&Junk> = if dry_run || yes {
        println!(
            "{}",
            format!("{:>8}  {:>5}  {:<12} {}", "SIZE", "AGE", "KIND", "PATH").dim()
        );
        for line in &lines {
            println!("{line}");
        }
        println!("{}", summary.as_str().bold());
        if dry_run {
            return Ok(());
        }
        junk.iter().collect()
    } else {
        let picked = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Found {summary}. Space to pick, a for all, enter to continue"
            ))
            .items(&lines)
            .max_length(20)
            .report(false)
            .interact_opt()?;
        let Some(picked) = picked else { return Ok(()) };
        picked.into_iter().map(|i| &junk[i]).collect()
    };

    if chosen.is_empty() {
        println!("Nothing picked.");
        return Ok(());
    }

    let chosen_size: u64 = chosen.iter().map(|j| j.size).sum();
    if !yes
        && !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Delete {} folder{} ({})?",
                chosen.len(),
                plural(chosen.len()),
                fmt::size(chosen_size)
            ))
            .default(false)
            .interact()?
    {
        return Ok(());
    }

    let deleted = Arc::new(Tally::default());
    let progress = Progress::start({
        let (deleted, folders) = (deleted.clone(), chosen.len());
        move |spinner| {
            let bytes = deleted.bytes.load(Ordering::Relaxed);
            format!(
                "{spinner} Deleting {} {} of {} · {}/{folders} folders",
                progress::bar(bytes as f64 / chosen_size.max(1) as f64, 24),
                fmt::size(bytes),
                fmt::size(chosen_size),
                deleted.count.load(Ordering::Relaxed),
            )
        }
    });
    let results: Vec<(&Junk, io::Result<()>)> = chosen
        .into_par_iter()
        .map(|j| {
            let result = delete_counting(j, &deleted.bytes);
            deleted.count.fetch_add(1, Ordering::Relaxed);
            (j, result)
        })
        .collect();
    drop(progress);
    let mut freed = 0;
    for (junk, result) in results {
        match result {
            Ok(()) => freed += junk.size,
            Err(err) => eprintln!("{} {}: {err}", "✘".red(), fmt::path(&junk.path)),
        }
    }
    println!("{} Freed {}", "✔".green(), fmt::size(freed).bold());
    Ok(())
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("tidyup-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    #[test]
    fn finds_only_folders_next_to_their_project_file() {
        let root = temp_dir("find");
        touch(&root.join("web/package.json"));
        touch(&root.join("web/node_modules/react/index.js"));
        touch(&root.join("web/node_modules/nested/node_modules/x.js"));
        touch(&root.join("cli/Cargo.toml"));
        touch(&root.join("cli/target/debug/cli"));
        touch(&root.join("notes/target/keep-me.txt"));
        touch(&root.join(".hidden/package.json"));
        touch(&root.join(".hidden/node_modules/x.js"));

        let mut found: Vec<_> = find(&root)
            .into_iter()
            .map(|(path, rule)| (display(&path, &root), rule.kind))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                ("cli/target".to_string(), "Rust"),
                ("web/node_modules".to_string(), "Node")
            ]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn matches_project_files_by_extension_and_path() {
        let root = temp_dir("markers");
        touch(&root.join("api/Api.csproj"));
        touch(&root.join("api/bin/Debug/api.dll"));
        touch(&root.join("game/ProjectSettings/ProjectVersion.txt"));
        touch(&root.join("game/Library/cache.bin"));
        touch(&root.join("infra/main.tf"));
        touch(&root.join("infra/.terraform/providers/x"));
        touch(&root.join("photos/Library/keep.jpg"));
        touch(&root.join("scripts/bin/keep.sh"));

        let mut found: Vec<_> = find(&root)
            .into_iter()
            .map(|(path, rule)| (display(&path, &root), rule.kind))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                ("api/bin".to_string(), ".NET"),
                ("game/Library".to_string(), "Unity"),
                ("infra/.terraform".to_string(), "Terraform"),
            ]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn measures_and_deletes() {
        let root = temp_dir("measure");
        touch(&root.join("package.json"));
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/big.bin"), vec![0u8; 100_000]).unwrap();

        let junk = std::sync::Mutex::new(Vec::new());
        scan(&root, |j| junk.lock().unwrap().push(j));
        let junk = junk.into_inner().unwrap();
        assert_eq!(junk.len(), 1);
        assert!(junk[0].size >= 100_000);

        delete(&junk[0]).unwrap();
        assert!(!root.join("node_modules").exists());
        assert!(root.join("package.json").exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn counts_hard_links_only_when_all_are_inside() {
        let root = temp_dir("links");
        let inside = root.join("target");
        fs::create_dir_all(&inside).unwrap();
        fs::write(inside.join("a"), vec![1u8; 200_000]).unwrap();
        fs::hard_link(inside.join("a"), inside.join("a-link")).unwrap();
        fs::write(inside.join("b"), vec![2u8; 300_000]).unwrap();
        fs::hard_link(inside.join("b"), root.join("b-outside")).unwrap();

        let size = disk_usage(&inside);
        assert!((200_000..300_000).contains(&size), "got {size}");

        fs::remove_dir_all(root).unwrap();
    }
}
