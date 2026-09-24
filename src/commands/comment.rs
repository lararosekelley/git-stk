use anyhow::{Result, bail};
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;
use crate::git;
use crate::providers::detect_review_provider;
use crate::style;

/// Post a comment on a branch's review - for instance to ask a review bot for
/// another pass.
#[derive(Debug, clap::Args)]
pub struct Comment {
    /// The comment to post, as the platform's markdown.
    #[arg(allow_hyphen_values = true)]
    message: String,
    /// Branch whose review to comment on (defaults to the current branch).
    #[arg(long, add = ArgValueCompleter::new(completions::branch_candidates))]
    branch: Option<String>,
    /// Print what would be posted without posting it.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
}

impl Run for Comment {
    fn run(self) -> Result<()> {
        comment(&self.message, self.branch.as_deref(), self.dry_run)
    }
}

fn comment(message: &str, branch: Option<&str>, dry_run: bool) -> Result<()> {
    if message.trim().is_empty() {
        bail!("the comment is empty");
    }
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    let (provider, review_provider) = detect_review_provider()?;

    // Closed-inclusive: platforms take comments on merged and closed reviews too.
    let Some(review) = review_provider.review_for_branch_including_closed(&branch)? else {
        bail!(
            "no {} review found for {branch}; submit it first with `git stk submit`",
            provider.kind
        );
    };

    if dry_run {
        anstream::println!("would comment on {} {}", review.id, style::dim(&review.url));
        return Ok(());
    }
    let output = review_provider.comment_on_review(&review, message)?;
    anstream::println!("commented on {} {}", review.id, style::dim(&review.url));
    if !output.is_empty() {
        anstream::println!("{}", style::dim(&output));
    }
    Ok(())
}
