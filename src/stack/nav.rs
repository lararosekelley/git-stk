//! Moving around the stack and printing it.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use super::{
    children_map, children_of, current_stack_branches, parent_map, parent_of, root_for,
    trunk_branch,
};
use crate::git;
use crate::prompt;
use crate::providers::ReviewAnnotation;
use crate::style;

/// How a navigation command reports where it landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavOutput {
    /// Announce the switch on stdout - the default.
    Announce,
    /// Print only the destination directory on stdout, so `cd "$(git stk up
    /// --from-path)"` moves the caller's shell. Everything else goes to stderr.
    Path,
}

/// Go to `branch`, or - under `--from-path` - say where to go.
///
/// A branch living in another worktree cannot be checked out, but it can be
/// walked to, so `--from-path` hands back that worktree and lets the shell do
/// what git cannot. Without it there is nothing the caller can act on, so the
/// collision stays an error.
fn navigate_to(branch: &str, output: NavOutput) -> Result<()> {
    if output == NavOutput::Announce {
        return git::checkout(branch);
    }

    if let Some(path) = git::worktree_holding(branch).ok().flatten() {
        anstream::eprintln!(
            "{} is checked out in the worktree at {}",
            style::paint(style::BRANCH, branch),
            git::display_path(&path)
        );
        println!("{}", git::display_path(&path));
        return Ok(());
    }

    git::checkout_silently(branch)?;
    anstream::eprintln!("switched to {}", git::switched_to(branch));
    // "." rather than the repo root: the branch is here, only HEAD moved, so
    // the caller's `cd` must not drag them out of a subdirectory.
    println!(".");
    Ok(())
}

/// Under `--from-path`, a command that lands nowhere still has to print a
/// destination or the caller's `cd` fails on empty input.
fn stay_put(message: &str, output: NavOutput) {
    match output {
        NavOutput::Announce => anstream::println!("{message}"),
        NavOutput::Path => {
            anstream::eprintln!("{message}");
            println!(".");
        }
    }
}

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

/// "N branch(es)", for the errors of a walk that ran out of stack.
fn branches(count: usize) -> String {
    format!("{count} branch{}", if count == 1 { "" } else { "es" })
}

/// Walk `distance` branches down, towards the trunk.
pub fn checkout_parent(distance: usize, output: NavOutput) -> Result<()> {
    let current = git::current_branch()?;
    let mut at = current.clone();
    for moved in 0..distance {
        let Some(parent) = parent_of(&at)? else {
            if moved == 0 {
                bail!("{current} has no stack parent");
            }
            bail!(
                "cannot go down {distance}: {current} is only {} above {at}",
                branches(moved)
            );
        };
        at = parent;
    }

    navigate_to(&at, output)
}

/// Walk `distance` branches up, towards the leaf, prompting at each fork the
/// way `top` does. `up <branch>` names a child instead and always moves one.
pub fn checkout_child(branch: Option<&str>, distance: usize, output: NavOutput) -> Result<()> {
    let current = git::current_branch()?;
    if let Some(branch) = branch {
        let children = children_of(&current)?;
        if !children.iter().any(|child| child == branch) {
            bail!("{branch} is not a stack child of {current}");
        }
        return navigate_to(branch, output);
    }

    let mut at = current.clone();
    for moved in 0..distance {
        let children = children_of(&at)?;
        match children.as_slice() {
            [child] => at = child.clone(),
            [] => {
                if moved == 0 {
                    bail!("{current} has no stack children");
                }
                bail!(
                    "cannot go up {distance}: {current} is only {} below {at}",
                    branches(moved)
                );
            }
            _ => match pick_child(&format!("{at} has multiple stack children:"), &children)? {
                Some(child) => at = child,
                None if moved == 0 => bail!("choose one with `git stk up <branch>`"),
                None => bail!("walk up from {at} with `git stk up <branch>`"),
            },
        }
    }

    navigate_to(&at, output)
}

/// Check out the leaf of the current stack, following single children. A
/// fork is ambiguous, like `up` without a branch.
pub fn checkout_top(output: NavOutput) -> Result<()> {
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
        stay_put(
            &format!("{current} is already at the top of the stack"),
            output,
        );
        return Ok(());
    }
    navigate_to(&top, output)
}

/// Check out the bottom of the current stack: the branch just above the
/// trunk. From the trunk itself, a single stacked child is unambiguous.
pub fn checkout_bottom(output: NavOutput) -> Result<()> {
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
        stay_put(
            &format!("{current} is already at the bottom of the stack"),
            output,
        );
        return Ok(());
    }
    navigate_to(&bottom, output)
}

pub fn print_stack(reviews: &BTreeMap<String, ReviewAnnotation>, commits: bool) -> Result<()> {
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
    let worktrees = worktree_map();
    let ctx = TreeCtx {
        current: &current,
        trunk: trunk.as_deref(),
        children: &children,
        parents: &parents,
        reviews,
        sizes: &sizes,
        worktrees: &worktrees,
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

/// Print every stack, not just the current one, each as its own block separated
/// by a blank line. Stacks that merely share the trunk are drawn separately -
/// one per direct trunk child - each repeating the trunk as its base, so they
/// read as distinct piles rather than one tangled tree. Rootless fragments print
/// above the trunk-anchored ones. The branch you are on is marked wherever it
/// appears.
pub fn print_all_stacks(reviews: &BTreeMap<String, ReviewAnnotation>, commits: bool) -> Result<()> {
    let current = git::current_branch()?;
    let parents = parent_map()?;
    let children = children_map(&parents);
    let trunk = trunk_branch(&git::local_branches()?);

    // Rootless fragments: stacks not anchored on the trunk, each walked up to
    // its own topmost ancestor. Lone branches with no parent or children never
    // enter `parents`, so they are left out.
    let mut rootless = Vec::new();
    let mut seen = BTreeSet::new();
    for branch in parents
        .iter()
        .flat_map(|(child, parent)| [child.clone(), parent.clone()])
    {
        let root = root_for(&branch, &parents);
        if Some(root.as_str()) != trunk.as_deref() && seen.insert(root.clone()) {
            rootless.push(root);
        }
    }
    rootless.sort();

    // Each direct child of the trunk is the base of a distinct stack sharing the
    // trunk; a fork deeper in a stack shares a real branch, not just the trunk,
    // so it stays within one block.
    let trunk_bases: Vec<String> = trunk
        .as_deref()
        .and_then(|name| children.get(name))
        .cloned()
        .unwrap_or_default();

    if rootless.is_empty() && trunk_bases.is_empty() {
        anstream::println!("no stacked branches");
        return Ok(());
    }

    let sizes = diff_sizes(parents.keys().cloned(), &parents);
    let worktrees = worktree_map();
    let width = term_width();
    let mut first = true;
    let mut render = |root: &str, block_children: &BTreeMap<String, Vec<String>>| {
        if !first {
            anstream::println!();
        }
        first = false;
        let ctx = TreeCtx {
            current: &current,
            trunk: trunk.as_deref(),
            children: block_children,
            parents: &parents,
            reviews,
            sizes: &sizes,
            worktrees: &worktrees,
            commits,
            width,
        };
        let mut lines = Vec::new();
        collect_tree_lines(&ctx, root, 0, &mut BTreeSet::new(), &mut lines);
        for line in lines.iter().rev() {
            anstream::println!("{line}");
        }
    };

    // Rootless fragments first, trunk-anchored stacks last so their trunk lines
    // sit at the bottom of the output.
    for root in &rootless {
        render(root, &children);
    }
    if let Some(name) = trunk.as_deref() {
        for base in &trunk_bases {
            // Restrict the trunk to this one base so the block shows a single
            // stack sitting on its own trunk line.
            let mut block_children = children.clone();
            block_children.insert(name.to_owned(), vec![base.clone()]);
            render(name, &block_children);
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
    reviews: &'a BTreeMap<String, ReviewAnnotation>,
    sizes: &'a BTreeMap<String, (usize, usize)>,
    /// Branches checked out in other worktrees, and where. Answers "where does
    /// this branch actually live" - the one thing a multi-worktree user cannot
    /// get from the tree otherwise.
    worktrees: &'a BTreeMap<String, std::path::PathBuf>,
    /// `--commits`: list each branch's own commits beneath it.
    commits: bool,
    /// Terminal width, for truncating commit subjects.
    width: usize,
}

/// Branches held by other worktrees, keyed for lookup while drawing. Best
/// effort: a failed listing just means no annotations, never a failed `list`.
fn worktree_map() -> BTreeMap<String, std::path::PathBuf> {
    git::worktree_branches()
        .unwrap_or_default()
        .into_iter()
        .collect()
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
    // Optional annotations in one paren group: the CI dot and dimmed open
    // review number, then the diff size against the parent in faded green/red,
    // like a diff.
    let mut tags: Vec<String> = Vec::new();
    if let Some(review) = ctx.reviews.get(branch) {
        // A queued review shows just the clock - it is waiting to land, so its
        // (usually pending) CI dot would only be noise alongside it.
        let marker = if review.queued {
            crate::providers::QUEUED_MARK
        } else {
            review.checks.dot()
        };
        tags.push(format!("{marker}{}", style::paint(style::DIM, &review.id)));
        // The platform's own stack, when it keeps one - distinct from the tree
        // itself, which is git-stk's view of the same branches.
        if let Some(stack) = review.stack {
            tags.push(style::paint(
                style::DIM,
                &format!(
                    "{}{}/{}",
                    crate::providers::STACKED_MARK,
                    stack.position,
                    stack.size
                ),
            ));
        }
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
    // Outside the metrics group and dimmed: a location, not a measurement, and
    // scannable down the column when several branches live elsewhere.
    if let Some(path) = ctx.worktrees.get(branch) {
        line.push_str(&style::paint(
            style::DIM,
            &format!("  {}", git::display_path(path)),
        ));
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

    // With --reviews, list the review's tallies under it, like --commits. The
    // annotation carries a summary only when the flag is set. `lines` is
    // printed reversed, so push them in reverse display order (see above).
    if Some(branch) != ctx.trunk
        && let Some(review) = ctx.reviews.get(branch)
        && let Some(summary) = &review.summary
    {
        let indent = "  ".repeat(depth + 1);
        let summary_lines = summary.lines();
        if summary_lines.is_empty() {
            lines.push(format!(
                "{indent}{}",
                style::paint(style::DIM, "(no reviews)")
            ));
        } else {
            for text in summary_lines.iter().rev() {
                lines.push(format!("{indent}{}", style::paint(style::DIM, text)));
            }
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
