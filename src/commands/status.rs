use anyhow::Result;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;
use crate::providers::{ReviewState, detect_review_provider};
use crate::style;
use crate::{git, stack};

/// Print local and remote stack status for a branch.
#[derive(Debug, clap::Args)]
pub struct Status {
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
    let parent = stack::parent_of(&branch)?;
    let children = stack::children_of(&branch)?;

    anstream::println!("branch: {}", style::paint(style::CURRENT, &branch));
    match parent.as_deref() {
        Some(parent) => anstream::println!("parent: {}", style::paint(style::BRANCH, parent)),
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
                    anstream::println!(
                        "review: {} {} {} -> {}",
                        review.id,
                        style::state(&review.state),
                        style::paint(style::BRANCH, &review.branch),
                        style::paint(style::BRANCH, &review.base)
                    );
                    anstream::println!("url: {}", style::paint(style::DIM, &review.url));

                    if let Some(parent) = parent.as_deref()
                        && parent != review.base
                    {
                        anstream::println!(
                            "{} review base is {}, local parent is {parent} - run `git stk submit`",
                            style::paint(style::WARN, "warning:"),
                            review.base
                        );
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
    match &review {
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
                    match parent_review.state {
                        ReviewState::Merged => hints.push(format!(
                            "parent review {} is merged - run `git stk sync`",
                            parent_review.id
                        )),
                        ReviewState::Closed => hints.push(format!(
                            "parent review {} was closed without merging - \
                             retarget {branch} with `git stk adopt`",
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
