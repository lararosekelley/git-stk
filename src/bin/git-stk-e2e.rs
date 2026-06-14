//! Live end-to-end smoke test against a real review provider.
//!
//! Creates an ephemeral repo on the provider, runs the stacked-branch lifecycle
//! with the built `git-stk`, and deletes the repo on the way out (even on
//! failure, via a drop guard). Gated behind the `e2e` feature so it never
//! ships; invoked by `.github/workflows/e2e.yml`.
//!
//! Scenarios, all on one ephemeral repo, GitHub and GitLab:
//!   1. core lifecycle  - build a stack -> submit -> restack -> squash-merge
//!   2. issue auto-close - a branch that references an issue closes it on merge
//!   3. metadata surgery - adopt, repair, rename
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

    /// The provider CLI binary (repo create/delete, issue/review queries).
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

    core_lifecycle(provider, &slug, work)?;
    issue_autoclose(provider, &slug, work)?;
    metadata_surgery(work)?;

    Ok(())
}

/// Build a two-branch stack, submit, amend + restack, then squash-merge the
/// whole thing. The merge is the path that squash-broke before: feat/b must
/// rebase onto main dropping feat/a's squashed commit, with no conflict.
fn core_lifecycle(provider: Provider, slug: &str, work: &Path) -> Result<(), String> {
    stk(work, &["new", "feat/a"])?;
    commit(work, "a.txt", "a\n", "a work")?;
    stk(work, &["new", "feat/b"])?;
    commit(work, "b.txt", "b\n", "b work")?;

    stk(work, &["submit", "--stack", "--push"])?;
    let open = open_review_count(provider, slug)?;
    if open != 2 {
        return Err(format!("expected 2 open reviews after submit, saw {open}"));
    }

    stk(work, &["bottom"])?;
    write(work, "a.txt", "a\na2\n")?;
    git(work, &["commit", "-am", "a work edit"])?;
    stk(work, &["restack", "--push"])?;

    // The ephemeral repo has no required checks; --wait (one cell) clears via
    // the grace window, --no-wait (the rest) merges straight through.
    let merge_wait = if std::env::var("STK_E2E_WAIT").as_deref() == Ok("true") {
        "--wait"
    } else {
        "--no-wait"
    };
    stk(work, &["merge", "--all", merge_wait, "--yes"])?;

    let after = open_review_count(provider, slug)?;
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

/// A branch whose name references an issue gets a `Closes #N` line in its
/// review, so merging it closes the issue on the provider.
fn issue_autoclose(provider: Provider, slug: &str, work: &Path) -> Result<(), String> {
    let number = create_issue(provider, slug, "e2e auto-close")?;
    let branch = format!("{number}-fix");

    stk(work, &["new", &branch])?;
    commit(work, "fix.txt", "fix\n", &format!("fix #{number}"))?;
    stk(work, &["submit", "--push"])?;
    stk(work, &["merge", "-y"])?;

    // The merged "Closes #N" closes the issue; allow a brief lag.
    for attempt in 0..5 {
        if attempt > 0 {
            sleep(Duration::from_secs(2));
        }
        if issue_state(provider, slug, number)? == "closed" {
            return Ok(());
        }
    }
    Err(format!("issue #{number} did not close after merge"))
}

/// The local-metadata paths: adopt a branch made outside git-stk, rebuild lost
/// metadata with repair, then rename a branch and reconcile its review.
fn metadata_surgery(work: &Path) -> Result<(), String> {
    // adopt: attach a branch created outside git-stk onto the trunk.
    git(work, &["switch", "-c", "loose", "main"])?;
    commit(work, "loose.txt", "loose\n", "loose work")?;
    stk(work, &["adopt"])?;
    expect_parent(work, "loose", "main")?;
    stk(work, &["submit", "--push"])?; // a review for repair to read

    // repair: rebuild the parent after the metadata is lost.
    git(work, &["config", "--unset", "branch.loose.stkParent"])?;
    stk(work, &["repair"])?;
    expect_parent(work, "loose", "main")?;

    // rename: retarget the child and reconcile the review on the next submit.
    stk(work, &["new", "child"])?;
    commit(work, "child.txt", "child\n", "child work")?;
    stk(work, &["submit", "--stack", "--push"])?;
    stk(work, &["rename", "loose", "relabeled"])?;
    expect_parent(work, "child", "relabeled")?;
    expect_parent(work, "relabeled", "main")?;
    // Reconciling submit: closes the stale review on the old name and opens a
    // fresh one for relabeled (exercises the close/create paths; a captured
    // stdin auto-confirms the close prompt). Success is the assertion.
    stk(work, &["submit", "--stack", "--push"])?;
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

fn create_issue(provider: Provider, slug: &str, title: &str) -> Result<u64, String> {
    let output = match provider {
        Provider::Github => sh(
            "gh",
            &[
                "issue", "create", "--repo", slug, "--title", title, "--body", "e2e",
            ],
            None,
        )?,
        Provider::Gitlab => sh(
            "glab",
            &[
                "issue",
                "create",
                "-R",
                slug,
                "--title",
                title,
                "--description",
                "e2e",
                "--yes",
            ],
            None,
        )?,
    };
    // Both providers print the issue URL; the number is the last path segment.
    // (GitHub uses .../issues/N; GitLab now uses .../-/work_items/N.)
    let segment = output.rsplit('/').next().unwrap_or_default();
    let digits: String = segment.chars().take_while(char::is_ascii_digit).collect();
    digits
        .parse()
        .map_err(|_| format!("no issue number in create output: {output}"))
}

/// The issue's state via the provider API: `closed` on both (GitLab open is
/// `opened`, but closed is `closed` on both, which is all we check).
fn issue_state(provider: Provider, slug: &str, number: u64) -> Result<String, String> {
    let output = match provider {
        Provider::Github => sh(
            "gh",
            &["api", &format!("repos/{slug}/issues/{number}")],
            None,
        )?,
        Provider::Gitlab => sh(
            "glab",
            &[
                "api",
                &format!("projects/{}/issues/{number}", slug.replace('/', "%2F")),
            ],
            None,
        )?,
    };
    let value: serde_json::Value =
        serde_json::from_str(&output).map_err(|e| format!("parse issue: {e}: {output}"))?;
    Ok(value["state"].as_str().unwrap_or_default().to_owned())
}

fn expect_parent(work: &Path, branch: &str, want: &str) -> Result<(), String> {
    let got = git(
        work,
        &["config", "--get", &format!("branch.{branch}.stkParent")],
    )?;
    if got != want {
        return Err(format!("{branch} parent is {got:?}, expected {want:?}"));
    }
    Ok(())
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
        // `--yes` works for both gh and glab repo delete. (GitLab schedules
        // delayed deletion rather than removing immediately - that's its
        // default, and the unique names mean it never blocks a later run.)
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

/// Write a file, stage everything, and commit.
fn commit(dir: &Path, file: &str, contents: &str, message: &str) -> Result<(), String> {
    write(dir, file, contents)?;
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-m", message])?;
    Ok(())
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
