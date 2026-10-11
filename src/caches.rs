//! Caches that package managers, IDEs and Docker keep outside any project.
//!
//! Everything here is downloaded or built again on demand; deleting it costs a slower
//! next install or build, nothing more.

use std::{
    env, fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::SystemTime,
};

use rayon::prelude::*;

use crate::{
    clean::{self, Junk, Source},
    demo,
};

/// Cache folders by tool, relative to the home folder. Missing ones are skipped, so
/// macOS and Linux locations can sit side by side.
const CACHES: &[(&str, &[&str])] = &[
    (
        "Xcode",
        &[
            "Library/Developer/Xcode/DerivedData",
            "Library/Developer/Xcode/iOS DeviceSupport",
            "Library/Developer/CoreSimulator/Caches",
        ],
    ),
    ("npm cache", &[".npm/_cacache"]),
    (
        "pnpm store",
        &["Library/pnpm/store", ".local/share/pnpm/store"],
    ),
    ("Yarn cache", &["Library/Caches/Yarn", ".cache/yarn"]),
    ("Bun cache", &[".bun/install/cache"]),
    (
        "Cargo cache",
        &[
            ".cargo/registry/cache",
            ".cargo/registry/src",
            ".cargo/git/checkouts",
        ],
    ),
    ("Go modules", &["go/pkg/mod"]),
    ("Gradle cache", &[".gradle/caches"]),
    ("Maven repo", &[".m2/repository"]),
    ("pip cache", &["Library/Caches/pip", ".cache/pip"]),
    ("uv cache", &["Library/Caches/uv", ".cache/uv"]),
    ("Homebrew", &["Library/Caches/Homebrew"]),
    ("CocoaPods", &["Library/Caches/CocoaPods"]),
];

/// Label shown in place of a path for Docker's images and build cache.
pub const DOCKER_LABEL: &str = "Docker: unused images and build cache";

/// Measures every cache folder that exists, plus Docker's, calling `on_found` for each.
pub fn scan(on_found: impl Fn(Junk) + Sync) {
    if demo::enabled() {
        return demo::caches(on_found);
    }
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let folders: Vec<(PathBuf, &'static str)> = CACHES
        .iter()
        .flat_map(|(kind, paths)| paths.iter().map(|path| (home.join(path), *kind)))
        .filter(|(path, _)| path.is_dir())
        .collect();

    rayon::join(
        || {
            folders.into_par_iter().for_each(|(path, kind)| {
                let last_used = fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                on_found(Junk {
                    size: clean::disk_usage(&path),
                    path,
                    kind,
                    last_used,
                    source: Source::Cache,
                });
            });
        },
        || {
            if let Some(junk) = docker() {
                on_found(junk);
            }
        },
    );
}

/// Docker's unused images and build cache, sized from `docker system df`. `None` when
/// Docker isn't installed or isn't running.
fn docker() -> Option<Junk> {
    let output = Command::new("docker")
        .args(["system", "df", "--format", "{{json .}}"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let size = parse_docker_df(&String::from_utf8_lossy(&output.stdout));
    (size > 0).then(|| Junk {
        path: PathBuf::from(DOCKER_LABEL),
        kind: "Docker",
        size,
        last_used: SystemTime::now(),
        source: Source::Docker,
    })
}

/// Removes every image no container uses and the whole build cache. Volumes and
/// containers are left alone: they can hold data that can't be rebuilt.
pub fn prune_docker() -> std::io::Result<()> {
    for args in [
        ["image", "prune", "--all", "--force"],
        ["builder", "prune", "--all", "--force"],
    ] {
        let status = Command::new("docker")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "docker {} prune failed",
                args[0]
            )));
        }
    }
    Ok(())
}

/// Sums what `docker system df` reports as reclaimable for images and build cache.
fn parse_docker_df(output: &str) -> u64 {
    output
        .lines()
        .filter(|line| {
            json_field(line, "Type").is_some_and(|kind| kind == "Images" || kind == "Build Cache")
        })
        .filter_map(|line| json_field(line, "Reclaimable"))
        .filter_map(|reclaimable| parse_docker_size(reclaimable.split(' ').next()?))
        .sum()
}

/// Reads a string field from one line of Docker's flat JSON output.
fn json_field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let start = line.find(&format!("\"{name}\":\""))? + name.len() + 4;
    let end = line[start..].find('"')?;
    Some(&line[start..start + end])
}

/// Parses Docker's human sizes (`60.73GB`, `102.4kB`, `0B`), which use decimal units.
fn parse_docker_size(size: &str) -> Option<u64> {
    let unit_at = size.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = size.split_at(unit_at);
    let multiplier: f64 = match unit.to_ascii_uppercase().as_str() {
        "B" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    Some((number.parse::<f64>().ok()? * multiplier).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_system_df() {
        let output = r#"{"Active":"8","Reclaimable":"60.73GB (95%)","Size":"63.81GB","TotalCount":"35","Type":"Images"}
{"Active":"11","Reclaimable":"102.4kB (31%)","Size":"323.6kB","TotalCount":"18","Type":"Containers"}
{"Active":"15","Reclaimable":"102.2MB (3%)","Size":"3.382GB","TotalCount":"20","Type":"Local Volumes"}
{"Active":"0","Reclaimable":"21.47GB","Size":"21.47GB","TotalCount":"330","Type":"Build Cache"}"#;
        assert_eq!(parse_docker_df(output), 60_730_000_000 + 21_470_000_000);
    }

    #[test]
    fn parses_docker_sizes() {
        assert_eq!(parse_docker_size("0B"), Some(0));
        assert_eq!(parse_docker_size("102.4kB"), Some(102_400));
        assert_eq!(parse_docker_size("3.382GB"), Some(3_382_000_000));
        assert_eq!(parse_docker_size("lots"), None);
    }
}
