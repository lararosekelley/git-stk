use anyhow::Result;
use clap::ArgAction;

use crate::cli::{PushMode, UpdateRefsMode};
use crate::commands::Run;

/// Rebase every branch in the current stack onto its parent, from
/// anywhere in the stack.
#[derive(Debug, clap::Args)]
pub struct Restack {
    /// Pass --update-refs to git rebase.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_update_refs")]
    update_refs: bool,
    /// Do not pass --update-refs to git rebase.
    #[arg(long, action = ArgAction::SetTrue)]
    no_update_refs: bool,
    /// Force-push (with lease) every rebased branch afterwards.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_push")]
    push: bool,
    /// Do not push rebased branches, overriding stk.pushOnRestack.
    #[arg(long, action = ArgAction::SetTrue)]
    no_push: bool,
    /// Print the rebase plan without rebasing anything.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
}

impl Run for Restack {
    fn run(self) -> Result<()> {
        crate::stack::restack(
            UpdateRefsMode::from_flags(self.update_refs, self.no_update_refs),
            PushMode::from_flags(self.push, self.no_push),
            self.dry_run,
        )
    }
}

/// Continue an interrupted restack after resolving conflicts.
#[derive(Debug, clap::Args)]
pub struct Continue {}

impl Run for Continue {
    fn run(self) -> Result<()> {
        crate::stack::continue_restack()
    }
}

/// Abort an interrupted restack.
#[derive(Debug, clap::Args)]
pub struct Abort {}

impl Run for Abort {
    fn run(self) -> Result<()> {
        crate::stack::abort_restack()
    }
}
