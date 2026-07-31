use anyhow::Result;

use crate::commands::Run;

/// Move to the bottom of the stack: check out the branch just above the
/// trunk.
#[derive(Debug, clap::Args)]
pub struct Bottom {
    /// Print the destination directory instead of announcing the switch, so
    /// `cd "$(git stk bottom --from-path)"` follows the branch - including into another
    /// worktree, which cannot be checked out here.
    #[arg(long)]
    from_path: bool,
}

impl Run for Bottom {
    fn run(self) -> Result<()> {
        crate::stack::checkout_bottom(self.nav_output())
    }
}

impl Bottom {
    fn nav_output(&self) -> crate::stack::NavOutput {
        if self.from_path {
            crate::stack::NavOutput::Path
        } else {
            crate::stack::NavOutput::Announce
        }
    }
}
