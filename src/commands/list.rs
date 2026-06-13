use std::collections::BTreeMap;

use anyhow::Result;
use clap::ValueEnum;

use crate::commands::Run;
use crate::providers::{
    ReviewRequest, ReviewState, detect_review_provider, owned_review_for_branch,
};
use crate::{git, stack};

/// Branch -> open-review id (e.g. `#12`), in one provider call. Best effort:
/// an absent or failing provider (offline, no gh/glab) yields an empty map, so
/// the tree still prints, just without numbers.
fn review_numbers() -> BTreeMap<String, String> {
    let Some((_, provider)) = detect_review_provider().ok() else {
        return BTreeMap::new();
    };
    let Ok(reviews) = provider.open_reviews() else {
        return BTreeMap::new();
    };
    reviews
        .into_iter()
        .map(|review| (review.branch, review.id))
        .collect()
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
}

impl Run for List {
    fn run(self) -> Result<()> {
        match (self.format, self.all) {
            (Some(format), _) => list_formatted(format),
            (None, true) => crate::stack::print_all_stacks(&review_numbers()),
            (None, false) => crate::stack::print_stack(&review_numbers()),
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
    let branches: Vec<String> = stack::branch_and_descendants(&root)?
        .into_iter()
        .skip(1) // the root is the base, not part of the stack
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
                    review.label(),
                    review.url,
                    review.state
                );
            }
            (Format::Markdown, None) => println!("{number}. `{branch}` (no review)"),
            // The bare URL on its own line is what chat apps auto-link.
            (Format::Plain, Some(review)) => {
                println!("{number}. {} - {}", review.label(), review.state);
                println!("   {}", review.url);
            }
            (Format::Plain, None) => println!("{number}. {branch} (no review)"),
        }
    }

    Ok(())
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
