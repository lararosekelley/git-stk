//! Live end-to-end smoke test against a real review provider.
//!
//! Creates an ephemeral repo on the provider, runs the core stacked-branch
//! lifecycle with the built `git-stk`, and deletes the repo on the way out
//! (even on failure, via a drop guard). Gated behind the `e2e` feature so it
//! never ships; invoked by `.github/workflows/e2e.yml`.
//!
//! Covers the core lifecycle (build a stack -> submit -> restack ->
//! squash-merge -> sync) on GitHub and GitLab. Deeper paths (issue auto-close,
//! adopt/repair/rename) are TODO.
//!
//! Env:
//!   STK_E2E_PROVIDER  `github` or `gitlab`
//!   STK_E2E_OWNER     owner/namespace for the ephemeral repo
//!   STK_E2E_WAIT      `true` to use `merge --all --wait`; otherwise `--no-wait`
//!   GIT_STK_BIN       path to the `git-stk` binary under test
//!   GH_TOKEN          gh auth (github), with `repo` + `delete_repo`
//!   GITLAB_TOKEN      glab auth (gitlab), with `api`
//!
//! Push auth (gh credential helper / git credential store) is wired by the
//! workflow before this runs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn main() {
    match run() {
        Ok(()) => println!("e2e: PASSED"),
        Err(error) => {
            eprintln!("e2e: FAILED: {error}");
            std::process::exit(1);
        }
    }
}

#[derive(Clone, Copy)]
enum Provider {
    Github,
    Gitlab,
}

impl Provider {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "github" => Ok(Self::Github),
            "gitlab" => Ok(Self::Gitlab),
            other => Err(format!("unknown STK_E2E_PROVIDER {other:?}")),
        }
    }

    /// The provider CLI binary (used for repo create/delete and review listing).
    fn cli(self) -> &'static str {
        match self {
            Self::Github => "gh",
            Self::Gitlab => "glab",
        }
    }

    fn host(self) -> &'static str {
        match self {
            Self::Github => "github.com",
            Self::Gitlab => "gitlab.com",
        }
    }
}

fn run() -> Result<(), String> {
    let provider = Provider::parse(&env("STK_E2E_PROVIDER"))?;
    let owner = env("STK_E2E_OWNER");

    // Unique name without a rand dependency: process id plus a nanosecond stamp.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let slug = format!("{owner}/git-stk-e2e-{}-{stamp}", std::process::id());

    // Create the ephemeral repo first; the guard deletes it no matter how we
    // exit. Both providers seed an initial commit on `main`.
    create_repo(provider, &slug)?;
    let _repo = RepoGuard {
        provider,
        slug: slug.clone(),
    };

    let dir = clone(provider, &slug)?;
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

    // 2. Submit the stack; expect two open reviews, feat/b targeting feat/a.
    stk(work, &["submit", "--stack", "--push"])?;
    let open = open_review_count(provider, &slug)?;
    if open != 2 {
        return Err(format!("expected 2 open reviews after submit, saw {open}"));
    }

    // 3. Amend the bottom and restack; the child must follow.
    stk(work, &["bottom"])?;
    write(work, "a.txt", "a\na2\n")?;
    git(work, &["commit", "-am", "a work edit"])?;
    stk(work, &["restack", "--push"])?;

    // 4. Land the whole stack with the squash strategy. The ephemeral repo has
    //    no required checks; `--wait` (one cell) clears via the grace window,
    //    `--no-wait` (the rest) merges straight through. This is the path that
    //    squash-broke before: feat/b must rebase onto main dropping feat/a's
    //    squashed commit, with no conflict.
    let merge_wait = if std::env::var("STK_E2E_WAIT").as_deref() == Ok("true") {
        "--wait"
    } else {
        "--no-wait"
    };
    stk(work, &["merge", "--all", merge_wait, "--yes"])?;

    // 5. Everything landed: no open reviews, both files on the trunk.
    let after = open_review_count(provider, &slug)?;
    if after != 0 {
        return Err(format!(
            "expected no open reviews after merge --all, saw {after}"
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

fn create_repo(provider: Provider, slug: &str) -> Result<(), String> {
    match provider {
        Provider::Github => sh(
            "gh",
            &["repo", "create", slug, "--private", "--add-readme"],
            None,
        ),
        // --readme + --defaultBranch seed an initial commit on `main`; glab
        // otherwise defaults the branch to `master`.
        Provider::Gitlab => sh(
            "glab",
            &[
                "repo",
                "create",
                slug,
                "--private",
                "--readme",
                "--defaultBranch",
                "main",
            ],
            None,
        ),
    }
    .map(|_| ())
}

fn clone(provider: Provider, slug: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("git-stk-e2e-{}", std::process::id()));
    let url = format!("https://{}/{slug}.git", provider.host());
    // A just-created repo can lag a moment before it is cloneable; retry briefly.
    let mut last = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            sleep(Duration::from_secs(2));
        }
        let _ = std::fs::remove_dir_all(&dir);
        match sh("git", &["clone", &url, &dir.to_string_lossy()], None) {
            Ok(_) => return Ok(dir),
            Err(error) => last = error,
        }
    }
    Err(format!("clone failed after retries: {last}"))
}

/// Number of open reviews on the repo, parsed from the provider's list JSON.
fn open_review_count(provider: Provider, slug: &str) -> Result<usize, String> {
    let output = match provider {
        Provider::Github => sh(
            "gh",
            &[
                "pr", "list", "--repo", slug, "--state", "open", "--json", "number",
            ],
            None,
        )?,
        // glab mr list defaults to open; --output json prints a bare array.
        Provider::Gitlab => sh(
            "glab",
            &["mr", "list", "-R", slug, "--output", "json"],
            None,
        )?,
    };
    let value: serde_json::Value =
        serde_json::from_str(&output).map_err(|e| format!("parse review list: {e}: {output}"))?;
    Ok(value.as_array().map_or(0, Vec::len))
}

/// Deletes the ephemeral repo when dropped, so a panic or early return can't
/// leave it behind. Best-effort: a failed delete only warns.
struct RepoGuard {
    provider: Provider,
    slug: String,
}

impl Drop for RepoGuard {
    fn drop(&mut self) {
        eprintln!("e2e: deleting ephemeral repo {}", self.slug);
        // `--yes` works for both gh and glab repo delete.
        let status = Command::new(self.provider.cli())
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
