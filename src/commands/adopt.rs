use anyhow::{Context, Result};
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;

/// Attach an existing branch to a parent. With no arguments, adopts the
/// branch you are on onto the trunk.
#[derive(Debug, clap::Args)]
pub struct Adopt {
    /// The branch to adopt (defaults to the current branch).
    #[arg(add = ArgValueCompleter::new(completions::branch_candidates))]
    branch: Option<String>,
    /// The stack parent (defaults to the trunk).
    #[arg(long, add = ArgValueCompleter::new(completions::branch_candidates))]
    parent: Option<String>,
    /// Print what would change without writing any metadata.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
}

impl Run for Adopt {
    fn run(self) -> Result<()> {
        let branch = match self.branch {
            Some(branch) => branch,
            None => crate::git::current_branch()?,
        };
        let parent = match self.parent {
            Some(parent) => parent,
            None => crate::stack::trunk_branch(&crate::git::local_branches()?)
                .context("could not detect the trunk branch; pass --parent")?,
        };
        crate::stack::adopt_branch(&branch, &parent, self.dry_run)
    }
}
