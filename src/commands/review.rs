use anyhow::{Result, bail};
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;
use crate::providers::{ReviewRef, detect_review_provider};
use crate::style;

/// Print the review request for a branch.
#[derive(Debug, clap::Args)]
pub struct Review {
    /// Branch, review number (12, #12), or review URL (defaults to the current
    /// branch).
    #[arg(value_name = "TARGET", add = ArgValueCompleter::new(completions::branch_candidates))]
    target: Option<String>,
}

impl Run for Review {
    fn run(self) -> Result<()> {
        print_review(self.target.as_deref())
    }
}

fn print_review(target: Option<&str>) -> Result<()> {
    let (provider, review_provider) = detect_review_provider()?;
    let review = match ReviewRef::parse(target, provider.kind)? {
        ReviewRef::Number(number) => review_provider.review_by_number(number)?,
        ReviewRef::Elsewhere { id, .. } => return Err(ReviewRef::elsewhere(&id)),
        ReviewRef::Branch(branch) => match review_provider.review_for_branch(&branch)? {
            Some(review) => review,
            None => bail!("no {} review found for {branch}", provider.kind),
        },
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
