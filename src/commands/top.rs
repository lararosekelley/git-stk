use anyhow::Result;

use crate::commands::Run;

/// Move to the top of the stack: check out its leaf branch.
#[derive(Debug, clap::Args)]
pub struct Top {
    /// Print the destination directory instead of announcing the switch, so
    /// `cd "$(git stk top --from-path)"` follows the branch - including into another
    /// worktree, which cannot be checked out here.
    #[arg(long)]
    from_path: bool,
}

impl Run for Top {
    fn run(self) -> Result<()> {
        crate::stack::checkout_top(self.nav_output())
    }
}

impl Top {
    fn nav_output(&self) -> crate::stack::NavOutput {
        if self.from_path {
            crate::stack::NavOutput::Path
        } else {
            crate::stack::NavOutput::Announce
        }
    }
}
