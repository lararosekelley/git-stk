use std::collections::BTreeSet;

use anyhow::{Result, bail};
use clap::ArgAction;

use crate::cli::{FetchMode, PushMode, UpdateRefsMode};
use crate::commands::Run;
use crate::commands::cleanup::{
    Landing, cleanup_branch_deletion, cleanup_finished_branch, deletion_blocker, landing_for,
    report_kept,
};
use crate::providers::{ReviewState, detect_review_provider};
use crate::settings;
use crate::style;
use crate::{git, stack};

/// Sync the stack with remote state: fetch the trunk, refresh metadata from
/// reviews, clean up finished branches, then restack and push.
#[derive(Debug, clap::Args)]
pub struct Sync {
    /// Print what would change without changing anything.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
    /// Force-push (with lease) rebased branches after the restack.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_push")]
    push: bool,
    /// Do not push rebased branches, overriding stk.pushOnRestack.
    #[arg(long, action = ArgAction::SetTrue)]
    no_push: bool,
}

impl Run for Sync {
    fn run(self) -> Result<()> {
        sync(self.dry_run, PushMode::from_flags(self.push, self.no_push))
    }
}

pub(crate) fn sync(dry_run: bool, push_mode: PushMode) -> Result<()> {
    let current = git::current_branch()?;
    let local_branches = git::local_branches()?;
    let trunk = stack::trunk_branch(&local_branches);

    // Snapshot before the fetch/cleanup/restack rewrites anything. (When
    // `merge` calls sync, merge has already snapshotted; this no-ops.)
    if !dry_run {
        stack::snapshot("sync");
    }

    // 1. Fetch the trunk so merged work is visible locally.
    let remote = settings::remote()?;
    let has_remote = git::remote_url(&remote)?.is_some();
    if let Some(trunk) = &trunk {
        if !has_remote {
            anstream::println!("no remote {remote}; skipped fetch");
        } else if stack::trunk_held_elsewhere(trunk)? {
            // Nothing to do: git will not fetch into a trunk another worktree
            // holds, and the sync runs against the local one.
        } else if dry_run {
            anstream::println!("would fetch {trunk} from {remote}");
        } else if current == *trunk {
            git::pull_ff_only()?;
        } else {
            git::fetch_branch(&remote, trunk)?;
        }
    }

    // 2. The stack containing the current branch (the trunk itself has no
    //    review and is never synced).
    let root = stack::stack_root(&current)?;
    let branches = stack::current_stack_branches(&current)?;

    // A stack rooted off the trunk sits on a branch that is not part of it: no
    // stack parent of its own, with layers stacked on top. Adopting it from
    // its own review - a release PR into the trunk, say - would hand a shared
    // branch to restack, which rebases and force-pushes it; letting it count
    // as finished would delete it locally. It is the stack's base, so sync
    // leaves its metadata and its ref alone. `repair` remains the explicit
    // path for rebuilding a parent when that is genuinely what is wanted.
    let base = stack::unanchored_base(&branches)?;

    let (provider, review_provider) = match detect_review_provider() {
        Ok(pair) => pair,
        // A bare local repo - no remote and no provider configured (the demo
        // provider sets one, so it isn't this case) - has no review state to
        // sync against, so there is nothing to do rather than an error. A
        // remote that exists but isn't recognized is a real config error and
        // still surfaces.
        Err(_) if !has_remote => {
            if branches.is_empty() {
                anstream::println!("no stacked branches to sync");
            } else {
                anstream::println!("no remote configured - nothing to sync");
                anstream::println!(
                    "{}",
                    style::dim("run `git stk restack` to refresh local branches")
                );
            }
            return Ok(());
        }
        Err(error) => return Err(error),
    };

    // 3. Classify every branch: refresh metadata from open reviews, collect
    //    the finished ones for cleanup - merged, plus closed under
    //    `stk.cleanClosed`. `closed` tracks which of them never reached the
    //    trunk, since that changes both the cleanup and the final report.
    let clean_closed = settings::bool_setting(settings::CLEAN_CLOSED_KEY)?;
    let mut finished = Vec::new();
    let mut closed = BTreeSet::new();
    let mut synced = 0;
    let mut skipped = 0;

    for branch in &branches {
        if Some(branch) == base.as_ref() {
            // A recorded base is a fact; one read off the shape is a guess -
            // and skipping it means its own metadata never gets rebuilt here.
            // Say which, and name the command that does rebuild it.
            let note = if stack::is_floor(branch)? {
                format!("skipped {branch}: this stack's base")
            } else {
                format!(
                    "skipped {branch}: nothing below it in this stack, so it reads as the base; \
                     `git stk repair` if it is a stacked branch"
                )
            };
            anstream::println!("{}", style::dim(&note));
            skipped += 1;
            continue;
        }

        // Closed-inclusive so a review closed without merging gets a
        // truthful skip instead of "no review found".
        let Some(review) = review_provider.review_for_branch_including_closed(branch)? else {
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

        if review.branch != *branch {
            anstream::println!(
                "{}",
                style::dim(&format!(
                    "skipped {branch}: {} review belongs to {}",
                    provider.kind, review.branch
                ))
            );
            skipped += 1;
            continue;
        }

        if let Some(landing) = landing_for(&review.state, clean_closed) {
            anstream::println!(
                "{}: review {} is {}",
                style::branch(branch),
                review.id,
                style::state(&review.state)
            );
            finished.push(branch.clone());
            if landing == Landing::Closed {
                closed.insert(branch.clone());
            }
            continue;
        }

        // A closed review's base is dead state: surface it, but never let
        // it drive the stack metadata.
        if review.state == ReviewState::Closed {
            anstream::println!(
                "{}",
                style::dim(&format!(
                    "skipped {branch}: review {} was closed without merging",
                    review.id
                ))
            );
            skipped += 1;
            continue;
        }

        // If this branch's parent finished in this same sync, leave its retarget
        // to cleanup_finished_branch (step 6): it decides the fork point from
        // how the parent ended - pinned past a squash merge, dropped for a
        // closed branch. Recording a base off the new parent here would lose
        // that - the provider may have already retargeted the review to the
        // trunk (GitLab does this when the parent branch is deleted).
        if let Some(parent) = stack::parent_of(branch)?
            && finished.contains(&parent)
        {
            continue;
        }

        if review.branch == review.base {
            bail!("refusing to set {branch} as its own stack parent");
        }

        if !dry_run {
            stack::set_parent(branch, &review.base)?;
            stack::record_base(branch, &review.base);
        }
        anstream::println!(
            "{} {} -> {} {}",
            if dry_run { "would sync" } else { "synced" },
            style::branch(&review.branch),
            style::branch(&review.base),
            style::dim(&format!("({})", review.id))
        );
        synced += 1;
    }

    anstream::println!(
        "{}",
        style::success(&format!(
            "sync complete: {synced} {}synced, {skipped} skipped",
            if dry_run { "would be " } else { "" }
        ))
    );

    // 4. Refresh the stack overview ledger in every review body while the
    //    finished branches and their reviews are still resolvable, so their
    //    entries get restyled rather than dropped.
    let branch_parents = stack::branch_parents(&branches)?;
    crate::notes::update_stack_notes(review_provider.as_ref(), &branch_parents, dry_run, false)?;

    let survivors: Vec<String> = branches
        .iter()
        .filter(|branch| !finished.contains(branch))
        .cloned()
        .collect();

    // 5. Move off any branch that is about to be deleted, onto the first
    //    survivor (the new stack bottom) or the trunk.
    let mut position = current.clone();
    if finished.contains(&current) {
        let target = survivors
            .first()
            .cloned()
            .or_else(|| trunk.clone())
            .unwrap_or(root.clone());
        let held = git::worktree_holding(&target)?;
        if let Some(path) = held {
            // The place to land lives in another worktree. Staying put is not a
            // failure - the review is already finished - and it leaves
            // `position` on that branch, so the deletion below keeps it, which
            // is right: it is still checked out right here.
            anstream::println!(
                "{}",
                style::warn(&format!(
                    "stayed on {current}: {target} is checked out in the worktree at {}",
                    git::display_path(&path)
                ))
            );
        } else if git::in_linked_worktree() {
            // This worktree exists for the branch we are standing on. Checking
            // the trunk out here would repoint someone's dedicated checkout,
            // and - because the branch would no longer be held - let the
            // deletion below take it while leaving the worktree behind with
            // nothing pointing at it. Stay; the branch is kept below, and a
            // cleanup from the main checkout can finish it.
            anstream::println!(
                "{}",
                style::warn(&format!(
                    "stayed on {current}: this is its own worktree, not the main checkout"
                ))
            );
        } else if dry_run {
            anstream::println!("would switch to {}", style::branch(&target));
            position = target;
        } else {
            git::checkout(&target)?;
            position = target;
        }
    }

    // 6. Clean up the finished branches: retarget children, then delete. One
    //    whose ref cannot go yet keeps its metadata, so it stays in the stack
    //    for a later cleanup instead of quietly dropping out of it.
    for branch in &finished {
        let landing = if closed.contains(branch) {
            Landing::Closed
        } else {
            Landing::Merged
        };
        if let Some(reason) = deletion_blocker(branch, &position)? {
            report_kept(branch, &reason);
            continue;
        }
        cleanup_finished_branch(review_provider.as_ref(), branch, landing, dry_run)?;
        cleanup_branch_deletion(branch, landing, dry_run)?;
    }

    // 7. Restack the remainder (and push, per flags/config).
    if dry_run {
        anstream::println!("would restack the remaining stack");
    } else if !survivors.is_empty() {
        // sync already fetched the trunk in step 1, so the restack must not.
        stack::restack(
            FetchMode::Disabled,
            UpdateRefsMode::Config,
            push_mode,
            false,
        )?;
    }

    // 8. Where to look next: the lowest surviving layer. The base is not one -
    //    there is nothing of ours to review or land on it.
    match survivors
        .iter()
        .find(|branch| Some(*branch) != base.as_ref())
    {
        Some(bottom) => match review_provider.review_for_branch(bottom)? {
            Some(review) => anstream::println!(
                "next up: {} -> {} {}",
                style::branch(bottom),
                review.id,
                style::dim(&review.url)
            ),
            None => anstream::println!(
                "next up: {} {}",
                style::branch(bottom),
                style::dim("(no review yet)")
            ),
        },
        None => {
            // The layers landed in whatever the stack sits on: its own base
            // when it is rooted off the trunk, the trunk otherwise.
            let landed_into = base.clone().or(trunk).unwrap_or(root);
            // Only claim a merge when there was one: a stack cleaned up under
            // `stk.cleanClosed` may have been closed rather than landed.
            let ending = if closed.is_empty() {
                format!("stack complete: everything merged into {landed_into}")
            } else {
                format!("stack complete: nothing left above {landed_into} - merged or closed")
            };
            anstream::println!("{}", style::success(&ending));
        }
    }

    Ok(())
}
