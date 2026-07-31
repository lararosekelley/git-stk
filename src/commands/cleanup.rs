use anyhow::Result;
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;
use crate::providers::{
    ReviewProvider, ReviewState, detect_review_provider, owned_review_for_branch,
};
use crate::style;
use crate::{git, stack};

/// Clean up local metadata for merged review requests and delete their
/// branches.
///
/// Unlike `merge`, this does not prompt: it only ever deletes branches whose
/// review is *merged*, so the work is already in the trunk and the ref is
/// recoverable from the reflog - the same reason `sync` deletes merged
/// branches unprompted. `--dry-run` previews and `--keep-branch` retains them.
#[derive(Debug, clap::Args)]
pub struct Cleanup {
    /// Branch to clean up (defaults to the current branch).
    #[arg(add = ArgValueCompleter::new(completions::branch_candidates))]
    branch: Option<String>,
    /// Print what would change without updating local metadata.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
    /// Keep cleaned merged branches instead of deleting them.
    #[arg(long, action = ArgAction::SetTrue)]
    keep_branch: bool,
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
    let mut cleaned = 0;
    let mut skipped = 0;
    let mut retargeted = 0;

    // Snapshot before any branch is retargeted or deleted.
    if !dry_run {
        stack::snapshot("cleanup");
    }

    // Refresh the stack overview ledger while the merged branches and their
    // reviews are still resolvable, so their entries get restyled rather
    // than dropped - mirroring sync.
    let branch_parents = stack::branch_parents(&branches)?;
    crate::notes::update_stack_notes(review_provider.as_ref(), &branch_parents, dry_run, false)?;

    for branch in branches {
        retargeted +=
            recover_deleted_parent(review_provider.as_ref(), &branch, &local_branches, dry_run)?;
        // Closed-inclusive so a review closed without merging gets a
        // truthful skip instead of "no review found". Only merged reviews
        // are ever cleaned: a closed review's work is not in the trunk.
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

        if review.state != ReviewState::Merged {
            anstream::println!(
                "{}",
                style::dim(&format!(
                    "skipped {branch}: review {} is {}",
                    review.id, review.state
                ))
            );
            skipped += 1;
            continue;
        }

        cleanup_merged_branch(review_provider.as_ref(), &branch, dry_run)?;
        cleanup_branch_deletion(&branch, &current_branch, dry_run, !keep_branch)?;
        cleaned += 1;
    }

    let retargeted_note = if retargeted > 0 {
        format!(", {retargeted} retargeted")
    } else {
        String::new()
    };
    anstream::println!(
        "{}",
        style::success(&format!(
            "cleanup complete: {cleaned} cleaned, {skipped} skipped{retargeted_note}"
        ))
    );
    Ok(())
}

/// A merged parent deleted remotely (and pruned locally) leaves `branch`
/// pointing at nothing, but the merged review still remembers its base.
/// Retarget past the gap; the recorded fork point stays valid because it
/// lives in the branch's own history. Returns how many branches moved.
fn recover_deleted_parent(
    review_provider: &dyn ReviewProvider,
    branch: &str,
    local_branches: &[String],
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
    if review.state != ReviewState::Merged
        || review.base == *branch
        || !local_branches.contains(&review.base)
    {
        return Ok(0);
    }

    anstream::println!(
        "{}: parent {} is gone, but review {} merged into {}",
        style::branch(branch),
        style::branch(&parent),
        review.id,
        style::branch(&review.base)
    );
    anstream::println!(
        "{} retarget {} -> {}",
        if dry_run { "would" } else { "will" },
        style::branch(branch),
        style::branch(&review.base)
    );
    update_child_review_base(review_provider, branch, &review.base, dry_run)?;
    if !dry_run {
        stack::set_parent(branch, &review.base)?;
    }
    Ok(1)
}

pub(crate) fn cleanup_merged_branch(
    review_provider: &dyn ReviewProvider,
    branch: &str,
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
                    // Record the fork point off the merged branch before
                    // retargeting, so the next restack replays only the
                    // child's own commits even after a squash merge.
                    if let Ok(base) = git::merge_base(branch, &child) {
                        stack::set_base(&child, &base)?;
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

pub(crate) fn cleanup_branch_deletion(
    branch: &str,
    current_branch: &str,
    dry_run: bool,
    delete_branch: bool,
) -> Result<()> {
    if !delete_branch {
        return Ok(());
    }

    // The checked out branch cannot be deleted; keep it and let the user
    // switch away instead of failing the rest of the cleanup.
    if branch == current_branch {
        anstream::println!(
            "{}",
            style::dim(&format!(
                "kept {branch}: cannot delete the checked out branch"
            ))
        );
        return Ok(());
    }

    // Nor can a branch another worktree holds - but a worktree git-stk created
    // for this branch is ours to remove, and must go first: git refuses to
    // delete a branch a worktree still holds.
    if let Some(path) = git::worktree_holding(branch)? {
        let ours = stack::owned_worktree(branch).is_some_and(|owned| git::same_path(&owned, &path));
        if !ours {
            // The user's own worktree. Naming where it lives keeps the rest of
            // the cleanup running - a landed stack should not stop halfway
            // because one branch has a worktree parked on it.
            anstream::println!(
                "{}",
                style::dim(&format!(
                    "kept {branch}: checked out in the worktree at {}",
                    git::display_path(&path)
                ))
            );
            return Ok(());
        }

        // Ours, but not ours to throw away: uncommitted work in it is not
        // covered by any snapshot, so keep it and say so.
        if git::worktree_has_changes(&path) {
            anstream::println!(
                "{}",
                style::dim(&format!(
                    "kept {branch}: its worktree at {} has uncommitted changes",
                    git::display_path(&path)
                ))
            );
            return Ok(());
        }

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

    anstream::println!(
        "{} delete branch {}",
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
