use anyhow::{Result, bail};
use clap::ArgAction;

use crate::commands::Run;
use crate::prompt::confirm;
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
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y', action = ArgAction::SetTrue)]
    yes: bool,
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

        // What is about to happen, before any of it does. A stack is dissolved
        // whole, several can cover one line, and the line reaches the whole
        // subtree above you - so this can take apart reviews that are nowhere
        // on screen. There is no undo: `undo` restores local metadata, and
        // this is a `POST`.
        for stack in &found {
            anstream::println!(
                "{} dissolve stack {} ({})",
                if self.dry_run { "would" } else { "will" },
                stack.number,
                stack
                    .layers
                    .iter()
                    .map(|layer| layer.id.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        if self.dry_run {
            return Ok(());
        }

        let reviews: usize = found.iter().map(|stack| stack.layers.len()).sum();
        if !self.yes
            && !confirm(&format!(
                "dissolve {} stack{}, leaving {reviews} review{} standalone? [y/N] ",
                found.len(),
                if found.len() == 1 { "" } else { "s" },
                if reviews == 1 { "" } else { "s" }
            ))?
        {
            anstream::println!("unstack cancelled");
            return Ok(());
        }

        // Keep going after a failure rather than leaving the rest of the line
        // silently still stacked: which ones survived is the whole answer for
        // a command someone reaches for to get unstuck.
        let mut failed: Vec<(u64, anyhow::Error)> = Vec::new();
        for stack in &found {
            match review_provider.unstack_reviews(stack) {
                Ok(Some(line)) => anstream::println!("{line}"),
                Ok(None) => anstream::println!(
                    "{}",
                    style::dim("this provider does not keep stacks; nothing to dissolve")
                ),
                Err(error) => failed.push((stack.number, error)),
            }
        }

        if let Some((_, first)) = failed.pop() {
            for (number, error) in &failed {
                anstream::eprintln!("{}", style::warn(&format!("stack {number}: {error:#}")));
            }
            return Err(first);
        }
        Ok(())
    }
}
