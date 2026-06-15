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

pub fn remote_url(remote: &str) -> Result<Option<String>> {
    // git remote get-url exits 2 when the remote does not exist.
    output_codes(&["remote", "get-url", remote], &[2], "git remote get-url")
}

pub fn checkout(branch: &str) -> Result<()> {
    status(&["switch", branch]).with_context(|| format!("failed to check out {branch}"))?;
    anstream::println!(
        "switched to {}",
        crate::style::paint(crate::style::BRANCH, branch)
    );
    Ok(())
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

pub fn push_force_with_lease(remote: &str, branches: &[String]) -> Result<()> {
    let mut args = vec!["push", "--force-with-lease", remote];
    args.extend(branches.iter().map(String::as_str));

    status(&args).with_context(|| format!("failed to push branches to {remote}"))
}

/// Push branches and set upstream tracking; used before submitting so new
/// branches exist remotely and rebased ones are safely updated.
pub fn push_set_upstream_force_with_lease(remote: &str, branches: &[String]) -> Result<()> {
    let mut args = vec!["push", "--set-upstream", "--force-with-lease", remote];
    args.extend(branches.iter().map(String::as_str));

    status(&args).with_context(|| format!("failed to push branches to {remote}"))
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

pub fn rebase_continue() -> Result<()> {
    // Passthrough: continuing a rebase can open the user's editor.
    status_passthrough(&["rebase", "--continue"]).context("failed to continue rebase")
}

pub fn rebase_abort() -> Result<()> {
    status(&["rebase", "--abort"]).context("failed to abort rebase")
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
