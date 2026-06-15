use anyhow::{Result, anyhow, bail};
use clap::ArgAction;

use crate::commands::Run;
use crate::git;
use crate::stack;
use crate::style;

/// Split the current branch's commits into a stack of branches, bottom-up. The
/// current branch is reused as the leaf (it keeps its name and tip); new
/// branches are created beneath it for the commits below. Non-destructive: the
/// new branches point at the existing commits, so nothing is rewritten.
#[derive(Debug, clap::Args)]
pub struct Split {
    /// One branch per commit, named from each commit's subject - no editor.
    #[arg(long, action = ArgAction::SetTrue)]
    per_commit: bool,
    /// Print the plan without creating branches or writing metadata.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
}

impl Run for Split {
    fn run(self) -> Result<()> {
        if !self.per_commit {
            // The interactive editor-todo flow (grouping + renaming) is the next
            // increment; the per-commit path is the foundation it builds on.
            bail!("interactive grouping is not implemented yet; pass --per-commit for now");
        }
        split_per_commit(self.dry_run)
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

/// The branch's base: its recorded stack parent, or the trunk.
fn base_of(branch: &str) -> Result<String> {
    if let Some(parent) = stack::parent_of(branch)? {
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
    fn unique_name_appends_a_counter_on_collision() {
        let mut used = std::collections::BTreeSet::new();
        assert_eq!(unique_name("fix", &mut used), "fix");
        assert_eq!(unique_name("fix", &mut used), "fix-2");
        assert_eq!(unique_name("fix", &mut used), "fix-3");
    }
}
