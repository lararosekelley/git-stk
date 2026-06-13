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
    behind_parent_hint, checkout_bottom, checkout_child, checkout_parent, checkout_top,
    print_all_stacks, print_children, print_parent, print_stack,
};
pub use restack::{abort_restack, continue_restack, restack};
pub use snapshot::{take as snapshot, undo};

const PARENT_KEY: &str = "stkParent";
const BASE_KEY: &str = "stkBase";
/// Marks a branch as the rename of another that still has an open review, so
/// the next submit can replace and close that review.
const RENAMED_FROM_KEY: &str = "stkRenamedFrom";

pub fn create_branch(branch: &str) -> Result<()> {
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
    git::create_branch(branch)?;
    set_parent(branch, &parent)?;
    record_base(branch, &parent);
    anstream::println!(
        "created {} with parent {}",
        style::branch(branch),
        style::branch(&parent)
    );
    Ok(())
}

/// Insert a new empty branch directly above the current one, moving the
/// current branch's children onto it. The new branch shares the current tip,
/// so descendants stay correctly based; commit to it, then `restack` to
/// replay them. Any uncommitted changes ride onto the new branch, like `new`.
pub fn insert_branch(branch: &str) -> Result<()> {
    ensure_absent(branch)?;
    let current = git::current_branch()?;
    let children = children_of(&current)?;

    snapshot::take("new --insert");
    git::create_branch(branch)?; // off current; leaves us on the new branch
    set_parent(branch, &current)?;
    record_base(branch, &current);
    for child in &children {
        set_parent(child, branch)?;
        record_base(child, branch);
    }

    anstream::println!(
        "inserted {} above {}",
        style::branch(branch),
        style::branch(&current)
    );
    for child in &children {
        anstream::println!(
            "retargeted {} -> {}",
            style::branch(child),
            style::branch(branch)
        );
    }
    Ok(())
}

/// Insert a new empty branch directly below the current one, moving the
/// current branch onto it. Branches from the current branch's parent, so it
/// requires a clean worktree. Commit to it, then `restack`.
pub fn prepend_branch(branch: &str) -> Result<()> {
    ensure_absent(branch)?;
    let current = git::current_branch()?;
    let parent =
        parent_of(&current)?.context("current branch has no stack parent to prepend below")?;
    if !git::worktree_is_clean()? {
        bail!(
            "working tree has uncommitted changes; commit or stash before `git stk new --prepend`"
        );
    }

    snapshot::take("new --prepend");
    git::checkout(&parent)?;
    git::create_branch(branch)?; // off the parent; leaves us on the new branch
    set_parent(branch, &parent)?;
    record_base(branch, &parent);
    set_parent(&current, branch)?;
    record_base(&current, branch);

    anstream::println!(
        "inserted {} between {} and {}",
        style::branch(branch),
        style::branch(&parent),
        style::branch(&current)
    );
    anstream::println!(
        "retargeted {} -> {}",
        style::branch(&current),
        style::branch(branch)
    );
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

pub fn adopt_branch(branch: &str, parent: &str) -> Result<()> {
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

    set_parent(branch, parent)?;
    record_base(branch, parent);
    anstream::println!(
        "attached {} to {}",
        style::branch(branch),
        style::branch(parent)
    );
    Ok(())
}

pub fn detach_branch(branch: Option<&str>) -> Result<()> {
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    unset_parent(&branch)?;
    unset_base(&branch)?;
    anstream::println!("detached {}", style::branch(&branch));
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

/// Every branch in the stack `branch` belongs to, trunk excluded: the whole
/// subtree under the stack's root (so fork siblings are included too), unlike
/// [`stack_line`] which is only `branch`'s own line. The root is the trunk for
/// an anchored stack - dropped here - or an unanchored base branch, which is a
/// real stacked branch and stays in.
pub fn current_stack_branches(branch: &str) -> Result<Vec<String>> {
    let root = stack_root(branch)?;
    let trunk = trunk_branch(&git::local_branches()?);
    Ok(branch_and_descendants(&root)?
        .into_iter()
        .filter(|candidate| Some(candidate) != trunk.as_ref())
        .collect())
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
    for branch in current_stack_branches(&current)? {
        if let Some(parent) = parent_of(&branch)? {
            parents.insert(branch, Value::String(parent));
        }
    }
    if parents.is_empty() {
        return Ok(());
    }

    let document = json!({ "trunk": trunk, "parents": parents });
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

    // Fetch every listed branch first, so each parent resolves locally before
    // we record it.
    let local: BTreeSet<String> = git::local_branches()?.into_iter().collect();
    for branch in parents.keys() {
        if !local.contains(branch) {
            git::fetch_branch(remote, branch)
                .with_context(|| format!("failed to fetch {branch} from {remote}"))?;
        }
    }

    let mut attached = 0;
    for (branch, parent) in parents {
        let Some(parent) = parent.as_str() else {
            continue;
        };
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

/// The stack path from the bottom up to (and including) `branch`,
/// parent-first; descendants above it are left out.
pub fn path_from_root(branch: &str) -> Result<Vec<String>> {
    let trunk = trunk_branch(&git::local_branches()?);
    let mut path = vec![branch.to_owned()];
    let mut seen = BTreeSet::from([branch.to_owned()]);

    let mut cursor = branch.to_owned();
    while let Some(parent) = parent_of(&cursor)? {
        if Some(&parent) == trunk.as_ref() || !seen.insert(parent.clone()) {
            break;
        }
        path.push(parent.clone());
        cursor = parent;
    }

    path.reverse();
    Ok(path)
}

/// (branch, parent) pairs for the branches that have a stack parent;
/// branches without one are skipped.
pub fn branch_parents(branches: &[String]) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for branch in branches {
        if let Some(parent) = parent_of(branch)? {
            pairs.push((branch.clone(), parent));
        }
    }
    Ok(pairs)
}

fn parent_map() -> Result<BTreeMap<String, String>> {
    let mut parents = BTreeMap::new();
    for branch in git::local_branches()? {
        if let Some(parent) = parent_of(&branch)? {
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

pub(crate) fn parent_of(branch: &str) -> Result<Option<String>> {
    git::config_get(&parent_key(branch))
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

fn parent_key(branch: &str) -> String {
    format!("branch.{branch}.{PARENT_KEY}")
}

fn base_key(branch: &str) -> String {
    format!("branch.{branch}.{BASE_KEY}")
}

fn renamed_from_key(branch: &str) -> String {
    format!("branch.{branch}.{RENAMED_FROM_KEY}")
}
