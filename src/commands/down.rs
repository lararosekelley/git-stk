use anyhow::Result;

use crate::commands::Run;

/// Move down the stack: check out the current branch's parent.
#[derive(Debug, clap::Args)]
pub struct Down {
    /// Print the destination directory instead of announcing the switch, so
    /// `cd "$(git stk down --from-path)"` follows the branch - including into another
    /// worktree, which cannot be checked out here.
    #[arg(long)]
    from_path: bool,
}

impl Run for Down {
    fn run(self) -> Result<()> {
        crate::stack::checkout_parent(self.nav_output())
    }
}

impl Down {
    fn nav_output(&self) -> crate::stack::NavOutput {
        if self.from_path {
            crate::stack::NavOutput::Path
        } else {
            crate::stack::NavOutput::Announce
        }
    }
}
