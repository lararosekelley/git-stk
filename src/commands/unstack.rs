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
        if line.is_empty() {
            bail!("not on a stacked branch; nothing to unstack");
        }

        // The whole line, not just the layers git-stk records a parent for:
        // a stack need not begin where your local line does, one made outside
        // git-stk need not align with it at all or be adopted here, and the
        // line's own base can be a layer of the platform's. A failed lookup is
        // an error here, not "no stack" - this is the whole command, so
        // answering "already gone" for an expired token would be a lie.
        let (_, review_provider) = detect_review_provider()?;
        let found = review_provider.native_stacks_covering(&line)?;
        if found.is_empty() {
            anstream::println!(
                "{}",
                style::dim("no platform stack recorded for this stack; nothing to dissolve")
            );
            return Ok(());
        }

        for stack in &found {
            if self.dry_run {
                anstream::println!(
                    "would dissolve stack {} ({})",
                    stack.number,
                    stack
                        .layers
                        .iter()
                        .map(|layer| layer.id.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                continue;
            }
            match review_provider.unstack_reviews(stack)? {
                Some(line) => anstream::println!("{line}"),
                None => anstream::println!(
                    "{}",
                    style::dim("this provider does not keep stacks; nothing to dissolve")
                ),
            }
        }
        Ok(())
    }
}
