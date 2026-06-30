use std::collections::BTreeMap;

use anyhow::{Result, bail};
use clap::ArgAction;

use crate::cli::{FetchMode, PushMode, UpdateRefsMode};
use crate::commands::Run;
use crate::{git, settings, stack, style};

/// Amend staged fixes into the commits that introduced the lines they touch.
///
/// Each hunk is routed to the commit a `git blame` attributes its lines to;
/// hunks that cannot be attributed are left in place.
#[derive(Debug, clap::Args)]
pub struct Absorb {
    /// Show the hunk -> commit routing without changing anything.
    #[arg(long, short = 'n', action = ArgAction::SetTrue)]
    dry_run: bool,
    /// Also absorb unstaged tracked changes, not just staged ones, overriding
    /// stk.absorbIncludeUnstaged.
    #[arg(long, action = ArgAction::SetTrue)]
    include_unstaged: bool,
}

impl Run for Absorb {
    fn run(self) -> Result<()> {
        let include_unstaged =
            self.include_unstaged || settings::bool_setting(settings::ABSORB_INCLUDE_UNSTAGED_KEY)?;
        let cached = !include_unstaged;

        let diff = git::diff_against_head(cached)?;
        if diff.trim().is_empty() {
            bail!(
                "no {} changes to absorb",
                if cached { "staged" } else { "tracked" }
            );
        }

        let current = git::current_branch()?;
        let owners = commit_owners(&current)?;
        let routes: Vec<Route> = parse_diff(&diff)
            .into_iter()
            .flat_map(|file| file.into_routes(&owners))
            .collect::<Result<_>>()?;

        if self.dry_run {
            print_plan(&routes);
            return Ok(());
        }

        apply(&current, routes)
    }
}

/// Fold the attributed hunks into their target commits and settle the stack.
/// Atomic: if the rewrite hits a conflict it is aborted and rolled back so
/// the working tree is left exactly as it was.
fn apply(current: &str, routes: Vec<Route>) -> Result<()> {
    let path = stack::path_from_root(current)?;

    // Branches that fork off the path keep their old parents until they are
    // restacked after the fold. Note them now to decide whether that second
    // pass is needed.
    let mut forked = false;
    for branch in &path {
        for child in stack::children_of(branch)? {
            if !path.contains(&child) {
                forked = true;
            }
        }
    }

    let targets = group_targets(&routes);
    if targets.is_empty() {
        bail!("no changes could be attributed to a stack commit (try `--dry-run`)");
    }
    if !git::supports_rebase_update_refs()? {
        bail!("absorb needs a Git that supports `rebase --update-refs` (2.38+)");
    }

    let base = absorb_base(&path)?;
    stack::snapshot("absorb");
    let orig_head = git::rev_parse("HEAD")?;

    // Phase 1: commit each target's hunks as a fixup! of its commit, then an
    // autosquash rebase folds them in. `--update-refs` carries the path's
    // branch refs. Atomic: a conflict here rolls back, nothing changed.
    git::reset_index()?;
    for (sha, hunks) in &targets {
        let staged = git::apply_cached(&build_patch(hunks)).and_then(|()| git::commit_fixup(sha));
        if let Err(error) = staged {
            let _ = git::reset_soft(&orig_head);
            return Err(error.context("could not stage the fixes to absorb"));
        }
    }

    // Unattributed hunks stay in the worktree; stash them so the rebase runs
    // on a clean tree, and restore them afterward.
    let stashed = !git::worktree_is_clean()?;
    if stashed {
        git::stash_push()?;
    }

    if git::rebase_autosquash(&base, true).is_err() {
        let _ = git::rebase_abort();
        let _ = git::reset_soft(&orig_head);
        if stashed {
            let _ = git::stash_pop();
        }
        bail!(
            "absorb hit a conflict folding the fixes in - rolled back, nothing changed; \
             amend those commits manually (`git stk down`, edit, `git stk restack`)"
        );
    }

    if stashed {
        git::stash_pop()?;
    }
    for (index, branch) in path.iter().enumerate() {
        let parent = if index == 0 {
            stack::parent_of(branch)?
        } else {
            Some(path[index - 1].clone())
        };
        if let Some(parent) = parent {
            stack::record_base(branch, &parent);
        }
    }

    report_absorbed(&targets, &routes);

    // Phase 2: the fold rewrote the path's commits, so any branch forking off
    // it still points at the old ones. Restack settles those onto the
    // rewritten parents (and prints the push hint for the whole stack). A
    // conflict here is resumable - `git stk continue`/`abort`, and `git stk
    // undo` reverts the whole absorb.
    if forked {
        stack::restack(
            FetchMode::Disabled,
            UpdateRefsMode::Enabled,
            PushMode::Disabled,
            false,
        )
    } else {
        report_push_hint(&path)
    }
}

/// The commit each target absorbs into, with the hunks bound for it, oldest
/// commit first.
fn group_targets(routes: &[Route]) -> Vec<(String, Vec<&Route>)> {
    let mut order = Vec::new();
    let mut by_sha: BTreeMap<String, Vec<&Route>> = BTreeMap::new();
    for route in routes {
        if let Route::Absorb { sha, .. } = route {
            if !by_sha.contains_key(sha) {
                order.push(sha.clone());
            }
            by_sha.entry(sha.clone()).or_default().push(route);
        }
    }
    order
        .into_iter()
        .map(|sha| {
            let hunks = by_sha.remove(&sha).unwrap_or_default();
            (sha, hunks)
        })
        .collect()
}

/// Reassemble a patch for one target: each file's header once, then its
/// hunks, in file order.
fn build_patch(hunks: &[&Route]) -> String {
    struct FilePatch<'a> {
        file: &'a str,
        header: &'a [String],
        bodies: Vec<&'a [String]>,
    }

    let mut by_file: Vec<FilePatch> = Vec::new();
    for route in hunks {
        if let Route::Absorb {
            file, header, body, ..
        } = route
        {
            match by_file.iter_mut().find(|patch| patch.file == file) {
                Some(patch) => patch.bodies.push(body),
                None => by_file.push(FilePatch {
                    file,
                    header,
                    bodies: vec![body],
                }),
            }
        }
    }

    let mut patch = String::new();
    for file in by_file {
        for line in file.header {
            patch.push_str(line);
            patch.push('\n');
        }
        for body in file.bodies {
            for line in body {
                patch.push_str(line);
                patch.push('\n');
            }
        }
    }
    patch
}

/// The commit `base..HEAD` rebases onto: the bottom branch's parent, or its
/// recorded fork point when the bottom is rootless.
fn absorb_base(path: &[String]) -> Result<String> {
    let Some(bottom) = path.first() else {
        bail!("current branch is not in a stack");
    };
    if let Some(parent) = stack::parent_of(bottom)? {
        return Ok(parent);
    }
    if let Some(base) = stack::base_of(bottom)? {
        return Ok(base);
    }
    bail!("could not determine the stack base for {bottom}")
}

/// Map every stack commit (current branch and below) to the branch that owns
/// it, so a blamed sha resolves to a branch. Commits outside this map - the
/// trunk's, or older - are not absorbable.
fn commit_owners(current: &str) -> Result<BTreeMap<String, String>> {
    let path = stack::path_from_root(current)?; // bottom -> current, parent-first
    let mut owners = BTreeMap::new();

    for (index, branch) in path.iter().enumerate() {
        let parent = if index == 0 {
            stack::parent_of(branch)?
        } else {
            Some(path[index - 1].clone())
        };
        let range = match parent {
            Some(parent) => format!("{parent}..{branch}"),
            None => match stack::base_of(branch)? {
                Some(base) => format!("{base}..{branch}"),
                None => continue,
            },
        };
        for sha in git::rev_list(&range)? {
            owners.entry(sha).or_insert_with(|| branch.clone());
        }
    }
    Ok(owners)
}

/// A diff for one file: its header lines (verbatim, for re-applying) and its
/// hunks.
struct FileDiff {
    path: String,
    from_path: String,
    header: Vec<String>,
    hunks: Vec<RawHunk>,
}

struct RawHunk {
    pre_start: usize,
    pre_len: usize,
    body: Vec<String>,
}

impl FileDiff {
    /// Attribute each hunk to a commit (or a reason it cannot be).
    fn into_routes(self, owners: &BTreeMap<String, String>) -> Vec<Result<Route>> {
        let file = self.path;
        let header = self.header;
        self.hunks
            .into_iter()
            .map(|hunk| route_hunk(&file, &header, hunk, owners))
            .collect()
    }
}

enum Route {
    Absorb {
        file: String,
        line: usize,
        header: Vec<String>,
        body: Vec<String>,
        branch: String,
        sha: String,
        subject: String,
    },
    Skip {
        file: String,
        line: usize,
        reason: String,
    },
}

fn route_hunk(
    file: &str,
    header: &[String],
    hunk: RawHunk,
    owners: &BTreeMap<String, String>,
) -> Result<Route> {
    let skip = |reason: &str| {
        Ok(Route::Skip {
            file: file.to_owned(),
            line: hunk.pre_start,
            reason: reason.to_owned(),
        })
    };

    if hunk.pre_len == 0 {
        return skip("added lines - no commit to attribute");
    }

    let shas = git::blame_line_shas(file, hunk.pre_start, hunk.pre_len)?;
    match shas.as_slice() {
        [] => skip("could not attribute"),
        [sha] => match owners.get(sha) {
            Some(branch) => Ok(Route::Absorb {
                file: file.to_owned(),
                line: hunk.pre_start,
                header: header.to_vec(),
                body: hunk.body,
                branch: branch.clone(),
                sha: sha.clone(),
                subject: git::commit_subject(sha)?,
            }),
            None => skip("owned by a commit outside the stack"),
        },
        _ => skip("spans multiple commits"),
    }
}

/// Parse a `git diff --unified=0` into per-file diffs.
fn parse_diff(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            files.push(FileDiff {
                path: String::new(),
                from_path: String::new(),
                header: vec![line.to_owned()],
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };

        if let Some(path) = line.strip_prefix("--- ") {
            file.from_path = strip_diff_prefix(path);
            file.header.push(line.to_owned());
        } else if let Some(path) = line.strip_prefix("+++ ") {
            file.path = match strip_diff_prefix(path).as_str() {
                "/dev/null" => file.from_path.clone(),
                resolved => resolved.to_owned(),
            };
            file.header.push(line.to_owned());
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some((pre_start, pre_len)) = parse_pre_image(rest) {
                file.hunks.push(RawHunk {
                    pre_start,
                    pre_len,
                    body: vec![line.to_owned()],
                });
            }
        } else if let Some(hunk) = file.hunks.last_mut() {
            hunk.body.push(line.to_owned());
        } else {
            file.header.push(line.to_owned());
        }
    }
    files
}

/// `a/foo`, `b/foo`, or `/dev/null` -> the bare path.
fn strip_diff_prefix(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_owned()
}

/// From a hunk header body like "-12,3 +12,2 @@ ...", read the pre-image
/// `(start, len)`. A missing length means one line.
fn parse_pre_image(rest: &str) -> Option<(usize, usize)> {
    let token = rest.split_whitespace().next()?.strip_prefix('-')?;
    let (start, len) = match token.split_once(',') {
        Some((start, len)) => (start.parse().ok()?, len.parse().ok()?),
        None => (token.parse().ok()?, 1),
    };
    Some((start, len))
}

fn print_plan(routes: &[Route]) {
    let absorbed = routes
        .iter()
        .filter(|route| matches!(route, Route::Absorb { .. }))
        .count();
    anstream::println!(
        "absorb plan ({absorbed} of {} hunk{})",
        routes.len(),
        if routes.len() == 1 { "" } else { "s" }
    );
    print_absorb_lines(routes);
    print_skips(routes);
}

fn report_absorbed(targets: &[(String, Vec<&Route>)], routes: &[Route]) {
    let hunks: usize = targets.iter().map(|(_, hunks)| hunks.len()).sum();
    anstream::println!(
        "{}",
        style::success(&format!(
            "absorbed {hunks} hunk{} into {} commit{}",
            if hunks == 1 { "" } else { "s" },
            targets.len(),
            if targets.len() == 1 { "" } else { "s" }
        ))
    );
    print_absorb_lines(routes);
    print_skips(routes);
}

/// The push hint for a single line of rewritten branches. (When the stack
/// forks, the phase-2 restack prints its own hint covering every branch.)
fn report_push_hint(branches: &[String]) -> Result<()> {
    let remote = settings::remote()?;
    anstream::println!("remote branches may be stale; push them with:");
    anstream::println!(
        "{}",
        style::dim(&format!(
            "  git push --force-with-lease {remote} {}",
            branches.join(" ")
        ))
    );
    Ok(())
}

fn print_absorb_lines(routes: &[Route]) {
    for route in routes {
        if let Route::Absorb {
            file,
            line,
            branch,
            sha,
            subject,
            ..
        } = route
        {
            anstream::println!(
                "  {file}:{line} -> {} {}",
                style::branch(branch),
                style::dim(&format!("{} {subject}", &sha[..7.min(sha.len())]))
            );
        }
    }
}

fn print_skips(routes: &[Route]) {
    let skipped: Vec<&Route> = routes
        .iter()
        .filter(|route| matches!(route, Route::Skip { .. }))
        .collect();
    if skipped.is_empty() {
        return;
    }
    anstream::println!("{}", style::dim("unabsorbed (left in place):"));
    for route in skipped {
        if let Route::Skip { file, line, reason } = route {
            anstream::println!("  {file}:{line} {}", style::dim(reason));
        }
    }
}
