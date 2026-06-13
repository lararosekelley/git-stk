use anyhow::{Result, bail};
use clap::ArgAction;

use crate::cli::PushMode;
use crate::commands::Run;
use crate::commands::sync::sync;
use crate::prompt::confirm;
use crate::providers::{MergeBlocker, ProviderKind, ReviewProvider, ReviewRequest, ReviewState};
use crate::providers::{detect_provider, review_provider};
use crate::settings;
use crate::stack;
use crate::style;

/// Merge the review at the bottom of the stack, then sync.
#[derive(Debug, clap::Args)]
pub struct Merge {
    /// Print what would happen without merging anything.
    #[arg(long, action = ArgAction::SetTrue)]
    dry_run: bool,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y', action = ArgAction::SetTrue)]
    yes: bool,
    /// Schedule the merge for when required checks pass instead of merging
    /// now.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "all")]
    auto: bool,
    /// Repeat merge-and-sync bottom-up until the whole stack has landed.
    #[arg(long, action = ArgAction::SetTrue)]
    all: bool,
    /// With --all: wait for each review's checks before merging it.
    #[arg(long, action = ArgAction::SetTrue, requires = "all", conflicts_with = "no_wait")]
    wait: bool,
    /// With --all: do not wait for checks, overriding stk.mergeWait.
    #[arg(long, action = ArgAction::SetTrue, requires = "all")]
    no_wait: bool,
}

impl Run for Merge {
    fn run(self) -> Result<()> {
        if self.all {
            // Waiting: --wait forces it on, --no-wait off; otherwise
            // stk.mergeWait decides.
            let wait = if self.wait {
                true
            } else if self.no_wait {
                false
            } else {
                settings::bool_setting(settings::MERGE_WAIT_KEY)?
            };
            merge_all(self.dry_run, self.yes, wait)
        } else {
            merge(self.dry_run, self.yes, self.auto)
        }
    }
}

fn merge(dry_run: bool, yes: bool, auto: bool) -> Result<()> {
    let Some(bottom) = bottom_branch()? else {
        bail!(nothing_to_merge_hint()?);
    };

    let provider = detect_provider()?;
    let review_provider = review_provider(provider.kind);
    let review = open_review_for(review_provider.as_ref(), provider.kind, &bottom)?;

    let strategy = settings::merge_strategy()?;
    let mode = if auto {
        format!("{strategy}, auto")
    } else {
        strategy.clone()
    };
    let label = review.label();

    if dry_run {
        println!("would merge {label} into {} ({mode})", review.base);
        println!("would sync afterwards");
        return Ok(());
    }

    if !yes
        && !confirm(&format!(
            "merge {label} into {} ({mode})? [y/N] ",
            review.base
        ))?
    {
        println!("merge cancelled");
        return Ok(());
    }

    stack::snapshot("merge");
    match merge_and_check(review_provider.as_ref(), &review, &strategy, auto)? {
        // Reconcile everything the merge changed: fetch, clean up, restack,
        // push.
        MergeOutcome::Merged => sync(false, PushMode::Config),
        MergeOutcome::Scheduled => Ok(()),
    }
}

/// Land the whole stack: merge the bottom review and sync, bottom-up, until
/// the stack is complete. One confirmation up front; a merge that only gets
/// scheduled stops the loop, and with `wait` each review's checks settle
/// before its merge.
fn merge_all(dry_run: bool, yes: bool, wait: bool) -> Result<()> {
    let Some(bottom) = bottom_branch()? else {
        bail!(nothing_to_merge_hint()?);
    };

    let provider = detect_provider()?;
    let review_provider = review_provider(provider.kind);
    let strategy = settings::merge_strategy()?;

    // What is about to land, bottom-up, for the dry run and the prompt: the
    // current branch's own line, not sibling stacks sharing the trunk.
    let current = crate::git::current_branch()?;
    let branches = stack::stack_line(&current)?;
    let count = branches.len();

    if dry_run {
        for branch in &branches {
            let review = open_review_for(review_provider.as_ref(), provider.kind, branch)?;
            if wait {
                println!("would wait for checks on {}", review.id);
            }
            println!(
                "would merge {} into {} ({strategy})",
                review.label(),
                review.base
            );
        }
        println!("would sync after each merge");
        return Ok(());
    }

    let base = stack::parent_for_branch(&bottom)?.unwrap_or_else(|| "its base".to_owned());
    if !yes
        && !confirm(&format!(
            "merge {count} review{} into {base}, bottom-up ({strategy})? [y/N] ",
            if count == 1 { "" } else { "s" }
        ))?
    {
        println!("merge cancelled");
        return Ok(());
    }

    stack::snapshot("merge --all");

    // Each sync removes the merged bottom, so the loop is bounded by the
    // number of branches it started with.
    let mut landed = 0;
    for _ in 0..count {
        let Some(bottom) = bottom_branch()? else {
            break;
        };
        let review = open_review_for(review_provider.as_ref(), provider.kind, &bottom)?;

        // Each sync force-pushes the next branch and restarts its checks;
        // waiting here is what turns the landing into one command.
        if wait {
            anstream::println!(
                "waiting for checks on {} {}",
                review.id,
                style::dim("(ctrl-c is safe; rerun `git stk merge --all` to resume)")
            );
            if !review_provider.wait_for_checks(&review)? {
                bail!(
                    "checks failed for {}; fix them and rerun `git stk merge --all`",
                    review.id
                );
            }
        }

        match merge_and_check(review_provider.as_ref(), &review, &strategy, false)? {
            MergeOutcome::Merged => {
                sync(false, PushMode::Config)?;
                landed += 1;
            }
            MergeOutcome::Scheduled => break,
        }
    }

    anstream::println!(
        "{}",
        style::success(&format!(
            "merge complete: {landed} of {count} review{} merged",
            if count == 1 { "" } else { "s" }
        ))
    );
    Ok(())
}

/// The bottom of the stack containing the current branch: the first branch on
/// its line above the trunk. (A rootless fragment's own root is its bottom.)
fn bottom_branch() -> Result<Option<String>> {
    let current = crate::git::current_branch()?;
    Ok(stack::stack_line(&current)?.into_iter().next())
}

/// "Nothing to merge" message, tailored to call out the trunk - a natural
/// place to be standing, but never part of a stack - rather than implying the
/// repo has no stacks at all.
fn nothing_to_merge_hint() -> Result<String> {
    let current = crate::git::current_branch()?;
    let trunk = stack::trunk_branch(&crate::git::local_branches()?);
    // Only blame the trunk when it actually carries stacks: then standing on it
    // is the footgun. An empty repo on the trunk just has nothing to merge.
    let on_trunk_with_stacks =
        Some(&current) == trunk.as_ref() && !stack::children_for_branch(&current)?.is_empty();
    Ok(if on_trunk_with_stacks {
        format!("you are on the trunk ({current}); check out a stacked branch first")
    } else {
        "no stacked branches to merge".to_owned()
    })
}

/// The branch's review, validated as mergeable: it exists, is open, and
/// still targets the branch's stack parent.
fn open_review_for(
    review_provider: &dyn ReviewProvider,
    kind: ProviderKind,
    branch: &str,
) -> Result<ReviewRequest> {
    let Some(review) = review_provider.review_for_branch(branch)? else {
        bail!("no {kind} review found for {branch}; submit the stack first");
    };
    if review.state != ReviewState::Open {
        bail!(
            "review {} for {branch} is {}, not open",
            review.id,
            review.state
        );
    }

    let expected_base = stack::parent_for_branch(branch)?;
    if let Some(expected) = &expected_base
        && *expected != review.base
    {
        bail!(
            "review {} targets {}, but {branch}'s stack parent is {expected}; \
             run `git stk submit` first",
            review.id,
            review.base
        );
    }

    Ok(review)
}

enum MergeOutcome {
    Merged,
    Scheduled,
}

/// Merge the review and report what actually happened: gh --auto and glab's
/// default auto-merge schedule the merge instead of performing it, and only
/// a review that reads merged afterwards should start a sync.
fn merge_and_check(
    review_provider: &dyn ReviewProvider,
    review: &ReviewRequest,
    strategy: &str,
    auto: bool,
) -> Result<MergeOutcome> {
    let label = review.label();

    let output = match review_provider.merge_review(review, strategy, auto) {
        Ok(output) => output,
        Err(error) => return Err(explain_merge_failure(review_provider, review, error)),
    };
    if !output.is_empty() {
        println!("{output}");
    }

    match review_provider.review_for_branch(&review.branch)? {
        Some(after) if after.state == ReviewState::Merged => {
            anstream::println!("{}", style::success(&format!("merged {label}")));
            Ok(MergeOutcome::Merged)
        }
        _ => {
            anstream::println!(
                "{}",
                style::warn(&format!(
                    "merge scheduled for {label}; rerun `git stk sync` once checks pass"
                ))
            );
            Ok(MergeOutcome::Scheduled)
        }
    }
}

/// Turn a rejected merge into an actionable error. Ask the platform why from
/// its structured status first; only if that is inconclusive (or the query
/// itself fails) fall back to matching the CLI's error text, then surface the
/// raw error.
fn explain_merge_failure(
    review_provider: &dyn ReviewProvider,
    review: &ReviewRequest,
    error: anyhow::Error,
) -> anyhow::Error {
    match review_provider
        .merge_blocker(review)
        .unwrap_or(MergeBlocker::None)
    {
        MergeBlocker::ChecksPending => checks_not_green_error(review),
        MergeBlocker::Conflicts => anyhow::anyhow!(
            "{} conflicts with {} - resolve the conflicts, push, and rerun `git stk merge`",
            review.id,
            review.base
        ),
        // The platform did not say (or the status query failed): fall back to
        // the CLI's error wording before surfacing it raw.
        MergeBlocker::None => {
            let text = error.to_string().to_lowercase();
            if text.contains("status check") || text.contains("not mergeable") {
                checks_not_green_error(review)
            } else {
                error
            }
        }
    }
}

fn checks_not_green_error(review: &ReviewRequest) -> anyhow::Error {
    anyhow::anyhow!(
        "{}'s required checks are not green yet - wait and rerun `git stk merge`, \
         or schedule with `git stk merge --auto`",
        review.id
    )
}
