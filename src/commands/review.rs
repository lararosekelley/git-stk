use anyhow::{Result, bail};
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;
use crate::git;
use crate::providers::detect_review_provider;
use crate::style;

/// Print the review request for a branch.
#[derive(Debug, clap::Args)]
pub struct Review {
    #[arg(add = ArgValueCompleter::new(completions::branch_candidates))]
    branch: Option<String>,
}

impl Run for Review {
    fn run(self) -> Result<()> {
        print_review(self.branch.as_deref())
    }
}

pub fn print_review(branch: Option<&str>) -> Result<()> {
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    let (provider, review_provider) = detect_review_provider()?;

    let Some(review) = review_provider.review_for_branch(&branch)? else {
        bail!("no {} review found for {branch}", provider.kind);
    };

    anstream::println!(
        "{} {} -> {} {} {}",
        review.id,
        style::branch(&review.branch),
        style::branch(&review.base),
        style::state(&review.state),
        style::dim(&review.url)
    );
    Ok(())
}
