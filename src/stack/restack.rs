//! The rebase engine: restack a whole stack parent-first, persisting enough
//! state across conflicts for `continue`/`abort` to resume or unwind.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use super::{children_map, collect_descendants, fork_point, line_base, parent_map, record_base};
use crate::cli::{FetchMode, PushMode, UpdateRefsMode};
use crate::git;
use crate::prompt;
use crate::providers::detect_review_provider;
use crate::settings;
use crate::style;

const STATE_FILE: &str = "stack-state";

pub fn restack(
    fetch_mode: FetchMode,
    update_refs_mode: UpdateRefsMode,
    push_mode: PushMode,
    dry_run: bool,
) -> Result<()> {
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

    // Update the trunk from the remote first so branches rebase onto its
    // latest tip; otherwise warn when a base the stack sits on has moved on the
    // remote, so "up to date" is never read off a stale local trunk.
    if settings::fetch_enabled(fetch_mode)? {
        fetch_trunk(dry_run)?;
    }
    warn_bases_behind_remote(&branches, &parents)?;

    let update_refs = resolve_update_refs(update_refs_mode)?;
    let push = settings::push_enabled(push_mode, settings::PUSH_ON_RESTACK_KEY)?;
    let frozen = with_frozen_ancestors(frozen_branches(&branches), &branches, &parents);

    // Before anything is mutated - the snapshot, the diverged-remote
    // cherry-picks, the first rebase. A branch held elsewhere cannot be rebased,
    // and finding out mid-loop leaves the stack half-rewritten.
    ensure_no_worktree_blocks(&branches, &parents, &frozen, &BTreeSet::new())?;

    if dry_run {
        reconcile_diverged_remotes(&branches, &frozen, push, true)?;
        return print_restack_plan(&branches, &parents, &frozen, update_refs, push);
    }

    super::snapshot("restack");
    // Pull in any commits the remote branches have but the local stack lacks
    // before the rebase loop, so descendants replay onto the reconciled tips
    // and the later force-push lease matches instead of looping on "run sync".
    reconcile_diverged_remotes(&branches, &frozen, push, false)?;
    clear_state()?;
    let all = branches.clone();
    restack_branches(branches, &parents, &frozen, update_refs, push, &all)
}

/// Branches in the restack set whose review is itself locked by a merge queue /
/// merge train. Resolves the provider best-effort - no remote, or an
/// unrecognized host, means no provider and so nothing frozen, which is exactly
/// right for a purely local restack. [`with_frozen_ancestors`] then widens this
/// to the branches that must move with them.
fn frozen_branches(branches: &[String]) -> BTreeSet<String> {
    let Ok((_, provider)) = detect_review_provider() else {
        return BTreeSet::new();
    };
    provider.enqueued_branches(branches).unwrap_or_default()
}

/// Widen the directly-queued set to every branch *below* a queued one in the
/// restack set. A queued review is computed (and merged) against its base, so
/// rebasing or force-pushing any ancestor would move that base out from under
/// the frozen tip and invalidate the queue entry. Freezing therefore propagates
/// down the parent chain to the line base; descendants need no such treatment,
/// since their (frozen) parent does not move and they stay up to date.
fn with_frozen_ancestors(
    queued: BTreeSet<String>,
    branches: &[String],
    parents: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let in_set: BTreeSet<&str> = branches.iter().map(String::as_str).collect();
    let mut frozen = queued.clone();
    for branch in &queued {
        let mut current = branch.clone();
        while let Some(parent) = parents.get(&current) {
            // Stop at the line base (parent outside the set), and short-circuit
            // when a shared ancestor was already frozen by an earlier branch.
            if !in_set.contains(parent.as_str()) || !frozen.insert(parent.clone()) {
                break;
            }
            current = parent.clone();
        }
    }
    frozen
}

/// The line printed for a branch held out of the restack because a review in
/// its stack sits in a merge queue / merge train - either this branch's own, or
/// a descendant's, whose base this branch must not move.
fn frozen_note(branch: &str) -> String {
    format!(
        "{} {}: not rebased or pushed (a branch in this stack is in a merge queue; dequeue it to continue)",
        style::warn("frozen"),
        style::branch(branch),
    )
}

/// The plan, read-only: which branches would rebase and which already sit
/// on their parents.
fn print_restack_plan(
    branches: &[String],
    parents: &BTreeMap<String, String>,
    frozen: &BTreeSet<String>,
    update_refs: bool,
    push: bool,
) -> Result<()> {
    for branch in branches {
        if frozen.contains(branch) {
            anstream::println!("{}", frozen_note(branch));
            continue;
        }

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
        let pushable: Vec<&str> = branches
            .iter()
            .filter(|branch| !frozen.contains(*branch))
            .map(String::as_str)
            .collect();
        if pushable.is_empty() {
            anstream::println!(
                "{}",
                style::dim("nothing to push: every branch is in a merge queue")
            );
        } else {
            anstream::println!(
                "would push {} to {}",
                style::branch(&pushable.join(" ")),
                settings::remote()?
            );
        }
    }
    Ok(())
}

/// Refuse the restack if a branch it would rebase is checked out in another
/// worktree. Two failures this heads off: git refuses to rebase such a branch
/// outright, aborting the run with earlier branches already rewritten; and
/// `git rebase --update-refs` *silently* skips refs it cannot move, which would
/// leave a parent behind while its child is rewritten and quietly break the
/// ancestry the stack depends on.
fn ensure_no_worktree_blocks(
    branches: &[String],
    parents: &BTreeMap<String, String>,
    frozen: &BTreeSet<String>,
    already_moving: &BTreeSet<String>,
) -> Result<()> {
    let held = git::worktree_branches()?;
    if held.is_empty() {
        return Ok(());
    }

    // Which branches the loop will actually rebase. `branches` is ordered
    // parent-first, so a parent's fate is known before its children are asked.
    // `already_moving` seeds branches whose rebase is underway but whose ref has
    // not caught up yet - a paused rebase holds the work in detached HEAD, so
    // their children would otherwise read as up to date and escape the check.
    let mut rebasing: BTreeSet<&str> = already_moving.iter().map(String::as_str).collect();
    let mut blocked = Vec::new();
    for branch in branches {
        if frozen.contains(branch) {
            continue;
        }
        let Some(parent) = parents.get(branch) else {
            continue;
        };
        // A branch whose parent is moving will be rebased even if it sits on
        // that parent's tip right now, so "up to date" only settles it when the
        // parent stays put.
        if !rebasing.contains(parent.as_str()) && up_to_date(branch, parent)? {
            continue;
        }
        rebasing.insert(branch.as_str());
        if let Some((_, path)) = held.iter().find(|(name, _)| name == branch) {
            blocked.push((branch.clone(), path.clone()));
        }
    }

    if blocked.is_empty() {
        return Ok(());
    }

    // Re-running from the holding worktree only helps when that worktree is the
    // only one in the way *and* nothing checked out here needs rebasing.
    // Otherwise the two worktrees hold each other's branches and the advice
    // sends the user in a circle.
    let held_by = git::distinct_paths(blocked.iter().map(|(_, path)| path.as_path()));
    let here_moves = git::current_branch()
        .ok()
        .is_some_and(|branch| rebasing.contains(branch.as_str()));
    let delegate = match held_by.as_slice() {
        [only] if !here_moves => Some(only.as_path()),
        _ => None,
    };

    bail!(worktree_block_message(&blocked, &held_by, delegate));
}

/// The refusal, with a remedy that holds in every layout. Detaching is what this
/// leads with: `git worktree remove` cannot free the main worktree, and moving
/// the restack into the holding worktree only works in the single-blocker case,
/// so neither is safe to offer unconditionally.
fn worktree_block_message(
    blocked: &[(String, PathBuf)],
    held_by: &[PathBuf],
    delegate: Option<&Path>,
) -> String {
    let mut message =
        String::from("restack would rebase branches checked out in other worktrees:\n");
    for (branch, path) in blocked {
        message.push_str(&format!("  {branch} in {}\n", git::describe_worktree(path)));
    }
    message.push_str("git cannot rebase a branch another worktree holds. Free ");
    message.push_str(if held_by.len() == 1 { "it" } else { "each one" });
    message.push_str(" by detaching there:\n");
    for path in held_by {
        message.push_str(&format!("  {}\n", git::detach_command(path)));
    }
    message.push_str("then check ");
    message.push_str(if blocked.len() == 1 {
        "the branch"
    } else {
        "those branches"
    });
    message.push_str(" out again once the restack finishes");
    if let Some(path) = delegate {
        message.push_str(&format!(
            ",\nor run the restack from {} instead",
            git::display_path(path)
        ));
    }
    message
}

/// Sitting exactly on the parent tip with a fresh fork point: nothing to do.
fn up_to_date(branch: &str, parent: &str) -> Result<bool> {
    let parent_tip = git::rev_parse(parent)?;
    Ok(
        fork_point(branch, parent)?.as_deref() == Some(parent_tip.as_str())
            && git::is_ancestor(parent, branch).unwrap_or(false),
    )
}

/// Fast-forward the trunk from the remote before restacking. Fetching the
/// branch in place (rather than the whole remote) keeps it cheap; on the trunk
/// itself a plain fast-forward pull does the same. A missing remote is a no-op,
/// not an error - there is simply nothing to pull.
fn fetch_trunk(dry_run: bool) -> Result<()> {
    let Some(trunk) = super::trunk_branch(&git::local_branches()?) else {
        return Ok(());
    };
    let remote = settings::remote()?;
    if git::remote_url(&remote)?.is_none() {
        anstream::println!(
            "{}",
            style::dim(&format!("no remote {remote}; skipped fetch"))
        );
        return Ok(());
    }
    if super::trunk_held_elsewhere(&trunk)? {
        return Ok(());
    }

    if dry_run {
        anstream::println!("would fetch {} from {remote}", style::branch(&trunk));
        return Ok(());
    }
    if git::current_branch()? == trunk {
        git::pull_ff_only()?;
    } else {
        git::fetch_branch(&remote, &trunk)?;
    }
    anstream::println!("fetched {} from {remote}", style::branch(&trunk));
    Ok(())
}

/// Warn when a base the stack rebases onto - the trunk, or any parent outside
/// the restack set - is behind its remote-tracking branch. Without this, a
/// branch sitting exactly on a stale local base reads as "up to date" while the
/// base on the remote has moved on. Best-effort: no remote, or no
/// remote-tracking ref to compare against, means nothing to warn about.
fn warn_bases_behind_remote(branches: &[String], parents: &BTreeMap<String, String>) -> Result<()> {
    let remote = settings::remote()?;
    if git::remote_url(&remote)?.is_none() {
        return Ok(());
    }

    let in_stack: BTreeSet<&String> = branches.iter().collect();
    let external: BTreeSet<&String> = branches
        .iter()
        .filter_map(|branch| parents.get(branch))
        .filter(|parent| !in_stack.contains(parent))
        .collect();

    for base in external {
        let tracking = format!("{remote}/{base}");
        if git::rev_parse(&tracking).is_err() {
            continue;
        }
        let behind = git::commits_behind(base, &tracking).unwrap_or(0);
        if behind > 0 {
            anstream::eprintln!(
                "{}",
                style::warn(&format!(
                    "{base} is {behind} commit{} behind {tracking}; run `git stk restack --fetch` or `git stk sync` to update it first",
                    if behind == 1 { "" } else { "s" }
                ))
            );
        }
    }
    Ok(())
}

/// Before the rebase-and-force-push, reconcile any branch whose remote tip
/// carries commits the local branch lacks - a commit made straight on the
/// host's web UI, a committed review suggestion, a bot's edit. A blind
/// force-push would drop them and `--force-with-lease` rightly refuses, which
/// left `sync` looping on "the remote has moved on - run sync" with no way to
/// pull those commits in. Offer to cherry-pick them onto the local branch; the
/// rebase loop that follows replays descendants onto the reconciled tip, and
/// the push lease then matches.
///
/// Only runs when we are about to push (a no-push restack leaves the remote
/// untouched, so a divergence is not yet fatal) and a remote exists. Under
/// `--dry-run` it reports without fetching or changing anything.
fn reconcile_diverged_remotes(
    branches: &[String],
    frozen: &BTreeSet<String>,
    push: bool,
    dry_run: bool,
) -> Result<()> {
    if !push {
        return Ok(());
    }
    let remote = settings::remote()?;
    if git::remote_url(&remote)?.is_none() {
        return Ok(());
    }

    // Frozen branches are held back from the push, so their remote is not
    // touched and any divergence there is not this run's problem.
    let pushable: Vec<String> = branches
        .iter()
        .filter(|branch| !frozen.contains(*branch))
        .cloned()
        .collect();
    if pushable.is_empty() {
        return Ok(());
    }

    // restack/sync only fetched the trunk, so origin/<branch> can be stale
    // here; refresh the stack branches' tracking refs so the check (and the
    // lease that follows) see the true remote. A dry run must touch nothing,
    // so it compares against whatever is already known locally.
    if !dry_run {
        git::fetch_tracking(&remote, &pushable)?;
    }

    let mut diverged: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for branch in &pushable {
        let tracking = format!("{remote}/{branch}");
        // No tracking ref means the branch was never pushed - nothing upstream
        // to reconcile against.
        if git::rev_parse(&tracking).is_err() {
            continue;
        }
        let extra = git::remote_only_commits(branch, &tracking)?;
        if extra.is_empty() {
            continue;
        }
        // Commits the remote has and we do not, but nothing in them that the
        // local branch lacks. A squash merge is the usual way this happens: it
        // lands the parent's commits as one whose patch id matches none of
        // them, so `--cherry-pick` cannot see they are gone on purpose, and
        // the restack that dropped them was right. There is nothing to
        // incorporate, so do not ask.
        if git::merge_adds_nothing(branch, &tracking)? {
            continue;
        }
        diverged.push((branch.clone(), extra));
    }

    if diverged.is_empty() {
        return Ok(());
    }

    for (branch, commits) in &diverged {
        anstream::eprintln!(
            "{}",
            style::warn(&format!(
                "{remote}/{branch} has {} commit{} not in your local {branch}:",
                commits.len(),
                if commits.len() == 1 { "" } else { "s" },
            ))
        );
        for (sha, subject) in commits {
            anstream::eprintln!("  {} {subject}", style::dim(sha));
        }
    }

    if dry_run {
        anstream::println!(
            "{}",
            style::dim("would offer to cherry-pick these into your local branches before pushing")
        );
        return Ok(());
    }

    if !prompt::confirm("cherry-pick these into your local branches before pushing? [y/N] ")? {
        // Declining used to end the run, leaving the only way forward a
        // command the user had to assemble themselves - and the one that reads
        // as the more dangerous of the two. Offer it instead: the restack that
        // follows pushes with `--force-with-lease`, so answering yes here is
        // exactly "discard them", and the lease still refuses if the remote
        // moves again between now and then.
        if prompt::confirm("discard them and overwrite the remote branches instead? [y/N] ")? {
            return Ok(());
        }
        bail!(
            "remote branches have commits not in your local stack\n\
             incorporate them (`git switch <branch> && git cherry-pick <sha>`) and re-run, \
             or discard them with `git push --force-with-lease {remote} <branch>`"
        );
    }

    // Cherry-pick oldest-first onto each diverged branch. The branch's relation
    // to its parent is unchanged, so the rebase loop leaves it "up to date" and
    // keeps the picked commits, while its descendants rebase onto the new tip.
    let start = git::current_branch()?;
    for (branch, commits) in &diverged {
        git::checkout(branch)?;
        for (sha, _) in commits {
            if let Err(error) = git::cherry_pick(sha) {
                anstream::eprintln!(
                    "{}",
                    style::warn(&format!("conflict cherry-picking {sha} onto {branch}"))
                );
                eprintln!("resolve conflicts, run `git cherry-pick --continue`, then re-run");
                eprintln!("or run `git cherry-pick --abort` to bail out");
                return Err(error);
            }
        }
    }
    git::checkout(&start)?;
    anstream::println!(
        "{}",
        style::success(&format!(
            "incorporated remote commits into {}",
            diverged
                .iter()
                .map(|(branch, _)| branch.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ))
    );
    Ok(())
}

pub fn continue_restack() -> Result<()> {
    let Some(state) = RestackState::read()? else {
        bail!("no interrupted restack found");
    };

    // State on file with no rebase behind it: the run failed before any rebase
    // started. Clear it here rather than failing on git's "no rebase in
    // progress" - a leftover file also blocks `git stk undo`.
    if !git::rebase_in_progress() {
        clear_state()?;
        bail!(
            "no rebase is in progress, so there is nothing to continue\n\
             cleared the leftover restack state; re-run `git stk restack` to pick up where it stopped"
        );
    }

    ensure_no_worktree_blocks(
        &state.remaining,
        &parent_map()?,
        &state.frozen.iter().cloned().collect(),
        &BTreeSet::from([state.branch.clone()]),
    )?;

    if let Err(error) = git::rebase_continue() {
        anstream::eprintln!("{}", style::warn("restack still has conflicts"));
        eprintln!("resolve conflicts, then run `git stk continue`");
        eprintln!("or run `git stk abort`");
        return Err(error);
    }

    record_base(&state.branch, &state.parent);

    let frozen: BTreeSet<String> = state.frozen.iter().cloned().collect();
    if state.remaining.is_empty() {
        clear_state()?;
        finish_restack(&state.all, &frozen, state.push)?;
        return Ok(());
    }

    let parents = parent_map()?;
    restack_branches(
        state.remaining,
        &parents,
        &frozen,
        state.update_refs,
        state.push,
        &state.all,
    )
}

pub fn abort_restack() -> Result<()> {
    // Same escape hatch as `continue`: with no rebase to unwind, aborting means
    // dropping the leftover state so the stack is usable again.
    if !git::rebase_in_progress() {
        if RestackState::read()?.is_none() {
            bail!("no restack to abort");
        }
        clear_state()?;
        anstream::println!("cleared leftover restack state; no rebase was in progress");
        return Ok(());
    }

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
    frozen: &BTreeSet<String>,
    update_refs: bool,
    push: bool,
    all: &[String],
) -> Result<()> {
    for (index, branch) in branches.iter().enumerate() {
        if frozen.contains(branch) {
            anstream::println!("{}", frozen_note(branch));
            continue;
        }

        let Some(parent) = parents.get(branch) else {
            bail!("{branch} has no stack parent");
        };

        // Replay only the branch's own commits, from its current fork point, so
        // commits already upstream - landed via squash or rebase merges, or
        // trunk commits behind a stale recorded base - are not repeated. With
        // no fork point to anchor on, fall back to a plain rebase.
        let base = fork_point(branch, parent)?;

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
            // A rebase that never started is not a conflict: `continue` and
            // `abort` would both fail on it, and recording state would only
            // block `undo` too. Let the failure stand on its own.
            if !git::rebase_in_progress() {
                return Err(error);
            }

            let remaining = branches[index + 1..].to_vec();
            RestackState {
                branch: branch.to_owned(),
                parent: parent.to_owned(),
                remaining,
                update_refs,
                push,
                all: all.to_vec(),
                frozen: frozen.iter().cloned().collect(),
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
    finish_restack(all, frozen, push)
}

/// After every branch has been rebased: push the rewritten branches, or print
/// the exact command so stale remote PR diffs are a copy-paste away from fixed.
/// Frozen branches (in a merge queue / merge train) are held back from the
/// push - pushing them would be rejected (GitHub) or drop them from the queue
/// (GitLab) - so only their pushable siblings are sent.
fn finish_restack(branches: &[String], frozen: &BTreeSet<String>, push: bool) -> Result<()> {
    anstream::println!("{}", style::success("restack complete"));

    let remote = settings::remote()?;
    let pushable: Vec<String> = branches
        .iter()
        .filter(|branch| !frozen.contains(*branch))
        .cloned()
        .collect();
    if pushable.is_empty() {
        anstream::println!(
            "{}",
            style::dim("nothing to push: every branch is in a merge queue")
        );
        return Ok(());
    }

    if push {
        // Only the branches that actually landed: a branch enqueued between the
        // freeze check and the push is held back, warned about, and dropped here
        // so the "pushed ..." line never contradicts that warning.
        let pushed = git::push_force_with_lease(&remote, &pushable)?;
        if pushed.is_empty() {
            anstream::println!(
                "{}",
                style::dim("nothing pushed: every branch is in a merge queue")
            );
        } else {
            anstream::println!("pushed {} to {remote}", style::branch(&pushed.join(" ")));
            // Keep the shared parent map in step with the pushed branches.
            super::publish_metadata(&remote);
        }
    } else {
        anstream::println!("remote branches may be stale; push them with:");
        anstream::println!(
            "{}",
            style::dim(&format!(
                "  git push --force-with-lease {remote} {}",
                pushable.join(" ")
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
    /// Branches frozen by a merge queue / merge train, so the resumed restack
    /// keeps skipping them and the final push keeps holding them back.
    frozen: Vec<String>,
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
        let mut frozen = Vec::new();

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
            } else if let Some(value) = line.strip_prefix("frozen=") {
                frozen = value
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
            frozen,
        }))
    }

    fn write(&self) -> Result<()> {
        let path = state_path()?;
        let contents = format!(
            "branch={}\nparent={}\nupdateRefs={}\npush={}\nremaining={}\nall={}\nfrozen={}\n",
            self.branch,
            self.parent,
            self.update_refs,
            self.push,
            self.remaining.join("\t"),
            self.all.join("\t"),
            self.frozen.join("\t")
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

/// Whether a restack is paused on a conflict, awaiting continue/abort. Requires
/// a live rebase, not just state on file: a run that failed before any rebase
/// started leaves state behind, and treating that as "in progress" would wedge
/// `undo` as well as `continue`/`abort`.
pub(super) fn in_progress() -> bool {
    state_path().map(|path| path.exists()).unwrap_or(false) && git::rebase_in_progress()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `main -> a -> b -> c`, as the restack records it: each branch's parent
    /// is the one below it. `main` (the trunk) is outside the restack set.
    fn linear_parents() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("a".to_owned(), "main".to_owned()),
            ("b".to_owned(), "a".to_owned()),
            ("c".to_owned(), "b".to_owned()),
        ])
    }

    fn set(branches: &[&str]) -> BTreeSet<String> {
        branches.iter().map(|b| (*b).to_owned()).collect()
    }

    #[test]
    fn a_queued_middle_branch_freezes_everything_below_it() {
        // b is in the queue; a (its base) must not move, or b's queue entry
        // goes stale. c, above b, is left to its no-op rebase on a frozen b.
        let branches = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let frozen = with_frozen_ancestors(set(&["b"]), &branches, &linear_parents());
        assert_eq!(frozen, set(&["a", "b"]));
    }

    #[test]
    fn a_queued_bottom_branch_freezes_only_itself() {
        // The common case: the bottom of the stack is queued, so there is no
        // ancestor in the set to carry the freeze to.
        let branches = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let frozen = with_frozen_ancestors(set(&["a"]), &branches, &linear_parents());
        assert_eq!(frozen, set(&["a"]));
    }

    #[test]
    fn freeze_stops_at_the_line_base_not_the_trunk() {
        // Restacking only the b..c subtree: a is the line base and not in the
        // set, so freezing c must not try to reach past it to main.
        let branches = vec!["b".to_owned(), "c".to_owned()];
        let frozen = with_frozen_ancestors(set(&["c"]), &branches, &linear_parents());
        assert_eq!(frozen, set(&["b", "c"]));
    }

    #[test]
    fn nothing_queued_freezes_nothing() {
        let branches = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let frozen = with_frozen_ancestors(BTreeSet::new(), &branches, &linear_parents());
        assert!(frozen.is_empty());
    }
}
