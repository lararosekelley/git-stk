use std::collections::BTreeSet;
use std::io::IsTerminal;

use anyhow::{Result, anyhow, bail};
use clap::ArgAction;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, MultiSelect};

use crate::commands::Run;
use crate::git;
use crate::stack;
use crate::style;

/// Split the current branch's commits into a stack of branches, bottom-up.
///
/// The current branch is reused as the leaf (it keeps its name and tip); new
/// branches are created beneath it for the commits below. Non-destructive: the
/// new branches point at the existing commits, so nothing is rewritten.
#[derive(Debug, clap::Args)]
pub struct Split {
    /// One branch per commit, named from each commit's subject; skips the
    /// interactive grouping picker.
    #[arg(long, action = ArgAction::SetTrue)]
    per_commit: bool,
    /// Print the plan without creating branches or writing metadata.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
}

impl Run for Split {
    fn run(self) -> Result<()> {
        if self.per_commit {
            split_per_commit(self.dry_run)
        } else {
            split_interactive(self.dry_run)
        }
    }
}

/// A planned branch: its name and the commit it should point at.
struct Plan {
    name: String,
    sha: String,
}

fn split_per_commit(dry_run: bool) -> Result<()> {
    let branch = git::current_branch()?;
    let base = base_of(&branch)?;

    // Commits on the branch above its base, oldest first.
    let mut commits = git::rev_list(&format!("{base}..{branch}"))?;
    commits.reverse();
    if commits.len() < 2 {
        bail!(
            "{branch} has {} commit(s) above {base}; need at least 2 to split",
            commits.len()
        );
    }

    // One branch per commit, bottom-up. The top commit keeps the original
    // branch name (the leaf); the rest get names slugged from their subjects.
    let existing: std::collections::BTreeSet<String> = git::local_branches()?.into_iter().collect();
    let mut used: std::collections::BTreeSet<String> = existing.clone();
    let mut plan: Vec<Plan> = Vec::new();
    let last = commits.len() - 1;
    for (index, sha) in commits.iter().enumerate() {
        let name = if index == last {
            branch.clone()
        } else {
            let subject = git::commit_subject(sha)?;
            unique_name(&slugify(&subject), &mut used)
        };
        plan.push(Plan {
            name,
            sha: sha.clone(),
        });
    }

    apply(&branch, &base, &plan, dry_run)
}

/// Interactive split: pick which commits start a branch, then name each one.
/// The top commit always stays on the original branch (the leaf).
fn split_interactive(dry_run: bool) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "the interactive split needs a terminal; pass --per-commit for a non-interactive split"
        );
    }
    let branch = git::current_branch()?;
    let base = base_of(&branch)?;

    let mut commits = git::rev_list(&format!("{base}..{branch}"))?;
    commits.reverse();
    if commits.len() < 2 {
        bail!(
            "{branch} has {} commit(s) above {base}; need at least 2 to split",
            commits.len()
        );
    }
    let subjects: Vec<String> = commits
        .iter()
        .map(|sha| git::commit_subject(sha))
        .collect::<Result<_>>()?;

    // Phase 1: which commits start a new branch. The top commit is always the
    // leaf, so only the commits below it are offered; default is one each.
    let below = commits.len() - 1;
    let labels: Vec<String> = (0..below)
        .map(|i| format!("{}  {}", &commits[i][..8], subjects[i]))
        .collect();
    let theme = ColorfulTheme::default();
    let checked = MultiSelect::with_theme(&theme)
        .with_prompt(format!(
            "Each checked commit starts a new branch (unchecked folds into the one below); {branch} stays the leaf"
        ))
        .items(&labels)
        .defaults(&vec![true; below])
        .interact()?;
    let starts = group_starts(&checked, below);

    // Phase 2: name each new branch (bottom-up), defaulting to a slug of the
    // group's bottom commit. The leaf reuses the original branch name.
    let mut taken: BTreeSet<String> = git::local_branches()?.into_iter().collect();
    let mut plan: Vec<Plan> = Vec::new();
    for (group, &start) in starts.iter().enumerate() {
        let end = starts.get(group + 1).copied().unwrap_or(below);
        let default = unique_name(&slugify(&subjects[start]), &mut taken.clone());
        let name: String = Input::with_theme(&theme)
            .with_prompt(format!(
                "Name for new branch {}/{}",
                group + 1,
                starts.len()
            ))
            .default(default)
            .interact_text()?;
        if !stack::is_safe_ref_name(&name) {
            bail!("{name:?} is not a valid branch name");
        }
        if name == branch {
            bail!("a new branch cannot reuse the leaf's name {branch}");
        }
        if !taken.insert(name.clone()) {
            bail!("branch name {name:?} is already taken");
        }
        plan.push(Plan {
            name,
            sha: commits[end - 1].clone(),
        });
    }
    plan.push(Plan {
        name: branch.clone(),
        sha: commits[commits.len() - 1].clone(),
    });

    apply(&branch, &base, &plan, dry_run)
}

/// The commit indices that begin a branch: always the bottom (0) plus each one
/// the user checked, sorted and deduped. Consecutive commits between two starts
/// form one branch.
fn group_starts(checked: &[usize], below: usize) -> Vec<usize> {
    let mut starts: Vec<usize> = std::iter::once(0)
        .chain(checked.iter().copied().filter(|&index| index < below))
        .collect();
    starts.sort_unstable();
    starts.dedup();
    starts
}

/// The branch's base: its recorded stack parent, or the trunk.
///
/// Refuses a recorded stack base outright. `split` rewrites nothing - it
/// creates branches at existing commits - but it does stamp a `stkParent` on
/// the branch it splits, and a base has no parent by design: the marker would
/// outrank that metadata everywhere else, leaving the two disagreeing.
fn base_of(branch: &str) -> Result<String> {
    if stack::is_floor(branch)? {
        bail!(
            "{branch} is a stack's base, so splitting it would give it a stack \
             parent it should not have; `git stk detach {branch}` first if it \
             is not a base"
        );
    }
    if let Some(parent) = stack::stacked_parent_of(branch)? {
        return Ok(parent);
    }
    stack::trunk_branch(&git::local_branches()?)
        .filter(|trunk| trunk != branch)
        .ok_or_else(|| {
            anyhow!("could not determine a base for {branch}; adopt it onto a parent first")
        })
}

/// Create a branch per plan entry (bottom-up), reusing the original branch as
/// the leaf. Each branch's parent is the one below it; the bottom's is `base`.
fn apply(branch: &str, base: &str, plan: &[Plan], dry_run: bool) -> Result<()> {
    if !dry_run {
        stack::snapshot("split");
    }
    for (index, entry) in plan.iter().enumerate() {
        let parent = if index == 0 {
            base
        } else {
            &plan[index - 1].name
        };
        let leaf = index == plan.len() - 1;
        if leaf {
            anstream::println!(
                "{} {} {} onto {}",
                verb(dry_run),
                style::branch(&entry.name),
                style::dim("(leaf)"),
                style::branch(parent)
            );
        } else {
            anstream::println!(
                "{} {} at {} onto {}",
                verb(dry_run),
                style::branch(&entry.name),
                style::dim(&entry.sha[..8]),
                style::branch(parent)
            );
        }
        if dry_run {
            continue;
        }
        if !leaf {
            git::create_branch_at(&entry.name, &entry.sha)?;
        }
        stack::set_parent(&entry.name, parent)?;
        stack::record_base(&entry.name, parent);
    }
    if !dry_run {
        anstream::println!(
            "{}",
            style::success(&format!("split {branch} into {} branches", plan.len()))
        );
    }
    Ok(())
}

fn verb(dry_run: bool) -> &'static str {
    if dry_run { "would create" } else { "created" }
}

/// A branch-name slug from a commit subject: lowercase, non-alphanumeric runs
/// collapsed to a single dash, trimmed, capped in length. Empty input (e.g. an
/// all-punctuation subject) falls back to "branch".
fn slugify(subject: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in subject.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash {
                slug.push('-');
                pending_dash = false;
            }
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() {
            pending_dash = true;
        }
        if slug.len() >= 50 {
            break;
        }
    }
    if slug.is_empty() {
        "branch".to_owned()
    } else {
        slug
    }
}

/// Make `base` unique against names already taken, appending -2, -3, ... and
/// recording the result so later calls avoid it too.
fn unique_name(base: &str, used: &mut std::collections::BTreeSet<String>) -> String {
    if used.insert(base.to_owned()) {
        return base.to_owned();
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}-{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_lowercases_and_dashes() {
        assert_eq!(slugify("Fix the thing"), "fix-the-thing");
        assert_eq!(slugify("Add API endpoint (v2)"), "add-api-endpoint-v2");
        assert_eq!(slugify("  leading/trailing  "), "leading-trailing");
    }

    #[test]
    fn slugify_falls_back_when_empty() {
        assert_eq!(slugify("!!!"), "branch");
        assert_eq!(slugify(""), "branch");
    }

    #[test]
    fn group_starts_always_includes_the_bottom_and_dedups() {
        assert_eq!(group_starts(&[1, 2], 3), vec![0, 1, 2]); // all checked -> one per commit
        assert_eq!(group_starts(&[1], 3), vec![0, 1]); // the top commit folds into group 2
        assert_eq!(group_starts(&[], 3), vec![0]); // everything folds into one branch
        assert_eq!(group_starts(&[0, 2], 3), vec![0, 2]); // checking the bottom is moot
    }

    #[test]
    fn unique_name_appends_a_counter_on_collision() {
        let mut used = std::collections::BTreeSet::new();
        assert_eq!(unique_name("fix", &mut used), "fix");
        assert_eq!(unique_name("fix", &mut used), "fix-2");
        assert_eq!(unique_name("fix", &mut used), "fix-3");
    }
}
