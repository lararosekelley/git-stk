use anyhow::Result;
use clap::ArgAction;

use crate::commands::Run;
use crate::providers::{detect_review_provider, owned_review_for_branch};
use crate::style;
use crate::{git, settings, stack};

/// Rebuild or verify local stack metadata from reviews and ancestry.
#[derive(Debug, clap::Args)]
pub struct Repair {
    /// Print what would change without updating local metadata.
    #[arg(long, action = ArgAction::SetTrue)]
    dry_run: bool,
    /// Rebuild the stack from the metadata another machine pushed, fetching
    /// any of its branches that are missing locally.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "dry_run")]
    from_remote: bool,
}

impl Run for Repair {
    fn run(self) -> Result<()> {
        if self.from_remote {
            repair_from_remote()
        } else {
            repair(self.dry_run)
        }
    }
}

/// Rehydrate a stack on this machine from the metadata ref pushed elsewhere.
fn repair_from_remote() -> Result<()> {
    let remote = settings::remote()?;
    let attached = stack::apply_remote_metadata(&remote)?;
    anstream::println!(
        "{}",
        style::success(&format!(
            "rebuilt {attached} branch{} from {remote}",
            if attached == 1 { "" } else { "es" }
        ))
    );
    Ok(())
}

/// Rebuild or verify local stack metadata. For branches missing a parent,
/// try the provider's review base first, then nearest-ancestor inference.
/// For branches with a parent, verify it exists and the recorded fork point
/// is still valid, re-deriving it when stale.
pub fn repair(dry_run: bool) -> Result<()> {
    let branches = git::local_branches()?;
    let trunk = stack::trunk_branch(&branches);

    // Provider lookup is best effort: repair must work without a remote or
    // an authenticated gh/glab.
    let provider = detect_review_provider()
        .ok()
        .map(|(detected, client)| (detected.kind, client));

    let mut repaired = 0;
    let mut verified = 0;
    let mut unresolved = 0;

    for branch in &branches {
        if Some(branch.as_str()) == trunk.as_deref() {
            continue;
        }

        if let Some(parent) = stack::parent_for_branch(branch)? {
            if !branches.contains(&parent) {
                anstream::println!(
                    "{}",
                    style::warn(&format!(
                        "{branch}: parent {parent} does not exist locally; \
                         fix with `git stk adopt` or `git stk detach {branch}`"
                    ))
                );
                unresolved += 1;
                continue;
            }

            let base_valid = matches!(
                stack::base_for_branch(branch)?,
                Some(base) if git::is_ancestor(&base, branch).unwrap_or(false)
            );
            if base_valid {
                verified += 1;
            } else {
                anstream::println!(
                    "{}: {} fork point from {}",
                    style::branch(branch),
                    if dry_run {
                        "would re-record"
                    } else {
                        "re-recorded"
                    },
                    style::branch(&parent)
                );
                if !dry_run {
                    stack::record_base(branch, &parent);
                }
                repaired += 1;
            }
            continue;
        }

        let mut found: Option<(String, String)> = None;
        if let Some((kind, review_provider)) = &provider
            && let Ok(Some(review)) = owned_review_for_branch(&**review_provider, branch)
            && review.base != *branch
        {
            if branches.contains(&review.base) {
                found = Some((review.base.clone(), format!("{kind} review {}", review.id)));
            } else {
                anstream::println!(
                    "{}",
                    style::warn(&format!(
                        "{branch}: review {} targets {}, which is not a local branch",
                        review.id, review.base
                    ))
                );
            }
        }

        if found.is_none() {
            match nearest_ancestor_branch(branch, &branches)? {
                Ancestry::One(parent) => found = Some((parent, "ancestry".to_owned())),
                Ancestry::None => {
                    anstream::println!(
                        "{}",
                        style::warn(&format!(
                            "{branch}: no parent found; attach manually with \
                             `git stk adopt {branch} --parent <parent>`"
                        ))
                    );
                }
                Ancestry::Ambiguous(candidates) => {
                    anstream::println!(
                        "{}",
                        style::warn(&format!(
                            "{branch}: ambiguous parent candidates ({}); attach manually with \
                             `git stk adopt`",
                            candidates.join(", ")
                        ))
                    );
                }
            }
        }

        match found {
            Some((parent, source)) => {
                anstream::println!(
                    "{}: {} parent {} {}",
                    style::branch(branch),
                    if dry_run { "would set" } else { "set" },
                    style::branch(&parent),
                    style::dim(&format!("(from {source})"))
                );
                if !dry_run {
                    stack::set_parent_for_branch(branch, &parent)?;
                    stack::record_base(branch, &parent);
                }
                repaired += 1;
            }
            None => unresolved += 1,
        }
    }

    anstream::println!(
        "{}",
        style::success(&format!(
            "repair complete: {repaired} {}repaired, {verified} verified, {unresolved} unresolved",
            if dry_run { "would be " } else { "" }
        ))
    );
    Ok(())
}

enum Ancestry {
    One(String),
    None,
    Ambiguous(Vec<String>),
}

/// Find the nearest other local branch whose tip is a strict ancestor of
/// `branch` - the best guess at its stack parent.
fn nearest_ancestor_branch(branch: &str, branches: &[String]) -> Result<Ancestry> {
    let tip = git::rev_parse(branch)?;

    let mut candidates: Vec<(String, String)> = Vec::new();
    for other in branches {
        if other == branch {
            continue;
        }
        let other_tip = git::rev_parse(other)?;
        // Equal tips (e.g. a just-created branch) leave the direction
        // ambiguous, so they are not usable candidates.
        if other_tip != tip && git::is_ancestor(other, branch)? {
            candidates.push((other.clone(), other_tip));
        }
    }

    // Keep only the nearest candidates: drop any that are ancestors of
    // another candidate (i.e. further from the branch).
    let nearest: Vec<String> = candidates
        .iter()
        .filter(|(candidate, candidate_tip)| {
            !candidates.iter().any(|(other, other_tip)| {
                other != candidate
                    && other_tip != candidate_tip
                    && git::is_ancestor(candidate, other).unwrap_or(false)
            })
        })
        .map(|(candidate, _)| candidate.clone())
        .collect();

    Ok(match nearest.len() {
        0 => Ancestry::None,
        1 => Ancestry::One(nearest.into_iter().next().expect("one candidate")),
        _ => Ancestry::Ambiguous(nearest),
    })
}
