use anyhow::Result;

use crate::cli::parse_distance;
use crate::commands::Run;

/// Move down the stack: check out the current branch's parent.
#[derive(Debug, clap::Args)]
pub struct Down {
    /// How many branches to move down.
    #[arg(value_name = "COUNT", value_parser = parse_distance, allow_negative_numbers = true)]
    count: Option<usize>,
    /// Print the destination directory instead of announcing the switch, so
    /// `cd "$(git stk down --from-path)"` follows the branch - including into another
    /// worktree, which cannot be checked out here.
    #[arg(long)]
    from_path: bool,
}

impl Run for Down {
    fn run(self) -> Result<()> {
        crate::stack::checkout_parent(self.count.unwrap_or(1), self.nav_output())
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
