use anyhow::{Result, bail};
use clap::ArgAction;

use crate::cli::{PushMode, UpdateRefsMode};
use crate::commands::Run;
use crate::commands::cleanup::{cleanup_branch_deletion, cleanup_merged_branch};
use crate::providers::{ReviewState, detect_review_provider};
use crate::settings;
use crate::style;
use crate::{git, stack};

/// Sync the stack with remote state: fetch the trunk, refresh metadata from
/// reviews, clean up merged branches, then restack and push.
#[derive(Debug, clap::Args)]
pub struct Sync {
    /// Print what would change without changing anything.
    #[arg(long, action = ArgAction::SetTrue)]
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
    if let Some(trunk) = &trunk {
        if git::remote_url(&remote)?.is_none() {
            println!("no remote {remote}; skipped fetch");
        } else if dry_run {
            println!("would fetch {trunk} from {remote}");
        } else if current == *trunk {
            git::pull_ff_only()?;
        } else {
            git::fetch_branch(&remote, trunk)?;
        }
    }

    // 2. The stack containing the current branch (the trunk itself has no
    //    review and is never synced).
    let root = stack::stack_root(&current)?;
    let branches: Vec<String> = stack::branch_and_descendants(&root)?
        .into_iter()
        .filter(|branch| Some(branch) != trunk.as_ref())
        .collect();

    let (provider, review_provider) = detect_review_provider()?;

    // 3. Classify every branch: refresh metadata from open reviews, collect
    //    merged ones for cleanup.
    let mut merged = Vec::new();
    let mut synced = 0;
    let mut skipped = 0;

    for branch in &branches {
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

        if review.state == ReviewState::Merged {
            anstream::println!(
                "{}: review {} is {}",
                style::branch(branch),
                review.id,
                style::state(&review.state)
            );
            merged.push(branch.clone());
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

        if review.branch == review.base {
            bail!("refusing to set {branch} as its own stack parent");
        }

        if !dry_run {
            stack::set_parent_for_branch(branch, &review.base)?;
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
    //    merged branches and their reviews are still resolvable, so their
    //    entries get restyled rather than dropped.
    let branch_parents = stack::branch_parents(&branches)?;
    crate::notes::update_stack_notes(review_provider.as_ref(), &branch_parents, dry_run, false)?;

    let survivors: Vec<String> = branches
        .iter()
        .filter(|branch| !merged.contains(branch))
        .cloned()
        .collect();

    // 5. Move off any branch that is about to be deleted, onto the first
    //    survivor (the new stack bottom) or the trunk.
    let mut position = current.clone();
    if merged.contains(&current) {
        let target = survivors
            .first()
            .cloned()
            .or_else(|| trunk.clone())
            .unwrap_or(root.clone());
        if dry_run {
            anstream::println!("would switch to {}", style::branch(&target));
        } else {
            git::checkout(&target)?;
        }
        position = target;
    }

    // 6. Clean up the merged branches: retarget children, then delete.
    for branch in &merged {
        cleanup_merged_branch(review_provider.as_ref(), branch, dry_run)?;
        cleanup_branch_deletion(branch, &position, dry_run, true)?;
    }

    // 7. Restack the remainder (and push, per flags/config).
    if dry_run {
        println!("would restack the remaining stack");
    } else if !survivors.is_empty() {
        stack::restack(UpdateRefsMode::Config, push_mode, false)?;
    }

    // 8. Where to look next.
    match survivors.first() {
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
            let base = trunk.unwrap_or(root);
            anstream::println!(
                "{}",
                style::success(&format!("stack complete: everything merged into {base}"))
            );
        }
    }

    Ok(())
}
