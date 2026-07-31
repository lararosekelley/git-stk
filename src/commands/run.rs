use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, anyhow, bail};
use clap::ArgAction;

use crate::stack;
use crate::style;

/// Where the scratch worktree lives: under the common git dir, so it is out of
/// the way of file watchers and ignore rules, invisible to `git status`, and
/// shared by every worktree of the repo. One fixed path is safe because `run`
/// holds the git-stk lock for its whole window, so no second run can be using it.
const SCRATCH_DIR: &str = "git-stk-run-worktree";

/// Run a command on every branch in the stack, bottom-up, with a pass/fail summary.
///
/// Answers "does each layer build on its own?" before submitting - each PR is
/// supposed to be independently green.
#[derive(Debug, clap::Args)]
pub struct Run {
    /// Stop at the first branch whose command fails.
    #[arg(long, action = ArgAction::SetTrue)]
    fail_fast: bool,
    /// Walk your own checkout across the stack instead of using a scratch
    /// worktree. Needed when the command depends on untracked build output
    /// (`node_modules`, `target/`) that a fresh worktree will not have; requires
    /// a clean tree, and moves HEAD while it runs.
    #[arg(long, action = ArgAction::SetTrue)]
    no_worktree: bool,
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
        let original = crate::git::current_branch()?;
        let branches = stack::current_stack_branches(&original)?;

        if branches.is_empty() {
            bail!("no stacked branches to run on");
        }

        let (program, args) = self
            .command
            .split_first()
            .expect("clap requires at least one command word");

        let results = if self.no_worktree {
            // Walking the user's own checkout: switching with uncommitted
            // changes would drag them across the stack or fail outright.
            if !crate::git::worktree_is_clean()? {
                bail!(
                    "working tree has uncommitted changes; commit or stash before \
                     `git stk run --no-worktree`"
                );
            }
            // Always return to where we started, even if a checkout or the
            // command errors partway through.
            let result = run_each_in_place(&branches, program, args, self.fail_fast);
            let _ = crate::git::checkout(&original);
            result?
        } else {
            // A scratch worktree leaves the user's tree - and their HEAD -
            // completely alone, so uncommitted work is no obstacle.
            let scratch = ScratchWorktree::create(&branches[0])?;
            run_each_in(
                scratch.path(),
                &cwd_within_repo(),
                &branches,
                program,
                args,
                self.fail_fast,
            )?
        };

        print_summary(&results);

        if results.iter().any(|(_, passed)| !passed) {
            bail!("`{program}` failed on one or more branches");
        }
        Ok(())
    }
}

/// A throwaway detached worktree, removed on drop. Detached so it holds no
/// branch and cannot collide with the user's checkout.
struct ScratchWorktree {
    path: PathBuf,
}

impl ScratchWorktree {
    fn create(commit: &str) -> Result<Self> {
        let path = crate::git::git_common_path_absolute(SCRATCH_DIR)?;

        // A leftover from a crashed or killed run. `run` holds the lock for its
        // whole window, so anything here is stale by definition - reclaim it
        // rather than failing.
        if path.exists() {
            let _ = crate::git::worktree_remove(&path);
            let _ = std::fs::remove_dir_all(&path);
        }

        crate::git::worktree_add_detached(&path, commit)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchWorktree {
    fn drop(&mut self) {
        // Best effort on every exit path. A hard kill skips Drop entirely; the
        // reclaim in `create` is what covers that.
        let _ = crate::git::worktree_remove(&self.path);
    }
}

/// Run the command against each branch inside `worktree`, moving its detached
/// HEAD rather than touching any branch.
fn run_each_in(
    worktree: &Path,
    subdirectory: &Path,
    branches: &[String],
    program: &str,
    args: &[String],
    fail_fast: bool,
) -> Result<Vec<(String, bool)>> {
    let mut results = Vec::new();
    for branch in branches {
        crate::git::checkout_detached_in(worktree, branch)?;
        anstream::println!("{}", style::branch(branch));
        // Re-derived per branch: the subdirectory may not exist on all of them.
        let dir = mirrored_dir(worktree, subdirectory);
        let passed = run_once(&dir, program, args)?;
        results.push((branch.clone(), passed));
        if !passed && fail_fast {
            break;
        }
    }
    Ok(results)
}

/// Check out each branch in the user's own worktree and run the command there.
fn run_each_in_place(
    branches: &[String],
    program: &str,
    args: &[String],
    fail_fast: bool,
) -> Result<Vec<(String, bool)>> {
    // The user's own directory, not the repo root: this path walks their
    // checkout, so a command run from a subdirectory must still run there.
    let here = std::env::current_dir().or_else(|_| crate::git::repo_root())?;
    let mut results = Vec::new();
    for branch in branches {
        crate::git::checkout(branch)?;
        anstream::println!("{}", style::branch(branch));
        let passed = run_once(&here, program, args)?;
        results.push((branch.clone(), passed));
        if !passed && fail_fast {
            break;
        }
    }
    Ok(results)
}

/// Where the user stands, relative to the repo root - so a scratch worktree can
/// put the command in the same place. Empty at the root, or when the two cannot
/// be compared.
fn cwd_within_repo() -> PathBuf {
    let (Ok(cwd), Ok(root)) = (std::env::current_dir(), crate::git::repo_root()) else {
        return PathBuf::new();
    };
    // Canonicalized before comparing: symlinked or /tmp-style paths would
    // otherwise fail to match their own repo root.
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let root = root.canonicalize().unwrap_or(root);
    cwd.strip_prefix(&root)
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// `subdirectory` inside `worktree`, when the branch checked out there actually
/// has it. Falls back to the worktree root rather than failing: a package
/// directory may simply not exist yet on an earlier branch in the stack.
fn mirrored_dir(worktree: &Path, subdirectory: &Path) -> PathBuf {
    let candidate = worktree.join(subdirectory);
    if candidate.is_dir() {
        candidate
    } else {
        worktree.to_path_buf()
    }
}

/// One invocation, with stdio inherited so output streams through live.
fn run_once(dir: &Path, program: &str, args: &[String]) -> Result<bool> {
    match Command::new(program).args(args).current_dir(dir).status() {
        Ok(status) => Ok(status.success()),
        // The command never launched (e.g. not found). It would fail
        // identically on every branch, so stop with a clear error rather
        // than reporting a bogus FAIL down the whole stack - the branches
        // are fine, the command is what's wrong.
        Err(error) => Err(spawn_error(program, args, &error)),
    }
}

/// A command that could not be spawned, distinguished from one that ran and
/// exited non-zero. The common cause is passing the whole command as a single
/// quoted string, so the "program" is really `cmd arg arg` and no such binary
/// exists; hint at the unquoted form when that shape is detected.
fn spawn_error(program: &str, args: &[String], error: &std::io::Error) -> anyhow::Error {
    let mut message = format!("failed to run `{program}`: {error}");
    if args.is_empty() && program.split_whitespace().count() > 1 {
        message.push_str(&format!(
            "\nhint: pass the command unquoted after `--`, e.g. `git stk run -- {program}`"
        ));
    }
    anyhow!(message)
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
