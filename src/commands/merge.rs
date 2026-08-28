use anyhow::{Result, bail};
use clap::ArgAction;

use crate::cli::PushMode;
use crate::commands::Run;
use crate::commands::sync::sync;
use crate::prompt::confirm;
use crate::providers::{
    MergeBlocker, ProviderKind, ReviewProvider, ReviewRequest, ReviewState, WaitOutcome,
    detect_review_provider,
};
use crate::settings;
use crate::stack;
use crate::style;

/// Merge the review at the bottom of the stack, then sync.
#[derive(Debug, clap::Args)]
pub struct Merge {
    /// Print what would happen without merging anything.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
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

    let (provider, review_provider) = detect_review_provider()?;
    let review = open_review_for(review_provider.as_ref(), provider.kind, &bottom)?;

    let strategy = settings::merge_strategy()?;
    let mode = if auto {
        format!("{strategy}, auto")
    } else {
        strategy.clone()
    };
    let label = review.label();

    if dry_run {
        // Same refusal the real run would raise, rather than advertising a
        // mode that is about to be declined - including in the mode string
        // itself, which is the last surface that would still say `auto`. Best
        // effort: a provider that cannot answer leaves the dry run as it was.
        let mut mode = mode;
        if auto
            && review_provider
                .native_stack_for(&review.branch)
                .is_ok_and(|found| found.is_some())
        {
            anstream::println!(
                "{}",
                style::warn(&format!(
                    "{} is in a stack the platform owns, so --auto would be refused; \
                     it merges when you run `git stk merge` with checks green",
                    review.id
                ))
            );
            mode = mode.replace(", auto", "");
        }
        anstream::println!("would merge {label} into {} ({mode})", review.base);
        anstream::println!("would sync afterwards");
        return Ok(());
    }

    if !yes
        && !confirm(&format!(
            "merge {label} into {} ({mode})? [y/N] ",
            review.base
        ))?
    {
        anstream::println!("merge cancelled");
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

    let (provider, review_provider) = detect_review_provider()?;
    let strategy = settings::merge_strategy()?;

    // What is about to land, bottom-up, for the dry run and the prompt: the
    // current branch's own line, not sibling stacks sharing the trunk.
    let current = crate::git::current_branch()?;
    let line = stack::stack_line(&current)?;
    let branches = stack::stacked_layers(&line)?;
    let count = branches.len();

    // An off-trunk line's base is not part of this landing, and it has to stay
    // that way for the whole loop rather than be re-derived each iteration:
    // the `sync` between merges re-records the base's parent from its own
    // review (#308), which would otherwise make it the lowest stacked branch
    // next time round and land it - unprompted, since the confirmation below
    // names it as the destination, not as something being merged.
    let pinned_base = stack::unanchored_base(&line)?;

    if dry_run {
        for branch in &branches {
            let review = open_review_for(review_provider.as_ref(), provider.kind, branch)?;
            if wait {
                anstream::println!("would wait for checks on {}", review.id);
            }
            anstream::println!(
                "would merge {} into {} ({strategy})",
                review.label(),
                review.base
            );
        }
        anstream::println!("would sync after each merge");
        return Ok(());
    }

    let base = stack::parent_of(&bottom)?.unwrap_or_else(|| "its base".to_owned());
    if !yes
        && !confirm(&format!(
            "merge {count} review{} into {base}, bottom-up ({strategy})? [y/N] ",
            if count == 1 { "" } else { "s" }
        ))?
    {
        anstream::println!("merge cancelled");
        return Ok(());
    }

    stack::snapshot("merge --all");

    // Each sync removes the merged bottom, so the loop is bounded by the
    // number of branches it started with.
    let mut landed = 0;
    for _ in 0..count {
        let Some(bottom) = bottom_branch_excluding(pinned_base.as_deref())? else {
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
            match review_provider.wait_for_checks(&review)? {
                WaitOutcome::Passed => {}
                WaitOutcome::Failed => bail!(
                    "checks failed for {}; fix them and rerun `git stk merge --all`",
                    review.id
                ),
                // Merged out-of-band while we waited: skip the redundant merge,
                // let sync reconcile it, and carry on with the next review.
                WaitOutcome::Landed => {
                    anstream::println!(
                        "{}",
                        style::warn(&format!(
                            "{} was merged outside git-stk; syncing instead",
                            review.id
                        ))
                    );
                    sync(false, PushMode::Config)?;
                    landed += 1;
                    continue;
                }
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

/// The bottom of the stack containing the current branch: the lowest branch on
/// its line that actually stacks on something. A line rooted off the trunk
/// keeps its parentless root - the base the branch above targets - and that
/// base is never merged: with no parent recorded there is nothing to check its
/// review against, and it is typically not ours to land (a release line, say).
fn bottom_branch() -> Result<Option<String>> {
    bottom_branch_excluding(None)
}

/// [`bottom_branch`], with `exclude` held out of the search by name. `merge
/// --all` pins the line's base this way: metadata written mid-run must not be
/// able to promote it into the landing.
fn bottom_branch_excluding(exclude: Option<&str>) -> Result<Option<String>> {
    let current = crate::git::current_branch()?;
    let line = stack::stack_line(&current)?;
    Ok(stack::stacked_layers(&line)?
        .into_iter()
        .find(|branch| Some(branch.as_str()) != exclude))
}

/// "Nothing to merge" message, tailored to call out the trunk - a natural
/// place to be standing, but never part of a stack - rather than implying the
/// repo has no stacks at all.
fn nothing_to_merge_hint() -> Result<String> {
    let current = crate::git::current_branch()?;
    let trunk = stack::trunk_branch(&crate::git::local_branches()?);
    // Only blame the trunk when the repo actually has a stack: then standing on
    // it is the footgun. An empty repo on the trunk just has nothing to merge.
    // "Has a stack" is not "the trunk has children" - a stack rooted off the
    // trunk leaves the trunk childless while plainly being one.
    let on_trunk_with_stacks = Some(&current) == trunk.as_ref() && stack::has_stacked_branches()?;
    if on_trunk_with_stacks {
        return Ok(format!(
            "you are on the trunk ({current}); check out a stacked branch first"
        ));
    }
    // Standing on a branch with no stack parent: there is a branch here, just
    // no base recorded to merge it into. Say which, rather than implying the
    // repo has no stacks.
    // A recorded base standing alone is not missing metadata - it is the
    // branch a stack sat on. Suggesting `adopt` here would re-root it: `adopt`
    // defaults to the branch you are on.
    if stack::is_floor(&current)? {
        return Ok(format!(
            "{current} is a stack's base, and nothing is stacked on it - \
             there is nothing to merge"
        ));
    }
    if Some(&current) != trunk.as_ref() && stack::parent_of(&current)?.is_none() {
        return Ok(format!(
            "{current} has no stack parent, so there is no base to merge it into; \
             attach it with `git stk adopt --parent <parent>`, or rebuild its metadata \
             with `git stk repair`"
        ));
    }
    Ok("no stacked branches to merge".to_owned())
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

    // A base and a local parent that disagree normally mean the review needs
    // resubmitting, and the merge would otherwise land into the wrong branch.
    //
    // There is one state where the disagreement is expected instead: a layer
    // that GitHub still owes a retarget. `cleanup` moves the local parent as
    // the layer below lands and deliberately leaves the review to GitHub,
    // which retargets it on its own clock - so between those two moments the
    // two differ, and bailing would stop `merge --all` halfway and name
    // `submit`, which refuses outright for a review in a stack.
    //
    // "Owed a retarget" is narrower than "in a stack", and the difference is
    // the stack's bottom - the layer `merge` acts on. Nothing lands below it,
    // so GitHub never moves its base, and a disagreement there is the
    // ordinary re-rooted-line bug this guard exists to catch. Only a layer
    // above the bottom is exempt.
    let expected_base = stack::parent_of(branch)?;
    if let Some(expected) = &expected_base
        && *expected != review.base
        && !review_provider
            .platform_will_base_on(&review, expected)
            .unwrap_or(false)
    {
        if review_provider
            .platform_refuses_base_change(&review)
            .unwrap_or(false)
        {
            bail!(
                "review {} targets {}, but {branch}'s stack parent is {expected} - \
                 it is in a platform stack, which has no base change left to make \
                 here and refuses one by hand; dissolve the stack on the platform, \
                 then run `git stk submit`",
                review.id,
                review.base
            );
        }
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
    // Our own refusal is already exact - re-diagnosing it against the merge
    // blocker can answer "--auto is not available here" with "rerun with
    // --auto", which is the reverse of what was said.
    if error
        .downcast_ref::<crate::providers::MergeRefused>()
        .is_some()
    {
        return error;
    }
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
