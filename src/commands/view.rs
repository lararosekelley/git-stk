use anyhow::{Result, bail};
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;
use crate::providers::{ReviewRef, detect_review_provider};
use crate::style;

/// Open a branch's review in the browser.
#[derive(Debug, clap::Args)]
pub struct View {
    /// Branch, review number (12, #12), or review URL (defaults to the current
    /// branch).
    #[arg(value_name = "TARGET", add = ArgValueCompleter::new(completions::branch_candidates))]
    target: Option<String>,
}

impl Run for View {
    fn run(self) -> Result<()> {
        view(self.target.as_deref())
    }
}

fn view(target: Option<&str>) -> Result<()> {
    let (provider, review_provider) = detect_review_provider()?;
    let review = match ReviewRef::parse(target, provider.kind)? {
        ReviewRef::Number(number) => review_provider.review_by_number(number)?,
        // Closed-inclusive: opening a merged or closed review is still useful.
        ReviewRef::Branch(branch) => {
            match review_provider.review_for_branch_including_closed(&branch)? {
                Some(review) => review,
                None => bail!(
                    "no {} review found for {branch}; submit it first with `git stk submit`",
                    provider.kind
                ),
            }
        }
    };

    anstream::println!(
        "opening {} {} {}",
        review.id,
        style::state(&review.state),
        style::dim(&review.url)
    );
    let output = review_provider.open_review(&review)?;
    if !output.is_empty() {
        println!("{output}");
    }
    Ok(())
}
