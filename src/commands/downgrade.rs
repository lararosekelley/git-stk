use anyhow::Result;
use clap::ArgAction;

use crate::commands::Run;

/// Step git-stk back to an earlier release.
#[derive(Debug, clap::Args)]
pub struct Downgrade {
    /// Downgrade to this version instead of the release just before the
    /// installed one. Must be older than the installed version and no older
    /// than the earliest release `downgrade` can reach.
    #[arg(long)]
    to: Option<String>,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y', action = ArgAction::SetTrue)]
    yes: bool,
}

impl Run for Downgrade {
    fn run(self) -> Result<()> {
        crate::upgrade::downgrade(self.to, self.yes)
    }
}
