//! Human-friendly formatting for sizes, durations and paths.

use std::{
    env,
    fmt::Display,
    io::IsTerminal,
    path::Path,
    sync::OnceLock,
    time::{Duration, SystemTime},
};

use crossterm::style::{Color, ContentStyle, Stylize};

/// Styling for plain (non-dashboard) output. Off when stdout isn't a terminal or `NO_COLOR` is set.
pub trait Paint: Display + Sized {
    fn paint(self, style: ContentStyle) -> String {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        let enabled = *ENABLED.get_or_init(|| {
            std::io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        });
        if enabled {
            style.apply(self).to_string()
        } else {
            self.to_string()
        }
    }
    fn with(self, color: Color) -> String {
        self.paint(ContentStyle::new().with(color))
    }
    fn bold(self) -> String {
        self.paint(ContentStyle::new().bold())
    }
    fn dim(self) -> String {
        self.paint(ContentStyle::new().dim())
    }
    fn green(self) -> String {
        self.with(Color::Green)
    }
    fn yellow(self) -> String {
        self.with(Color::Yellow)
    }
    fn red(self) -> String {
        self.with(Color::Red)
    }
    fn cyan(self) -> String {
        self.with(Color::Cyan)
    }
}

impl<T: Display> Paint for T {}

/// `1234567` → `"1.2 MB"` (decimal units, like Finder).
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ if value < 10.0 => format!("{value:.1} {}", UNITS[unit]),
        _ => format!("{value:.0} {}", UNITS[unit]),
    }
}

/// `7980` → `"2h 13m"`.
pub fn duration(secs: u64) -> String {
    let (days, hours, mins, secs) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// How long ago `time` was, compactly: `"today"`, `"12d"`, `"3mo"`, `"2y"`.
pub fn age(time: SystemTime) -> String {
    let days = SystemTime::now()
        .duration_since(time)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / 86_400;
    match days {
        0 => "today".to_string(),
        1..=29 => format!("{days}d"),
        30..=364 => format!("{}mo", days / 30),
        _ => format!("{}y", days / 365),
    }
}

/// Displays a path with the home folder shortened to `~`.
pub fn path(path: &Path) -> String {
    if let Some(home) = env::var_os("HOME")
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return Path::new("~").join(rest).display().to_string();
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1_234_567), "1.2 MB");
        assert_eq!(size(45_600_000_000), "46 GB");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(42), "42s");
        assert_eq!(duration(125), "2m 5s");
        assert_eq!(duration(7980), "2h 13m");
        assert_eq!(duration(3 * 86_400 + 4 * 3600), "3d 4h");
    }
}
