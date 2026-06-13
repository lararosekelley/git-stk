use anyhow::Result;
use clap::ArgAction;

use crate::commands::Run;

/// Create a new child branch from the current branch.
#[derive(Debug, clap::Args)]
pub struct New {
    branch: String,
    /// Insert above the current branch, moving its children onto the new one.
    #[arg(long, conflicts_with = "prepend")]
    insert: bool,
    /// Insert below the current branch, moving it onto the new one.
    #[arg(long)]
    prepend: bool,
    /// Print what would change (the branch, and any retargeted children)
    /// without creating or moving anything.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
}

impl Run for New {
    fn run(self) -> Result<()> {
        if self.insert {
            crate::stack::insert_branch(&self.branch, self.dry_run)
        } else if self.prepend {
            crate::stack::prepend_branch(&self.branch, self.dry_run)
        } else {
            crate::stack::create_branch(&self.branch, self.dry_run)
        }
    }
}
