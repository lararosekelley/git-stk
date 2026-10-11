use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::browser;
use crate::commands::Run;
use crate::completions;
use crate::git;
use crate::providers::{ReviewRef, detect_review_provider};
use crate::style;
use crate::tour::Tour as ReviewTour;

/// Open a step-by-step tour of a review's changes in the browser
/// (experimental).
#[derive(Debug, clap::Args)]
pub struct Tour {
    /// Branch, review number (12, #12), or review URL (defaults to the current
    /// branch).
    #[arg(value_name = "TARGET", add = ArgValueCompleter::new(completions::branch_candidates))]
    target: Option<String>,
    /// Write the tour without opening it.
    #[arg(long, action = ArgAction::SetTrue)]
    no_open: bool,
    /// Write the tour to this file instead of under the repository's git
    /// directory.
    #[arg(long, short = 'o', value_name = "PATH")]
    output: Option<PathBuf>,
}

impl Run for Tour {
    fn run(self) -> Result<()> {
        write_tour(self.target.as_deref(), self.output, !self.no_open)
    }
}

fn write_tour(target: Option<&str>, output: Option<PathBuf>, open: bool) -> Result<()> {
    let (provider, review_provider) = detect_review_provider()?;
    let review = match ReviewRef::parse(target, provider.kind)? {
        ReviewRef::Number(number) => review_provider.review_by_number(number)?,
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

    let label = review.label();
    let tour = ReviewTour::load(provider.kind, review_provider.as_ref(), review)?;
    let path = match output {
        Some(path) => path,
        None => git::git_common_path_absolute("stk/tours")?.join(tour.file_name()),
    };
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, tour.render())
        .with_context(|| format!("failed to write {}", path.display()))?;

    let files = tour.file_count();
    anstream::println!(
        "wrote a tour of {label} ({files} {}) to {}",
        if files == 1 { "file" } else { "files" },
        style::dim(&path.display().to_string())
    );
    if open && let Err(error) = browser::open(&path) {
        anstream::eprintln!("could not open a browser ({error:#}); open the file above instead");
    }
    Ok(())
}
