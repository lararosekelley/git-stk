//! Stack metadata: the `branch.<name>.stkParent`/`stkBase` annotations and
//! the structural queries built on them. Navigation lives in [`nav`], the
//! rebase engine in [`restack`].

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::git;
use crate::settings;
use crate::style;

/// Shared ref carrying the stack's parent map, so another clone can rebuild
/// the metadata. Pushed/fetched explicitly; a normal fetch ignores it.
const METADATA_REF: &str = "refs/stk/metadata";
const METADATA_FILE: &str = "stack.json";

mod nav;
mod restack;
mod snapshot;

pub use nav::{
    NavOutput, behind_parent_hint, checkout_bottom, checkout_child, checkout_parent, checkout_top,
    print_all_stacks, print_children, print_parent, print_stack,
};
pub use restack::{abort_restack, continue_restack, restack};
pub use snapshot::{take as snapshot, undo};

const PARENT_KEY: &str = "stkParent";
const BASE_KEY: &str = "stkBase";
/// Marks a branch as the rename of another that still has an open review, so
/// the next submit can replace and close that review.
const RENAMED_FROM_KEY: &str = "stkRenamedFrom";
/// Marks a branch as a stack floor: the branch a stack sits on when it is
/// rooted somewhere other than the trunk - a release line, say. git-stk does
/// not manage a floor. It is never submitted, pushed, rebased, merged, or
/// re-parented, so a shared branch cannot be pulled into a stack and rewritten.
/// Recorded when a stack is rooted off-trunk, because the shape alone stops
/// being visible once the branches above it land.
const FLOOR_KEY: &str = "stkFloor";
/// Records that git-stk created this branch's worktree, and where. Only these
/// are ours to remove; a worktree the user made by hand stays theirs.
const WORKTREE_KEY: &str = "stkWorktree";

pub fn create_branch(branch: &str, dry_run: bool) -> Result<()> {
    let parent = git::current_branch()?;
    // `new` creates the branch; an existing one is an adopt, not a create.
    if git::local_branches()?
        .iter()
        .any(|existing| existing == branch)
    {
        bail!(
            "branch {branch} already exists - adopt it onto {parent} \
             with `git stk adopt {branch} --parent {parent}`"
        );
    }
    if !dry_run {
        git::create_branch(branch)?;
        set_parent(branch, &parent)?;
        record_base(branch, &parent);
    }
    anstream::println!(
        "{} {} with parent {}",
        if dry_run { "would create" } else { "created" },
        style::branch(branch),
        style::branch(&parent)
    );
    mark_floor_if_rooting(&parent, dry_run)?;
    Ok(())
}

/// Create `branch` in a new worktree of its own instead of checking it out here,
/// leaving the current worktree on the branch it was already on.
///
/// The directory is derived from the branch name under [`settings::worktree_dir`],
/// nested so `feat/a` keeps a basename matching the branch tail - the way git's
/// own path-to-branch guessing would read it.
pub fn create_branch_in_worktree(branch: &str, dry_run: bool) -> Result<()> {
    let parent = git::current_branch()?;
    ensure_absent(branch)?;

    let path = settings::worktree_path_for(branch)?;
    if path.exists() {
        bail!(
            "{} already exists; remove it or pick another branch name",
            path.display()
        );
    }

    if !dry_run {
        git::worktree_add_new_branch(&path, branch, &parent)?;
        // Provenance first: this worktree is ours, so cleanup may remove it
        // later. Recorded before the metadata below so a failure there cannot
        // leave a worktree on disk that nothing claims.
        set_owned_worktree(branch, &path)?;
        set_parent(branch, &parent)?;
        record_base(branch, &parent);
    }
    anstream::println!(
        "{} {} with parent {} in the worktree at {}",
        if dry_run { "would create" } else { "created" },
        style::branch(branch),
        style::branch(&parent),
        git::display_path(&path)
    );
    if !dry_run {
        anstream::println!(
            "{}",
            style::dim(&format!("cd {}", git::display_path(&path)))
        );
    }
    mark_floor_if_rooting(&parent, dry_run)?;
    Ok(())
}

/// Whether the trunk cannot be fetched because another worktree has it checked
/// out, reporting the skip when so.
///
/// Git refuses `fetch <remote> <trunk>:<trunk>` while another worktree holds the
/// trunk - the normal state of a worktree-per-branch layout. Skipping beats the
/// alternatives: failing outright would make `sync` and `merge` unusable from a
/// worktree, and fetching only the remote-tracking ref would leave the local
/// trunk quietly stale for the rebase that follows.
pub fn trunk_held_elsewhere(trunk: &str) -> Result<bool> {
    let Some(path) = git::worktree_holding(trunk)? else {
        return Ok(false);
    };
    anstream::println!(
        "{}",
        style::warn(&format!(
            "skipped fetching {trunk}: it is checked out in the worktree at {}",
            git::display_path(&path)
        ))
    );
    anstream::println!(
        "{}",
        style::dim(&format!(
            "using the local {trunk}; fast-forward it there to pick up the remote"
        ))
    );
    Ok(true)
}

/// The worktree git-stk created for `branch`, if it created one and it is still
/// there. Only these are ours to remove.
pub fn owned_worktree(branch: &str) -> Option<std::path::PathBuf> {
    recorded_worktree(branch).filter(|path| path.exists())
}

/// What the marker says, whether or not the directory still exists. `repair`
/// needs the raw value to spot a marker pointing at nothing.
pub fn recorded_worktree(branch: &str) -> Option<std::path::PathBuf> {
    git::config_get(&format!("branch.{branch}.{WORKTREE_KEY}"))
        .ok()
        .flatten()
        .map(std::path::PathBuf::from)
}

/// Record that git-stk owns `branch`'s worktree at `path`.
pub fn set_owned_worktree(branch: &str, path: &std::path::Path) -> Result<()> {
    git::config_set(
        &format!("branch.{branch}.{WORKTREE_KEY}"),
        &path.to_string_lossy(),
    )
}

/// Forget that git-stk owns a worktree for `branch`.
pub fn unset_owned_worktree(branch: &str) -> Result<()> {
    git::config_unset(&format!("branch.{branch}.{WORKTREE_KEY}"))
}

/// Insert a new empty branch directly above the current one, moving the
/// current branch's children onto it. The new branch shares the current tip,
/// so descendants stay correctly based; commit to it, then `restack` to
/// replay them. Any uncommitted changes ride onto the new branch, like `new`.
pub fn insert_branch(branch: &str, dry_run: bool) -> Result<()> {
    ensure_absent(branch)?;
    let current = git::current_branch()?;
    let children = children_of(&current)?;

    if !dry_run {
        snapshot::take("new --insert");
        git::create_branch(branch)?; // off current; leaves us on the new branch
        set_parent(branch, &current)?;
        record_base(branch, &current);
        for child in &children {
            set_parent(child, branch)?;
            record_base(child, branch);
        }
    }

    anstream::println!(
        "{} {} above {}",
        if dry_run { "would insert" } else { "inserted" },
        style::branch(branch),
        style::branch(&current)
    );
    for child in &children {
        anstream::println!(
            "{} {} -> {}",
            if dry_run {
                "would retarget"
            } else {
                "retargeted"
            },
            style::branch(child),
            style::branch(branch)
        );
    }
    mark_floor_if_rooting(&current, dry_run)?;
    Ok(())
}

/// Insert a new empty branch directly below the current one, moving the
/// current branch onto it. Branches from the current branch's parent, so it
/// requires a clean worktree. Commit to it, then `restack`.
pub fn prepend_branch(branch: &str, dry_run: bool) -> Result<()> {
    ensure_absent(branch)?;
    let current = git::current_branch()?;
    let parent = stacked_parent_of(&current)?
        .context("current branch has no stack parent to prepend below")?;
    if !git::worktree_is_clean()? {
        bail!(
            "working tree has uncommitted changes; commit or stash before `git stk new --prepend`"
        );
    }

    if !dry_run {
        snapshot::take("new --prepend");
        git::checkout(&parent)?;
        git::create_branch(branch)?; // off the parent; leaves us on the new branch
        set_parent(branch, &parent)?;
        record_base(branch, &parent);
        set_parent(&current, branch)?;
        record_base(&current, branch);
    }

    anstream::println!(
        "{} {} between {} and {}",
        if dry_run { "would insert" } else { "inserted" },
        style::branch(branch),
        style::branch(&parent),
        style::branch(&current)
    );
    anstream::println!(
        "{} {} -> {}",
        if dry_run {
            "would retarget"
        } else {
            "retargeted"
        },
        style::branch(&current),
        style::branch(branch)
    );
    mark_floor_if_rooting(&parent, dry_run)?;
    Ok(())
}

fn ensure_absent(branch: &str) -> Result<()> {
    if git::local_branches()?
        .iter()
        .any(|existing| existing == branch)
    {
        bail!("branch {branch} already exists");
    }
    Ok(())
}

/// The trunk branch: the remote's default branch when known locally,
/// otherwise a conventional name that exists.
pub fn trunk_branch(branches: &[String]) -> Option<String> {
    let remote = settings::remote().unwrap_or_else(|_| settings::DEFAULT_REMOTE.to_owned());
    if let Some(default) = git::remote_default_branch(&remote) {
        return Some(default);
    }

    ["main", "master"]
        .iter()
        .find(|name| branches.iter().any(|branch| branch == *name))
        .map(|name| (*name).to_owned())
}

pub fn adopt_branch(branch: &str, parent: &str, dry_run: bool) -> Result<()> {
    if branch == parent {
        bail!("a branch cannot be its own stack parent");
    }

    let branches: BTreeSet<_> = git::local_branches()?.into_iter().collect();
    if !branches.contains(branch) {
        bail!("branch {branch} does not exist");
    }
    if !branches.contains(parent) {
        bail!("parent branch {parent} does not exist");
    }
    if branch_and_descendants(branch)?
        .iter()
        .any(|descendant| descendant == parent)
    {
        bail!("{parent} is already below {branch} in the stack; that would form a cycle");
    }

    if !dry_run {
        set_parent(branch, parent)?;
        record_base(branch, parent);
    }
    anstream::println!(
        "{} {} to {}",
        if dry_run { "would attach" } else { "attached" },
        style::branch(branch),
        style::branch(parent)
    );
    // Adopting a branch onto a parent says it is a layer, so it is no longer a
    // base - otherwise it stays out of `submit`/`merge` while `restack` treats
    // it as ordinary. Announced like the recording, and on a dry run too: it
    // removes protection, which is the direction that most wants saying.
    if is_floor(branch)? {
        if !dry_run {
            clear_floor(branch)?;
        }
        anstream::println!(
            "{}",
            style::dim(&format!(
                "{} {branch} is no longer a stack base",
                if dry_run {
                    "would record that"
                } else {
                    "recorded that"
                }
            ))
        );
    }
    mark_floor_if_rooting(parent, dry_run)?;
    Ok(())
}

pub fn detach_branch(branch: Option<&str>) -> Result<()> {
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    unset_parent(&branch)?;
    unset_base(&branch)?;
    // Also the way to say "stop treating this as a stack base" - and the
    // escape every base hint names, so it confirms what it cleared.
    let was_floor = is_floor(&branch)?;
    clear_floor(&branch)?;
    anstream::println!("detached {}", style::branch(&branch));
    if was_floor {
        anstream::println!(
            "{}",
            style::dim(&format!("{branch} is no longer a stack base"))
        );
    }
    Ok(())
}

/// Rename a branch and keep the stack intact. Git moves the branch's own
/// metadata with the rename; children pointing at the old name are
/// retargeted here.
pub fn rename_branch(old: &str, new: &str, dry_run: bool) -> Result<()> {
    let children = children_of(old)?;

    if !dry_run {
        snapshot::take("rename");
        git::rename_branch(old, new)?;
    }
    anstream::println!(
        "{} {} -> {}",
        if dry_run { "would rename" } else { "renamed" },
        style::branch(old),
        style::branch(new)
    );

    for child in &children {
        if !dry_run {
            set_parent(child, new)?;
        }
        anstream::println!(
            "{} {} -> {}",
            if dry_run {
                "would retarget"
            } else {
                "retargeted"
            },
            style::branch(child),
            style::branch(new)
        );
    }
    Ok(())
}

/// Record that `branch` is the rename of `old`, whose open review the next
/// submit should replace and close.
pub fn set_renamed_from(branch: &str, old: &str) -> Result<()> {
    git::config_set(&renamed_from_key(branch), old)
}

/// The branch `branch` was renamed from, if a replaced review is still pending.
pub fn renamed_from(branch: &str) -> Result<Option<String>> {
    git::config_get(&renamed_from_key(branch))
}

/// Drop the rename marker once its review has been handled.
pub fn clear_renamed_from(branch: &str) -> Result<()> {
    git::config_unset(&renamed_from_key(branch))
}

/// Record the fork point between a branch and its parent (best effort; e.g.
/// unrelated histories have no merge base, which is not an error here).
pub fn record_base(branch: &str, parent: &str) {
    if let Ok(base) = git::merge_base(parent, branch) {
        let _ = git::config_set(&base_key(branch), &base);
    }
}

/// The commit to replay `branch` from when rebasing onto `parent`: the tighter
/// of its recorded fork point and the live `merge_base(parent, branch)`. The
/// recorded base is trusted only when it is a descendant of (or equal to) the
/// live merge base - a fork point at least as recent, as after the parent is
/// rewritten and its old commits leave the branch's history. A recorded base
/// that is a proper *ancestor* of the live merge base is stale: the true fork
/// point has moved past it (e.g. the branch was rebased onto a newer trunk out
/// of band), so the merge base wins and only the branch's own commits replay.
/// With neither available there is nothing to anchor on.
pub(crate) fn fork_point(branch: &str, parent: &str) -> Result<Option<String>> {
    let recorded = base_of(branch)?.filter(|base| git::is_ancestor(base, branch).unwrap_or(false));
    let merge_base = git::merge_base(parent, branch).ok();
    Ok(match (recorded, merge_base) {
        (Some(recorded), Some(merge_base)) => Some(
            if git::is_ancestor(&merge_base, &recorded).unwrap_or(false) {
                recorded
            } else {
                merge_base
            },
        ),
        (recorded, merge_base) => recorded.or(merge_base),
    })
}

/// Whether `branch`'s recorded fork point is still current: present, an
/// ancestor of the branch, and not stale (not a proper ancestor of the live
/// `merge_base(parent, branch)`). A stale one must be re-recorded.
pub(crate) fn base_is_current(branch: &str, parent: &str) -> Result<bool> {
    let Some(base) = base_of(branch)? else {
        return Ok(false);
    };
    if !git::is_ancestor(&base, branch).unwrap_or(false) {
        return Ok(false);
    }
    Ok(match git::merge_base(parent, branch) {
        Ok(merge_base) => git::is_ancestor(&merge_base, &base).unwrap_or(false),
        Err(_) => true,
    })
}

/// The root of the stack containing `branch` (the base everything sits on).
pub fn stack_root(branch: &str) -> Result<String> {
    let parents = parent_map()?;
    Ok(root_for(branch, &parents))
}

pub fn branch_and_descendants(branch: &str) -> Result<Vec<String>> {
    let parents = parent_map()?;
    let children = children_map(&parents);
    let mut branches = vec![branch.to_owned()];
    let mut visited = BTreeSet::from([branch.to_owned()]);
    collect_descendants(branch, &children, &mut branches, &mut visited);
    Ok(branches)
}

/// Every branch in the stack containing `branch`, parent-first: the line from
/// the stack bottom up through `branch`, plus everything above it. Sibling
/// stacks that share only the trunk are left out - they branch off the trunk
/// separately, not through `branch`. The trunk itself is excluded; an
/// unanchored root stays in (`path_from_root` keeps it).
pub fn stack_line(branch: &str) -> Result<Vec<String>> {
    // The trunk is not part of any stack, so standing on it your line is empty
    // - its descendants are sibling stacks, each left for its own submit.
    // Without this, `branch_and_descendants(trunk)` would pull in every stack.
    let trunk = trunk_branch(&git::local_branches()?);
    if Some(branch) == trunk.as_deref() {
        return Ok(Vec::new());
    }

    let mut line = path_from_root(branch)?; // [bottom ..= branch]
    let above = branch_and_descendants(branch)?; // [branch, ..descendants]
    line.extend(above.into_iter().skip(1)); // append above-branch, dropping the duplicate

    // `path_from_root` keeps its starting branch even when that is the trunk
    // (you are standing on it); a trunk is never part of a stack.
    line.retain(|candidate| Some(candidate) != trunk.as_ref());
    Ok(line)
}

/// The base of `branch`'s own line: its topmost non-trunk ancestor (the branch
/// just above the trunk), or `branch` itself when it has no parent. This is the
/// anchor for "the current stack" - the subtree under it includes genuine fork
/// siblings but excludes stacks that merely share the trunk - so `restack`,
/// `list`, `sync`, and `run` all agree on scope. Unlike [`stack_root`], which
/// collapses a trunk-anchored line all the way to the trunk and so sweeps in
/// every sibling stack.
pub(crate) fn line_base(branch: &str) -> Result<String> {
    Ok(path_from_root(branch)?
        .into_iter()
        .next()
        .unwrap_or_else(|| branch.to_owned()))
}

/// Every branch in the stack `branch` belongs to, trunk excluded: the whole
/// subtree under the stack's base (so fork siblings are included too), unlike
/// [`stack_line`] which is only `branch`'s own line. The base is the bottom of
/// `branch`'s line - its topmost non-trunk ancestor - so sibling stacks that
/// merely share the trunk are left out, exactly as they are for [`stack_line`].
/// For an unanchored stack the base is its real root branch, itself stacked,
/// so it stays in.
pub fn current_stack_branches(branch: &str) -> Result<Vec<String>> {
    let base = line_base(branch)?;
    let trunk = trunk_branch(&git::local_branches()?);
    Ok(branch_and_descendants(&base)?
        .into_iter()
        .filter(|candidate| Some(candidate) != trunk.as_ref())
        .collect())
}

/// The branches `list` may annotate with review info: the current stack, or -
/// with `all` - every stacked branch. A superset of what the tree actually
/// draws is fine here; its only job is to bound which branches get a per-branch
/// review lookup, so `list` never queries every open PR in the repo.
pub fn listed_branches(all: bool) -> Result<BTreeSet<String>> {
    if all {
        Ok(parent_map()?
            .into_iter()
            .flat_map(|(child, parent)| [child, parent])
            .collect())
    } else {
        let current = git::current_branch()?;
        Ok(current_stack_branches(&current)?.into_iter().collect())
    }
}

/// Publish the current stack's parent map to the shared metadata ref so
/// another clone can rebuild it. Best effort: a failure warns but never aborts
/// the push that triggered it.
pub fn publish_metadata(remote: &str) {
    if let Err(error) = try_publish_metadata(remote) {
        anstream::eprintln!(
            "{}",
            style::warn(&format!("could not publish stack metadata: {error:#}"))
        );
    }
}

fn try_publish_metadata(remote: &str) -> Result<()> {
    let current = git::current_branch()?;
    let trunk = trunk_branch(&git::local_branches()?);

    let mut parents = serde_json::Map::new();
    let mut floors = Vec::new();
    for branch in current_stack_branches(&current)? {
        // Marker first, matching every reader: a base that picked up a stray
        // parent must still publish as a base. Published as a layer it would
        // arrive with `floors: []` and revoke the marker on the other clone -
        // the protected machine exporting the damage.
        if is_floor(&branch)? {
            floors.push(Value::String(branch));
        } else if let Some(parent) = parent_of(&branch)? {
            parents.insert(branch, Value::String(parent));
        }
    }
    if parents.is_empty() {
        return Ok(());
    }

    let document = json!({ "trunk": trunk, "parents": parents, "floors": floors });
    git::write_blob_ref(METADATA_REF, METADATA_FILE, &document.to_string())?;
    git::push_ref(remote, METADATA_REF)
}

/// Rebuild local stack metadata from the shared ref, fetching any listed
/// branch that is not present locally. Returns how many branches it attached.
pub fn apply_remote_metadata(remote: &str) -> Result<usize> {
    git::fetch_ref(remote, METADATA_REF)
        .context("no stack metadata on the remote - push it from the other machine first")?;
    let Some(content) = git::read_ref_file(METADATA_REF, METADATA_FILE)? else {
        bail!("the remote stack metadata is empty");
    };

    let document: Value =
        serde_json::from_str(&content).context("failed to parse remote stack metadata")?;
    let parents = document
        .get("parents")
        .and_then(Value::as_object)
        .context("remote stack metadata is malformed")?;

    // The metadata comes from a remote, so the names are untrusted. Drop any
    // that aren't safe to hand to git as an argument before they reach
    // `fetch`/`rebase`: a name like `--upload-pack=...` would be read as a git
    // option, not a branch.
    let mut pairs = Vec::new();
    for (branch, parent) in parents {
        let Some(parent) = parent.as_str() else {
            continue;
        };
        if !is_safe_ref_name(branch) || !is_safe_ref_name(parent) {
            anstream::eprintln!(
                "{}",
                style::warn(&format!(
                    "skipping unsafe stack metadata entry: {branch:?} -> {parent:?}"
                ))
            );
            continue;
        }
        pairs.push((branch.clone(), parent.to_owned()));
    }

    // The branch each stack sits on. It has no parent, so it is absent from the
    // map above - and without it this machine cannot tell a stack's base from a
    // branch whose metadata is missing. Untrusted names, same as the parents.
    // A document written before bases were recorded has no `floors` key at all.
    // That is not "no bases" - it is "this writer did not know about them" - so
    // the revocation below must not run, or an un-upgraded clone (whose `sync`
    // is the one that adopts the base) would clear the marker here and hand the
    // release line back to `restack`.
    let publishes_floors = document.get("floors").is_some();
    let floors: Vec<String> = document
        .get("floors")
        .and_then(Value::as_array)
        .map(|floors| {
            floors
                .iter()
                .filter_map(Value::as_str)
                .filter(|floor| is_safe_ref_name(floor))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    // Fetch every listed branch first, so each parent resolves locally before
    // we record it. Bases included: a fresh clone has only the trunk, and the
    // stack is unusable without the branch it sits on.
    let local: BTreeSet<String> = git::local_branches()?.into_iter().collect();
    for branch in pairs.iter().map(|(branch, _)| branch).chain(floors.iter()) {
        if !local.contains(branch) {
            git::fetch_branch(remote, branch)
                .with_context(|| format!("failed to fetch {branch} from {remote}"))?;
        }
    }

    for floor in &floors {
        if is_floor(floor)? {
            continue;
        }
        mark_floor(floor);
        // Recording a base another machine chose changes what this clone will
        // rebase, so it is not something to do in silence - the revocation
        // below announces the same membership change.
        anstream::println!("{} is now a stack base", style::branch(floor));
    }
    // A branch the other machine lists with a parent is a layer there, so any
    // floor recorded here is stale. Without this the ref can only ever add
    // floors, and the two clones quietly disagree about what is in the stack.
    for (branch, _) in pairs
        .iter()
        .filter(|(branch, _)| publishes_floors && !floors.contains(branch))
    {
        if is_floor(branch)? {
            clear_floor(branch)?;
            anstream::println!("{} is no longer a stack base", style::branch(branch));
        }
    }

    let mut attached = 0;
    for (branch, parent) in &pairs {
        set_parent(branch, parent)?;
        record_base(branch, parent);
        attached += 1;
        anstream::println!(
            "attached {} to {}",
            style::branch(branch),
            style::branch(parent)
        );
    }
    Ok(attached)
}

/// Whether a branch name from untrusted remote metadata is safe to hand to git
/// as an argument: non-empty, not an option (`-...`), and free of whitespace
/// and control characters. git rejects other invalid refs itself; this guards
/// the one thing it would not - a name it would parse as a flag.
pub(crate) fn is_safe_ref_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The stack path from the bottom up to (and including) `branch`,
/// parent-first; descendants above it are left out.
pub fn path_from_root(branch: &str) -> Result<Vec<String>> {
    let trunk = trunk_branch(&git::local_branches()?);
    let mut path = vec![branch.to_owned()];
    let mut seen = BTreeSet::from([branch.to_owned()]);

    let mut cursor = branch.to_owned();
    while let Some(parent) = stacked_parent_of(&cursor)? {
        if Some(&parent) == trunk.as_ref() || !seen.insert(parent.clone()) {
            break;
        }
        path.push(parent.clone());
        // A floor is where a line starts, like the trunk: keep it, as the base
        // the branch above targets, but never walk past it.
        if is_floor(&parent)? {
            break;
        }
        cursor = parent;
    }

    path.reverse();
    Ok(path)
}

/// The branches in `line` that actually stack on something - those with a
/// recorded parent. A line rooted off the trunk keeps its parentless root
/// (see [`path_from_root`]), and that root is the base the branch above it
/// targets, not a layer of the stack: nothing submits, pushes, or merges it.
/// `restack` and `absorb` already skip it; this is how the rest agree.
///
/// Only a line's first branch can be parentless - `path_from_root` stops
/// where the parents run out, and descendants are found through theirs - so
/// an empty result means the line is a single unstacked branch, which callers
/// report rather than silently skip.
pub fn stacked_layers(line: &[String]) -> Result<Vec<String>> {
    Ok(branch_parents(line)?
        .into_iter()
        .map(|(branch, _)| branch)
        .collect())
}

/// The base `branches` sits on, when that is a branch rather than the trunk:
/// the parentless root of a line rooted off-trunk, with layers stacked on it.
/// It is not part of the stack - nothing submits, pushes, merges, or
/// re-parents it - so callers hold it out of whatever they are about to do.
///
/// A single unstacked branch is not a base: nothing is stacked on it, and it
/// is usually a branch whose metadata is simply missing. That returns `None`,
/// so callers treat it as an ordinary branch (an error to submit, something
/// `sync` may still adopt) rather than silently skipping it.
pub fn unanchored_base(branches: &[String]) -> Result<Option<String>> {
    let layers = stacked_layers(branches)?;
    if layers.len() == branches.len() {
        return Ok(None);
    }
    if layers.is_empty() {
        // Nothing is stacked here, so the shape says nothing: only a recorded
        // floor is a base. This is what keeps a base a base after the branches
        // above it land, and still lets `sync` adopt a lone branch whose
        // metadata is simply missing.
        let [lone] = branches else {
            return Ok(None);
        };
        return Ok(is_floor(lone)?.then(|| lone.clone()));
    }
    Ok(branches
        .iter()
        .find(|branch| !layers.contains(branch))
        .cloned())
}

/// (branch, parent) pairs for the branches that stack on something. A branch
/// with no recorded parent is skipped, and so is a floor - it is the base the
/// stack sits on, whatever parent it may have picked up, and callers use this
/// to decide what to write to (review bodies, the metadata ref).
pub fn branch_parents(branches: &[String]) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for branch in branches {
        if let Some(parent) = stacked_parent_of(branch)? {
            pairs.push((branch.clone(), parent));
        }
    }
    Ok(pairs)
}

fn parent_map() -> Result<BTreeMap<String, String>> {
    let mut parents = BTreeMap::new();
    for branch in git::local_branches()? {
        if let Some(parent) = stacked_parent_of(&branch)? {
            parents.insert(branch, parent);
        }
    }
    Ok(parents)
}

fn collect_descendants(
    branch: &str,
    children: &BTreeMap<String, Vec<String>>,
    branches: &mut Vec<String>,
    visited: &mut BTreeSet<String>,
) {
    if let Some(branch_children) = children.get(branch) {
        for child in branch_children {
            if !visited.insert(child.to_owned()) {
                continue; // cyclic metadata; mirror the guard in path_from_root/root_for
            }
            branches.push(child.to_owned());
            collect_descendants(child, children, branches, visited);
        }
    }
}

/// Whether the repo has any stacked branch at all. Not the same question as
/// "does the trunk have children": a stack rooted off the trunk leaves the
/// trunk childless, so that proxy answers no for a repo that plainly has one.
pub(crate) fn has_stacked_branches() -> Result<bool> {
    Ok(!parent_map()?.is_empty())
}

pub(crate) fn children_of(parent: &str) -> Result<Vec<String>> {
    Ok(parent_map()?
        .into_iter()
        .filter_map(|(branch, branch_parent)| (branch_parent == parent).then_some(branch))
        .collect())
}

fn children_map(parents: &BTreeMap<String, String>) -> BTreeMap<String, Vec<String>> {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (branch, parent) in parents {
        children
            .entry(parent.to_owned())
            .or_default()
            .push(branch.to_owned());
    }
    children
}

fn root_for(branch: &str, parents: &BTreeMap<String, String>) -> String {
    let mut root = branch.to_owned();
    let mut seen = BTreeSet::new();

    while let Some(parent) = parents.get(&root) {
        if !seen.insert(root.clone()) {
            break;
        }
        root = parent.to_owned();
    }

    root
}

/// Record `parent` as a stack floor when a stack is being rooted on it: it is
/// not the trunk, and has no stack parent of its own. Called only where the
/// user says so - `new`, `adopt`, `insert`, `prepend` - never from `sync` or
/// `repair`, where a parentless parent is far more likely to be a branch whose
/// metadata has not been rebuilt yet than a base.
fn mark_floor_if_rooting(parent: &str, dry_run: bool) -> Result<()> {
    let trunk = trunk_branch(&git::local_branches()?);
    if Some(parent) == trunk.as_deref() || stacked_parent_of(parent)?.is_some() || is_floor(parent)?
    {
        return Ok(());
    }

    // Stacking on an unadopted branch is ambiguous - a release line and a
    // stack branch nobody has adopted yet look identical, and only the person
    // typing knows which this is. Record the reading that makes the branch
    // safe, but say so and name the way back, because the alternative reading
    // means the branch is frozen out of its own restacks until someone does.
    // Announced on a dry run too: this writes metadata to a branch the command
    // does not name, which is the last thing to leave to a surprise.
    if !dry_run {
        mark_floor(parent);
    }
    anstream::println!(
        "{}",
        style::dim(&format!(
            "{} {parent} as this stack's base; \
             if it is a stacked branch, run `git stk detach {parent}`",
            if dry_run { "would record" } else { "recorded" }
        ))
    );
    Ok(())
}

/// Whether `branch` is a stack floor - see [`FLOOR_KEY`].
pub fn is_floor(branch: &str) -> Result<bool> {
    Ok(git::config_get(&floor_key(branch))?.is_some())
}

/// Record `branch` as a stack floor. Best effort: a floor that fails to record
/// is still derived from the shape while branches sit on it, so a failure here
/// costs persistence, not protection.
pub fn mark_floor(branch: &str) {
    let _ = git::config_set(&floor_key(branch), "true");
}

pub fn clear_floor(branch: &str) -> Result<()> {
    git::config_unset(&floor_key(branch))
}

pub(crate) fn parent_of(branch: &str) -> Result<Option<String>> {
    git::config_get(&parent_key(branch))
}

/// The branch's stack parent for any purpose that walks or rewrites the stack:
/// `None` for a floor, whatever `stkParent` it may have picked up, because the
/// base a stack sits on is not ours to move. [`parent_of`] is the raw read,
/// kept for `repair` - which exists to fix such metadata - and for snapshots,
/// which record state exactly as it was.
pub(crate) fn stacked_parent_of(branch: &str) -> Result<Option<String>> {
    if is_floor(branch)? {
        return Ok(None);
    }
    parent_of(branch)
}

pub(crate) fn base_of(branch: &str) -> Result<Option<String>> {
    git::config_get(&base_key(branch))
}

pub(crate) fn set_parent(branch: &str, parent: &str) -> Result<()> {
    git::config_set(&parent_key(branch), parent)
}

pub(crate) fn unset_parent(branch: &str) -> Result<()> {
    git::config_unset(&parent_key(branch))
}

pub(crate) fn set_base(branch: &str, base: &str) -> Result<()> {
    git::config_set(&base_key(branch), base)
}

pub(crate) fn unset_base(branch: &str) -> Result<()> {
    git::config_unset(&base_key(branch))
}

fn floor_key(branch: &str) -> String {
    format!("branch.{branch}.{FLOOR_KEY}")
}

fn parent_key(branch: &str) -> String {
    format!("branch.{branch}.{PARENT_KEY}")
}

fn base_key(branch: &str) -> String {
    format!("branch.{branch}.{BASE_KEY}")
}

fn renamed_from_key(branch: &str) -> String {
    format!("branch.{branch}.{RENAMED_FROM_KEY}")
}

#[cfg(test)]
mod tests {
    use super::is_safe_ref_name;

    #[test]
    fn safe_ref_names_pass() {
        assert!(is_safe_ref_name("main"));
        assert!(is_safe_ref_name("feature/a"));
        assert!(is_safe_ref_name("user/fix-123"));
    }

    #[test]
    fn unsafe_ref_names_are_rejected() {
        // The injection vector: a name git would parse as an option.
        assert!(!is_safe_ref_name("--upload-pack=touch /tmp/pwned"));
        assert!(!is_safe_ref_name("-x"));
        // Whitespace / control chars (newline-bearing refspecs, etc.).
        assert!(!is_safe_ref_name("a branch"));
        assert!(!is_safe_ref_name("a\nb"));
        assert!(!is_safe_ref_name("a\tb"));
        assert!(!is_safe_ref_name(""));
    }
}
