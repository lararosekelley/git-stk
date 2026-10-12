//! A transient spinner and status line on stderr for slow, network-bound
//! work, so a wait reads as progress rather than a hang. Nothing is drawn
//! when stderr is not a terminal, or under `--verbose`, where raw git output
//! passes through.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use console::{Term, style};

use crate::git;

const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
const TICK: Duration = Duration::from_millis(100);

pub struct Progress {
    message: Arc<Mutex<String>>,
    running: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl Progress {
    pub fn start(message: impl Into<String>) -> Self {
        let message = Arc::new(Mutex::new(message.into()));
        let term = Term::stderr();
        let running = (term.features().is_attended() && !git::verbose()).then(|| {
            let stop = Arc::new(AtomicBool::new(false));
            let handle = {
                let (message, stop) = (Arc::clone(&message), Arc::clone(&stop));
                thread::spawn(move || {
                    for frame in FRAMES.iter().cycle() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let line = message.lock().map(|text| text.clone()).unwrap_or_default();
                        // A line that wraps cannot be redrawn in place: each
                        // tick would leave a copy above it.
                        let width = usize::from(term.size().1).saturating_sub(3).max(10);
                        let line = fit(&line, width);
                        let _ = term.clear_line();
                        let _ = term.write_str(&format!("{frame} {}", style(line).dim()));
                        thread::sleep(TICK);
                    }
                    let _ = term.clear_line();
                })
            };
            (stop, handle)
        });
        Self { message, running }
    }

    pub fn set(&self, message: impl Into<String>) {
        if let Ok(mut text) = self.message.lock() {
            *text = message.into();
        }
    }
}

impl Drop for Progress {
    /// Clears the line, so whatever is printed next starts clean.
    fn drop(&mut self) {
        if let Some((stop, handle)) = self.running.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }
}

/// `line` cut to `width` characters, ending in `...` when cut.
fn fit(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_owned();
    }
    let kept: String = line.chars().take(width.saturating_sub(3)).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_are_cut_to_fit() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("a line far too long", 10), "a line ...");
        assert_eq!(fit("a line far too long", 10).chars().count(), 10);
    }
}
