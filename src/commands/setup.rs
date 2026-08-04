use anyhow::Result;
use clap::ArgAction;

use crate::commands::Run;

/// Install the man page and wire up shell completions.
#[derive(Debug, clap::Args)]
pub struct Setup {
    /// Skip confirmation prompts.
    #[arg(long, short = 'y', action = ArgAction::SetTrue)]
    yes: bool,
    /// Only re-render generated assets (man page); never touch shell rc files.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "yes")]
    refresh: bool,
    /// Also define an `stk` shell function whose up/down/top/bottom cd into the
    /// worktree holding the branch (bash and zsh only).
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "refresh")]
    wrapper: bool,
}

impl Run for Setup {
    fn run(self) -> Result<()> {
        crate::setup::setup(self.yes, self.refresh, self.wrapper)
    }
}
