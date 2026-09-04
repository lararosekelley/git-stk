use anyhow::{Result, bail};
use clap::ArgAction;

use crate::cli::PushMode;
use crate::commands::Run;
use crate::commands::sync::sync;
use crate::prompt::confirm;
use crate::providers::{
    BaseGap, MergeBlocker, ProviderKind, ReviewProvider, ReviewRequest, ReviewState, WaitOutcome,
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
    /// Repeat merge-and-sync bottom-up until the whole stack has landed, or
    /// hand the stack to the merge queue in one call when one governs it.
    #[arg(long, action = ArgAction::SetTrue)]
    all: bool,
    /// With --all: wait for each review's checks before merging it - the top
    /// alone on the merge-queue handover, which makes one merge.
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
    match merge_and_check(
        review_provider.as_ref(),
        &review,
        &strategy,
        auto,
        QueuedNext::Sync,
    )? {
        // Reconcile everything the merge changed: fetch, clean up, restack,
        // push.
        MergeOutcome::Merged => sync(false, PushMode::Config),
        MergeOutcome::Enqueued | MergeOutcome::Scheduled => Ok(()),
    }
}

/// Land the whole stack: merge the bottom review and sync, bottom-up, until
/// the stack is complete. One confirmation up front; a merge that only gets
/// scheduled stops the loop, and with `wait` each review's checks settle
/// before its merge.
///
/// A merge queue takes the stack whole instead - see [`queued_stack_top`] -
/// so that path makes one merge, waits on that review only, and reports for
/// the line rather than looping.
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
    let base = stack::parent_of(&bottom)?.unwrap_or_else(|| "its base".to_owned());

    // A merge queue takes a stack whole, so the top is one call for the entire
    // line rather than one landing per layer.
    let queue_top = queued_stack_top(review_provider.as_ref(), &branches, &base);

    if dry_run {
        if let Some(top) = &queue_top {
            let review = open_review_for(review_provider.as_ref(), provider.kind, top)?;
            if wait {
                anstream::println!("would wait for checks on {}", review.id);
            }
            anstream::println!(
                "would add {count} reviews to {base}'s merge queue by merging {}",
                review.label()
            );
            return Ok(());
        }
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

    let prompt = match &queue_top {
        Some(_) => format!("add {count} reviews to {base}'s merge queue? [y/N] "),
        None => format!(
            "merge {count} review{} into {base}, bottom-up ({strategy})? [y/N] ",
            if count == 1 { "" } else { "s" }
        ),
    };
    if !yes && !confirm(&prompt)? {
        anstream::println!("merge cancelled");
        return Ok(());
    }

    stack::snapshot("merge --all");

    if let Some(top) = queue_top {
        return enqueue_whole_stack(
            review_provider.as_ref(),
            provider.kind,
            &top,
            &strategy,
            count,
            wait,
        );
    }

    // Each sync removes the merged bottom, so the loop is bounded by the
    // number of branches it started with.
    let mut landed = 0;
    let mut enqueued = false;
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
                WaitOutcome::Inconclusive => bail!(
                    "checks for {} stopped without a verdict - a cancelled run, or one \
                     waiting on a person; resolve it and rerun `git stk merge --all`",
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

        // `landed` merges are behind us and this is the next, so anything
        // above it is what makes a rerun worth naming.
        let above = landed + 1 < count;
        match merge_and_check(
            review_provider.as_ref(),
            &review,
            &strategy,
            false,
            if above {
                QueuedNext::SyncThen("git stk merge --all")
            } else {
                QueuedNext::Sync
            },
        )? {
            MergeOutcome::Merged => {
                sync(false, PushMode::Config)?;
                landed += 1;
            }
            MergeOutcome::Enqueued => {
                enqueued = true;
                break;
            }
            MergeOutcome::Scheduled => break,
        }
    }

    let mut summary = format!(
        "merge complete: {landed} of {count} review{} merged",
        if count == 1 { "" } else { "s" }
    );
    // Otherwise a run that did everything available to it reads as a run that
    // did nothing: the queue holds the only review that could have landed.
    if enqueued {
        summary.push_str(", 1 in the merge queue");
    }
    anstream::println!("{}", style::success(&summary));
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
    // The question is narrower than "is it in a stack": can the stack still
    // bring this base to the parent we have? It can reach the layer recorded
    // below and the stack's own base, and nowhere else - so a re-rooted or
    // reordered line, and the stack's bottom, get the ordinary refusal this
    // guard exists for.
    let expected_base = stack::parent_of(branch)?;
    if let Some(expected) = &expected_base
        && *expected != review.base
    {
        match review_provider.base_gap(&review, expected).unwrap_or(None) {
            // The platform is going to close this itself, as the layer below
            // lands. Carrying on is right: `merge --all` would otherwise stop
            // halfway and name `submit`, which refuses a review in a stack.
            Some(BaseGap::Platform) => {}
            Some(BaseGap::Sync) => bail!(
                "review {} already targets {} - the platform moved it when {expected} \
                 landed, and {branch}'s stack parent has not caught up; run \
                 `git stk sync` first",
                review.id,
                review.base
            ),
            Some(BaseGap::Neither) => bail!(
                "review {} targets {}, but {branch}'s stack parent is {expected} - \
                 its stack will not move it there, and the platform refuses a \
                 change by hand; run `git stk unstack`, then \
                 `git stk submit`",
                review.id,
                review.base
            ),
            None => bail!(
                "review {} targets {}, but {branch}'s stack parent is {expected}; \
                 run `git stk submit` first",
                review.id,
                review.base
            ),
        }
    }

    Ok(review)
}

/// What to tell the reader after a merge queue takes the review.
enum QueuedNext<'a> {
    /// `sync` clears the landed layer, then this command carries the stack on.
    SyncThen(&'a str),
    /// `sync` clears it, and that is all: plain `merge` has landed what it was
    /// asked to, and the last layer leaves nothing above it to merge.
    Sync,
    /// Nothing per review - one call handed the queue the whole line, and the
    /// caller reports for the stack.
    Deferred,
}

enum MergeOutcome {
    Merged,
    /// Handed to a merge queue. Distinct from `Scheduled` because the two
    /// resume differently: a scheduled merge is waiting on checks and `sync`
    /// picks it up, while a queued one has to *land* before the layer above
    /// targets the queue's branch and becomes eligible at all.
    Enqueued,
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
    next: QueuedNext<'_>,
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
        // A queue and a scheduled auto-merge both leave the review open, and
        // only the provider can tell them apart. Ask before falling back to
        // the scheduled wording, whose *condition* is wrong for a queue: the
        // entry lands on the queue's schedule, not when checks pass.
        //
        // Its command survives, though, and has to. A landed entry leaves the
        // layer merged on the platform and still recorded locally, so a merge
        // rerun straight away hits `open_review_for` and bails with "merged,
        // not open" - `sync` is what clears the layer and makes the next one
        // the bottom.
        _ if queued(review_provider, review) => {
            // What follows differs by scope. A rerun is only worth naming
            // while a layer remains *above* this one - `merge` lands the
            // bottom and is then done, and on the last layer `sync` leaves
            // nothing to merge at all, so naming a command there sends the
            // reader into "no stacked branches to merge". And when one call
            // handed the queue the whole line there is nothing to say per
            // review: the caller reports for the stack.
            let message = match next {
                QueuedNext::SyncThen(rerun) => format!(
                    "{label} is in the merge queue; once it lands, `git stk sync` \
                     reconciles the stack - then `{rerun}` to carry on"
                ),
                QueuedNext::Sync => format!(
                    "{label} is in the merge queue; once it lands, `git stk sync` \
                     reconciles the stack"
                ),
                QueuedNext::Deferred => format!("{label} is in the merge queue"),
            };
            anstream::println!("{}", style::warn(&message));
            Ok(MergeOutcome::Enqueued)
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

/// The top branch to merge when a merge queue will take the whole stack, or
/// `None` when `merge --all` should walk it bottom-up.
///
/// Three things have to hold, and a failure of any reads as `None`:
///
/// - a platform stack, since merging a layer only cascades down one - without
///   it the call merges that review into the layer below rather than landing
///   the line;
/// - that stack landing where the local line says it does, since the queue is
///   a property of the branch it lands in;
/// - its open layers being exactly the line, since the cascade takes every
///   open layer below the top.
fn queued_stack_top(
    review_provider: &dyn ReviewProvider,
    branches: &[String],
    base: &str,
) -> Option<String> {
    // One layer is the same call either way, so leave it on the path whose
    // reporting and sync behaviour is already settled.
    if branches.len() < 2 {
        return None;
    }
    let top = branches.last()?;
    let stack = review_provider.native_stack_for(top).ok().flatten()?;
    // The queue is a property of the branch the *stack* lands in, which is the
    // one its bottom review targets - not the parent recorded locally. Those
    // drift (`adopt --parent`, `repair`, a review opened before the local
    // parent moved), and the drift is invisible here: the top's own base is
    // still the layer below it, so `open_review_for` passes and the prompt
    // names a base the cascade never touches. The bottom-up walk refuses that
    // state outright through `base_gap`; this path has to refuse it too.
    if stack.base != base {
        return None;
    }
    if !review_provider.base_has_merge_queue(base).unwrap_or(false) {
        return None;
    }
    // The cascade takes the top and *everything open below it* in the platform
    // stack, so the two sets have to match exactly. Containment one way is not
    // enough: an open layer below the line's bottom - an off-trunk base, a
    // branch this checkout does not carry - would land in the same call,
    // unnamed by the prompt and unchecked by `open_review_for`, which the
    // bottom-up walk runs per layer.
    let position = stack.layers.iter().position(|layer| layer.branch == *top)?;
    let cascade: Vec<&String> = stack.layers[..=position]
        .iter()
        .filter(|layer| layer.open)
        .map(|layer| &layer.branch)
        .collect();
    (cascade.len() == branches.len() && branches.iter().all(|branch| cascade.contains(&branch)))
        .then(|| top.clone())
}

/// Hand the whole stack to the merge queue with one merge of its top.
fn enqueue_whole_stack(
    review_provider: &dyn ReviewProvider,
    kind: ProviderKind,
    top: &str,
    strategy: &str,
    count: usize,
    wait: bool,
) -> Result<()> {
    let review = open_review_for(review_provider, kind, top)?;
    if wait {
        anstream::println!(
            "waiting for checks on {} {}",
            review.id,
            style::dim("(ctrl-c is safe; rerun `git stk merge --all` to resume)")
        );
        match review_provider.wait_for_checks(&review)? {
            WaitOutcome::Passed => {}
            // Merged out-of-band while we waited. Merging it now would ask the
            // async endpoint to land a closed review and surface a raw `gh`
            // error; the bottom-up walk syncs instead, and so does this.
            WaitOutcome::Landed => {
                anstream::println!(
                    "{}",
                    style::warn(&format!(
                        "{} was merged outside git-stk; syncing instead",
                        review.id
                    ))
                );
                return sync(false, PushMode::Config);
            }
            WaitOutcome::Failed => bail!(
                "checks failed for {}; fix them and rerun `git stk merge --all`",
                review.id
            ),
            WaitOutcome::Inconclusive => bail!(
                "checks for {} stopped without a verdict - a cancelled run, or one \
                 waiting on a person; resolve it and rerun `git stk merge --all`",
                review.id
            ),
        }
    }

    match merge_and_check(
        review_provider,
        &review,
        strategy,
        false,
        QueuedNext::Deferred,
    )? {
        MergeOutcome::Enqueued => {
            anstream::println!(
                "{}",
                style::success(&format!(
                    "merge complete: {count} reviews added to the merge queue; \
                     `git stk sync` reconciles them as they land"
                ))
            );
            Ok(())
        }
        // The queue answered for the base a moment ago, so reaching either of
        // these means it stopped applying mid-run. Reported rather than
        // smoothed over: what landed is not what the prompt described.
        MergeOutcome::Merged => {
            anstream::println!(
                "{}",
                style::warn(&format!(
                    "{} merged without the queue taking the stack; \
                     rerun `git stk merge --all` for the rest",
                    review.id
                ))
            );
            sync(false, PushMode::Config)
        }
        // Enqueued but unconfirmed: the merge went through and `queued` could
        // not read the entry back. `merge_and_check` has already printed the
        // scheduled wording, whose condition is wrong for a queue, so say what
        // actually happened rather than returning bare on it.
        MergeOutcome::Scheduled => {
            anstream::println!(
                "{}",
                style::warn(&format!(
                    "{count} reviews were handed to the merge queue, but {}'s entry \
                     could not be read back; `git stk list` shows the queue once it \
                     registers",
                    review.id
                ))
            );
            Ok(())
        }
    }
}

/// Whether the review is sitting in a merge queue. Best-effort: the default
/// implementation is empty and a failed lookup degrades to `false`, which only
/// costs the queue wording, so it is never worth failing a merge over.
fn queued(review_provider: &dyn ReviewProvider, review: &ReviewRequest) -> bool {
    review_provider
        .enqueued_branches(std::slice::from_ref(&review.branch))
        .map(|queued| queued.contains(&review.branch))
        .unwrap_or(false)
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
    // Whether scheduling is even on the table here - the same question the dry
    // run asks before printing the mode.
    let can_schedule = !review_provider
        .native_stack_for(&review.branch)
        .is_ok_and(|found| found.is_some());
    match review_provider
        .merge_blocker(review)
        .unwrap_or(MergeBlocker::None)
    {
        MergeBlocker::ChecksPending => checks_not_green_error(review, can_schedule),
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
                checks_not_green_error(review, can_schedule)
            } else {
                error
            }
        }
    }
}

fn checks_not_green_error(review: &ReviewRequest, can_schedule: bool) -> anyhow::Error {
    // `--auto` is refused for a review in a platform stack, so recommending it
    // there answers one refusal with another.
    if can_schedule {
        anyhow::anyhow!(
            "{}'s required checks are not green yet - wait and rerun `git stk merge`, \
             or schedule with `git stk merge --auto`",
            review.id
        )
    } else {
        anyhow::anyhow!(
            "{}'s required checks are not green yet - wait and rerun `git stk merge`; \
             `--auto` is not available for a review in a stack",
            review.id
        )
    }
}
