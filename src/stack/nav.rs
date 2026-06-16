//! Moving around the stack and printing it.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use super::{
    children_map, children_of, current_stack_branches, parent_map, parent_of, root_for,
    trunk_branch,
};
use crate::git;
use crate::prompt;
use crate::style;

/// Offer a numbered pick of `children`; None when nothing was chosen
/// (non-interactive stdin, or an invalid answer).
fn pick_child(title: &str, children: &[String]) -> anyhow::Result<Option<String>> {
    let painted: Vec<String> = children
        .iter()
        .map(|child| style::paint(style::BRANCH, child))
        .collect();
    Ok(prompt::pick(title, &painted)?.map(|index| children[index].clone()))
}

pub fn print_parent(branch: Option<&str>) -> Result<()> {
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    match parent_of(&branch)? {
        Some(parent) => println!("{parent}"),
        None => bail!("{branch} has no stack parent"),
    }
    Ok(())
}

pub fn print_children(branch: Option<&str>) -> Result<()> {
    let branch = branch
        .map(str::to_owned)
        .map_or_else(git::current_branch, Ok)?;
    for child in children_of(&branch)? {
        println!("{child}");
    }
    Ok(())
}

pub fn checkout_parent() -> Result<()> {
    let current = git::current_branch()?;
    let Some(parent) = parent_of(&current)? else {
        bail!("{current} has no stack parent");
    };

    git::checkout(&parent)
}

pub fn checkout_child(branch: Option<&str>) -> Result<()> {
    let current = git::current_branch()?;
    let children = children_of(&current)?;
    let child = match (branch, children.as_slice()) {
        (Some(branch), _) => {
            if children.iter().any(|child| child == branch) {
                branch.to_owned()
            } else {
                bail!("{branch} is not a stack child of {current}");
            }
        }
        (None, [child]) => child.to_owned(),
        (None, []) => bail!("{current} has no stack children"),
        (None, _) => {
            match pick_child(
                &format!("{current} has multiple stack children:"),
                &children,
            )? {
                Some(child) => child,
                None => bail!("choose one with `git stk up <branch>`"),
            }
        }
    };

    git::checkout(&child)
}

/// Check out the leaf of the current stack, following single children. A
/// fork is ambiguous, like `up` without a branch.
pub fn checkout_top() -> Result<()> {
    let current = git::current_branch()?;
    let mut top = current.clone();
    loop {
        let children = children_of(&top)?;
        match children.as_slice() {
            [] => break,
            [child] => top = child.clone(),
            // A pick resolves the fork and the climb continues from there.
            _ => match pick_child(&format!("{top} has multiple stack children:"), &children)? {
                Some(child) => top = child,
                None => bail!("walk up from {top} with `git stk up <branch>`"),
            },
        }
    }

    if top == current {
        if children_of(&current)?.is_empty() && parent_of(&current)?.is_none() {
            bail!("{current} is not in a stack");
        }
        anstream::println!("{current} is already at the top of the stack");
        return Ok(());
    }
    git::checkout(&top)
}

/// Check out the bottom of the current stack: the branch just above the
/// trunk. From the trunk itself, a single stacked child is unambiguous.
pub fn checkout_bottom() -> Result<()> {
    let current = git::current_branch()?;
    let trunk = trunk_branch(&git::local_branches()?);

    let bottom = if Some(&current) == trunk.as_ref() {
        let children = children_of(&current)?;
        match children.as_slice() {
            [child] => child.clone(),
            [] => bail!("{current} has no stacked branches"),
            _ => {
                match pick_child(
                    &format!("{current} has multiple stack children:"),
                    &children,
                )? {
                    Some(child) => child,
                    None => bail!("choose one with `git stk up <branch>`"),
                }
            }
        }
    } else {
        let mut bottom = current.clone();
        while let Some(parent) = parent_of(&bottom)? {
            if Some(&parent) == trunk.as_ref() {
                break;
            }
            bottom = parent;
        }
        bottom
    };

    if bottom == current {
        if parent_of(&current)?.is_none() && children_of(&current)?.is_empty() {
            bail!("{current} is not in a stack");
        }
        anstream::println!("{current} is already at the bottom of the stack");
        return Ok(());
    }
    git::checkout(&bottom)
}

pub fn print_stack(reviews: &BTreeMap<String, String>, commits: bool) -> Result<()> {
    let current = git::current_branch()?;
    let parents = parent_map()?;
    let root = root_for(&current, &parents);
    let trunk = trunk_branch(&git::local_branches()?);

    // A lone branch (or the bare trunk) is not a stack - say so rather than
    // drawing a one-node "stack".
    if parent_of(&current)?.is_none() && children_of(&current)?.is_empty() {
        anstream::println!("no stacked branches");
        anstream::println!(
            "{}",
            style::dim("create one on top of the current branch with `git stk new <branch>`")
        );
        return Ok(());
    }

    // Scope to the current branch's own line (fork siblings included, stacks
    // that merely share the trunk excluded), so `list` shows just the stack you
    // are on; `--all` is for the rest. The trunk stays as the rendered root so
    // the stack still reads as sitting on its base.
    let stack: BTreeSet<String> = current_stack_branches(&current)?.into_iter().collect();
    let children: BTreeMap<String, Vec<String>> = children_map(&parents)
        .into_iter()
        .map(|(parent, kids)| {
            let kept = kids.into_iter().filter(|kid| stack.contains(kid)).collect();
            (parent, kept)
        })
        .collect();

    let sizes = diff_sizes(stack.iter().cloned(), &parents);
    let ctx = TreeCtx {
        current: &current,
        trunk: trunk.as_deref(),
        children: &children,
        parents: &parents,
        reviews,
        sizes: &sizes,
        commits,
        width: term_width(),
    };
    let mut lines = Vec::new();
    collect_tree_lines(&ctx, &root, 0, &mut BTreeSet::new(), &mut lines);

    // Leaf-first, trunk last: the stack reads like a pile sitting on its
    // base, matching the up/down direction of navigation.
    for line in lines.iter().rev() {
        anstream::println!("{line}");
    }

    for branch in &stack {
        if let Some(parent) = parents.get(branch)
            && let Some(hint) = behind_parent_hint(branch, parent)
        {
            anstream::println!("{} {hint}", style::paint(style::HINT, "hint:"));
        }
    }
    Ok(())
}

/// Print every stack, not just the current one: the trunk-rooted forest with
/// a single trunk line at the bottom, and each rootless fragment as its own
/// tree above it. The branch you are on is marked wherever it appears.
pub fn print_all_stacks(reviews: &BTreeMap<String, String>, commits: bool) -> Result<()> {
    let current = git::current_branch()?;
    let parents = parent_map()?;
    let children = children_map(&parents);
    let trunk = trunk_branch(&git::local_branches()?);

    // The root of every stack: each branch with a stack relationship, walked
    // up to its topmost ancestor. Trunk-anchored stacks all resolve to the
    // trunk; rootless fragments resolve to their own top. Lone branches with
    // no parent or children never enter `parents`, so they are left out.
    let mut roots = Vec::new();
    let mut seen = BTreeSet::new();
    for branch in parents
        .iter()
        .flat_map(|(child, parent)| [child.clone(), parent.clone()])
    {
        let root = root_for(&branch, &parents);
        if seen.insert(root.clone()) {
            roots.push(root);
        }
    }

    if roots.is_empty() {
        anstream::println!("no stacked branches");
        return Ok(());
    }

    // Render the trunk-rooted forest last so its trunk line sits at the very
    // bottom; rootless fragments stack above it.
    roots.sort_by_key(|root| Some(root.as_str()) == trunk.as_deref());

    let sizes = diff_sizes(parents.keys().cloned(), &parents);
    let ctx = TreeCtx {
        current: &current,
        trunk: trunk.as_deref(),
        children: &children,
        parents: &parents,
        reviews,
        sizes: &sizes,
        commits,
        width: term_width(),
    };
    for (index, root) in roots.iter().enumerate() {
        if index > 0 {
            anstream::println!();
        }
        let mut lines = Vec::new();
        collect_tree_lines(&ctx, root, 0, &mut BTreeSet::new(), &mut lines);
        for line in lines.iter().rev() {
            anstream::println!("{line}");
        }
    }

    // Behind-parent hints span every stack.
    for (branch, parent) in &parents {
        if let Some(hint) = behind_parent_hint(branch, parent) {
            anstream::println!("{} {hint}", style::paint(style::HINT, "hint:"));
        }
    }
    Ok(())
}

/// A restack nudge when `branch` is missing commits from its parent's tip.
/// Local-only; a missing parent yields nothing.
pub fn behind_parent_hint(branch: &str, parent: &str) -> Option<String> {
    let behind = git::commits_behind(branch, parent)
        .ok()
        .filter(|count| *count > 0)?;
    Some(format!(
        "{branch} is {behind} commit{} behind {parent} - run `git stk restack`",
        if behind == 1 { "" } else { "s" }
    ))
}

/// The read-only context for rendering a stack tree, threaded through the
/// recursion so each call only varies `branch`/`depth` and the accumulators.
struct TreeCtx<'a> {
    current: &'a str,
    trunk: Option<&'a str>,
    children: &'a BTreeMap<String, Vec<String>>,
    parents: &'a BTreeMap<String, String>,
    reviews: &'a BTreeMap<String, String>,
    sizes: &'a BTreeMap<String, (usize, usize)>,
    /// `--commits`: list each branch's own commits beneath it.
    commits: bool,
    /// Terminal width, for truncating commit subjects.
    width: usize,
}

/// Terminal width for truncation, defaulting to 80 when not a terminal.
fn term_width() -> usize {
    console::Term::stdout()
        .size_checked()
        .map_or(80, |(_, cols)| cols as usize)
}

/// Per-branch diff size (added, deleted lines) against its stack parent, for
/// each branch that has one. Best effort: a branch with no parent, or whose
/// diff cannot be read, is left out of the map and simply shows no size.
fn diff_sizes(
    branches: impl IntoIterator<Item = String>,
    parents: &BTreeMap<String, String>,
) -> BTreeMap<String, (usize, usize)> {
    let mut sizes = BTreeMap::new();
    for branch in branches {
        if let Some(parent) = parents.get(&branch)
            && let Ok(size) = git::diff_numstat(parent, &branch)
        {
            sizes.insert(branch, size);
        }
    }
    sizes
}

fn collect_tree_lines(
    ctx: &TreeCtx,
    branch: &str,
    depth: usize,
    seen: &mut BTreeSet<String>,
    lines: &mut Vec<String>,
) {
    // A graphite-style rail: a filled marker on the branch you are on.
    let mut line = "  ".repeat(depth);
    if branch == ctx.current {
        line.push_str(&style::paint(style::CURRENT, &format!("\u{25c9} {branch}")));
    } else {
        line.push_str("\u{25cb} ");
        line.push_str(&style::paint(style::BRANCH, branch));
    }
    if Some(branch) == ctx.trunk {
        line.push_str(&style::paint(style::DIM, " (trunk)"));
    }
    // Optional annotations in one paren group: the dimmed open review number,
    // then the diff size against the parent in faded green/red, like a diff.
    let mut tags: Vec<String> = Vec::new();
    if let Some(id) = ctx.reviews.get(branch) {
        tags.push(style::paint(style::DIM, id));
    }
    // An empty branch (same tip as its parent) shows no size rather than a
    // noisy "+0/-0".
    if let Some((added, deleted)) = ctx.sizes.get(branch)
        && (*added > 0 || *deleted > 0)
    {
        tags.push(format!(
            "{}{}{}",
            style::paint(style::ADDED, &format!("+{added}")),
            style::paint(style::DIM, "/"),
            style::paint(style::REMOVED, &format!("-{deleted}")),
        ));
    }
    if !tags.is_empty() {
        let separator = style::paint(style::DIM, ", ");
        line.push_str(&style::paint(style::DIM, " ("));
        line.push_str(&tags.join(&separator));
        line.push_str(&style::paint(style::DIM, ")"));
    }

    // With --commits, list the branch's own commits (parent..branch) under it.
    // `lines` is printed reversed, so push them here (before the branch line)
    // oldest-first: the reversal then shows them newest-first - git log order -
    // directly below the branch. The trunk and parentless roots have no "own"
    // commits to show.
    if ctx.commits
        && Some(branch) != ctx.trunk
        && let Some(parent) = ctx.parents.get(branch)
    {
        let indent = "  ".repeat(depth + 1);
        match git::log_oneline(&format!("{parent}..{branch}")) {
            Ok(commits) if !commits.is_empty() => {
                for (sha, subject) in commits.iter().rev() {
                    let budget = ctx
                        .width
                        .saturating_sub(indent.len() + sha.len() + 2)
                        .max(16);
                    let subject = console::truncate_str(subject, budget, "…");
                    lines.push(format!(
                        "{indent}{}  {}",
                        style::paint(style::DIM, sha),
                        style::paint(style::DIM, &subject)
                    ));
                }
            }
            Ok(_) => lines.push(format!(
                "{indent}{}",
                style::paint(style::DIM, "(no commits)")
            )),
            Err(_) => {}
        }
    }

    lines.push(line);

    if !seen.insert(branch.to_owned()) {
        lines.push(format!("{}<cycle detected>", "  ".repeat(depth + 1)));
        return;
    }

    if let Some(branch_children) = ctx.children.get(branch) {
        for child in branch_children {
            collect_tree_lines(ctx, child, depth + 1, seen, lines);
        }
    }
}
