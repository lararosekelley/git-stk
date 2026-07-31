use anyhow::Result;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::Run;
use crate::completions;

/// Move up the stack: check out a child of the current branch.
#[derive(Debug, clap::Args)]
pub struct Up {
    #[arg(add = ArgValueCompleter::new(completions::child_branch_candidates))]
    branch: Option<String>,
    /// Print the destination directory instead of announcing the switch, so
    /// `cd "$(git stk up --from-path)"` follows the branch - including into another
    /// worktree, which cannot be checked out here.
    #[arg(long)]
    from_path: bool,
}

impl Run for Up {
    fn run(self) -> Result<()> {
        crate::stack::checkout_child(self.branch.as_deref(), self.nav_output())
    }
}

impl Up {
    fn nav_output(&self) -> crate::stack::NavOutput {
        if self.from_path {
            crate::stack::NavOutput::Path
        } else {
            crate::stack::NavOutput::Announce
        }
    }
}
