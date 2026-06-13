//! A coarse advisory lock so two git-stk processes never run state-mutating
//! commands at once. Git locks its own index and refs, but not git-stk's
//! multi-step orchestration (snapshot, rebases, metadata, provider calls), so
//! a concurrent run could clobber the undo snapshot or half-rewrite the stack.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::git;

const LOCK_FILE: &str = "stk-lock";

/// Held for the duration of a mutating command; removes the lock file on drop.
/// Outside a git repo it is a no-op, so the command surfaces its own error.
pub struct Lock {
    path: Option<PathBuf>,
}

impl Lock {
    /// Take the lock for `command`, or fail if another git-stk process holds
    /// it. Naming the command makes the contention message actionable.
    pub fn acquire(command: &str) -> Result<Self> {
        let Ok(path) = git::git_common_path(LOCK_FILE) else {
            // Not a git repo: nothing to guard, and the command will report
            // the real problem itself.
            return Ok(Self { path: None });
        };
        let path = PathBuf::from(path);

        // Two passes: if the first finds a lock whose holder process has died
        // (a `kill -9`, a crash, a power loss mid-command), reclaim it and try
        // once more. A second failure means it is genuinely held, or another
        // run reclaimed it first.
        for reclaim in [true, false] {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    // Best effort: the holder line feeds the error message and
                    // the staleness check.
                    let _ = writeln!(file, "{} {command}", std::process::id());
                    return Ok(Self { path: Some(path) });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let holder = fs::read_to_string(&path).unwrap_or_default();
                    let holder = holder.trim().to_owned();
                    if reclaim && holder_is_stale(&holder) {
                        anstream::eprintln!(
                            "{}",
                            crate::style::dim(&format!(
                                "reclaiming a stale git-stk lock; its holder ({holder}) is gone"
                            ))
                        );
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    let by = if holder.is_empty() {
                        String::new()
                    } else {
                        format!(" ({holder})")
                    };
                    bail!(
                        "another git stk operation is in progress{by}; wait for it to \
                         finish, or remove {} if it is stale",
                        path.display()
                    );
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to take the lock at {}", path.display()));
                }
            }
        }
        unreachable!("the second pass always returns or bails");
    }
}

/// Whether the lock's recorded holder process is gone, so the lock is stale
/// and safe to reclaim. Conservative: an unparseable holder, a live process,
/// a process owned by another user, or a platform we cannot probe all read as
/// "not stale" - keep the lock and let the contention message stand.
fn holder_is_stale(holder: &str) -> bool {
    holder
        .split_whitespace()
        .next()
        .and_then(|token| token.parse::<i32>().ok())
        .is_some_and(process_is_dead)
}

#[cfg(unix)]
fn process_is_dead(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // kill(pid, 0) sends no signal; it just probes existence. 0 means alive,
    // ESRCH means no such process (dead), and EPERM means it exists but is
    // owned by another user (alive, and not ours to reclaim).
    if unsafe { libc::kill(pid, 0) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_is_dead(_pid: i32) -> bool {
    // No portable existence probe without a platform crate; never auto-reclaim
    // here - the contention message tells the user how to clear a stale lock.
    false
}

impl Drop for Lock {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}
