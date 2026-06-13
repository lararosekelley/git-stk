use anyhow::{Result, bail};
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::cli::PushMode;
use crate::commands::Run;
use crate::completions;
use crate::providers::{ReviewProvider, detect_review_provider};
use crate::settings;
use crate::style;
use crate::{git, stack};

/// Create or update a remote review request for a branch.
#[derive(Debug, clap::Args)]
pub struct Submit {
    #[arg(add = ArgValueCompleter::new(completions::branch_candidates))]
    branch: Option<String>,
    /// Print what would change without creating or updating reviews.
    #[arg(long, action = ArgAction::SetTrue)]
    dry_run: bool,
    /// Submit the whole stack parent-first, from anywhere in it.
    #[arg(long, conflicts_with = "branch")]
    stack: bool,
    /// Submit only the current branch, overriding stk.submitStack.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "stack")]
    no_stack: bool,
    /// Submit the stack from its bottom through the current branch only,
    /// leaving work-in-progress branches above it unsubmitted.
    #[arg(
        long,
        action = ArgAction::SetTrue,
        conflicts_with_all = ["branch", "stack", "no_stack"],
    )]
    downstack: bool,
    /// Push branches (-u --force-with-lease) before submitting.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_push")]
    push: bool,
    /// Do not push branches, overriding stk.pushOnSubmit.
    #[arg(long, action = ArgAction::SetTrue)]
    no_push: bool,
    /// Set a description block at the top of the review body; an empty
    /// string clears it. Applies to the current or named branch only.
    #[arg(long, short = 'd')]
    desc: Option<String>,
    /// Create new reviews as drafts.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_draft")]
    draft: bool,
    /// Create new reviews ready for review, overriding stk.submitDraft.
    #[arg(long, action = ArgAction::SetTrue)]
    no_draft: bool,
    /// Mark the submitted branches' existing draft reviews as ready.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "draft")]
    ready: bool,
    /// Rebuild each review's stack overview from the live stack plus merged
    /// history, dropping closed or orphaned rows that drifted in. Stack mode.
    #[arg(long, action = ArgAction::SetTrue)]
    rebuild_overview: bool,
}

impl Run for Submit {
    fn run(self) -> Result<()> {
        // Stack mode: --stack forces it on; --no-stack or an explicit branch
        // forces it off; otherwise stk.submitStack decides.
        let submit_stack = if self.stack {
            true
        } else if self.no_stack || self.branch.is_some() {
            false
        } else {
            settings::bool_setting(settings::SUBMIT_STACK_KEY)?
        };

        // Draft mode: --draft forces it on, --no-draft off; otherwise
        // stk.submitDraft decides.
        let draft = if self.draft {
            true
        } else if self.no_draft {
            false
        } else {
            settings::bool_setting(settings::SUBMIT_DRAFT_KEY)?
        };

        submit(
            self.branch.as_deref(),
            submit_stack,
            self.downstack,
            self.dry_run,
            PushMode::from_flags(self.push, self.no_push),
            self.desc.as_deref(),
            draft,
            self.ready,
            self.rebuild_overview,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub fn submit(
    branch: Option<&str>,
    submit_stack: bool,
    downstack: bool,
    dry_run: bool,
    push_mode: crate::cli::PushMode,
    desc: Option<&str>,
    draft: bool,
    ready: bool,
    rebuild_overview: bool,
) -> Result<()> {
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    // The description targets this branch's review even in stack mode.
    let desc_branch = branch.clone();

    let branches = if downstack {
        // Bottom of the stack through the current branch: anything above is
        // work in progress that stays local.
        stack::path_from_root(&branch)?
    } else if submit_stack {
        // The stack containing the current branch: its own line, bottom
        // through current and out to its descendants. Sibling stacks that
        // merely share the trunk are left for their own submit.
        stack::stack_line(&branch)?
    } else {
        vec![branch.clone()]
    };

    // The trunk is never part of a stack, so a stack-wide submit from it has
    // nothing of its own to submit (its descendants are sibling stacks). Say so
    // plainly rather than pushing an empty set or sweeping every stack.
    if submit_stack || downstack {
        let trunk = stack::trunk_branch(&git::local_branches()?);
        if Some(&branch) == trunk.as_ref() {
            if stack::children_for_branch(&branch)?.is_empty() {
                bail!("no stacked branches to submit");
            }
            bail!("you are on the trunk ({branch}); check out a stacked branch first");
        }
    }

    let branch_parents = branch_parents(&branches)?;

    // Push after stack validation but before any provider calls: creating a
    // review requires the branch to exist remotely, and -u --force-with-lease
    // covers both first pushes and safely updating rebased branches.
    let push = settings::push_enabled(push_mode, settings::PUSH_ON_SUBMIT_KEY)?;
    if push {
        let remote = settings::remote()?;
        if dry_run {
            anstream::println!(
                "would push {} to {remote}",
                style::branch(&branches.join(" "))
            );
        } else {
            git::push_set_upstream_force_with_lease(&remote, &branches)?;
            anstream::println!("pushed {} to {remote}", style::branch(&branches.join(" ")));
            // Carry the stack's parent map along so another clone can rebuild
            // it with `git stk repair --from-remote`.
            stack::publish_metadata(&remote);
        }
    }

    let (_, review_provider) = detect_review_provider()?;
    let mut summary = SubmitSummary::default();

    for (branch, parent) in &branch_parents {
        summary.record(submit_branch(
            review_provider.as_ref(),
            branch,
            parent,
            dry_run,
            draft,
        )?);
    }

    // Flip drafts in scope to ready for review (the escape hatch for
    // stk.submitDraft users).
    if ready {
        for branch in &branches {
            let Some(review) = review_provider.review_for_branch(branch)? else {
                continue;
            };
            if review.branch != *branch || !review.draft {
                continue;
            }
            if dry_run {
                println!("would mark {} ready", review.id);
                continue;
            }
            let output = review_provider.mark_ready(&review)?;
            anstream::println!("marked {} ready", review.id);
            if !output.is_empty() {
                println!("{output}");
            }
        }
    }

    // A renamed branch's fresh review now exists, so retire the review the old
    // name still heads. Only handle this when the ledger prune below actually
    // runs (stack-wide submit): the marker is the sole signal that identifies
    // the stale row across every other overview, so closing and clearing it in
    // a single-branch submit - which never prunes - would orphan those rows
    // permanently. Left set, the marker waits for a later `submit --stack`.
    let renamed: Vec<(String, String)> = if submit_stack || downstack {
        branch_parents
            .iter()
            .filter_map(|(branch, _)| {
                stack::renamed_from(branch)
                    .ok()
                    .flatten()
                    .map(|old| (branch.clone(), old))
            })
            .collect()
    } else {
        Vec::new()
    };
    for (_, old) in &renamed {
        close_superseded_review(review_provider.as_ref(), old, dry_run)?;
    }

    // After every review exists, write the description, link any issue the
    // branch name references, then (in stack mode) write the stack overview
    // into each body.
    if let Some(desc) = desc {
        crate::notes::update_description_note(
            review_provider.as_ref(),
            &desc_branch,
            desc,
            dry_run,
        )?;
    }
    crate::notes::update_closes_notes(review_provider.as_ref(), &branches, dry_run)?;
    if submit_stack || downstack {
        crate::notes::update_stack_notes(
            review_provider.as_ref(),
            &branch_parents,
            dry_run,
            rebuild_overview,
        )?;
    }

    // The ledger has now pruned the superseded entries, so drop the markers.
    if !dry_run {
        for (branch, _) in &renamed {
            stack::clear_renamed_from(branch)?;
        }
    }

    anstream::println!(
        "{}",
        style::success(&format!(
            "submit complete: {} created, {} updated, {} skipped",
            summary.created, summary.updated, summary.skipped
        ))
    );
    Ok(())
}

/// Retire the open review still heading a renamed-away branch. The fresh
/// review already exists, so closing here never leaves the work without one.
/// Prompts (default yes; a non-interactive run proceeds) before closing.
fn close_superseded_review(
    review_provider: &dyn ReviewProvider,
    old: &str,
    dry_run: bool,
) -> Result<()> {
    let Some(review) = review_provider.review_for_branch(old)? else {
        return Ok(());
    };
    if review.branch != *old {
        return Ok(());
    }

    if dry_run {
        println!("would close superseded review {} for {old}", review.id);
        return Ok(());
    }
    if !crate::prompt::confirm_default_yes(&format!(
        "close the replaced review {} for {old} and delete its branch? [Y/n] ",
        review.id
    ))? {
        println!("kept review {} for {old}", review.id);
        return Ok(());
    }

    review_provider.close_review(&review, true)?;
    anstream::println!("closed superseded review {} for {old}", review.id);
    Ok(())
}

fn branch_parents(branches: &[String]) -> Result<Vec<(String, String)>> {
    let mut branch_parents = Vec::new();
    for branch in branches {
        let Some(parent) = stack::parent_for_branch(branch)? else {
            bail!("{branch} has no stack parent; run `git stk adopt` or `git stk sync` first");
        };
        branch_parents.push((branch.to_owned(), parent));
    }
    Ok(branch_parents)
}

fn submit_branch(
    review_provider: &dyn ReviewProvider,
    branch: &str,
    parent: &str,
    dry_run: bool,
    draft: bool,
) -> Result<SubmitAction> {
    if let Some(review) = review_provider.review_for_branch(branch)? {
        if review.base == parent {
            if dry_run {
                println!(
                    "would skip {} -> {} ({})",
                    review.branch, review.base, review.id
                );
            } else {
                anstream::println!(
                    "{}",
                    style::dim(&format!(
                        "{} already targets {} ({})",
                        review.branch, review.base, review.id
                    ))
                );
            }
            return Ok(SubmitAction::Skipped);
        }

        let output = if dry_run {
            String::new()
        } else {
            review_provider.update_review_base(&review, parent)?
        };
        anstream::println!(
            "{} {} -> {} {}",
            if dry_run { "would update" } else { "updated" },
            style::branch(&review.branch),
            style::branch(parent),
            style::dim(&format!("({})", review.id))
        );
        if !output.is_empty() {
            println!("{output}");
        }
    } else {
        let output = if dry_run {
            String::new()
        } else {
            review_provider.create_review(branch, parent, draft)?
        };
        anstream::println!(
            "{} {} -> {}",
            if dry_run { "would create" } else { "created" },
            style::branch(branch),
            style::branch(parent)
        );
        if !output.is_empty() {
            println!("{output}");
        }
        return Ok(SubmitAction::Created);
    }

    Ok(SubmitAction::Updated)
}

#[derive(Debug, Default)]
struct SubmitSummary {
    created: usize,
    updated: usize,
    skipped: usize,
}

impl SubmitSummary {
    fn record(&mut self, action: SubmitAction) {
        match action {
            SubmitAction::Created => self.created += 1,
            SubmitAction::Updated => self.updated += 1,
            SubmitAction::Skipped => self.skipped += 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SubmitAction {
    Created,
    Updated,
    Skipped,
}
