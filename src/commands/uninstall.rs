use anyhow::Result;
use clap::ArgAction;

use crate::commands::Run;

/// Remove what `git stk setup` and the installer added.
///
/// Shell completion wiring, the man page, and the config/receipt directory.
/// The binary itself is reported, not deleted.
#[derive(Debug, clap::Args)]
pub struct Uninstall {
    /// Print what would be removed without removing anything.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y', action = ArgAction::SetTrue)]
    yes: bool,
}

impl Run for Uninstall {
    fn run(self) -> Result<()> {
        crate::setup::uninstall(self.dry_run, self.yes)
    }
}
