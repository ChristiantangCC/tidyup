//! A one-line spinner/progress indicator on stderr for the plain commands.

use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Redraws a status line until dropped. Does nothing when stderr isn't a terminal.
pub struct Progress {
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Progress {
    /// Starts redrawing `render(spinner frame)` every 80 ms.
    pub fn start(render: impl Fn(char) -> String + Send + 'static) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let thread = io::stderr().is_terminal().then(|| {
            let done = done.clone();
            thread::spawn(move || {
                let mut frame = 0;
                while !done.load(Ordering::Relaxed) {
                    eprint!("\r\x1b[2K{}", render(FRAMES[frame % FRAMES.len()]));
                    let _ = io::stderr().flush();
                    frame += 1;
                    thread::sleep(Duration::from_millis(80));
                }
                eprint!("\r\x1b[2K");
            })
        });
        Self { done, thread }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `0.4, 10` → `"████░░░░░░"`.
pub fn bar(fraction: f64, width: usize) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars() {
        assert_eq!(bar(0.0, 4), "░░░░");
        assert_eq!(bar(0.5, 4), "██░░");
        assert_eq!(bar(1.7, 4), "████");
    }
}
