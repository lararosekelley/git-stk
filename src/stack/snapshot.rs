//! Undo support: capture the current stack's branch tips and metadata
//! before a mutating command rewrites them, and restore that capture on
//! `git stk undo`. Local only - pushes and platform merges are not
//! reverted.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{base_of, branch_and_descendants, parent_of, stack_root};
use crate::git;
use crate::style;

const SNAPSHOT_FILE: &str = "stk-undo";

// One snapshot per process: the outermost mutating command captures state;
// inner calls (sync's restack, merge's sync) must not overwrite it.
static TAKEN: AtomicBool = AtomicBool::new(false);

/// Record the current stack so `undo` can restore it. The `label` names the
/// operation being undone. No-ops after the first call in a process, and is
/// best effort: a snapshot failure never blocks the command itself.
pub fn take(label: &str) {
    if TAKEN.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Err(error) = capture(label) {
        // The command should still run; we just lose undo for it.
        let _ = error;
    }
}

fn capture(label: &str) -> Result<()> {
    let head = git::current_branch()?;
    let root = stack_root(&head)?;

    let branches: Vec<Value> = branch_and_descendants(&root)?
        .into_iter()
        .map(|branch| {
            json!({
                "name": branch,
                "sha": git::branch_sha(&branch),
                "parent": parent_of(&branch).ok().flatten(),
                "base": base_of(&branch).ok().flatten(),
            })
        })
        .collect();

    let snapshot = json!({
        "label": label,
        "head": head,
        "branches": branches,
    });
    let path = git::git_path(SNAPSHOT_FILE)?;
    std::fs::write(&path, snapshot.to_string())
        .with_context(|| format!("failed to write {path}"))?;
    Ok(())
}

/// Restore the most recent snapshot: reset branch tips and metadata to their
/// pre-mutation state. Refuses on a dirty worktree (it resets the current
/// branch) and consumes the snapshot so it is one-shot.
pub fn undo() -> Result<()> {
    let path = git::git_path(SNAPSHOT_FILE)?;
    let Ok(contents) = std::fs::read_to_string(&path) else {
        anyhow::bail!("nothing to undo");
    };
    let snapshot: Value = serde_json::from_str(&contents).context("failed to parse undo state")?;

    if super::restack::in_progress() {
        anyhow::bail!(
            "a restack is in progress; finish with `git stk continue` or `git stk abort` first"
        );
    }
    if !git::worktree_is_clean()? {
        anyhow::bail!(
            "worktree has uncommitted changes; commit or stash them before `git stk undo`"
        );
    }

    let label = snapshot["label"].as_str().unwrap_or("the last operation");
    let head = snapshot["head"].as_str().unwrap_or_default().to_owned();
    let branches = snapshot["branches"].as_array().cloned().unwrap_or_default();

    // Before the first ref moves: nothing here is recoverable per-branch, so a
    // blocked restore has to fail whole. The snapshot survives the bail, so
    // freeing the worktree and re-running works.
    let blocked = blocked_by_other_worktrees(&branches, &head)?;
    if !blocked.is_empty() {
        anyhow::bail!(blocked_message(&blocked));
    }

    let mut restored = 0;
    for entry in &branches {
        let name = entry["name"].as_str().unwrap_or_default();
        if name.is_empty() {
            continue;
        }

        // Refs first: recreate deleted branches, rewind moved ones.
        if let Some(sha) = entry["sha"].as_str() {
            git::update_ref(name, sha)?;
        }

        // Then metadata, set or cleared to match the snapshot.
        restore_config(name, "stkParent", entry["parent"].as_str())?;
        restore_config(name, "stkBase", entry["base"].as_str())?;
        restored += 1;
    }

    // Put HEAD back where it was and sync the worktree to the restored tip
    // (clean-tree precondition makes this lossless).
    if !head.is_empty() && git::branch_sha(&head).is_some() {
        if git::current_branch().ok().as_deref() != Some(&head) {
            git::checkout(&head)?;
        }
        git::reset_hard()?;
    }

    std::fs::remove_file(&path).ok();

    anstream::println!(
        "{}",
        style::success(&format!("undid {label}: restored {restored} branches"))
    );
    anstream::println!(
        "{}",
        style::dim("local refs and metadata only; pushes and merged reviews are not reverted")
    );
    Ok(())
}

/// Snapshot branches the restore would move that another worktree holds.
/// `update_ref` succeeds on those without complaint, leaving that worktree's
/// index and working tree describing a commit its branch no longer points at -
/// it silently acquires staged changes nobody made. Refusing keeps `undo` as
/// conservative as its clean-tree precondition already implies: the other
/// worktree may hold uncommitted work, and the snapshot does not cover it.
fn blocked_by_other_worktrees(
    branches: &[Value],
    head: &str,
) -> Result<Vec<(String, std::path::PathBuf)>> {
    let held = git::worktree_branches()?;
    if held.is_empty() {
        return Ok(Vec::new());
    }
    let holder = |branch: &str| {
        held.iter()
            .find(|(name, _)| name == branch)
            .map(|(_, path)| path.clone())
    };

    let mut blocked = Vec::new();
    for entry in branches {
        let name = entry["name"].as_str().unwrap_or_default();
        // Only refs that actually move: one already at its recorded sha changes
        // nothing in the worktree holding it.
        let Some(sha) = entry["sha"].as_str() else {
            continue;
        };
        if name.is_empty() || git::branch_sha(name).as_deref() == Some(sha) {
            continue;
        }
        if let Some(path) = holder(name) {
            blocked.push((name.to_owned(), path));
        }
    }

    // The restore ends by checking `head` out. Another worktree holding it
    // fails that checkout too - after every ref has already been rewound.
    if !head.is_empty()
        && git::current_branch().ok().as_deref() != Some(head)
        && !blocked.iter().any(|(name, _)| name == head)
        && let Some(path) = holder(head)
    {
        blocked.push((head.to_owned(), path));
    }

    Ok(blocked)
}

fn blocked_message(blocked: &[(String, std::path::PathBuf)]) -> String {
    let mut message = String::from("undo would rewind branches checked out in other worktrees:\n");
    for (branch, path) in blocked {
        message.push_str(&format!("  {branch} in {}\n", git::describe_worktree(path)));
    }
    let held_by = git::distinct_paths(blocked.iter().map(|(_, path)| path.as_path()));
    message.push_str(
        "those worktrees would keep an index and working tree the branch no longer matches. \
         Free ",
    );
    // Detaching rather than removing: the main worktree can hold a branch too,
    // and `git worktree remove` refuses on it.
    message.push_str(if held_by.len() == 1 { "it" } else { "each one" });
    message.push_str(" by detaching there, then re-run:\n");
    for path in &held_by {
        message.push_str(&format!("  {}\n", git::detach_command(path)));
    }
    message.truncate(message.trim_end().len());
    message
}

fn restore_config(branch: &str, key: &str, value: Option<&str>) -> Result<()> {
    let full = format!("branch.{branch}.{key}");
    match value {
        Some(value) => git::config_set(&full, value),
        None => git::config_unset(&full),
    }
}
