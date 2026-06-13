use anyhow::{Result, bail};
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;
use crate::git;
use crate::providers::detect_review_provider;
use crate::style;

/// Open a branch's review in the browser.
#[derive(Debug, clap::Args)]
pub struct View {
    #[arg(add = ArgValueCompleter::new(completions::branch_candidates))]
    branch: Option<String>,
}

impl Run for View {
    fn run(self) -> Result<()> {
        view(self.branch.as_deref())
    }
}

fn view(branch: Option<&str>) -> Result<()> {
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    let (provider, review_provider) = detect_review_provider()?;

    // Closed-inclusive: opening a merged or closed review is still useful.
    let Some(review) = review_provider.review_for_branch_including_closed(&branch)? else {
        bail!(
            "no {} review found for {branch}; submit it first with `git stk submit`",
            provider.kind
        );
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
