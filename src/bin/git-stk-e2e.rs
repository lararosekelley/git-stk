//! Live end-to-end smoke test against a real review provider.
//!
//! Creates an ephemeral repo on the provider, runs the core stacked-branch
//! lifecycle with the built `git-stk`, and deletes the repo on the way out
//! (even on failure, via a drop guard). Gated behind the `e2e` feature so it
//! never ships; invoked by `.github/workflows/e2e.yml`.
//!
//! This is the proof-of-concept cell: GitHub only, the core lifecycle
//! (build a stack -> submit -> restack -> squash-merge -> sync). GitLab and the
//! deeper paths (issue auto-close, adopt/repair/rename) are TODO once the
//! mechanism is proven in CI.
//!
//! Env:
//!   STK_E2E_PROVIDER  `github` (only github implemented so far)
//!   STK_E2E_OWNER     owner (user or org) for the ephemeral repo
//!   GIT_STK_BIN       path to the `git-stk` binary under test
//!   GH_TOKEN          gh auth, with `repo` + `delete_repo` scope
//!
//! Assumes `gh auth setup-git` has run so git can push over HTTPS.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    match run() {
        Ok(()) => println!("e2e: PASSED"),
        Err(error) => {
            eprintln!("e2e: FAILED: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let provider = env("STK_E2E_PROVIDER");
    if provider != "github" {
        return Err(format!(
            "provider {provider:?} not implemented yet (PoC is github-only)"
        ));
    }
    let owner = env("STK_E2E_OWNER");

    // Unique name without a rand dependency: process id plus a nanosecond stamp.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let slug = format!("{owner}/git-stk-e2e-{}-{stamp}", std::process::id());

    // Create the ephemeral repo first; the guard deletes it no matter how we
    // exit. --add-readme gives an initial commit on the default branch.
    sh(
        "gh",
        &["repo", "create", &slug, "--private", "--add-readme"],
        None,
    )?;
    let _repo = RepoGuard { slug: slug.clone() };

    let dir = clone(&slug)?;
    let work = dir.as_path();

    // Identity + squash strategy (the path the squash-merge restack-drop fix
    // lives on, so the e2e doubles as that regression's live guard).
    git(work, &["config", "user.email", "e2e@git-stk.test"])?;
    git(work, &["config", "user.name", "git-stk e2e"])?;
    git(work, &["config", "stk.mergeStrategy", "squash"])?;

    // 1. Build a two-branch stack.
    stk(work, &["new", "feat/a"])?;
    write(work, "a.txt", "a\n")?;
    git(work, &["add", "."])?;
    git(work, &["commit", "-m", "a work"])?;
    stk(work, &["new", "feat/b"])?;
    write(work, "b.txt", "b\n")?;
    git(work, &["add", "."])?;
    git(work, &["commit", "-m", "b work"])?;

    // 2. Submit the stack; expect two open PRs, feat/b targeting feat/a.
    stk(work, &["submit", "--stack", "--push"])?;
    let open = sh(
        "gh",
        &[
            "pr", "list", "--repo", &slug, "--state", "open", "--json", "number",
        ],
        None,
    )?;
    let count = open.matches("\"number\"").count();
    if count != 2 {
        return Err(format!(
            "expected 2 open PRs after submit, saw {count}: {open}"
        ));
    }

    // 3. Amend the bottom and restack; the child must follow.
    stk(work, &["bottom"])?;
    write(work, "a.txt", "a\na2\n")?;
    git(work, &["commit", "-am", "a work edit"])?;
    stk(work, &["restack", "--push"])?;

    // 4. Land the whole stack with the squash strategy. No branch protection on
    //    the ephemeral repo, so each `--wait` clears via the "no checks" grace
    //    window. This is the path that squash-broke before: feat/b must rebase
    //    onto main dropping feat/a's squashed commit, with no conflict.
    stk(work, &["merge", "--all", "--wait", "--yes"])?;

    // 5. Everything landed: no open PRs, both files on the trunk.
    let after = sh(
        "gh",
        &[
            "pr", "list", "--repo", &slug, "--state", "open", "--json", "number",
        ],
        None,
    )?;
    if after.contains("\"number\"") {
        return Err(format!(
            "expected no open PRs after merge --all, saw: {after}"
        ));
    }
    git(work, &["switch", "main"])?;
    git(work, &["pull", "--ff-only"])?;
    for file in ["a.txt", "b.txt"] {
        if !work.join(file).exists() {
            return Err(format!("{file} missing on main after the stack landed"));
        }
    }

    Ok(())
}

/// Deletes the ephemeral repo when dropped, so a panic or early return can't
/// leave it behind. Best-effort: a failed delete only warns.
struct RepoGuard {
    slug: String,
}

impl Drop for RepoGuard {
    fn drop(&mut self) {
        eprintln!("e2e: deleting ephemeral repo {}", self.slug);
        let status = Command::new("gh")
            .args(["repo", "delete", &self.slug, "--yes"])
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            eprintln!(
                "e2e: WARNING: failed to delete {} - delete it by hand",
                self.slug
            );
        }
    }
}

fn clone(slug: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("git-stk-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    sh("gh", &["repo", "clone", slug, &dir.to_string_lossy()], None)?;
    Ok(dir)
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("missing required env var {key}"))
}

fn write(dir: &Path, file: &str, contents: &str) -> Result<(), String> {
    std::fs::write(dir.join(file), contents).map_err(|e| format!("write {file}: {e}"))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    sh("git", args, Some(dir))
}

fn stk(dir: &Path, args: &[&str]) -> Result<String, String> {
    let bin = env("GIT_STK_BIN");
    sh(&bin, args, Some(dir))
}

/// Run a command, echoing it; fail with stderr on a non-zero exit.
fn sh(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    eprintln!("e2e: $ {program} {}", args.join(" "));
    let mut command = Command::new(program);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let output = command
        .output()
        .map_err(|e| format!("failed to spawn {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} exited {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
