use anyhow::Result;
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;
use crate::providers::{
    ReviewProvider, ReviewState, detect_review_provider, owned_review_for_branch,
};
use crate::settings;
use crate::style;
use crate::{git, stack};

/// Clean up local metadata for finished review requests and delete their
/// branches.
///
/// Unlike `merge`, this does not prompt: a merged branch's work is already in
/// the trunk and the ref is recoverable from the reflog (and `git stk undo`) -
/// the same reason `sync` deletes merged branches unprompted. Under
/// `stk.cleanClosed` it also cleans up branches whose review was closed without
/// merging; those commits are upstream nowhere, so the deletion says as much
/// and their children keep them. `--dry-run` previews and `--keep-branch`
/// retains them.
#[derive(Debug, clap::Args)]
pub struct Cleanup {
    /// Branch to clean up (defaults to the current branch).
    #[arg(add = ArgValueCompleter::new(completions::branch_candidates))]
    branch: Option<String>,
    /// Print what would change without updating local metadata.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
    /// Keep cleaned branches instead of deleting them.
    #[arg(long, action = ArgAction::SetTrue)]
    keep_branch: bool,
}

/// Whether the branch being cleaned up reached the trunk. A merged branch's
/// commits are upstream, so its children's fork points can be pinned past
/// them; a closed branch's commits live nowhere else, so its children have to
/// keep them.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum Landing {
    Merged,
    Closed,
}

/// Which reviews `cleanup` and `sync` act on: merged always, closed only under
/// `stk.cleanClosed` - for some workflows closing a review means the branch is
/// done too. `None` is a review to leave alone.
pub(crate) fn landing_for(state: &ReviewState, clean_closed: bool) -> Option<Landing> {
    match state {
        ReviewState::Merged => Some(Landing::Merged),
        ReviewState::Closed if clean_closed => Some(Landing::Closed),
        _ => None,
    }
}

impl Run for Cleanup {
    fn run(self) -> Result<()> {
        cleanup(self.branch.as_deref(), self.dry_run, self.keep_branch)
    }
}

pub fn cleanup(branch: Option<&str>, dry_run: bool, keep_branch: bool) -> Result<()> {
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    let branches = stack::branch_and_descendants(&branch)?;
    let current_branch = git::current_branch()?;
    let local_branches = git::local_branches()?;
    let (provider, review_provider) = detect_review_provider()?;
    let clean_closed = settings::bool_setting(settings::CLEAN_CLOSED_KEY)?;
    let mut cleaned = 0;
    let mut skipped = 0;
    let mut kept = 0;
    let mut retargeted = 0;

    // Snapshot before any branch is retargeted or deleted.
    if !dry_run {
        stack::snapshot("cleanup");
    }

    // Refresh the stack overview ledger while the finished branches and their
    // reviews are still resolvable, so their entries get restyled rather
    // than dropped - mirroring sync.
    let branch_parents = stack::branch_parents(&branches)?;
    crate::notes::update_stack_notes(review_provider.as_ref(), &branch_parents, dry_run, false)?;

    for branch in branches {
        retargeted += recover_deleted_parent(
            review_provider.as_ref(),
            &branch,
            &local_branches,
            clean_closed,
            dry_run,
        )?;
        // Closed-inclusive so a review closed without merging gets a truthful
        // skip instead of "no review found" - and so `stk.cleanClosed` can act
        // on it.
        let Some(review) = review_provider.review_for_branch_including_closed(&branch)? else {
            anstream::println!(
                "{}",
                style::dim(&format!(
                    "skipped {branch}: no {} review found",
                    provider.kind
                ))
            );
            skipped += 1;
            continue;
        };

        let Some(landing) = landing_for(&review.state, clean_closed) else {
            anstream::println!(
                "{}",
                style::dim(&format!(
                    "skipped {branch}: review {} is {}",
                    review.id, review.state
                ))
            );
            skipped += 1;
            continue;
        };

        // `--keep-branch` keeps the ref on purpose, so the deletion guards do
        // not apply: clean the metadata and stop there. Otherwise a ref that
        // cannot go yet keeps its metadata too, and stays in the stack.
        if !keep_branch && let Some(reason) = deletion_blocker(&branch, &current_branch)? {
            report_kept(&branch, &reason);
            kept += 1;
            continue;
        }

        cleanup_finished_branch(review_provider.as_ref(), &branch, landing, dry_run)?;
        if !keep_branch {
            cleanup_branch_deletion(&branch, landing, dry_run)?;
        }
        cleaned += 1;
    }

    // Only mention the extras when there are any, so the common line stays
    // short.
    let kept_note = if kept > 0 {
        format!(", {kept} kept")
    } else {
        String::new()
    };
    let retargeted_note = if retargeted > 0 {
        format!(", {retargeted} retargeted")
    } else {
        String::new()
    };
    anstream::println!(
        "{}",
        style::success(&format!(
            "cleanup complete: {cleaned} cleaned, {skipped} skipped{kept_note}{retargeted_note}"
        ))
    );
    Ok(())
}

/// A finished parent deleted remotely (and pruned locally) leaves `branch`
/// pointing at nothing, but the review still remembers its base. Retarget past
/// the gap. Returns how many branches moved.
fn recover_deleted_parent(
    review_provider: &dyn ReviewProvider,
    branch: &str,
    local_branches: &[String],
    clean_closed: bool,
    dry_run: bool,
) -> Result<usize> {
    let Some(parent) = stack::parent_of(branch)? else {
        return Ok(0);
    };
    if local_branches.contains(&parent) {
        return Ok(0);
    }

    // Provider lookups go by ref name, so the review outlives the branch.
    // Best effort: anything unresolved stays for `git stk repair`.
    let Ok(Some(review)) = owned_review_for_branch(review_provider, &parent) else {
        return Ok(0);
    };
    let Some(landing) = landing_for(&review.state, clean_closed) else {
        return Ok(0);
    };
    if review.base == *branch || !local_branches.contains(&review.base) {
        return Ok(0);
    }

    match landing {
        Landing::Merged => anstream::println!(
            "{}: parent {} is gone, but review {} merged into {}",
            style::branch(branch),
            style::branch(&parent),
            review.id,
            style::branch(&review.base)
        ),
        Landing::Closed => anstream::println!(
            "{}: parent {} is gone; review {} was closed against {}",
            style::branch(branch),
            style::branch(&parent),
            review.id,
            style::branch(&review.base)
        ),
    }
    anstream::println!(
        "{} retarget {} -> {}",
        if dry_run { "would" } else { "will" },
        style::branch(branch),
        style::branch(&review.base)
    );
    update_child_review_base(review_provider, branch, &review.base, dry_run)?;
    if !dry_run {
        // A merged parent's fork point stays valid: it lives in this branch's
        // own history and its commits are upstream. A closed parent's commits
        // survive only here, so a fork point recorded off it would make the
        // restack drop them.
        if landing == Landing::Closed {
            stack::unset_base(branch)?;
        }
        stack::set_parent(branch, &review.base)?;
    }
    Ok(1)
}

/// Retarget a finished branch's children onto its parent, then detach the
/// branch itself. The children's recorded fork points depend on `landing`: see
/// [`Landing`].
pub(crate) fn cleanup_finished_branch(
    review_provider: &dyn ReviewProvider,
    branch: &str,
    landing: Landing,
    dry_run: bool,
) -> Result<()> {
    let parent = stack::parent_of(branch)?;
    let descendants = stack::branch_and_descendants(branch)?;
    let direct_children: Vec<_> = descendants
        .into_iter()
        .skip(1)
        .filter_map(|child| match stack::parent_of(&child) {
            Ok(Some(child_parent)) if child_parent == branch => Some(Ok(child)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<_>>()?;

    for child in direct_children {
        match parent.as_deref() {
            Some(parent) => {
                anstream::println!(
                    "{} retarget {} -> {}",
                    if dry_run { "would" } else { "will" },
                    style::branch(&child),
                    style::branch(parent)
                );
                update_child_review_base(review_provider, &child, parent, dry_run)?;
                if !dry_run {
                    match landing {
                        // Record the fork point off the merged branch before
                        // retargeting, so the next restack replays only the
                        // child's own commits even after a squash merge.
                        Landing::Merged => {
                            if let Ok(base) = git::merge_base(branch, &child) {
                                stack::set_base(&child, &base)?;
                            }
                        }
                        // A closed branch's commits landed nowhere, and the
                        // child was written on top of them: drop the fork
                        // point so the restack replays everything the child
                        // has that its new parent lacks, closed commits
                        // included.
                        Landing::Closed => stack::unset_base(&child)?,
                    }
                    stack::set_parent(&child, parent)?;
                }
            }
            None => {
                anstream::println!(
                    "{} detach {}",
                    if dry_run { "would" } else { "will" },
                    style::branch(&child)
                );
                if !dry_run {
                    stack::unset_parent(&child)?;
                    stack::unset_base(&child)?;
                }
            }
        }
    }
    anstream::println!(
        "{} detach {}",
        if dry_run { "would" } else { "will" },
        style::branch(branch)
    );
    if !dry_run {
        stack::unset_parent(branch)?;
        stack::unset_base(branch)?;
    }

    Ok(())
}

/// Why `branch`'s ref cannot go yet, or `None` when it can. Asked *before*
/// anything is written: a branch that has to stay keeps its stack metadata too,
/// so it stays in the stack for a later cleanup rather than being silently
/// unstacked.
pub(crate) fn deletion_blocker(branch: &str, current_branch: &str) -> Result<Option<String>> {
    // The checked out branch cannot be deleted; keep it and let the user
    // switch away instead of failing the rest of the cleanup.
    if branch == current_branch {
        return Ok(Some("cannot delete the checked out branch".to_owned()));
    }

    // Nor can a branch another worktree holds - but a worktree git-stk created
    // for this branch is ours to remove.
    let Some(path) = git::worktree_holding(branch)? else {
        return Ok(None);
    };
    if !stack::owned_worktree(branch).is_some_and(|owned| git::same_path(&owned, &path)) {
        // The user's own worktree. Naming where it lives keeps the rest of the
        // cleanup running - a landed stack should not stop halfway because one
        // branch has a worktree parked on it.
        return Ok(Some(format!(
            "checked out in the worktree at {}",
            git::display_path(&path)
        )));
    }
    // Ours, but not ours to throw away: uncommitted work in it is not covered
    // by any snapshot.
    if git::worktree_has_changes(&path) {
        return Ok(Some(format!(
            "its worktree at {} has uncommitted changes",
            git::display_path(&path)
        )));
    }
    Ok(None)
}

/// Report a finished branch whose ref stays for now, and why.
pub(crate) fn report_kept(branch: &str, reason: &str) {
    anstream::println!(
        "{}",
        style::dim(&format!(
            "kept {branch}: {reason} - still stacked, so a later cleanup can finish it"
        ))
    );
}

/// Delete `branch`, removing the worktree git-stk made for it first: git refuses
/// to delete a branch a worktree still holds. Call [`deletion_blocker`] first -
/// this assumes the ref is free to go.
pub(crate) fn cleanup_branch_deletion(branch: &str, landing: Landing, dry_run: bool) -> Result<()> {
    if let Some(path) = stack::owned_worktree(branch).filter(|owned| {
        git::worktree_holding(branch)
            .ok()
            .flatten()
            .is_some_and(|held| git::same_path(owned, &held))
    }) {
        anstream::println!(
            "{} remove worktree {}",
            if dry_run { "would" } else { "will" },
            git::display_path(&path)
        );
        if !dry_run {
            git::worktree_remove(&path)?;
            stack::unset_owned_worktree(branch)?;
        }
    }

    // A closed branch's commits are in no other branch, so say so on the way
    // out and name the way back: the snapshot `undo` restores is the only
    // handle most people will have.
    let caveat = match landing {
        Landing::Merged => String::new(),
        Landing::Closed => style::dim(" (closed, not merged - `git stk undo` restores it)"),
    };
    anstream::println!(
        "{} delete branch {}{caveat}",
        if dry_run { "would" } else { "will" },
        style::branch(branch)
    );
    if !dry_run {
        git::delete_branch(branch)?;
    }

    Ok(())
}

fn update_child_review_base(
    review_provider: &dyn ReviewProvider,
    child: &str,
    parent: &str,
    dry_run: bool,
) -> Result<()> {
    let Some(review) = review_provider.review_for_branch(child)? else {
        return Ok(());
    };

    if review.state == ReviewState::Merged || review.base == parent {
        return Ok(());
    }

    anstream::println!(
        "{} update review {} -> {} {}",
        if dry_run { "would" } else { "will" },
        style::branch(&review.branch),
        style::branch(parent),
        style::dim(&format!("({})", review.id))
    );
    if !dry_run {
        let output = review_provider.update_review_base(&review, parent)?;
        if !output.is_empty() {
            println!("{output}");
        }
    }

    Ok(())
}
