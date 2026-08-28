use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::cli::PushMode;
use crate::commands::Run;
use crate::completions;
use crate::providers::{NativeStack, ReviewProvider, ReviewState, detect_review_provider};
use crate::settings;
use crate::style;
use crate::{git, stack};

/// Create or update a remote review request for a branch.
#[derive(Debug, clap::Args)]
pub struct Submit {
    /// Branch to submit (defaults to the current branch).
    #[arg(add = ArgValueCompleter::new(completions::branch_candidates))]
    branch: Option<String>,
    /// Print what would change without creating or updating reviews.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
    /// Submit the whole stack parent-first, from anywhere in it.
    #[arg(long, conflicts_with = "branch")]
    stack: bool,
    /// Submit only the current branch, overriding stk.submitStack.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "stack")]
    no_stack: bool,
    /// Submit the stack from its bottom through the current branch only,
    /// leaving work-in-progress branches above it unsubmitted.
    #[arg(
        long,
        action = ArgAction::SetTrue,
        conflicts_with_all = ["branch", "stack", "no_stack"],
    )]
    downstack: bool,
    /// Push branches (-u --force-with-lease) before submitting.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_push")]
    push: bool,
    /// Do not push branches, overriding stk.pushOnSubmit.
    #[arg(long, action = ArgAction::SetTrue)]
    no_push: bool,
    /// Set the review's title, replacing the branch tip's commit subject.
    /// Applies to the current or named branch only.
    #[arg(long, short = 't', value_name = "TEXT")]
    title: Option<String>,
    /// Set a description block at the top of the review body; an empty
    /// string clears it. Applies to the current or named branch only.
    #[arg(long, short = 'd')]
    desc: Option<String>,
    /// Read the description block from a markdown or text file instead of an
    /// inline string, handy for agent-authored bodies. Incompatible with
    /// --desc; an empty file clears the block, like `--desc ""`.
    #[arg(
        long = "desc-file",
        value_name = "PATH",
        value_hint = clap::ValueHint::FilePath,
        conflicts_with = "desc",
    )]
    desc_file: Option<PathBuf>,
    /// Request reviews from these users or teams on every submitted review
    /// (comma-separated, or repeat the flag). A leading `@` is optional and
    /// stripped, so `@foo,@bar` and `foo,bar` mean the same. GitHub/Gitea team
    /// reviewers use the `org/team` form (`@my-org/backend`).
    #[arg(long, value_name = "CSV", value_delimiter = ',')]
    reviewers: Vec<String>,
    /// Create new reviews as drafts.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_draft")]
    draft: bool,
    /// Create new reviews ready for review, overriding stk.submitDraft.
    #[arg(long, action = ArgAction::SetTrue)]
    no_draft: bool,
    /// Mark the submitted branches' existing draft reviews as ready.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "draft")]
    ready: bool,
    /// Rebuild each review's stack overview from the live stack plus merged
    /// history, dropping closed or orphaned rows that drifted in. Stack mode.
    #[arg(long, action = ArgAction::SetTrue)]
    rebuild_overview: bool,
}

impl Run for Submit {
    fn run(self) -> Result<()> {
        // Stack mode: --stack forces it on; --no-stack or an explicit branch
        // forces it off; otherwise stk.submitStack decides.
        let submit_stack = if self.stack {
            true
        } else if self.no_stack || self.branch.is_some() {
            false
        } else {
            settings::bool_setting(settings::SUBMIT_STACK_KEY)?
        };

        // Draft mode: --draft forces it on, --no-draft off; otherwise
        // stk.submitDraft decides.
        let draft = if self.draft {
            true
        } else if self.no_draft {
            false
        } else {
            settings::bool_setting(settings::SUBMIT_DRAFT_KEY)?
        };

        // A file source resolves to the same description string; clap's
        // conflicts_with guarantees at most one of the two is set.
        let desc = match self.desc_file {
            Some(path) => {
                let path = expand_tilde(path);
                let raw = std::fs::read_to_string(&path).with_context(|| {
                    format!("failed to read description file {}", path.display())
                })?;
                Some(raw.trim().to_owned())
            }
            None => self.desc,
        };

        // Unlike a description, a title has no "clear it" meaning - every
        // review must have one - so an empty string is a mistake, not a verb.
        let title = match self.title {
            Some(title) if title.trim().is_empty() => bail!("--title cannot be empty"),
            Some(title) => Some(title.trim().to_owned()),
            None => None,
        };

        submit(SubmitOptions {
            branch: self.branch,
            submit_stack,
            downstack: self.downstack,
            dry_run: self.dry_run,
            push_mode: PushMode::from_flags(self.push, self.no_push),
            title,
            desc,
            reviewers: normalize_reviewers(&self.reviewers),
            draft,
            ready: self.ready,
            rebuild_overview: self.rebuild_overview,
        })
    }
}

/// Clean a raw `--reviewers` list: trim each entry, drop one optional leading
/// `@` (so `@foo` and `foo` are the same), discard blanks, and de-duplicate
/// while preserving order. A team keeps its `org/team` form - only the `@`
/// prefix is stripped, never the slash. GitHub's Copilot reviewer is the one
/// login that *needs* its `@` (`@copilot`), so it is preserved (and a bare
/// `copilot` is canonicalized to it) rather than stripped like a username.
fn normalize_reviewers(raw: &[String]) -> Vec<String> {
    let mut reviewers: Vec<String> = Vec::new();
    for entry in raw {
        let trimmed = entry.trim();
        let stripped = trimmed.strip_prefix('@').unwrap_or(trimmed).trim();
        let name = if stripped.eq_ignore_ascii_case("copilot") {
            "@copilot"
        } else {
            stripped
        };
        if name.is_empty() || reviewers.iter().any(|seen| seen == name) {
            continue;
        }
        reviewers.push(name.to_owned());
    }
    reviewers
}

/// The resolved inputs for [`submit`] - one bundle instead of nine positional
/// arguments. `Submit::run` resolves the flag/config defaults and fills it.
pub struct SubmitOptions {
    pub branch: Option<String>,
    pub submit_stack: bool,
    pub downstack: bool,
    pub dry_run: bool,
    pub push_mode: crate::cli::PushMode,
    pub title: Option<String>,
    pub desc: Option<String>,
    pub reviewers: Vec<String>,
    pub draft: bool,
    pub ready: bool,
    pub rebuild_overview: bool,
}

/// Expand a leading `~` in a `--desc-file` path to the user's home, since the
/// path reaches us literally when the shell did not expand it (quoted, or
/// handed over by a script or agent). Cross-platform: `HOME` on Unix (and
/// Git Bash/WSL), falling back to `USERPROFILE` on native Windows.
fn expand_tilde(path: PathBuf) -> PathBuf {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    expand_tilde_with(path, home)
}

fn expand_tilde_with(path: PathBuf, home: Option<PathBuf>) -> PathBuf {
    let Some(rest) = path.to_str().and_then(|text| text.strip_prefix('~')) else {
        return path;
    };
    // Only bare `~` and `~/...` (or `~\...` on Windows) expand; a `~user` form
    // would need a passwd lookup we do not do, so leave it untouched.
    let mut chars = rest.chars();
    let tail = match chars.next() {
        None => "",
        Some(separator) if std::path::is_separator(separator) => chars.as_str(),
        Some(_) => return path,
    };
    let Some(home) = home else {
        return path;
    };
    if tail.is_empty() {
        home
    } else {
        home.join(tail)
    }
}

pub fn submit(options: SubmitOptions) -> Result<()> {
    let SubmitOptions {
        branch,
        submit_stack,
        downstack,
        dry_run,
        push_mode,
        title,
        desc,
        reviewers,
        draft,
        ready,
        rebuild_overview,
    } = options;

    let branch = branch.map_or_else(git::current_branch, Ok)?;
    // The title and description target this branch's review even in stack mode.
    let target_branch = branch.clone();

    let mut branches = if downstack {
        // Bottom of the stack through the current branch: anything above is
        // work in progress that stays local.
        stack::path_from_root(&branch)?
    } else if submit_stack {
        // The stack containing the current branch: its own line, bottom
        // through current and out to its descendants. Sibling stacks that
        // merely share the trunk are left for their own submit.
        stack::stack_line(&branch)?
    } else {
        vec![branch.clone()]
    };

    // The trunk is never part of a stack, so a stack-wide submit from it has
    // nothing of its own to submit (its descendants are sibling stacks). Say so
    // plainly rather than pushing an empty set or sweeping every stack.
    if submit_stack || downstack {
        let trunk = stack::trunk_branch(&git::local_branches()?);
        if Some(&branch) == trunk.as_ref() {
            if !stack::has_stacked_branches()? {
                bail!("no stacked branches to submit");
            }
            bail!("you are on the trunk ({branch}); check out a stacked branch first");
        }
    }

    // A line rooted off the trunk keeps its parentless root - the branch the
    // one above it targets, not something to submit. Drop it so stack mode
    // ships exactly what `--no-stack` does, rather than refusing a shape
    // `new` and `adopt` both accept. Restack already treats such a root as
    // the floor; this keeps submit, and the push below, in step with it.
    let mut base = None;
    if submit_stack || downstack {
        base = stack::unanchored_base(&branches)?;
        // `--downstack` standing on an unmarked base: `path_from_root` stops
        // at the branch you are on, so the slice is just the base and there is
        // nothing for `unanchored_base` to read the shape from. Its children
        // still say what it is.
        let unmarked_base = base.is_none()
            && branches.len() == 1
            && stack::parent_of(&branches[0])?.is_none()
            && !stack::children_of(&branches[0])?.is_empty();

        if let Some(found) = &base {
            branches.retain(|branch| branch != found);
        }

        // Nothing left once the base is out: everything stacked here is above
        // you. Name the base rather than report a no-op, or let
        // `branch_parents` call it unstacked and offer to re-root it.
        if unmarked_base || (base.is_some() && branches.is_empty()) {
            let name = base.as_deref().unwrap_or_else(|| branches[0].as_str());
            return Err(base_has_nothing_to_submit(name)?);
        }

        if let Some(found) = &base {
            anstream::println!(
                "{}",
                style::dim(&format!("{found} is this stack's base; not submitted"))
            );
        }
    }

    // `--title`/`--desc` act on the current branch. When that is the base the
    // trim just dropped, it is not part of the stack and its review - a
    // release PR, say - is not ours to retitle or write a description into.
    let target_in_scope = branches.contains(&target_branch);

    let branch_parents = branch_parents(&branches)?;

    // Push after stack validation but before any provider calls: creating a
    // review requires the branch to exist remotely, and -u --force-with-lease
    // covers both first pushes and safely updating rebased branches.
    let push = settings::push_enabled(push_mode, settings::PUSH_ON_SUBMIT_KEY)?;

    // The base is not pushed with the stack - it is not ours to move - but it
    // is the base of the review opened for the branch above it. Every other
    // base is a branch we just pushed, so this is the one that can be missing
    // from the remote; catch it here rather than let the forge reject the
    // create with its own wording, after the push has already happened.
    if let Some(base) = &base
        && push
    {
        let remote = settings::remote()?;
        if !git::remote_has_branch(&remote, base)? {
            // Name the branch to re-root. `adopt` defaults to the branch you
            // are on, and standing on the base is a supported position here -
            // so a bare `adopt --parent` would re-root the base itself, the
            // very thing this stack stopped suggesting.
            let lowest = &branches[0];
            bail!(
                "{base} is this stack's base, but {remote} has no such branch; \
                 push {base} to {remote} first, or re-root the stack with \
                 `git stk adopt {lowest} --parent <parent>`"
            );
        }
    }
    if push {
        let remote = settings::remote()?;
        if dry_run {
            anstream::println!(
                "would push {} to {remote}",
                style::branch(&branches.join(" "))
            );
        } else {
            git::push_set_upstream_force_with_lease(&remote, &branches)?;
            anstream::println!("pushed {} to {remote}", style::branch(&branches.join(" ")));
            // Carry the stack's parent map along so another clone can rebuild
            // it with `git stk repair --from-remote`.
            stack::publish_metadata(&remote);
        }
    }

    let (provider, review_provider) = detect_review_provider()?;
    let mut summary = SubmitSummary::default();

    let mut created = Vec::new();
    for (branch, parent) in &branch_parents {
        // A new review opens under the given title directly, so it is never
        // briefly published under the commit subject.
        let branch_title = title.as_deref().filter(|_| *branch == target_branch);
        let action = submit_branch(
            review_provider.as_ref(),
            branch,
            parent,
            dry_run,
            draft,
            branch_title,
        )?;
        if action == SubmitAction::Created {
            created.push(branch.clone());
        }
        summary.record(action);
    }

    // Seed freshly created reviews from the repo's PR template before the
    // managed sections go in, so our content joins it rather than replacing it.
    // Without a user description the template is wrapped in the managed
    // description block; the branch that gets `--desc` keeps the template
    // freeform above a seam so the description reads as a distinct block below.
    let desc_target = desc.as_ref().map(|_| target_branch.as_str());
    crate::notes::seed_template_notes(
        review_provider.as_ref(),
        provider.kind,
        &created,
        desc_target,
        dry_run,
    )?;

    // Flip drafts in scope to ready for review (the escape hatch for
    // stk.submitDraft users).
    if ready {
        for branch in &branches {
            let Some(review) = review_provider.review_for_branch(branch)? else {
                continue;
            };
            if review.branch != *branch || !review.draft {
                continue;
            }
            if dry_run {
                anstream::println!("would mark {} ready", review.id);
                continue;
            }
            let output = review_provider.mark_ready(&review)?;
            anstream::println!("marked {} ready", review.id);
            if !output.is_empty() {
                println!("{output}");
            }
        }
    }

    // A renamed branch's fresh review now exists, so retire the review the old
    // name still heads. Only handle this when the ledger prune below actually
    // runs (stack-wide submit): the marker is the sole signal that identifies
    // the stale row across every other overview, so closing and clearing it in
    // a single-branch submit - which never prunes - would orphan those rows
    // permanently. Left set, the marker waits for a later `submit --stack`.
    let renamed: Vec<(String, String)> = if submit_stack || downstack {
        branch_parents
            .iter()
            .filter_map(|(branch, _)| {
                stack::renamed_from(branch)
                    .ok()
                    .flatten()
                    .map(|old| (branch.clone(), old))
            })
            .collect()
    } else {
        Vec::new()
    };
    // Track which markers are safe to drop: those whose old review was
    // actually retired (or had nothing to retire). A declined close keeps its
    // marker so a later submit re-offers the reconciliation.
    let mut reconciled: Vec<&str> = Vec::new();
    for (branch, old) in &renamed {
        if close_superseded_review(review_provider.as_ref(), old, dry_run)? {
            reconciled.push(branch);
        }
    }

    // After every review exists, set the title and description, link any issue
    // the branch name references, then (in stack mode) write the stack overview
    // into each body.
    if let Some(title) = &title {
        if !target_in_scope {
            anstream::println!("skipped title: {target_branch} is this stack's base");
        } else if !created.contains(&target_branch) {
            // Reviews created just now already carry the title; only one that
            // existed before this submit needs the edit.
            apply_title(review_provider.as_ref(), &target_branch, title, dry_run)?;
        }
    }
    if let Some(desc) = desc {
        if target_in_scope {
            crate::notes::update_description_note(
                review_provider.as_ref(),
                &target_branch,
                &desc,
                dry_run,
            )?;
        } else {
            anstream::println!("skipped description: {target_branch} is this stack's base");
        }
    }
    crate::notes::update_closes_notes(review_provider.as_ref(), &branches, dry_run)?;
    if submit_stack || downstack {
        crate::notes::update_stack_notes(
            review_provider.as_ref(),
            &branch_parents,
            dry_run,
            rebuild_overview,
        )?;
    }
    if submit_stack || downstack {
        register_native_stack(review_provider.as_ref(), &branches, dry_run)?;
    }
    apply_reviewers(review_provider.as_ref(), &branches, &reviewers, dry_run)?;

    // The ledger has now pruned the superseded entries, so drop the markers -
    // but only for reviews that were retired, not ones the user kept.
    if !dry_run {
        for branch in &reconciled {
            stack::clear_renamed_from(branch)?;
        }
    }

    anstream::println!(
        "{}",
        style::success(&format!(
            "submit complete: {} created, {} updated, {} skipped",
            summary.created, summary.updated, summary.skipped
        ))
    );
    Ok(())
}

/// Retire the open review still heading a renamed-away branch. The fresh
/// review already exists, so closing here never leaves the work without one.
/// Prompts (default yes; a non-interactive run proceeds) before closing.
///
/// Returns whether the supersession was reconciled: `true` when the old review
/// was closed or there was nothing to close, `false` when the user declined -
/// so the caller keeps the rename marker for a later submit to re-offer.
fn close_superseded_review(
    review_provider: &dyn ReviewProvider,
    old: &str,
    dry_run: bool,
) -> Result<bool> {
    let Some(review) = review_provider.review_for_branch(old)? else {
        return Ok(true);
    };
    if review.branch != *old {
        return Ok(true);
    }

    if dry_run {
        anstream::println!("would close superseded review {} for {old}", review.id);
        return Ok(true);
    }
    if !crate::prompt::confirm_default_yes(&format!(
        "close the replaced review {} for {old} and delete its branch? [Y/n] ",
        review.id
    ))? {
        anstream::println!("kept review {} for {old}", review.id);
        return Ok(false);
    }

    review_provider.close_review(&review, true)?;
    anstream::println!("closed superseded review {} for {old}", review.id);
    Ok(true)
}

/// Retitle the branch's existing review. Mirrors the description step: a
/// missing review, or one that heads a different branch, is passed over with a
/// note rather than failing the submit.
fn apply_title(
    review_provider: &dyn ReviewProvider,
    branch: &str,
    title: &str,
    dry_run: bool,
) -> Result<()> {
    let Some(review) = review_provider.review_for_branch(branch)? else {
        if dry_run {
            anstream::println!("would set the title on the review for {branch}");
        } else {
            anstream::println!("skipped title: no review found for {branch}");
        }
        return Ok(());
    };
    if review.branch != branch {
        anstream::println!(
            "skipped title: review {} belongs to {}",
            review.id,
            review.branch
        );
        return Ok(());
    }
    if dry_run {
        anstream::println!("would set the title in {}", review.id);
        return Ok(());
    }

    let output = review_provider.update_review_title(&review, title)?;
    anstream::println!("set title in {}", review.id);
    if !output.is_empty() {
        println!("{output}");
    }
    Ok(())
}

/// Request reviews from `reviewers` on every submitted branch's review. A
/// merged review is skipped - there is nothing left to review - and a branch
/// whose review is missing (or heads a different branch) is passed over with a
/// note, mirroring the description and closes steps. No-op with no reviewers.
fn apply_reviewers(
    review_provider: &dyn ReviewProvider,
    branches: &[String],
    reviewers: &[String],
    dry_run: bool,
) -> Result<()> {
    if reviewers.is_empty() {
        return Ok(());
    }
    let list = reviewers.join(", ");
    for branch in branches {
        let Some(review) = review_provider.review_for_branch(branch)? else {
            // On a dry run the review was likely never created; for real the
            // submit just failed to produce one, which deserves a mention.
            if dry_run {
                anstream::println!("would request reviews from {list} for {branch}");
            } else {
                anstream::println!("skipped reviewers: no review found for {branch}");
            }
            continue;
        };
        if review.branch != *branch || review.state == ReviewState::Merged {
            continue;
        }
        if dry_run {
            anstream::println!("would request reviews from {list} in {}", review.id);
            continue;
        }
        let output = review_provider.request_reviewers(&review, reviewers)?;
        anstream::println!("requested reviews from {list} in {}", review.id);
        if !output.is_empty() {
            println!("{output}");
        }
    }
    Ok(())
}

/// The error for a submit that resolves to nothing but the stack's base. Two
/// sentences, on whether anything is stacked on it - shared because both the
/// stack-mode trim and the single-branch path reach this state, and keeping
/// two copies in step has already failed twice.
fn base_has_nothing_to_submit(branch: &str) -> Result<anyhow::Error> {
    if stack::children_of(branch)?.is_empty() {
        return Ok(anyhow::anyhow!(
            "{branch} is this stack's base, and nothing is stacked on it"
        ));
    }
    // Name the branch in the remedy: `--stack` conflicts with naming one, so
    // it has to be run from there rather than pointed at it - otherwise
    // `submit <base>` from a sibling stack sends you to submit that one.
    Ok(anyhow::anyhow!(
        "{branch} is this stack's base; there is nothing below it to submit - \
         run `git stk submit --stack` from {branch} to submit the branches above it"
    ))
}

/// Hand the submitted stack to the platform, when it keeps stacks of its own -
/// GitHub, with `stk.githubStacks` on. `branches` is bottom-first, which is
/// the order a stack lands in, and every review now targets the branch below
/// it, which is the shape the stack is recorded against.
///
/// Best effort by design: the reviews already exist, and what this adds is
/// presentation - the stack map, and parallel review across layers. A failure
/// is reported and the submit still succeeds.
fn register_native_stack(
    review_provider: &dyn ReviewProvider,
    branches: &[String],
    dry_run: bool,
) -> Result<()> {
    if branches.is_empty() {
        return Ok(());
    }
    // Membership is per-review, so the line's bottom is not necessarily the
    // stack's: root the line lower with `adopt` and the bottom is a branch the
    // stack never held. Looking only there would read "no stack" for one that
    // exists and POST a duplicate holding reviews GitHub already has.
    let existing = branches
        .iter()
        .find_map(|branch| review_provider.native_stack_for(branch).ok().flatten());

    let mut reviews = Vec::with_capacity(branches.len());
    for branch in branches {
        let Some(review) = review_provider.review_for_branch(branch)? else {
            // A branch whose review is missing would register a stack with a
            // hole in it. On a dry run there is simply nothing to look at yet.
            if !dry_run {
                anstream::println!("skipped stack registration: no review found for {branch}");
            }
            return Ok(());
        };
        if review.branch != *branch {
            return Ok(());
        }
        reviews.push(review.id);
    }

    if dry_run {
        // Only say so when it would actually do something.
        if let Some(action) = would_register(review_provider, &reviews, existing.as_ref()) {
            anstream::println!("{action}");
        }
        return Ok(());
    }

    match review_provider.register_stack(&reviews, existing.as_ref()) {
        Ok(Some(line)) => anstream::println!("{line}"),
        Ok(None) => {}
        Err(error) => anstream::println!(
            "{}",
            style::warn(&format!("stack registration failed: {error}"))
        ),
    }
    Ok(())
}

/// What `register_stack` would report, for `--dry-run`. Mirrors its decision
/// without making the call.
/// Render what registration would do, from the same plan the real run acts on,
/// so a dry run cannot promise something the run then declines - and says
/// nothing at all on a provider that keeps no stacks, or with the setting off.
fn would_register(
    review_provider: &dyn ReviewProvider,
    reviews: &[String],
    existing: Option<&NativeStack>,
) -> Option<String> {
    if !review_provider.registers_stacks() {
        return None;
    }
    match crate::providers::plan_stack_registration(reviews, existing)? {
        crate::providers::StackPlan::Register(reviews) => {
            Some(format!("would register {} as a stack", reviews.join(" ")))
        }
        crate::providers::StackPlan::Extend { number, fresh } => Some(format!(
            "would extend stack {number} with {}",
            fresh.join(" ")
        )),
        crate::providers::StackPlan::Mismatch { number } => Some(format!(
            "would leave stack {number} as recorded: it no longer matches this stack"
        )),
    }
}

fn branch_parents(branches: &[String]) -> Result<Vec<(String, String)>> {
    let mut branch_parents = Vec::new();
    for branch in branches {
        // The trunk has no parent, so it would classify as a base below - it
        // is not, whatever its children are. Its own message first, as
        // `nothing_to_merge_hint` does. Most necessary in an off-trunk-only
        // repo: without it the trunk falls through and gets an `adopt` remedy
        // aimed at itself.
        if Some(branch) == stack::trunk_branch(&git::local_branches()?).as_ref() {
            // Position first, so every arm below inherits it. This is the one
            // path that can be pointed at a branch you are not on
            // (`--stack`/`--downstack` both conflict with naming one).
            // `is_ok_and` because `submit <branch>` works on a detached HEAD.
            if !git::current_branch().is_ok_and(|current| current == *branch) {
                bail!(
                    "{branch} is the trunk, so it is never part of a stack - \
                     name a stacked branch instead"
                );
            }
            if !stack::has_stacked_branches()? {
                bail!("no stacked branches to submit");
            }
            bail!("you are on the trunk ({branch}); check out a stacked branch first");
        }

        // Every entrance to "you named the base" lands here: a bare `submit`
        // (`stk.submitStack` is off by default) or `--no-stack`, where the
        // trim never ran; and `--downstack` standing on it, where
        // `path_from_root` stops at the branch you are on so the slice is just
        // the base. A recorded base counts whatever parent it picked up;
        // unmarked, its children are the only signal.
        let is_base = stack::is_floor(branch)?
            || (stack::parent_of(branch)?.is_none() && !stack::children_of(branch)?.is_empty());
        if is_base {
            return Err(base_has_nothing_to_submit(branch)?);
        }

        let Some(parent) = stack::parent_of(branch)? else {
            // Name the branch: `adopt` defaults to the one you are on, and
            // `submit <branch>` can be pointed at another - so the bare form
            // would re-root whatever you happen to be standing on.
            bail!(
                "{branch} has no stack parent; attach it with \
                 `git stk adopt {branch} --parent <parent>`, \
                 or rebuild its metadata with `git stk repair`"
            );
        };
        branch_parents.push((branch.to_owned(), parent));
    }
    Ok(branch_parents)
}

fn submit_branch(
    review_provider: &dyn ReviewProvider,
    branch: &str,
    parent: &str,
    dry_run: bool,
    draft: bool,
    title: Option<&str>,
) -> Result<SubmitAction> {
    if let Some(review) = review_provider.review_for_branch(branch)? {
        if review.base == parent {
            if dry_run {
                anstream::println!(
                    "would skip {} -> {} ({})",
                    review.branch,
                    review.base,
                    review.id
                );
            } else {
                anstream::println!(
                    "{}",
                    style::dim(&format!(
                        "{} already targets {} ({})",
                        review.branch, review.base, review.id
                    ))
                );
            }
            return Ok(SubmitAction::Skipped);
        }

        // A review in a registered GitHub stack has its base moved by GitHub
        // as the layer below it lands; retargeting by hand is refused there.
        // Say so instead of claiming a change git-stk did not make.
        if review_provider.platform_manages_base(&review)? {
            anstream::println!(
                "{}",
                style::dim(&format!(
                    "{} targets {} and is in a stack; the platform moves it as the stack lands",
                    review.id, review.base
                ))
            );
            return Ok(SubmitAction::Skipped);
        }

        let output = if dry_run {
            String::new()
        } else {
            review_provider.update_review_base(&review, parent)?
        };
        anstream::println!(
            "{} {} -> {} {}",
            if dry_run { "would update" } else { "updated" },
            style::branch(&review.branch),
            style::branch(parent),
            style::dim(&format!("({})", review.id))
        );
        if !output.is_empty() {
            println!("{output}");
        }
    } else {
        let output = if dry_run {
            String::new()
        } else {
            review_provider.create_review(branch, parent, draft, title)?
        };
        anstream::println!(
            "{} {} -> {}{}",
            if dry_run { "would create" } else { "created" },
            style::branch(branch),
            style::branch(parent),
            title.map_or_else(String::new, |title| format!(" titled \"{title}\""))
        );
        if !output.is_empty() {
            println!("{output}");
        }
        return Ok(SubmitAction::Created);
    }

    Ok(SubmitAction::Updated)
}

#[derive(Debug, Default)]
struct SubmitSummary {
    created: usize,
    updated: usize,
    skipped: usize,
}

impl SubmitSummary {
    fn record(&mut self, action: SubmitAction) {
        match action {
            SubmitAction::Created => self.created += 1,
            SubmitAction::Updated => self.updated += 1,
            SubmitAction::Skipped => self.skipped += 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SubmitAction {
    Created,
    Updated,
    Skipped,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Option<PathBuf> {
        Some(PathBuf::from("/home/dev"))
    }

    #[test]
    fn expand_tilde_resolves_a_bare_tilde_and_subpaths() {
        assert_eq!(
            expand_tilde_with(PathBuf::from("~"), home()),
            PathBuf::from("/home/dev")
        );
        assert_eq!(
            expand_tilde_with(PathBuf::from("~/notes/pr.md"), home()),
            PathBuf::from("/home/dev/notes/pr.md")
        );
    }

    #[test]
    fn expand_tilde_leaves_other_paths_untouched() {
        // Absolute, relative, `~user`, and an embedded (non-leading) tilde all
        // pass through unchanged.
        for raw in ["/etc/pr.md", "notes/pr.md", "~alice/pr.md", "docs/~x.md"] {
            assert_eq!(
                expand_tilde_with(PathBuf::from(raw), home()),
                PathBuf::from(raw)
            );
        }
    }

    #[test]
    fn expand_tilde_passes_through_when_home_is_unset() {
        assert_eq!(
            expand_tilde_with(PathBuf::from("~/pr.md"), None),
            PathBuf::from("~/pr.md")
        );
    }

    fn reviewers(raw: &[&str]) -> Vec<String> {
        normalize_reviewers(
            &raw.iter()
                .map(|entry| (*entry).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn normalize_reviewers_strips_at_and_trims() {
        // A leading `@` is optional, so both spellings normalize alike.
        assert_eq!(reviewers(&["@foo", "@bar"]), vec!["foo", "bar"]);
        assert_eq!(reviewers(&["foo", "bar"]), vec!["foo", "bar"]);
        assert_eq!(reviewers(&[" @foo ", "  bar"]), vec!["foo", "bar"]);
    }

    #[test]
    fn normalize_reviewers_keeps_team_paths_but_drops_the_at() {
        // Only the `@` prefix is stripped; the `org/team` slug stays intact.
        assert_eq!(
            reviewers(&["@my-org/backend", "acme/team"]),
            vec!["my-org/backend", "acme/team"]
        );
    }

    #[test]
    fn normalize_reviewers_drops_blanks_and_dedupes_in_order() {
        assert_eq!(
            reviewers(&["foo", "", "  ", "@foo", "bar", "@bar"]),
            vec!["foo", "bar"]
        );
    }

    #[test]
    fn normalize_reviewers_preserves_the_copilot_at_prefix() {
        // gh needs the literal `@copilot`; a bare `copilot` canonicalizes to it,
        // and both spellings collapse to one entry.
        assert_eq!(reviewers(&["@copilot"]), vec!["@copilot"]);
        assert_eq!(reviewers(&["copilot"]), vec!["@copilot"]);
        assert_eq!(reviewers(&["@Copilot", "copilot"]), vec!["@copilot"]);
    }

    #[cfg(windows)]
    #[test]
    fn expand_tilde_accepts_a_backslash_on_windows() {
        assert_eq!(
            expand_tilde_with(PathBuf::from(r"~\notes\pr.md"), home()),
            PathBuf::from("/home/dev").join(r"notes\pr.md")
        );
    }
}
