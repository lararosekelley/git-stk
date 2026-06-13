//! Moving around the stack and printing it.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use super::{
    branch_and_descendants, children_map, children_of, parent_map, parent_of, root_for,
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

pub fn print_stack(reviews: &BTreeMap<String, String>) -> Result<()> {
    let current = git::current_branch()?;
    let parents = parent_map()?;
    let root = root_for(&current, &parents);
    let children = children_map(&parents);
    let trunk = trunk_branch(&git::local_branches()?);

    let mut lines = Vec::new();
    collect_tree_lines(
        &root,
        &current,
        trunk.as_deref(),
        &children,
        reviews,
        0,
        &mut BTreeSet::new(),
        &mut lines,
    );

    // Leaf-first, trunk last: the stack reads like a pile sitting on its
    // base, matching the up/down direction of navigation.
    for line in lines.iter().rev() {
        anstream::println!("{line}");
    }

    for branch in branch_and_descendants(&root)? {
        if let Some(parent) = parents.get(&branch)
            && let Some(hint) = behind_parent_hint(&branch, parent)
        {
            anstream::println!("{} {hint}", style::paint(style::HINT, "hint:"));
        }
    }
    Ok(())
}

/// Print every stack, not just the current one: the trunk-rooted forest with
/// a single trunk line at the bottom, and each rootless fragment as its own
/// tree above it. The branch you are on is marked wherever it appears.
pub fn print_all_stacks(reviews: &BTreeMap<String, String>) -> Result<()> {
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

    for (index, root) in roots.iter().enumerate() {
        if index > 0 {
            anstream::println!();
        }
        let mut lines = Vec::new();
        collect_tree_lines(
            root,
            &current,
            trunk.as_deref(),
            &children,
            reviews,
            0,
            &mut BTreeSet::new(),
            &mut lines,
        );
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

#[allow(clippy::too_many_arguments)]
fn collect_tree_lines(
    branch: &str,
    current: &str,
    trunk: Option<&str>,
    children: &BTreeMap<String, Vec<String>>,
    reviews: &BTreeMap<String, String>,
    depth: usize,
    seen: &mut BTreeSet<String>,
    lines: &mut Vec<String>,
) {
    // A graphite-style rail: a filled marker on the branch you are on.
    let mut line = "  ".repeat(depth);
    if branch == current {
        line.push_str(&style::paint(style::CURRENT, &format!("\u{25c9} {branch}")));
    } else {
        line.push_str("\u{25cb} ");
        line.push_str(&style::paint(style::BRANCH, branch));
    }
    if Some(branch) == trunk {
        line.push_str(&style::paint(style::DIM, " (trunk)"));
    }
    // The branch's open review number, when one is known.
    if let Some(id) = reviews.get(branch) {
        line.push_str(&style::paint(style::DIM, &format!(" ({id})")));
    }
    lines.push(line);

    if !seen.insert(branch.to_owned()) {
        lines.push(format!("{}<cycle detected>", "  ".repeat(depth + 1)));
        return;
    }

    if let Some(branch_children) = children.get(branch) {
        for child in branch_children {
            collect_tree_lines(
                child,
                current,
                trunk,
                children,
                reviews,
                depth + 1,
                seen,
                lines,
            );
        }
    }
}
