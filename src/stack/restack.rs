//! The rebase engine: restack a whole stack parent-first, persisting enough
//! state across conflicts for `continue`/`abort` to resume or unwind.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

use anyhow::{Context, Result, bail};

use super::{base_of, children_map, collect_descendants, line_base, parent_map, record_base};
use crate::cli::{PushMode, UpdateRefsMode};
use crate::git;
use crate::settings;
use crate::style;

const STATE_FILE: &str = "stack-state";

pub fn restack(update_refs_mode: UpdateRefsMode, push_mode: PushMode, dry_run: bool) -> Result<()> {
    let current = git::current_branch()?;
    let parents = parent_map()?;
    // Restack the stack containing the current branch, from anywhere in it:
    // anchor on the bottom of its own line, then rebase that subtree
    // parent-first. Anchoring on the line base rather than the trunk leaves
    // sibling stacks that merely share the trunk alone - rebasing and
    // force-pushing those would touch work this restack was never asked about.
    let base = line_base(&current)?;
    let branches = restack_order(&base, &parents);

    if branches.is_empty() {
        anstream::println!("{}", style::dim("nothing to restack"));
        return Ok(());
    }

    let update_refs = resolve_update_refs(update_refs_mode)?;
    let push = settings::push_enabled(push_mode, settings::PUSH_ON_RESTACK_KEY)?;

    if dry_run {
        return print_restack_plan(&branches, &parents, update_refs, push);
    }

    super::snapshot("restack");
    clear_state()?;
    let all = branches.clone();
    restack_branches(branches, &parents, update_refs, push, &all)
}

/// The plan, read-only: which branches would rebase and which already sit
/// on their parents.
fn print_restack_plan(
    branches: &[String],
    parents: &BTreeMap<String, String>,
    update_refs: bool,
    push: bool,
) -> Result<()> {
    for branch in branches {
        let Some(parent) = parents.get(branch) else {
            bail!("{branch} has no stack parent");
        };

        if up_to_date(branch, parent)? {
            anstream::println!(
                "{} already up to date with {}",
                style::branch(branch),
                style::branch(parent)
            );
        } else {
            anstream::println!(
                "would rebase {} onto {}{}",
                style::branch(branch),
                style::branch(parent),
                if update_refs {
                    " with --update-refs"
                } else {
                    ""
                }
            );
        }
    }

    if push {
        anstream::println!(
            "would push {} to {}",
            style::branch(&branches.join(" ")),
            settings::remote()?
        );
    }
    Ok(())
}

/// The recorded fork point, when it is still an ancestor of the branch.
fn valid_base(branch: &str) -> Result<Option<String>> {
    Ok(match base_of(branch)? {
        Some(base) if git::is_ancestor(&base, branch).unwrap_or(false) => Some(base),
        _ => None,
    })
}

/// Sitting exactly on the parent tip with a fresh fork point: nothing to do.
fn up_to_date(branch: &str, parent: &str) -> Result<bool> {
    let parent_tip = git::rev_parse(parent)?;
    Ok(valid_base(branch)?.as_deref() == Some(parent_tip.as_str())
        && git::is_ancestor(parent, branch).unwrap_or(false))
}

pub fn continue_restack() -> Result<()> {
    let Some(state) = RestackState::read()? else {
        bail!("no interrupted restack found");
    };

    if let Err(error) = git::rebase_continue() {
        anstream::eprintln!("{}", style::warn("restack still has conflicts"));
        eprintln!("resolve conflicts, then run `git stk continue`");
        eprintln!("or run `git stk abort`");
        return Err(error);
    }

    record_base(&state.branch, &state.parent);

    if state.remaining.is_empty() {
        clear_state()?;
        finish_restack(&state.all, state.push)?;
        return Ok(());
    }

    let parents = parent_map()?;
    restack_branches(
        state.remaining,
        &parents,
        state.update_refs,
        state.push,
        &state.all,
    )
}

pub fn abort_restack() -> Result<()> {
    git::rebase_abort()?;
    clear_state()?;
    anstream::println!("restack aborted");
    Ok(())
}

fn restack_order(current: &str, parents: &BTreeMap<String, String>) -> Vec<String> {
    let children = children_map(parents);
    let mut branches = Vec::new();

    if parents.contains_key(current) {
        branches.push(current.to_owned());
    }

    let mut visited = BTreeSet::from([current.to_owned()]);
    collect_descendants(current, &children, &mut branches, &mut visited);
    branches
}

fn restack_branches(
    branches: Vec<String>,
    parents: &BTreeMap<String, String>,
    update_refs: bool,
    push: bool,
    all: &[String],
) -> Result<()> {
    for (index, branch) in branches.iter().enumerate() {
        let Some(parent) = parents.get(branch) else {
            bail!("{branch} has no stack parent");
        };

        // Replay only the commits after the recorded fork point so commits
        // that landed upstream via squash or rebase merges are not repeated.
        // A base that is no longer an ancestor (stale or garbage) falls back
        // to a plain rebase.
        let base = valid_base(branch)?;

        // Already sitting exactly on the parent tip with a fresh fork point:
        // skip the rebase entirely. (git rebase --update-refs would otherwise
        // replay and rewrite identical commits with new hashes.)
        if up_to_date(branch, parent)? {
            anstream::println!(
                "{} already up to date with {}",
                style::branch(branch),
                style::branch(parent)
            );
            continue;
        }

        if update_refs {
            anstream::println!(
                "rebasing {} onto {} with --update-refs",
                style::branch(branch),
                style::branch(parent)
            );
        } else {
            anstream::println!(
                "rebasing {} onto {}",
                style::branch(branch),
                style::branch(parent)
            );
        }
        let rebase_result = match &base {
            Some(base) => git::rebase_onto(parent, base, branch, update_refs),
            None => git::rebase(parent, branch, update_refs),
        };

        if let Err(error) = rebase_result {
            let remaining = branches[index + 1..].to_vec();
            RestackState {
                branch: branch.to_owned(),
                parent: parent.to_owned(),
                remaining,
                update_refs,
                push,
                all: all.to_vec(),
            }
            .write()?;

            anstream::eprintln!(
                "{}",
                style::warn(&format!("conflict while rebasing {branch} onto {parent}"))
            );
            eprintln!("resolve conflicts, then run `git stk continue`");
            eprintln!("or run `git stk abort`");
            return Err(error);
        }

        record_base(branch, parent);
    }

    clear_state()?;
    finish_restack(all, push)
}

/// After every branch has been rebased: push the rewritten branches, or print
/// the exact command so stale remote PR diffs are a copy-paste away from fixed.
fn finish_restack(branches: &[String], push: bool) -> Result<()> {
    anstream::println!("{}", style::success("restack complete"));

    let remote = settings::remote()?;
    if push {
        git::push_force_with_lease(&remote, branches)?;
        anstream::println!("pushed {} to {remote}", style::branch(&branches.join(" ")));
        // Keep the shared parent map in step with the pushed branches.
        super::publish_metadata(&remote);
    } else {
        anstream::println!("remote branches may be stale; push them with:");
        anstream::println!(
            "{}",
            style::dim(&format!(
                "  git push --force-with-lease {remote} {}",
                branches.join(" ")
            ))
        );
    }
    Ok(())
}

fn resolve_update_refs(mode: UpdateRefsMode) -> Result<bool> {
    match mode {
        UpdateRefsMode::Config => {
            let configured = git::config_get_bool(settings::UPDATE_REFS_KEY)?.unwrap_or(false);
            if configured && !git::supports_rebase_update_refs()? {
                eprintln!("stk.updateRefs is true, but this Git does not support --update-refs");
                return Ok(false);
            }
            Ok(configured)
        }
        UpdateRefsMode::Enabled => {
            if !git::supports_rebase_update_refs()? {
                bail!("--update-refs was requested, but this Git does not support it");
            }
            Ok(true)
        }
        UpdateRefsMode::Disabled => Ok(false),
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RestackState {
    branch: String,
    parent: String,
    remaining: Vec<String>,
    update_refs: bool,
    push: bool,
    /// Every branch in the interrupted restack, so the post-restack push (or
    /// push hint) can cover branches rebased before the conflict too.
    all: Vec<String>,
}

impl RestackState {
    fn read() -> Result<Option<Self>> {
        let path = state_path()?;
        if !path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut branch = None;
        let mut parent = None;
        let mut remaining = Vec::new();
        let mut update_refs = false;
        let mut push = false;
        let mut all = Vec::new();

        for line in contents.lines() {
            if let Some(value) = line.strip_prefix("branch=") {
                branch = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("parent=") {
                parent = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("updateRefs=") {
                update_refs = value == "true";
            } else if let Some(value) = line.strip_prefix("push=") {
                push = value == "true";
            } else if let Some(value) = line.strip_prefix("remaining=") {
                remaining = value
                    .split('\t')
                    .filter(|branch| !branch.is_empty())
                    .map(str::to_owned)
                    .collect();
            } else if let Some(value) = line.strip_prefix("all=") {
                all = value
                    .split('\t')
                    .filter(|branch| !branch.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
        }

        let Some(branch) = branch else {
            bail!("restack state is missing current branch");
        };
        let Some(parent) = parent else {
            bail!("restack state is missing parent branch");
        };

        Ok(Some(Self {
            branch,
            parent,
            remaining,
            update_refs,
            push,
            all,
        }))
    }

    fn write(&self) -> Result<()> {
        let path = state_path()?;
        let contents = format!(
            "branch={}\nparent={}\nupdateRefs={}\npush={}\nremaining={}\nall={}\n",
            self.branch,
            self.parent,
            self.update_refs,
            self.push,
            self.remaining.join("\t"),
            self.all.join("\t")
        );
        fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
    }
}

fn clear_state() -> Result<()> {
    let path = state_path()?;
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn state_path() -> Result<PathBuf> {
    Ok(PathBuf::from(git::git_path(STATE_FILE)?))
}

/// Whether a restack is paused on a conflict, awaiting continue/abort.
pub(super) fn in_progress() -> bool {
    state_path().map(|path| path.exists()).unwrap_or(false)
}
