use std::process::Command;

use anyhow::{Result, bail};
use clap::ArgAction;

use crate::stack;
use crate::style;

/// Run a command on every branch in the stack, bottom-up, with a pass/fail summary.
///
/// Answers "does each layer build on its own?" before submitting - each PR is
/// supposed to be independently green.
#[derive(Debug, clap::Args)]
pub struct Run {
    /// Stop at the first branch whose command fails.
    #[arg(long, action = ArgAction::SetTrue)]
    fail_fast: bool,
    /// The command to run on each branch (everything after `--`).
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required = true,
        num_args = 1..,
        value_name = "CMD"
    )]
    command: Vec<String>,
}

impl crate::commands::Run for Run {
    fn run(self) -> Result<()> {
        // Switching branches with uncommitted changes would drag them across
        // the stack or fail outright; require a clean tree.
        if !crate::git::worktree_is_clean()? {
            bail!("working tree has uncommitted changes; commit or stash before `git stk run`");
        }

        let original = crate::git::current_branch()?;
        let branches = stack::current_stack_branches(&original)?;

        if branches.is_empty() {
            bail!("no stacked branches to run on");
        }

        let (program, args) = self
            .command
            .split_first()
            .expect("clap requires at least one command word");

        // Always return to where we started, even if a checkout or the
        // command errors partway through.
        let result = run_each(&branches, program, args, self.fail_fast);
        let _ = crate::git::checkout(&original);
        let results = result?;

        print_summary(&results);

        if results.iter().any(|(_, passed)| !passed) {
            bail!("`{program}` failed on one or more branches");
        }
        Ok(())
    }
}

/// Check out each branch in turn and run the command, collecting pass/fail.
fn run_each(
    branches: &[String],
    program: &str,
    args: &[String],
    fail_fast: bool,
) -> Result<Vec<(String, bool)>> {
    let mut results = Vec::new();
    for branch in branches {
        crate::git::checkout(branch)?;
        anstream::println!("{}", style::branch(branch));
        // Inherit stdio so the command's output streams through live.
        let passed = Command::new(program)
            .args(args)
            .status()
            .is_ok_and(|status| status.success());
        results.push((branch.clone(), passed));
        if !passed && fail_fast {
            break;
        }
    }
    Ok(results)
}

fn print_summary(results: &[(String, bool)]) {
    let width = results.iter().map(|(b, _)| b.len()).max().unwrap_or(0);
    anstream::println!();
    for (branch, passed) in results {
        let pad = " ".repeat(width - branch.len());
        let marker = if *passed {
            style::success("ok")
        } else {
            style::paint(style::CLOSED, "FAIL")
        };
        anstream::println!("  {}{pad}  {marker}", style::branch(branch));
    }

    let passed = results.iter().filter(|(_, passed)| *passed).count();
    let total = results.len();
    anstream::println!(
        "{}",
        style::dim(&format!(
            "ran on {total} branch{}, {passed} passed",
            if total == 1 { "" } else { "es" }
        ))
    );
}
