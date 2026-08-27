use anyhow::{Result, bail};
use clap::ArgAction;

use crate::commands::Run;
use crate::providers::detect_review_provider;
use crate::stack;
use crate::style;

/// Dissolve the platform's own stack for the current stack, leaving its
/// reviews open and standalone.
///
/// Only GitHub keeps stacks. Registering one is opt-in via `stk.githubStacks`;
/// dissolving one is not, because a stack outlives the setting that created it
/// and may have been made outside git-stk entirely.
#[derive(Debug, clap::Args)]
pub struct Unstack {
    /// Print what would change without calling the platform.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
}

impl Run for Unstack {
    fn run(self) -> Result<()> {
        let current = crate::git::current_branch()?;
        let line = stack::stack_line(&current)?;
        let Some(bottom) = stack::stacked_layers(&line)?.into_iter().next() else {
            bail!("no stacked branches here; nothing to unstack");
        };

        let (_, review_provider) = detect_review_provider()?;
        let Some(found) = review_provider.native_stack_for(&bottom)? else {
            anstream::println!(
                "{}",
                style::dim("no platform stack recorded for this stack; nothing to dissolve")
            );
            return Ok(());
        };

        if self.dry_run {
            anstream::println!(
                "would dissolve stack {} ({})",
                found.number,
                found
                    .layers
                    .iter()
                    .map(|layer| layer.id.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            return Ok(());
        }

        match review_provider.unstack_reviews(&found)? {
            Some(line) => anstream::println!("{line}"),
            None => anstream::println!(
                "{}",
                style::dim("this provider does not keep stacks; nothing to dissolve")
            ),
        }
        Ok(())
    }
}
