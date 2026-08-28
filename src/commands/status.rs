use anyhow::Result;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;
use crate::providers::{BaseGap, CheckStatus, ReviewState, detect_review_provider};
use crate::style;
use crate::{git, stack};

/// Print local and remote stack status for a branch.
#[derive(Debug, clap::Args)]
pub struct Status {
    /// Branch to report on (defaults to the current branch).
    #[arg(add = ArgValueCompleter::new(completions::branch_candidates))]
    branch: Option<String>,
}

impl Run for Status {
    fn run(self) -> Result<()> {
        print_status(self.branch.as_deref())
    }
}

pub fn print_status(branch: Option<&str>) -> Result<()> {
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    // Marker-aware, like every other reader: a base with a stray `stkParent`
    // has no parent for any purpose, and reporting one here would produce a
    // "run `git stk restack`" hint that `restack` cannot act on.
    let is_base = stack::is_floor(&branch)?;
    let parent = stack::stacked_parent_of(&branch)?;
    let children = stack::children_of(&branch)?;

    anstream::println!("branch: {}", style::paint(style::CURRENT, &branch));
    // Where the branch actually lives, when that is not here. Best effort: a
    // failed listing just omits the line.
    if let Some(path) = git::worktree_holding(&branch).ok().flatten() {
        anstream::println!("worktree: {}", git::display_path(&path));
    }
    match parent.as_deref() {
        Some(parent) => anstream::println!("parent: {}", style::paint(style::BRANCH, parent)),
        // A recorded base has no parent by design, not by omission - say which,
        // so it is not read as a branch whose metadata went missing.
        None if is_base => anstream::println!("parent: none (this stack's base)"),
        None => anstream::println!("parent: none"),
    }
    if children.is_empty() {
        anstream::println!("children: none");
    } else {
        let children: Vec<String> = children
            .iter()
            .map(|child| style::paint(style::BRANCH, child))
            .collect();
        anstream::println!("children: {}", children.join(", "));
    }

    // Provider state is best-effort: a repo with no remote (or no provider
    // configured) still shows its local stack rather than hard-failing.
    let detected = detect_review_provider().ok();
    let review = match &detected {
        Some((provider, review_provider)) => {
            anstream::println!("provider: {} ({})", provider.kind, provider.source);
            // Closed-inclusive: a review closed without merging is part of the
            // branch's story, not "no review".
            let review = review_provider.review_for_branch_including_closed(&branch)?;
            match &review {
                Some(review) => {
                    // One call for the queue state, the CI rollup, and the
                    // platform stack together - the same query `list` makes,
                    // so the two cannot disagree about a stack's size. It
                    // reads open reviews only, so a merged or closed one falls
                    // back to the per-call path below.
                    let annotation = review_provider
                        .annotate_branches(std::slice::from_ref(&review.branch), false)
                        .ok()
                        .and_then(|mut found| found.remove(&review.branch));

                    // A queued review shows just the clock (it is waiting to
                    // land); otherwise the CI dot. Both best-effort - a failed
                    // lookup just omits the marker. When queued, the CI dot is
                    // suppressed, so skip fetching it.
                    let queued = match &annotation {
                        Some(annotation) => annotation.queued,
                        None => review_provider
                            .enqueued_branches(std::slice::from_ref(&review.branch))
                            .map(|set| set.contains(&review.branch))
                            .unwrap_or(false),
                    };
                    let marker = if queued {
                        crate::providers::QUEUED_MARK
                    } else {
                        match &annotation {
                            Some(annotation) => annotation.checks.dot(),
                            None => review_provider
                                .check_status(review)
                                .unwrap_or(CheckStatus::None)
                                .dot(),
                        }
                    };
                    anstream::println!(
                        "review: {marker}{} {} {} -> {}",
                        review.id,
                        style::state(&review.state),
                        style::paint(style::BRANCH, &review.branch),
                        style::paint(style::BRANCH, &review.base)
                    );
                    anstream::println!("url: {}", style::paint(style::DIM, &review.url));
                    // The platform's own stack, when it holds this review -
                    // which is what makes GitHub, not git-stk, the one that
                    // merges and retargets it. Both halves of the ratio come
                    // from whichever source answered, never one from each.
                    let stack = match &annotation {
                        Some(annotation) => annotation
                            .stack
                            .map(|at| (at.number, Some((at.position as usize, at.size as usize)))),
                        // The annotate query cannot see a merged or closed
                        // review, and a landed layer stays listed in its
                        // stack, so this is the only way to name it there.
                        None => review_provider
                            .native_stack_for(&review.branch)
                            .ok()
                            .flatten()
                            .map(|found| {
                                let at = found
                                    .position_of(&review.branch)
                                    .map(|at| (at as usize, found.layers.len()));
                                (found.number, at)
                            }),
                    };
                    if let Some((number, at)) = stack {
                        let position =
                            at.map_or_else(String::new, |(at, size)| format!(" ({at} of {size})"));
                        anstream::println!("stack: {} stack {number}{position}", provider.kind);
                    }

                    // A base and a local parent that disagree while the
                    // platform's stack can still close the gap is a chain
                    // part-way through unwinding, not a fault - and `submit`,
                    // which the warning names, refuses a review in a stack.
                    if let Some(parent) = parent.as_deref()
                        && parent != review.base
                    {
                        // Asked only on a disagreement, which is rare: the
                        // lookup costs a call, and the annotation cannot
                        // answer it.
                        match review_provider.base_gap(review, parent).unwrap_or(None) {
                            Some(BaseGap::Platform) => anstream::println!(
                                "{}",
                                style::paint(
                                    style::DIM,
                                    &format!(
                                        "review base is {}, local parent is {parent} - \
                                         the platform retargets it as the layer below lands",
                                        review.base
                                    )
                                )
                            ),
                            Some(BaseGap::Sync) => anstream::println!(
                                "{} review base is {}, local parent is {parent} - the \
                                 platform moved it when {parent} landed; run `git stk sync`",
                                style::paint(style::WARN, "warning:"),
                                review.base
                            ),
                            Some(BaseGap::Neither) => anstream::println!(
                                "{} review base is {}, local parent is {parent} - the \
                                 platform will not move it there and refuses a change by \
                                 hand; dissolve the stack on the platform, then run \
                                 `git stk submit`",
                                style::paint(style::WARN, "warning:"),
                                review.base
                            ),
                            None => anstream::println!(
                                "{} review base is {}, local parent is {parent} - run `git stk submit`",
                                style::paint(style::WARN, "warning:"),
                                review.base
                            ),
                        }
                    }
                }
                None => anstream::println!("review: none"),
            }
            review
        }
        None => {
            anstream::println!("{}", style::dim("provider: not detected (no review info)"));
            None
        }
    };

    // Teach the loop: the next command, derived from review states and
    // local drift. A sync covers the restack, so the nudges don't stack.
    let mut hints = Vec::new();
    if is_base {
        hints.push(format!(
            "{branch} is this stack's base, so nothing rebases, submits, or lands it - \
             `git stk detach {branch}` if it should be"
        ));
    }
    match &review {
        // `sync` and `cleanup` both skip a base on purpose, so the usual
        // remedies can never be satisfied here. Say what actually happened
        // rather than reprint a dead end every run.
        Some(review)
            if is_base && matches!(review.state, ReviewState::Merged | ReviewState::Closed) =>
        {
            hints.push(format!(
                "review {} is {} - git-stk leaves a stack's base alone, so this is yours to \
                 finish; `git stk detach {branch}` first if it should be managed",
                review.id,
                style::state(&review.state)
            ));
        }
        Some(review) if review.state == ReviewState::Merged => {
            hints.push(format!(
                "review {} is merged - run `git stk sync`",
                review.id
            ));
        }
        Some(review) if review.state == ReviewState::Closed => {
            hints.push(format!(
                "review {} was closed without merging - `git stk submit` opens a new review",
                review.id
            ));
        }
        _ => {}
    }
    if let Some(parent) = parent.as_deref() {
        if let Some((_, review_provider)) = &detected {
            match review_provider.review_for_branch_including_closed(parent) {
                Ok(Some(parent_review)) if parent_review.branch == parent => {
                    // `sync` skips a base before `landing_for`, so it never
                    // retargets a layer off one - pointing there would reprint
                    // every run. Name the re-root instead.
                    let parent_is_base = stack::is_floor(parent)?;
                    match parent_review.state {
                        // Only the states `landing_for` would have acted on:
                        // `Unknown(_)` covers things like GitLab's `locked`,
                        // which is still running, and printed nothing before.
                        _ if parent_is_base
                            && matches!(
                                parent_review.state,
                                ReviewState::Merged | ReviewState::Closed
                            ) =>
                        {
                            hints.push(format!(
                                "parent review {} is {} - {parent} is this stack's base, so \
                                 git-stk does not retarget off it; re-root with \
                                 `git stk adopt {branch} --parent <parent>`",
                                parent_review.id,
                                style::state(&parent_review.state)
                            ));
                        }
                        ReviewState::Merged => hints.push(format!(
                            "parent review {} is merged - run `git stk sync`",
                            parent_review.id
                        )),
                        ReviewState::Closed => hints.push(format!(
                            "parent review {} was closed without merging - \
                             retarget with `git stk adopt {branch} --parent <parent>`",
                            parent_review.id
                        )),
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        if hints.is_empty()
            && let Some(hint) = stack::behind_parent_hint(&branch, parent)
        {
            hints.push(hint);
        }
    }
    for hint in hints {
        anstream::println!("{} {hint}", style::paint(style::HINT, "hint:"));
    }

    Ok(())
}
