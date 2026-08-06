use anyhow::Result;
use clap_complete::engine::ArgValueCompleter;

use crate::cli::parse_distance;
use crate::commands::Run;
use crate::completions;

/// Move up the stack: check out a child of the current branch.
#[derive(Debug, clap::Args)]
pub struct Up {
    /// A child branch to check out, or how many branches to move up.
    #[arg(
        value_name = "BRANCH|COUNT",
        value_parser = parse_target,
        allow_negative_numbers = true,
        add = ArgValueCompleter::new(completions::child_branch_candidates)
    )]
    target: Option<Target>,
    /// Print the destination directory instead of announcing the switch, so
    /// `cd "$(git stk up --from-path)"` follows the branch - including into another
    /// worktree, which cannot be checked out here.
    #[arg(long)]
    from_path: bool,
}

/// Where to go, or how far: a number is read as a distance, so a branch whose
/// name is only digits has to be reached by name from elsewhere.
#[derive(Debug, Clone)]
enum Target {
    Branch(String),
    Distance(usize),
}

fn parse_target(value: &str) -> Result<Target, String> {
    if value.parse::<i64>().is_ok() {
        return parse_distance(value).map(Target::Distance);
    }
    Ok(Target::Branch(value.to_owned()))
}

impl Run for Up {
    fn run(self) -> Result<()> {
        let output = self.nav_output();
        match self.target {
            Some(Target::Branch(branch)) => crate::stack::checkout_child(Some(&branch), 1, output),
            Some(Target::Distance(distance)) => {
                crate::stack::checkout_child(None, distance, output)
            }
            None => crate::stack::checkout_child(None, 1, output),
        }
    }
}

impl Up {
    fn nav_output(&self) -> crate::stack::NavOutput {
        if self.from_path {
            crate::stack::NavOutput::Path
        } else {
            crate::stack::NavOutput::Announce
        }
    }
}
