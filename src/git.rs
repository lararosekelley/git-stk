use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Pass raw git output through instead of capturing it.
pub fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
}

fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn current_branch() -> Result<String> {
    output(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .context("failed to determine current branch")
}

/// Whether the working directory is inside a git work tree. Used for a clean
/// "not a git repository" message instead of letting git's raw error surface
/// from the first command that needs the repo.
pub fn is_in_repo() -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.starts_with(b"true"))
}

pub fn local_branches() -> Result<Vec<String>> {
    let output = output(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    Ok(output.lines().map(str::to_owned).collect())
}

pub fn git_path(path: &str) -> Result<String> {
    output(&["rev-parse", "--git-path", path])
}

/// The repository's top-level working-tree directory.
pub fn repo_root() -> Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(output(&[
        "rev-parse",
        "--show-toplevel",
    ])?))
}

/// Resolve `path` under the repo's *common* git dir, which all linked
/// worktrees share, rather than the per-worktree dir `git_path` returns. Use
/// this for state that guards or mirrors the shared config (`branch.*`), so
/// every worktree of a repo agrees on one file.
pub fn git_common_path(path: &str) -> Result<String> {
    let common_dir = output(&["rev-parse", "--git-common-dir"])?;
    Ok(std::path::Path::new(&common_dir)
        .join(path)
        .to_string_lossy()
        .into_owned())
}

/// Branches checked out in linked worktrees *other than this one*, paired with
/// the directory holding each. Git refuses to switch to, rebase, or delete a
/// branch another worktree holds, so callers check this before those.
pub fn worktree_branches() -> Result<Vec<(String, std::path::PathBuf)>> {
    let porcelain = output(&["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_branches(
        &porcelain,
        repo_root().ok().as_deref(),
    ))
}

/// Add a detached worktree at `path`, parked on `commit`. Detached on purpose:
/// it holds no branch, so it cannot collide with the user's checkout or any
/// other worktree.
///
/// `--force` because `path` is a scratch directory git-stk owns outright: a
/// killed run can leave the directory gone but still registered, and git then
/// refuses the path as "a missing but already registered worktree". Forcing is
/// scoped to that one path - `git worktree prune` would also clear entries for
/// the user's own worktrees that happen to be on unmounted volumes.
pub fn worktree_add_detached(path: &std::path::Path, commit: &str) -> Result<()> {
    let path = path.to_string_lossy().into_owned();
    status(&[
        "worktree", "add", "--detach", "--force", "--quiet", &path, commit,
    ])
    .with_context(|| format!("failed to create a worktree at {path}"))
}

/// Add a worktree at `path` holding a new branch created off `start`.
pub fn worktree_add_new_branch(path: &std::path::Path, branch: &str, start: &str) -> Result<()> {
    let path = path.to_string_lossy().into_owned();
    status(&["worktree", "add", "--quiet", "-b", branch, &path, start])
        .with_context(|| format!("failed to create a worktree for {branch} at {path}"))
}

/// Whether a worktree has uncommitted changes. Used before removing one git-stk
/// created, so work in it is never silently discarded.
pub fn worktree_has_changes(path: &std::path::Path) -> bool {
    let dir = path.to_string_lossy().into_owned();
    // Fails safe: if the state cannot be read at all, assume there is work to
    // lose. The caller removes with --force, so guessing "clean" here would
    // discard exactly what this guard exists to protect.
    output(&["-C", &dir, "status", "--porcelain"]).map_or(true, |out| !out.is_empty())
}

/// Remove a worktree, discarding anything in it. Only for worktrees git-stk
/// created and owns.
pub fn worktree_remove(path: &std::path::Path) -> Result<()> {
    let path = path.to_string_lossy().into_owned();
    status(&["worktree", "remove", "--force", &path])
        .with_context(|| format!("failed to remove the worktree at {path}"))
}

/// Move an existing worktree's detached HEAD to `commit`, without touching any
/// branch.
pub fn checkout_detached_in(worktree: &std::path::Path, commit: &str) -> Result<()> {
    let dir = worktree.to_string_lossy().into_owned();
    status(&["-C", &dir, "checkout", "--detach", "--quiet", commit])
        .with_context(|| format!("failed to check out {commit} in {dir}"))
}

/// An absolute path under the repo's common git dir. Callers that hand a path to
/// another process (a worktree location, a command's working directory) need it
/// absolute, since a relative one would be read against the wrong directory.
pub fn git_common_path_absolute(path: &str) -> Result<std::path::PathBuf> {
    let joined = git_common_path(path)?;
    std::path::absolute(&joined).with_context(|| format!("failed to resolve {joined}"))
}

/// The worktree holding `branch`, if one other than this one does.
pub fn worktree_holding(branch: &str) -> Result<Option<std::path::PathBuf>> {
    Ok(worktree_branches()?
        .into_iter()
        .find(|(name, _)| name == branch)
        .map(|(_, path)| path))
}

/// Whether `path` is the repo's main worktree. Worth telling apart because
/// `git worktree remove` refuses on it, so any advice that would free a branch
/// by removing its worktree is a dead end there.
pub fn is_main_worktree(path: &std::path::Path) -> bool {
    // Best effort: an unreadable listing just means the path goes undistinguished
    // and the advice stays the one that works everywhere.
    main_worktree().is_some_and(|main| same_path(&main, path))
}

/// The main worktree - the first record `git worktree list` reports.
fn main_worktree() -> Option<std::path::PathBuf> {
    parse_main_worktree(&output(&["worktree", "list", "--porcelain"]).ok()?)
}

fn parse_main_worktree(porcelain: &str) -> Option<std::path::PathBuf> {
    porcelain
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(std::path::PathBuf::from)
}

/// How to hand a branch back, as a command the user can paste. Detaching is what
/// the guards lead with because it works on every worktree: `git worktree remove`
/// refuses on the main one, and moving the operation into the holding worktree
/// only helps when that worktree is the only one in the way.
pub fn detach_command(path: &std::path::Path) -> String {
    // Quoted: a worktree path containing a space would otherwise be pasted back
    // as two arguments.
    format!("git -C \"{}\" checkout --detach", display_path(path))
}

/// A worktree path for a message, tagged when it is the main one so the reader
/// knows why removing it is not among the options.
pub fn describe_worktree(path: &std::path::Path) -> String {
    let shown = display_path(path);
    if is_main_worktree(path) {
        format!("{shown} (the main worktree)")
    } else {
        shown
    }
}

/// Collapse worktree paths to the distinct places involved, so a message about
/// three branches held by one worktree suggests freeing it once, not three times.
pub fn distinct_paths<'a>(
    paths: impl IntoIterator<Item = &'a std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let mut distinct: Vec<std::path::PathBuf> = Vec::new();
    for path in paths {
        if !distinct.iter().any(|seen| same_path(seen, path)) {
            distinct.push(path.to_path_buf());
        }
    }
    distinct
}

/// Parse `git worktree list --porcelain` into (branch, path) pairs. Records are
/// blank-line separated, each opening with `worktree <path>`; only those with a
/// `branch` line hold a branch, so bare and detached ones drop out. The record
/// rooted at `current` is excluded, letting callers read a hit as "someone else
/// holds this".
fn parse_worktree_branches(
    porcelain: &str,
    current: Option<&std::path::Path>,
) -> Vec<(String, std::path::PathBuf)> {
    let current = current.map(canonical);
    let mut held = Vec::new();
    let mut path: Option<std::path::PathBuf> = None;

    for line in porcelain.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(std::path::PathBuf::from(rest));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            // take() so a record without a branch line cannot borrow the next
            // record's path.
            if let Some(path) = path.take()
                && current.as_deref() != Some(canonical(&path).as_path())
            {
                held.push((branch.to_owned(), path));
            }
        }
    }

    held
}

/// Resolve a worktree path for comparison. Symlinked or `/tmp`-style paths
/// otherwise read as a different worktree than the one we are standing in.
fn canonical(path: &std::path::Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Whether two paths name the same place. Never compare worktree paths with
/// `==`: git reports its own resolved form, which differs from anything we
/// build ourselves - `/var` against `/private/var` on macOS, forward against
/// back slashes on Windows - so exact equality quietly reports "different".
pub fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    canonical(a) == canonical(b)
}

/// Render a worktree path for a message the user may paste back as a command.
/// Sibling worktrees are the common layout and `../wt-a` reads better than a
/// long absolute path. Only exact prefix matches are shortened, so the result is
/// always a usable path - never a guess.
pub fn display_path(path: &std::path::Path) -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return path.display().to_string();
    };

    if let Ok(rest) = path.strip_prefix(&cwd)
        && rest.components().next().is_some()
    {
        return format!("./{}", rest.display());
    }
    if let Some(up) = cwd.parent()
        && let Ok(rest) = path.strip_prefix(up)
        && rest.components().next().is_some()
    {
        return format!("../{}", rest.display());
    }

    path.display().to_string()
}

pub fn remote_url(remote: &str) -> Result<Option<String>> {
    // git remote get-url exits 2 when the remote does not exist.
    output_codes(&["remote", "get-url", remote], &[2], "git remote get-url")
}

/// The explanation for an operation git refuses because another worktree holds
/// `branch` - checkout and rebase both hit this, and should say the same thing.
/// Asked structurally rather than by matching git's wording, which varies across
/// versions and locales. An unanswerable query (very old git, an odd setup)
/// yields None and the caller falls through to git's own error, as before.
fn worktree_collision(branch: &str) -> Option<String> {
    let path = worktree_holding(branch).ok().flatten()?;
    Some(collision_message(
        branch,
        &display_path(&path),
        is_main_worktree(&path),
    ))
}

/// The wording, split out so it can be checked directly. The suggested commands
/// are quoted: a worktree path containing a space would otherwise be pasted back
/// as two arguments. Removal is only offered for a linked worktree - git refuses
/// to remove the main one, so suggesting it there sends the user nowhere.
fn collision_message(branch: &str, shown: &str, is_main: bool) -> String {
    let mut free = format!("free it with `git -C \"{shown}\" checkout --detach`");
    if !is_main {
        free.push_str(&format!(
            ", or drop that worktree with `git worktree remove \"{shown}\"`"
        ));
    }
    format!(
        "{branch} is checked out in the worktree at {shown}\n\
         work on it there with `cd \"{shown}\"`, or {free}"
    )
}

pub fn checkout(branch: &str) -> Result<()> {
    checkout_silently(branch)?;
    anstream::println!("switched to {}", switched_to(branch));
    Ok(())
}

/// Switch without announcing it on stdout, for callers whose stdout carries a
/// value a shell will consume. They report the switch themselves, on stderr.
pub fn checkout_silently(branch: &str) -> Result<()> {
    if let Some(message) = worktree_collision(branch) {
        bail!(message);
    }

    status(&["switch", branch]).with_context(|| format!("failed to check out {branch}"))
}

/// The "switched to <branch>" wording, so stdout and stderr callers agree.
pub fn switched_to(branch: &str) -> String {
    crate::style::paint(crate::style::BRANCH, branch)
}

pub fn create_branch(branch: &str) -> Result<()> {
    status(&["switch", "-c", branch]).with_context(|| format!("failed to create branch {branch}"))
}

/// Create a branch pointing at `sha` without checking it out or touching the
/// working tree - used by `split` to point new branches at existing commits.
pub fn create_branch_at(branch: &str, sha: &str) -> Result<()> {
    status(&["branch", branch, sha])
        .with_context(|| format!("failed to create branch {branch} at {sha}"))
}

/// Force-delete a branch. Use only once review state confirms it landed: a
/// squash merge leaves the commits non-ancestry-merged, so `git branch -d`
/// would refuse even though the work is in.
pub fn delete_branch(branch: &str) -> Result<()> {
    status(&["branch", "-D", branch]).with_context(|| format!("failed to delete branch {branch}"))
}

/// Rename a branch; git moves its `branch.<name>.*` config along with it.
pub fn rename_branch(old: &str, new: &str) -> Result<()> {
    status(&["branch", "-m", old, new]).with_context(|| format!("failed to rename {old} to {new}"))
}

/// Fast-forward a local branch from its remote without checking it out.
pub fn fetch_branch(remote: &str, branch: &str) -> Result<()> {
    let refspec = format!("{branch}:{branch}");
    status(&["fetch", remote, &refspec])
        .with_context(|| format!("failed to fetch {branch} from {remote}"))
}

pub fn pull_ff_only() -> Result<()> {
    status(&["pull", "--ff-only"]).context("failed to fast-forward from the remote")
}

/// Force-push `branches` (with lease), returning the branches that actually
/// landed. Normally that is all of them; the exception is the merge-queue
/// backstop below, which drops a held-back branch from the returned set so the
/// caller never reports a branch as both held and pushed.
pub fn push_force_with_lease(remote: &str, branches: &[String]) -> Result<Vec<String>> {
    let mut args = vec!["push", "--force-with-lease", remote];
    args.extend(branches.iter().map(String::as_str));

    run_lease_push(&args, remote, branches)
}

/// Run a force-with-lease push, returning the branches that actually landed,
/// and classifying the two rejections git-stk can explain better than raw git
/// output:
///
/// - **Merge queue** (GitHub locks a queued branch): the ref is rejected with
///   GH006 while its siblings push fine. `restack`/`sync` already freeze
///   branches they know are queued, so this is the backstop for one enqueued
///   mid-run - the held ref is reported and dropped from the returned set, the
///   successful refs stand, and the push is not failed.
/// - **Stale lease** (the remote moved on, usually because a branch in the
///   stack merged): the lease no longer matches, so git rejects with `stale
///   info`/`non-fast-forward`. `git stk sync` reconciles it, so say so instead
///   of leaving the user with git's plumbing error.
///
/// Anything else surfaces with git's own output, unchanged.
fn run_lease_push(args: &[&str], remote: &str, branches: &[String]) -> Result<Vec<String>> {
    // Verbose mode streams straight through, so there is no captured stderr to
    // classify; fall back to the plain path. (A rejection there still shows
    // git's own message, just without the friendlier translation.)
    if verbose() {
        status_passthrough(args).with_context(|| format!("failed to push branches to {remote}"))?;
        return Ok(branches.to_vec());
    }

    let output = Command::new("git")
        .args(args)
        .output()
        .context("failed to run git")?;
    if output.status.success() {
        return Ok(branches.to_vec());
    }

    // A GitHub branch sitting in a merge queue is locked, so its ref is rejected
    // with GH006 while its siblings push fine; git then exits non-zero even
    // though the rest landed. `restack`/`sync` already freeze branches they know
    // are queued, so this is the backstop for one enqueued mid-run: report the
    // held ref, drop it from the landed set, and let the successful refs stand.
    // Any rejection that is not purely the merge queue (a stale lease, a
    // non-fast-forward) still surfaces as an error.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Some(queued) = merge_queue_rejection(&stderr) {
        anstream::eprintln!(
            "{}",
            crate::style::warn(&format!(
                "{} {} in a merge queue and was not updated (dequeue its review to push it)",
                queued.join(", "),
                if queued.len() == 1 { "is" } else { "are" },
            ))
        );
        return Ok(landed_branches(branches, &queued));
    }

    if let Some(stale) = stale_rejection(&stderr) {
        // The user asked for a clean message, not raw git/GitHub noise, so the
        // captured output is dropped in favor of the actionable guidance.
        bail!(
            "could not push {} to {remote}: the remote has moved on \
             (a branch in the stack was likely merged or updated upstream)\n\
             run `git stk sync` to reconcile your local stack with the remote, then try again",
            stale.join(", "),
        );
    }

    let _ = std::io::stdout().write_all(&output.stdout);
    let _ = std::io::stderr().write_all(&output.stderr);
    bail!(
        "failed to push branches to {remote}: git exited with status {}",
        output.status
    )
}

/// The branches that landed: everything attempted except those held back by
/// the merge queue, preserving the attempted order.
fn landed_branches(attempted: &[String], held: &[String]) -> Vec<String> {
    attempted
        .iter()
        .filter(|branch| !held.iter().any(|name| name == *branch))
        .cloned()
        .collect()
}

/// The rejected refs when a push failed *only* because they are in a merge
/// queue, or None when any other failure is mixed in. A genuine lease/
/// fast-forward rejection (`stale info`, `non-fast-forward`, `fetch first`)
/// returns None so it is classified as stale instead; a queue rejection with
/// no such marker returns the branch names so the caller can report them and
/// carry on.
fn merge_queue_rejection(stderr: &str) -> Option<Vec<String>> {
    let lower = stderr.to_lowercase();
    let mentions_queue = lower.contains("merge queue") || lower.contains("queued for merging");
    if !mentions_queue {
        return None;
    }
    // A lease or fast-forward failure is a real problem, not a queue lock - do
    // not swallow a push that failed for those reasons too.
    if ["stale info", "non-fast-forward", "fetch first"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return None;
    }
    let rejected = rejected_refs(stderr);
    if rejected.is_empty() {
        None
    } else {
        Some(rejected)
    }
}

/// The rejected refs when a push was refused because the local side is behind
/// the remote: a `--force-with-lease` lease mismatch (`stale info`), or a plain
/// `non-fast-forward`/`fetch first`. This is the remote having moved on - in a
/// stack, almost always a lower branch that merged - which `git stk sync`
/// reconciles.
///
/// Returns Some only when *every* rejected ref is stale: the friendly "run
/// sync" message replaces git's raw output, so a non-stale rejection mixed in
/// (a permission denial, a declined hook) - which sync would not fix - must
/// fall through to git's own error instead of being hidden behind sync advice.
/// None when nothing was rejected, or any rejection was for another reason.
fn stale_rejection(stderr: &str) -> Option<Vec<String>> {
    let rejected: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("[remote rejected]") || line.contains("[rejected]"))
        .collect();
    if rejected.is_empty() || !rejected.iter().all(|line| line_is_stale(line)) {
        return None;
    }
    let names: Vec<String> = rejected
        .iter()
        .filter_map(|line| rejected_ref_name(line))
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

/// Whether a rejected-ref line was refused because the local side is behind the
/// remote (a `--force-with-lease` lease mismatch or a non-fast-forward), rather
/// than a permission/hook refusal. The reason is in the line's trailing `(…)`.
fn line_is_stale(line: &str) -> bool {
    let lower = line.to_lowercase();
    ["stale info", "non-fast-forward", "fetch first"]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// The remote-side ref name from a single `! [remote rejected] <local> ->
/// <remote> (reason)` line.
fn rejected_ref_name(line: &str) -> Option<String> {
    let after = line.split("-> ").nth(1)?;
    Some(after.split_whitespace().next()?.to_owned())
}

/// The remote-side ref names from a push's `! [remote rejected]`/`! [rejected]`
/// lines, regardless of reason.
fn rejected_refs(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| line.contains("[remote rejected]") || line.contains("[rejected]"))
        .filter_map(rejected_ref_name)
        .collect()
}

/// Push branches and set upstream tracking; used before submitting so new
/// branches exist remotely and rebased ones are safely updated.
pub fn push_set_upstream_force_with_lease(remote: &str, branches: &[String]) -> Result<()> {
    let mut args = vec!["push", "--set-upstream", "--force-with-lease", remote];
    args.extend(branches.iter().map(String::as_str));

    // submit does not need the landed set; a held-back branch is still warned
    // about inside run_lease_push.
    run_lease_push(&args, remote, branches)?;
    Ok(())
}

/// Store `content` as a single-file commit and point `reference` at it, so the
/// data rides along a normal ref push. Orphan each time: the ref just moves to
/// the new commit (callers force-push it, as it is regenerable).
pub fn write_blob_ref(reference: &str, file: &str, content: &str) -> Result<()> {
    let blob = output_with_stdin(&["hash-object", "-w", "--stdin"], content)
        .context("failed to hash stack metadata")?;
    let tree = output_with_stdin(&["mktree"], &format!("100644 blob {blob}\t{file}\n"))
        .context("failed to write stack metadata tree")?;
    let commit = output(&["commit-tree", &tree, "-m", "git-stk stack metadata"])
        .context("failed to commit stack metadata")?;
    status(&["update-ref", reference, &commit])
        .with_context(|| format!("failed to update {reference}"))
}

/// Force-push a single ref to `remote` (the value is regenerable, so
/// last-writer-wins is fine).
pub fn push_ref(remote: &str, reference: &str) -> Result<()> {
    status(&[
        "push",
        "--force",
        remote,
        &format!("{reference}:{reference}"),
    ])
    .with_context(|| format!("failed to push {reference} to {remote}"))
}

/// Force-fetch a single ref from `remote` into the same local ref.
pub fn fetch_ref(remote: &str, reference: &str) -> Result<()> {
    status(&["fetch", remote, &format!("+{reference}:{reference}")])
        .with_context(|| format!("failed to fetch {reference} from {remote}"))
}

/// The contents of `file` in the commit `reference` points at, or None when
/// the ref or file is absent.
pub fn read_ref_file(reference: &str, file: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["cat-file", "blob", &format!("{reference}:{file}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run git cat-file")?;
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
    } else {
        Ok(None)
    }
}

pub fn rebase(parent: &str, branch: &str, update_refs: bool) -> Result<()> {
    if let Some(message) = worktree_collision(branch) {
        bail!(message);
    }
    let mut args = vec!["rebase"];
    if update_refs {
        args.push("--update-refs");
    }
    args.extend([parent, branch]);

    status(&args).with_context(|| format!("failed to rebase {branch} onto {parent}"))
}

/// Rebase only the commits after `base`, replaying `base..branch` onto
/// `parent`. Used when the recorded fork point is known so commits that
/// landed upstream by squash or rebase are not replayed.
pub fn rebase_onto(parent: &str, base: &str, branch: &str, update_refs: bool) -> Result<()> {
    if let Some(message) = worktree_collision(branch) {
        bail!(message);
    }
    let mut args = vec!["rebase"];
    if update_refs {
        args.push("--update-refs");
    }
    args.extend(["--onto", parent, base, branch]);

    status(&args).with_context(|| format!("failed to rebase {branch} onto {parent} from {base}"))
}

pub fn rev_parse(rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    output(&["rev-parse", "--verify", &spec]).with_context(|| format!("failed to resolve {rev}"))
}

/// The commit a branch points at, or None when the branch does not exist.
pub fn branch_sha(branch: &str) -> Option<String> {
    rev_parse(branch).ok()
}

/// Point a branch at a commit, creating it if absent. Does not touch the
/// worktree.
pub fn update_ref(branch: &str, sha: &str) -> Result<()> {
    status(&["update-ref", &format!("refs/heads/{branch}"), sha])
        .with_context(|| format!("failed to update {branch} to {sha}"))
}

/// Reset the worktree and index to HEAD. Safe to lose nothing only on a
/// clean tree; callers must check [`worktree_is_clean`] first.
pub fn reset_hard() -> Result<()> {
    status(&["reset", "--hard"]).context("failed to reset the worktree")
}

/// Whether the worktree and index have no uncommitted changes.
pub fn worktree_is_clean() -> Result<bool> {
    Ok(output(&["status", "--porcelain"])?.is_empty())
}

/// Default branch of `remote` (from its locally-known HEAD symref), if any.
pub fn remote_default_branch(remote: &str) -> Option<String> {
    let reference = format!("refs/remotes/{remote}/HEAD");
    let full = output(&["symbolic-ref", "--short", &reference]).ok()?;
    full.strip_prefix(&format!("{remote}/")).map(str::to_owned)
}

/// How many commits `parent` has that `branch` does not: nonzero means the
/// branch needs a restack.
pub fn commits_behind(branch: &str, parent: &str) -> Result<usize> {
    let range = format!("{branch}..{parent}");
    let count = output(&["rev-list", "--count", &range])
        .with_context(|| format!("failed to count commits in {range}"))?;
    count
        .trim()
        .parse()
        .context("failed to parse rev-list count")
}

pub fn merge_base(a: &str, b: &str) -> Result<String> {
    output(&["merge-base", a, b])
        .with_context(|| format!("failed to find merge base of {a} and {b}"))
}

/// A unified-0 diff against HEAD: just the staged changes when `cached`,
/// otherwise all tracked changes (staged and unstaged). Zero context lines
/// so each hunk's pre-image range pinpoints exactly the lines it touches.
pub fn diff_against_head(cached: bool) -> Result<String> {
    // Pin a/ b/ prefixes: diff.mnemonicPrefix / diff.noprefix would otherwise
    // emit headers absorb's parser and `git apply` cannot read.
    let mut args = vec!["diff", "--unified=0", "--src-prefix=a/", "--dst-prefix=b/"];
    if cached {
        args.push("--cached");
    }
    args.push("HEAD");
    output(&args).context("failed to diff against HEAD")
}

/// The distinct commits that last touched lines `start..start+len` of `file`
/// in HEAD, newest blame wins per line. An empty range yields nothing.
pub fn blame_line_shas(file: &str, start: usize, len: usize) -> Result<Vec<String>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let range = format!("{start},{}", start + len - 1);
    let out = output(&[
        "blame",
        "HEAD",
        "-L",
        &range,
        "--line-porcelain",
        "--",
        file,
    ])
    .with_context(|| format!("failed to blame {file}"))?;

    let mut shas = Vec::new();
    for line in out.lines() {
        // Each porcelain block opens with "<40-hex sha> <orig> <final> ...";
        // other fields (author, summary, "previous", the tab-led content) do
        // not start with a bare 40-hex token.
        let token = line.split(' ').next().unwrap_or_default();
        if token.len() == 40
            && token.bytes().all(|byte| byte.is_ascii_hexdigit())
            && !shas.iter().any(|seen| seen == token)
        {
            shas.push(token.to_owned());
        }
    }
    Ok(shas)
}

/// The commits in `range` (e.g. "main..HEAD"), newest first.
pub fn rev_list(range: &str) -> Result<Vec<String>> {
    Ok(output(&["rev-list", range])
        .with_context(|| format!("failed to list commits in {range}"))?
        .lines()
        .map(str::to_owned)
        .collect())
}

/// `(short-sha, subject)` for each commit in `range` (e.g. "main..HEAD"),
/// newest first - one git call, for listing a branch's own commits.
pub fn log_oneline(range: &str) -> Result<Vec<(String, String)>> {
    Ok(output(&["log", "--format=%h%x09%s", range])
        .with_context(|| format!("failed to log {range}"))?
        .lines()
        .filter_map(|line| {
            line.split_once('\t')
                .map(|(sha, subject)| (sha.to_owned(), subject.to_owned()))
        })
        .collect())
}

/// A commit's subject line.
pub fn commit_subject(sha: &str) -> Result<String> {
    output(&["show", "--no-patch", "--format=%s", sha])
        .with_context(|| format!("failed to read subject of {sha}"))
}

/// A commit's body - everything after the subject line; empty when there is none.
pub fn commit_body(sha: &str) -> Result<String> {
    output(&["show", "--no-patch", "--format=%b", sha])
        .with_context(|| format!("failed to read body of {sha}"))
}

/// Stage a unified-0 patch into the index. `--unidiff-zero` is required for
/// git to accept the zero-context hunks absorb works with.
pub fn apply_cached(patch: &str) -> Result<()> {
    let mut child = Command::new("git")
        .args(["apply", "--cached", "--unidiff-zero"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run git apply")?;
    {
        let mut stdin = child.stdin.take().context("git apply has no stdin")?;
        stdin
            .write_all(patch.as_bytes())
            .context("failed to write patch to git apply")?;
    }
    let output = child
        .wait_with_output()
        .context("failed to run git apply")?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("git apply", &output.stderr))
    }
}

/// Commit the staged index as a `fixup!` of `sha`, for a later autosquash
/// rebase to fold in. Skips hooks: these are internal, transient commits.
pub fn commit_fixup(sha: &str) -> Result<()> {
    status(&["commit", "--no-verify", &format!("--fixup={sha}")])
        .with_context(|| format!("failed to create fixup commit for {sha}"))
}

/// Unstage everything, leaving the worktree contents untouched.
pub fn reset_index() -> Result<()> {
    status(&["reset", "--quiet"]).context("failed to reset the index")
}

/// Move HEAD to `sha`, returning any commits after it to the index.
pub fn reset_soft(sha: &str) -> Result<()> {
    status(&["reset", "--soft", sha]).with_context(|| format!("failed to reset to {sha}"))
}

/// Stash tracked worktree changes; pair with [`stash_pop`].
pub fn stash_push() -> Result<()> {
    status(&["stash", "push", "--quiet"]).context("failed to stash changes")
}

/// Restore the most recently stashed changes.
pub fn stash_pop() -> Result<()> {
    status(&["stash", "pop", "--quiet"]).context("failed to restore stashed changes")
}

/// Rebase `base..HEAD`, folding `fixup!` commits into their targets. The
/// generated todo is accepted unedited, so it needs no terminal.
pub fn rebase_autosquash(base: &str, update_refs: bool) -> Result<()> {
    let mut args = vec!["rebase", "--interactive", "--autosquash"];
    if update_refs {
        args.push("--update-refs");
    }
    args.push(base);

    let output = Command::new("git")
        .args(&args)
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_EDITOR", "true")
        .output()
        .context("failed to run git rebase")?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("git rebase --autosquash", &output.stderr))
    }
}

pub fn is_ancestor(ancestor: &str, descendant: &str) -> Result<bool> {
    // merge-base --is-ancestor exits 0 when it is, 1 when it is not.
    Ok(output_codes(
        &["merge-base", "--is-ancestor", ancestor, descendant],
        &[1],
        "git merge-base --is-ancestor",
    )?
    .is_some())
}

/// Lines added and deleted in `branch` relative to `base`, over the symmetric
/// `base...branch` range a forge uses for a review diff (the branch's own work
/// since it diverged). Binary files, which `--numstat` marks with `-`, count
/// as zero.
pub fn diff_numstat(base: &str, branch: &str) -> Result<(usize, usize)> {
    let output = output(&["diff", "--numstat", &format!("{base}...{branch}")])?;
    let mut added = 0;
    let mut deleted = 0;
    for line in output.lines() {
        let mut columns = line.split('\t');
        added += column_count(columns.next());
        deleted += column_count(columns.next());
    }
    Ok((added, deleted))
}

/// A `--numstat` count column: a number, or 0 for `-` (binary) or anything
/// unparseable.
fn column_count(column: Option<&str>) -> usize {
    column
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

pub fn supports_rebase_update_refs() -> Result<bool> {
    let output = Command::new("git")
        .args(["rebase", "-h"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to inspect git rebase help")?;

    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(help_mentions_update_refs(&help))
}

/// Whether the short help advertises --update-refs. Match the option name:
/// git renders it as `--update-refs` or `--[no-]update-refs` by version.
fn help_mentions_update_refs(help: &str) -> bool {
    help.contains("update-refs")
}

/// Whether a rebase is actually paused in this worktree. Distinguishes a real
/// conflict from git-stk merely having left state on file - git refuses to
/// rebase a branch another worktree holds, which fails the run without ever
/// starting a rebase to continue or abort.
pub fn rebase_in_progress() -> bool {
    ["rebase-merge", "rebase-apply"].iter().any(|dir| {
        git_path(dir)
            .map(|path| std::path::Path::new(&path).exists())
            .unwrap_or(false)
    })
}

pub fn rebase_continue() -> Result<()> {
    // Passthrough: continuing a rebase can open the user's editor.
    status_passthrough(&["rebase", "--continue"]).context("failed to continue rebase")
}

pub fn rebase_abort() -> Result<()> {
    status(&["rebase", "--abort"]).context("failed to abort rebase")
}

/// Cherry-pick a commit onto the current branch. On conflict git leaves the
/// cherry-pick in progress, so the error surfaces for the caller to tell the
/// user to resolve and `git cherry-pick --continue`.
pub fn cherry_pick(commit: &str) -> Result<()> {
    status(&["cherry-pick", commit]).with_context(|| format!("failed to cherry-pick {commit}"))
}

/// Refresh the remote-tracking refs (`<remote>/<branch>`) for `branches` that
/// exist on `remote`, in a single fetch. Branches absent from the remote (a
/// freshly created top of stack that was never pushed) are dropped rather than
/// failing the whole fetch. A no-op when none of them are on the remote.
pub fn fetch_tracking(remote: &str, branches: &[String]) -> Result<()> {
    let present = remote_branches_present(remote, branches)?;
    if present.is_empty() {
        return Ok(());
    }
    let mut args = vec!["fetch", remote];
    args.extend(present.iter().map(String::as_str));
    status(&args).with_context(|| format!("failed to fetch branches from {remote}"))
}

/// The subset of `branches` that exist as heads on `remote`, learned in one
/// `ls-remote` so a targeted fetch does not abort on a branch the remote has
/// never seen.
fn remote_branches_present(remote: &str, branches: &[String]) -> Result<Vec<String>> {
    if branches.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["ls-remote", "--heads", remote];
    args.extend(branches.iter().map(String::as_str));
    let listing =
        output(&args).with_context(|| format!("failed to query {remote} for branch heads"))?;
    let present: Vec<&str> = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter_map(|(_, name)| name.strip_prefix("refs/heads/"))
        .collect();
    Ok(branches
        .iter()
        .filter(|branch| present.contains(&branch.as_str()))
        .cloned()
        .collect())
}

/// The commits `tracking` (a `<remote>/<branch>` ref) has that `branch` lacks
/// *and* that have no patch-equivalent already on `branch` - the commits a
/// force-push would silently drop, e.g. one committed straight on the host's
/// web UI. `(short-sha, subject)` oldest-first, the order to cherry-pick them.
/// Empty in the normal post-rebase case, where every remote commit is
/// reproduced locally under a new hash.
pub fn remote_only_commits(branch: &str, tracking: &str) -> Result<Vec<(String, String)>> {
    let range = format!("{branch}...{tracking}");
    let mut commits: Vec<(String, String)> = output(&[
        "log",
        "--cherry-pick",
        "--right-only",
        "--no-merges",
        "--format=%h%x09%s",
        &range,
    ])
    .with_context(|| format!("failed to list remote-only commits in {range}"))?
    .lines()
    .filter_map(|line| {
        line.split_once('\t')
            .map(|(sha, subject)| (sha.to_owned(), subject.to_owned()))
    })
    .collect();
    // log is newest-first; cherry-pick wants oldest-first.
    commits.reverse();
    Ok(commits)
}

pub fn config_get(key: &str) -> Result<Option<String>> {
    // git config --get exits 1 when the key is unset.
    output_codes(&["config", "--get", key], &[1], "git config --get")
}

pub fn config_get_bool(key: &str) -> Result<Option<bool>> {
    let Some(value) = output_codes(
        &["config", "--type=bool", "--get", key],
        &[1],
        "git config --type=bool --get",
    )?
    else {
        return Ok(None);
    };
    match value.as_str() {
        "true" => Ok(Some(true)),
        "false" => Ok(Some(false)),
        _ => bail!("git config {key} is not a boolean: {value}"),
    }
}

pub fn config_get_regexp(pattern: &str) -> Result<Vec<(String, String)>> {
    // git config --get-regexp exits 1 when nothing matches.
    let Some(text) = output_codes(
        &["config", "--get-regexp", pattern],
        &[1],
        "git config --get-regexp",
    )?
    else {
        return Ok(Vec::new());
    };
    Ok(text
        .lines()
        .filter_map(|line| {
            line.split_once(' ')
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
        })
        .collect())
}

pub fn config_set(key: &str, value: &str) -> Result<()> {
    status(&["config", key, value]).with_context(|| format!("failed to set git config {key}"))
}

pub fn config_unset(key: &str) -> Result<()> {
    // git config --unset exits 5 when the key was not set; either way it is now
    // gone, so treat that as success.
    output_codes(&["config", "--unset", key], &[5], "git config --unset").map(|_| ())
}

/// Run a git command and map its exit code: trimmed stdout on success, `None`
/// for any code in `ok_empty` (an expected "nothing here" - e.g. `config
/// --get`'s 1, or `config --unset`'s 5), and an error otherwise. `label` names
/// the command for the error message.
fn output_codes(args: &[&str], ok_empty: &[i32], label: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run git")?;

    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        Some(code) if ok_empty.contains(&code) => Ok(None),
        _ => Err(command_error(label, &output.stderr)),
    }
}

fn output(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run git")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(command_error("git", &output.stderr))
    }
}

/// Like [`output`], but feeds `input` to the command on stdin (for plumbing
/// such as `hash-object --stdin` and `mktree`).
fn output_with_stdin(args: &[&str], input: &str) -> Result<String> {
    let mut child = Command::new("git")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run git")?;
    {
        let mut stdin = child.stdin.take().context("git has no stdin")?;
        stdin
            .write_all(input.as_bytes())
            .context("failed to write to git")?;
    }
    let output = child.wait_with_output().context("failed to run git")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(command_error("git", &output.stderr))
    }
}

/// Run git quietly: progress and advice only matter when something goes
/// wrong, so capture them and replay on failure. `--verbose` passes
/// everything through.
fn status(args: &[&str]) -> Result<()> {
    if verbose() {
        return status_passthrough(args);
    }

    let output = Command::new("git")
        .args(args)
        .output()
        .context("failed to run git")?;

    if output.status.success() {
        Ok(())
    } else {
        let _ = std::io::stdout().write_all(&output.stdout);
        let _ = std::io::stderr().write_all(&output.stderr);
        bail!("git exited with status {}", output.status)
    }
}

/// Inherit stdio unconditionally, for git commands that may need the
/// terminal (e.g. `rebase --continue` opening the editor).
fn status_passthrough(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .status()
        .context("failed to run git")?;

    if status.success() {
        Ok(())
    } else {
        bail!("git exited with status {status}")
    }
}

fn command_error(command: &str, stderr: &[u8]) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(stderr).trim().to_owned();
    if stderr.is_empty() {
        anyhow!("{command} failed")
    } else {
        anyhow!("{command} failed: {stderr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `git worktree list --porcelain` prints for a main worktree, a
    /// linked one, a detached one, and a bare repo.
    const PORCELAIN: &str = "\
worktree /repo
HEAD f7cff917cf874d0c6ff3108260fda91ac3271baf
branch refs/heads/feat/b

worktree /repo/../wt-a
HEAD 0700673acebfe459d480fa3bd616b2ecf6249fe1
branch refs/heads/feat/a

worktree /repo/../wt-detached
HEAD 25fb6254b4b1cd5cbe2b0d4b1f5b1cf6e7d8a9b0
detached
";

    #[test]
    fn worktree_parsing_keeps_branches_and_drops_detached_ones() {
        // No current worktree to exclude: every branch-holding record survives,
        // and the detached one - which holds no branch and so blocks nothing -
        // does not.
        let held = parse_worktree_branches(PORCELAIN, None);
        assert_eq!(
            held,
            vec![
                ("feat/b".to_owned(), std::path::PathBuf::from("/repo")),
                (
                    "feat/a".to_owned(),
                    std::path::PathBuf::from("/repo/../wt-a")
                ),
            ]
        );
    }

    #[test]
    fn worktree_parsing_excludes_the_worktree_we_are_standing_in() {
        // The point of the exclusion: a caller must be able to read a hit as
        // "another worktree holds this", never as its own checkout.
        let held = parse_worktree_branches(PORCELAIN, Some(std::path::Path::new("/repo")));
        assert_eq!(
            held,
            vec![(
                "feat/a".to_owned(),
                std::path::PathBuf::from("/repo/../wt-a")
            )]
        );
    }

    #[test]
    fn a_bare_record_does_not_lend_its_path_to_the_next_branch() {
        // A bare repo opens a record with no branch line. The following
        // worktree's branch must not be attributed to the bare path.
        let porcelain = "\
worktree /repo/.bare
bare

worktree /repo/wt-a
HEAD 0700673acebfe459d480fa3bd616b2ecf6249fe1
branch refs/heads/feat/a
";
        assert_eq!(
            parse_worktree_branches(porcelain, None),
            vec![("feat/a".to_owned(), std::path::PathBuf::from("/repo/wt-a"))]
        );
    }

    #[test]
    fn branch_names_containing_slashes_survive_the_refs_heads_strip() {
        // Only the refs/heads/ prefix comes off - the rest of the name is the
        // branch, slashes and all.
        let porcelain = "\
worktree /repo/wt
HEAD 0700673acebfe459d480fa3bd616b2ecf6249fe1
branch refs/heads/feat/deep/nested/name
";
        assert_eq!(
            parse_worktree_branches(porcelain, None)
                .first()
                .map(|(branch, _)| branch.as_str()),
            Some("feat/deep/nested/name")
        );
    }

    #[test]
    fn empty_porcelain_holds_nothing() {
        assert!(parse_worktree_branches("", None).is_empty());
    }

    #[test]
    fn a_collision_message_quotes_the_path_it_suggests_pasting() {
        // A worktree path with a space in it has to survive the round trip into
        // the user's shell.
        let message = collision_message("feat/a", "../my worktree", false);
        assert!(
            message.contains(r#"`cd "../my worktree"`"#),
            "cd suggestion is not pasteable: {message}"
        );
        assert!(
            message.contains(r#"`git worktree remove "../my worktree"`"#),
            "remove suggestion is not pasteable: {message}"
        );
        assert!(
            message.contains(r#"`git -C "../my worktree" checkout --detach`"#),
            "detach suggestion is not pasteable: {message}"
        );
    }

    #[test]
    fn a_collision_with_the_main_worktree_never_suggests_removing_it() {
        // `git worktree remove` refuses on the main worktree, so offering it
        // there would be advice the user cannot act on.
        let message = collision_message("feat/a", "../product", true);
        assert!(
            !message.contains("git worktree remove"),
            "the main worktree cannot be removed: {message}"
        );
        assert!(
            message.contains(r#"`git -C "../product" checkout --detach`"#),
            "no workable way to free the branch: {message}"
        );
    }

    #[test]
    fn the_main_worktree_is_the_first_record_listed() {
        let porcelain = "\
worktree /repo/product
HEAD 1111111111111111111111111111111111111111
branch refs/heads/feat/b

worktree /repo/product-worktrees/feat/a
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feat/a
";
        assert_eq!(
            parse_main_worktree(porcelain),
            Some(std::path::PathBuf::from("/repo/product"))
        );
    }

    #[test]
    fn no_listing_names_no_main_worktree() {
        assert_eq!(parse_main_worktree(""), None);
    }

    #[test]
    fn one_worktree_holding_three_branches_is_freed_once() {
        let held = [
            std::path::Path::new("../wt-a"),
            std::path::Path::new("../wt-a"),
            std::path::Path::new("../wt-b"),
        ];
        assert_eq!(
            distinct_paths(held),
            vec![
                std::path::PathBuf::from("../wt-a"),
                std::path::PathBuf::from("../wt-b")
            ]
        );
    }

    #[test]
    fn a_collision_message_names_the_branch_and_where_it_lives() {
        let message = collision_message("feat/a", "../wt-a", false);
        assert!(message.starts_with("feat/a is checked out in the worktree at ../wt-a"));
    }

    #[test]
    fn a_merge_queue_rejection_is_downgraded_to_the_queued_refs() {
        // The exact shape git prints when one ref of a multi-ref push is locked
        // by a GitHub merge queue while its sibling pushes fine.
        let stderr = "\
remote: error: GH006: Protected branch update failed for refs/heads/feat/tf-deploy.
remote: - A pull request for this branch has been added to a merge queue. Branches that
remote:   are queued for merging cannot be updated. To modify this branch, dequeue the
remote:   associated pull request.
To github.com:higharc/product
 + 016bb37...3a94024 feat/spa-env -> feat/spa-env (forced update)
 ! [remote rejected]         feat/tf-deploy -> feat/tf-deploy (protected branch hook declined)
error: failed to push some refs to 'github.com:higharc/product'";
        assert_eq!(
            merge_queue_rejection(stderr),
            Some(vec!["feat/tf-deploy".to_owned()])
        );
    }

    #[test]
    fn a_stale_lease_rejection_is_not_swallowed_even_with_a_queue_mention() {
        // A force-with-lease failure is a real problem; the queue wording in the
        // dependabot banner must not mask it.
        let stderr = "\
remote: GitHub found 270 vulnerabilities ... merge queue notes ...
 ! [rejected]        feat/tf-deploy -> feat/tf-deploy (stale info)
error: failed to push some refs";
        assert_eq!(merge_queue_rejection(stderr), None);
    }

    #[test]
    fn no_queue_mention_is_not_a_queue_rejection() {
        let stderr = " ! [remote rejected] feat/x -> feat/x (permission denied)";
        assert_eq!(merge_queue_rejection(stderr), None);
    }

    #[test]
    fn landed_branches_drops_only_the_held_ones() {
        let attempted = [
            "feat/a".to_owned(),
            "feat/b".to_owned(),
            "feat/c".to_owned(),
        ];
        // A branch held back by the queue is dropped; order is preserved so the
        // "pushed ..." line never names a branch warned as held.
        assert_eq!(
            landed_branches(&attempted, &["feat/b".to_owned()]),
            vec!["feat/a".to_owned(), "feat/c".to_owned()]
        );
        // Nothing held: everything landed.
        assert_eq!(landed_branches(&attempted, &[]), attempted.to_vec());
        // Every branch held: nothing landed.
        assert!(landed_branches(&attempted, &attempted).is_empty());
    }

    #[test]
    fn a_stale_lease_push_names_the_rejected_branch() {
        // The exact shape from a submit after a lower branch merged: one ref
        // pushes, the stale one is rejected by --force-with-lease.
        let stderr = "\
To github.com:higharc/product
   3a94024..d63a2b2  feat/spa-env -> feat/spa-env
 ! [rejected]                feat/tf-deploy -> feat/tf-deploy (stale info)
error: failed to push some refs to 'github.com:higharc/product'";
        assert_eq!(
            stale_rejection(stderr),
            Some(vec!["feat/tf-deploy".to_owned()])
        );
    }

    #[test]
    fn a_non_fast_forward_push_is_treated_as_stale() {
        let stderr = " ! [rejected]  feat/x -> feat/x (non-fast-forward)";
        assert_eq!(stale_rejection(stderr), Some(vec!["feat/x".to_owned()]));
    }

    #[test]
    fn an_unrelated_push_failure_is_not_classified_as_stale() {
        // Permission/network failures must keep their own error, not "run sync".
        let stderr = " ! [remote rejected] feat/x -> feat/x (permission denied)";
        assert_eq!(stale_rejection(stderr), None);
        assert_eq!(stale_rejection("fatal: could not read from remote"), None);
    }

    #[test]
    fn a_mixed_stale_and_non_stale_rejection_is_not_classified_as_stale() {
        // One ref is stale, another was refused for a reason `git stk sync`
        // will not fix; the clean message replaces git's output, so it must not
        // claim sync resolves the permission failure - fall through to raw git.
        let stderr = "\
 ! [rejected]                feat/tf-deploy -> feat/tf-deploy (stale info)
 ! [remote rejected]         feat/locked -> feat/locked (permission denied)
error: failed to push some refs";
        assert_eq!(stale_rejection(stderr), None);
    }

    #[test]
    fn help_mentions_update_refs_matches_pre_2_43_spelling() {
        assert!(help_mentions_update_refs(
            "    --update-refs    update branches that point to commits that are being rebased"
        ));
    }

    #[test]
    fn help_mentions_update_refs_matches_negatable_spelling() {
        assert!(help_mentions_update_refs(
            "    --[no-]update-refs    update branches that point to commits that are being rebased"
        ));
    }

    #[test]
    fn help_mentions_update_refs_rejects_help_without_the_option() {
        assert!(!help_mentions_update_refs(
            "    --[no-]autosquash    move commits that begin with squash!/fixup!"
        ));
    }

    #[test]
    fn detection_agrees_with_the_real_git_on_this_machine() {
        // Ground truth: `--update-refs -h` fails with "unknown option" on a
        // git without the flag and prints help on one that has it.
        let probe = Command::new("git")
            .args(["rebase", "--update-refs", "-h"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run git rebase probe");
        let probe_text = format!(
            "{}{}",
            String::from_utf8_lossy(&probe.stdout),
            String::from_utf8_lossy(&probe.stderr)
        );
        let real_support = !probe_text.contains("unknown option");

        assert_eq!(
            supports_rebase_update_refs().expect("detect support"),
            real_support
        );
    }
}
