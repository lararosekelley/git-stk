use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use clap::ValueEnum;

use crate::commands::Run;
use crate::providers::{
    ReviewAnnotation, ReviewRequest, ReviewState, detect_review_provider, label,
    owned_review_for_branch,
};
use crate::{git, stack};

/// Branch -> its open-review annotation (id, CI status, queue state, and -
/// with `detail` - review tallies), scoped to the `listed` branches the tree
/// draws so the provider never queries every open PR in the repo. The provider
/// batches this as tightly as it can (GitHub: one GraphQL call). Best effort:
/// an absent or failing provider (offline, no gh/glab) yields an empty map, so
/// the tree still prints, just without annotations.
fn review_annotations(
    listed: &BTreeSet<String>,
    detail: bool,
) -> BTreeMap<String, ReviewAnnotation> {
    let Some((_, provider)) = detect_review_provider().ok() else {
        return BTreeMap::new();
    };
    let branches: Vec<String> = listed.iter().cloned().collect();
    provider
        .annotate_branches(&branches, detail)
        .unwrap_or_default()
}

/// A shareable rendering of the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Markdown links - perfect in tracking issues and PR comments.
    Markdown,
    /// Plain text with bare URLs, for anywhere that does not render markdown
    /// links from a paste (e.g. Slack).
    Plain,
}

/// Print the current stack.
#[derive(Debug, clap::Args)]
pub struct List {
    /// Render a shareable summary instead of the tree.
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// Show every stack, not just the one you are on.
    #[arg(long, conflicts_with = "format")]
    all: bool,
    /// List each branch's own commits (short SHA + subject) beneath it.
    #[arg(long, conflicts_with = "format")]
    commits: bool,
    /// List each review's approvals, comments, and requested changes beneath it.
    #[arg(long, conflicts_with_all = ["format", "commits", "local"])]
    reviews: bool,
    /// Skip all provider lookups: draw the tree from local metadata only, with
    /// no review numbers, CI status, or queue info. Never touches the network.
    #[arg(long, conflicts_with_all = ["format", "reviews"])]
    local: bool,
}

impl Run for List {
    fn run(self) -> Result<()> {
        if let Some(format) = self.format {
            return list_formatted(format);
        }
        // --local draws from git metadata alone; otherwise annotate, scoping the
        // provider lookups to just the branches this tree draws.
        let annotations = if self.local {
            BTreeMap::new()
        } else {
            review_annotations(&stack::listed_branches(self.all)?, self.reviews)
        };
        if self.all {
            stack::print_all_stacks(&annotations, self.commits)
        } else {
            stack::print_stack(&annotations, self.commits)
        }
    }
}

/// Print the stack as a copy-paste summary for sharing: a summary line, then
/// the PRs bottom-to-top (merge order) with title, link/url, and state.
/// Degrades to plain branch names when reviews or the provider CLI are
/// unavailable.
pub fn list_formatted(format: Format) -> Result<()> {
    let current = git::current_branch()?;
    let root = stack::stack_root(&current)?;
    // Scope to the current branch's own line, like the tree view: sibling
    // stacks that only share the trunk are left out. The base (trunk, or an
    // unanchored root branch) is the summary's base, not a row.
    let branches: Vec<String> = stack::current_stack_branches(&current)?
        .into_iter()
        .filter(|branch| *branch != root)
        .collect();

    if branches.is_empty() {
        println!("no stacked branches");
        return Ok(());
    }

    let review_provider = detect_review_provider().ok().map(|(_, client)| client);
    let entries: Vec<(String, Option<ReviewRequest>)> = branches
        .iter()
        .map(|branch| {
            let review = review_provider
                .as_ref()
                .and_then(|rp| owned_review_for_branch(&**rp, branch).ok().flatten());
            (branch.clone(), review)
        })
        .collect();

    println!("{}", summary(&entries, &root, format));
    println!();
    for (index, (branch, review)) in entries.iter().enumerate() {
        let number = index + 1;
        match (format, review) {
            (Format::Markdown, Some(review)) => {
                println!(
                    "{number}. [{}]({}) - {}",
                    labeled_with_size(review, branch),
                    review.url,
                    review.state
                );
            }
            (Format::Markdown, None) => println!("{number}. `{branch}` (no review)"),
            // The bare URL on its own line is what chat apps auto-link.
            (Format::Plain, Some(review)) => {
                println!(
                    "{number}. {} - {}",
                    labeled_with_size(review, branch),
                    review.state
                );
                println!("   {}", review.url);
            }
            (Format::Plain, None) => println!("{number}. {branch} (no review)"),
        }
    }

    Ok(())
}

/// The review label with the branch's diff size folded into the id's
/// parentheses after a comma - e.g. `Title (#12, +9/-0)` - mirroring the tree.
/// The size is dropped when zero or unavailable, leaving the plain label.
fn labeled_with_size(review: &ReviewRequest, branch: &str) -> String {
    let id = match branch_diff_size(branch) {
        Some((added, deleted)) => format!("{}, +{added}/-{deleted}", review.id),
        None => review.id.clone(),
    };
    label(&review.title, &id)
}

/// A branch's diff size against its stack parent, dropped when zero or
/// unavailable - the same count the `git stk list` tree shows.
fn branch_diff_size(branch: &str) -> Option<(usize, usize)> {
    let parent = stack::parent_of(branch).ok().flatten()?;
    let (added, deleted) = git::diff_numstat(&parent, branch).ok()?;
    (added > 0 || deleted > 0).then_some((added, deleted))
}

/// One-line stack summary, e.g. "3 PRs, base `main`, 2 open / 1 merged"
/// (the base is unquoted in plain format).
fn summary(entries: &[(String, Option<ReviewRequest>)], base: &str, format: Format) -> String {
    let total = entries.len();
    let reviews: Vec<&ReviewRequest> = entries.iter().filter_map(|(_, r)| r.as_ref()).collect();
    let base = match format {
        Format::Markdown => format!("`{base}`"),
        Format::Plain => base.to_owned(),
    };

    let mut summary = if reviews.is_empty() {
        format!(
            "{total} branch{}, base {base}",
            if total == 1 { "" } else { "es" }
        )
    } else if reviews.len() == total {
        format!(
            "{total} PR{}, base {base}",
            if total == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "{total} branches ({} with reviews), base {base}",
            reviews.len()
        )
    };

    if !reviews.is_empty() {
        let mut counts = Vec::new();
        for (state, label) in [
            (ReviewState::Open, "open"),
            (ReviewState::Merged, "merged"),
            (ReviewState::Closed, "closed"),
        ] {
            let count = reviews
                .iter()
                .filter(|review| review.state == state)
                .count();
            if count > 0 {
                counts.push(format!("{count} {label}"));
            }
        }
        if !counts.is_empty() {
            summary.push_str(&format!(", {}", counts.join(" / ")));
        }
    }

    summary
}
