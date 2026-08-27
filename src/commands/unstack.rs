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
    /// Look the stack up and print what would be dissolved, without
    /// dissolving it.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
}

impl Run for Unstack {
    fn run(self) -> Result<()> {
        let current = crate::git::current_branch()?;
        let line = stack::stack_line(&current)?;
        let layers = stack::stacked_layers(&line)?;
        if layers.is_empty() {
            bail!("no stacked branches here; nothing to unstack");
        }

        // Every layer, not just the bottom: a stack need not begin where your
        // local line does, and one made outside git-stk need not align with it
        // at all. A failed lookup is an error here, not "no stack" - this is
        // the whole command, so answering "already gone" for an expired token
        // would be a lie.
        let (_, review_provider) = detect_review_provider()?;
        let Some(found) = review_provider.native_stack_covering(&layers)? else {
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
