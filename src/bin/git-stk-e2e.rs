//! Live end-to-end smoke test against a real review provider.
//!
//! Creates an ephemeral repo on the provider, runs the stacked-branch lifecycle
//! with the built `git-stk`, and deletes the repo on the way out (even on
//! failure, via a drop guard). Gated behind the `e2e` feature so it never
//! ships; invoked by `.github/workflows/e2e.yml`.
//!
//! Scenarios, all on one ephemeral repo, GitHub / GitLab / Gitea:
//!   1. core lifecycle   - build a stack -> submit -> restack -> squash-merge
//!   2. issue auto-close - a branch that references an issue closes it on merge
//!   3. metadata surgery - adopt, repair, rename
//!   4. conflict recovery - an interrupted restack: abort, then continue
//!   5. undo             - reverse the last stack-rewriting command
//!   6. split            - explode a branch into one stacked branch per commit
//!
//! Env:
//!   STK_E2E_PROVIDER    `github`, `gitlab`, or `gitea`
//!   STK_E2E_OWNER       owner/namespace for the ephemeral repo
//!   STK_E2E_WAIT        `true` to use `merge --all --wait`; otherwise `--no-wait`
//!   GIT_STK_BIN         path to the `git-stk` binary under test
//!   GH_TOKEN            gh auth (github), with `repo` + `delete_repo`
//!   GITLAB_TOKEN        glab auth (gitlab), with `api`
//!   STK_E2E_GITEA_URL   gitea instance base URL (gitea)
//!   STK_E2E_GITEA_LOGIN tea login name for the instance (gitea)
//!   GITEA_TOKEN         token for the embedded git-credential push URL (gitea)
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
    Gitea,
}

impl Provider {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "github" => Ok(Self::Github),
            "gitlab" => Ok(Self::Gitlab),
            "gitea" => Ok(Self::Gitea),
            other => Err(format!("unknown STK_E2E_PROVIDER {other:?}")),
        }
    }

    /// The provider CLI binary (repo create/delete, issue/review queries).
    fn cli(self) -> &'static str {
        match self {
            Self::Github => "gh",
            Self::Gitlab => "glab",
            Self::Gitea => "tea",
        }
    }
}

/// The Gitea instance base URL (e.g. `http://localhost:3000`) and the `tea`
/// login name to drive it; both come from the workflow's Docker service.
fn gitea_base() -> String {
    env("STK_E2E_GITEA_URL").trim_end_matches('/').to_owned()
}
fn gitea_login() -> String {
    env("STK_E2E_GITEA_LOGIN")
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
    // Non-interactive editor: `git stk continue` passes through to
    // `git rebase --continue`, which would otherwise open an editor for the
    // commit message and hang on a runner with no TTY.
    git(work, &["config", "core.editor", "true"])?;
    // The ephemeral Gitea instance isn't gitea.com/codeberg.org, so widen
    // detection to its host - which also exercises the self-hosted path live.
    if let Provider::Gitea = provider {
        let base = gitea_base();
        let host = base
            .split_once("://")
            .map_or(base.as_str(), |(_, host)| host);
        git(work, &["config", "stk.giteaHost", host])?;
    }

    core_lifecycle(provider, &slug, work)?;
    issue_autoclose(provider, &slug, work)?;
    metadata_surgery(work)?;
    conflict_recovery(work)?;
    undo_check(work)?;
    split_check(work)?;
    if let Provider::Github = provider {
        github_native_stack(provider, &slug, work)?;
    }

    Ok(())
}

/// GitHub's native stacked pull requests, end to end: register a stack on
/// submit, merge it through the asynchronous endpoint GitHub requires for a
/// stacked review, and dissolve one.
///
/// GitHub-only, and skipped when the repo does not have the feature: it is in
/// public preview, so a runner whose account lacks it must not fail the suite.
/// Everything here is what `FakeProvider` structurally cannot check - that the
/// real API accepts these calls and answers in the shapes we parse.
fn github_native_stack(provider: Provider, slug: &str, work: &Path) -> Result<(), String> {
    if !stacks_enabled(slug)? {
        eprintln!("skipping native stacks: not enabled for {slug}");
        return Ok(());
    }
    git(work, &["switch", "main"])?;
    git(work, &["pull", "--ff-only"])?;
    git(work, &["config", "stk.githubStacks", "true"])?;

    stk(work, &["new", "ns/one"])?;
    commit(work, "ns-one.txt", "one\n", "ns one")?;
    stk(work, &["new", "ns/two"])?;
    commit(work, "ns-two.txt", "two\n", "ns two")?;

    // Registering: the reviews must come back as one stack, bottom first.
    let submitted = stk(work, &["submit", "--stack", "--push"])?;
    if !submitted.contains("as a stack") {
        return Err(format!("submit did not register a stack:\n{submitted}"));
    }
    wait_for_review_count(provider, slug, 2)?;
    let stack = gh_json(&["api", &format!("repos/{slug}/stacks")])?;
    let layers = stack.split("\"number\":").count().saturating_sub(1);
    if layers < 3 {
        return Err(format!("expected a stack of two reviews, got:\n{stack}"));
    }

    // Dissolving, then registering again - the one-way door this closes.
    stk(work, &["unstack"])?;
    let after = gh_json(&["api", &format!("repos/{slug}/stacks")])?;
    if after.contains("\"open\":true") {
        return Err(format!("stack still open after unstack:\n{after}"));
    }
    stk(work, &["submit", "--stack"])?;

    // Merging: `gh pr merge` is refused for a stacked review, so this only
    // passes if the asynchronous endpoint is being used.
    stk(work, &["merge", "--all", "--no-wait", "--yes"])?;
    wait_for_review_count(provider, slug, 0)?;
    git(work, &["switch", "main"])?;
    git(work, &["pull", "--ff-only"])?;
    for file in ["ns-one.txt", "ns-two.txt"] {
        if !work.join(file).exists() {
            return Err(format!("{file} missing on main after the stack landed"));
        }
    }
    Ok(())
}

/// Whether the repo has stacked pull requests. The endpoint answers 200 with
/// an empty list when it does, and 404 when it does not.
fn stacks_enabled(slug: &str) -> Result<bool, String> {
    Ok(gh_json(&["api", &format!("repos/{slug}/stacks")]).is_ok())
}

fn gh_json(args: &[&str]) -> Result<String, String> {
    sh("gh", args, None)
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
    wait_for_review_count(provider, slug, 2)?;

    // --title retitles an existing review through each host's own CLI flag,
    // which only a live run exercises.
    stk(work, &["submit", "feat/a", "--title", "e2e retitled"])?;
    wait_for_review_title(provider, slug, "feat/a", "e2e retitled")?;

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

    wait_for_review_count(provider, slug, 0)?;
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

    // The merged "Closes #N" closes the issue, but GitLab does it in an async
    // background job that on gitlab.com routinely lags well past 10s under load
    // (GitHub closes near-synchronously). Poll up to ~30s; the loop returns the
    // instant it reads `closed`, so the longer ceiling is free for the fast
    // providers and only the GitLab tail pays for it.
    for attempt in 0..15 {
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

/// An interrupted restack: a parent edit and a child edit to the same line
/// force a conflict; `abort` must unwind it cleanly, and `continue` must finish
/// it after the conflict is resolved. Local-only (no provider).
fn conflict_recovery(work: &Path) -> Result<(), String> {
    git(work, &["switch", "main"])?;
    stk(work, &["new", "conflict/a"])?;
    commit(work, "shared.txt", "original\n", "a base")?;
    stk(work, &["new", "conflict/b"])?;
    commit(work, "shared.txt", "child-change\n", "b change")?;
    // Edit the same line on the parent so replaying the child conflicts.
    stk(work, &["bottom"])?;
    write(work, "shared.txt", "parent-change\n")?;
    git(work, &["commit", "-am", "a change"])?;

    // abort: the restack conflicts, abort restores a clean tree.
    expect_conflict(stk(work, &["restack", "--no-push"]))?;
    stk(work, &["abort"])?;
    let dirty = git(work, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err(format!("working tree not clean after abort:\n{dirty}"));
    }

    // continue: re-conflict, resolve, then continue finishes the restack.
    expect_conflict(stk(work, &["restack", "--no-push"]))?;
    write(work, "shared.txt", "resolved\n")?;
    git(work, &["add", "shared.txt"])?;
    stk(work, &["continue"])?;
    // conflict/b now sits directly on conflict/a's tip.
    let child_parent = git(work, &["rev-parse", "conflict/b~1"])?;
    let base = git(work, &["rev-parse", "conflict/a"])?;
    if child_parent != base {
        return Err(format!(
            "conflict/b not rebased onto conflict/a after continue ({child_parent} vs {base})"
        ));
    }
    Ok(())
}

/// `undo` reverses the last stack-rewriting command, restoring branch tips and
/// metadata. Here: a restack moves the child's tip, and undo restores it.
/// Local-only.
fn undo_check(work: &Path) -> Result<(), String> {
    git(work, &["switch", "main"])?;
    stk(work, &["new", "undo/a"])?;
    commit(work, "undo-a.txt", "a\n", "undo a")?;
    stk(work, &["new", "undo/b"])?;
    commit(work, "undo-b.txt", "b\n", "undo b")?;
    let before = git(work, &["rev-parse", "undo/b"])?;

    // A rewrite that moves undo/b's tip: commit on the parent, then restack.
    stk(work, &["bottom"])?;
    write(work, "undo-a.txt", "a\nmore\n")?;
    git(work, &["commit", "-am", "more a"])?;
    stk(work, &["restack", "--no-push"])?;
    let after = git(work, &["rev-parse", "undo/b"])?;
    if after == before {
        return Err("restack did not move undo/b, so there is nothing to undo".to_owned());
    }

    stk(work, &["undo"])?;
    let restored = git(work, &["rev-parse", "undo/b"])?;
    if restored != before {
        return Err(format!(
            "undo did not restore undo/b ({restored} vs {before})"
        ));
    }
    Ok(())
}

/// `split --per-commit` explodes a branch into one branch per commit, reusing
/// the original as the leaf. Here: a 2-commit branch becomes one new branch
/// beneath it. Local-only (no provider).
fn split_check(work: &Path) -> Result<(), String> {
    git(work, &["switch", "main"])?;
    stk(work, &["new", "split/work"])?;
    commit(work, "s1.txt", "1\n", "split one")?;
    commit(work, "s2.txt", "2\n", "split two")?;

    stk(work, &["split", "--per-commit"])?;

    // The bottom commit became `split-one` (slugged from its subject); the
    // original `split/work` stays the leaf, reparented onto it.
    let parent = git(work, &["config", "--get", "branch.split-one.stkParent"])?;
    if parent != "main" {
        return Err(format!("split-one parent is {parent:?}, expected main"));
    }
    let leaf_parent = git(work, &["config", "--get", "branch.split/work.stkParent"])?;
    if leaf_parent != "split-one" {
        return Err(format!(
            "split/work parent is {leaf_parent:?}, expected split-one"
        ));
    }
    // split-one points at the first commit (split/work's grandparent commit).
    if git(work, &["rev-parse", "split-one"])? != git(work, &["rev-parse", "split/work~1"])? {
        return Err("split-one does not point at split/work's first commit".to_owned());
    }
    Ok(())
}

/// Assert a restack stopped on a conflict (non-zero exit) rather than succeeding.
fn expect_conflict(result: Result<String, String>) -> Result<(), String> {
    match result {
        Err(_) => Ok(()),
        Ok(output) => Err(format!(
            "expected a restack conflict, but it succeeded: {output}"
        )),
    }
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
        // Create under the token user via the API; auto_init seeds main.
        Provider::Gitea => {
            let name = slug.split('/').next_back().unwrap_or_default();
            let body = format!(
                r#"{{"name":"{name}","private":true,"auto_init":true,"default_branch":"main"}}"#
            );
            sh(
                "tea",
                &[
                    "api",
                    "--login",
                    &gitea_login(),
                    "-X",
                    "POST",
                    "user/repos",
                    "-d",
                    &body,
                ],
                None,
            )
        }
    }
    .map(|_| ())
}

fn clone(provider: Provider, slug: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("git-stk-e2e-{}", std::process::id()));
    let url = match provider {
        Provider::Github => format!("https://github.com/{slug}.git"),
        Provider::Gitlab => format!("https://gitlab.com/{slug}.git"),
        // A clean URL (no token): the workflow wires a git credential store for
        // pushes, and `tea` reads the repo from this remote's host - which it
        // can't do if the token is embedded in the URL.
        Provider::Gitea => format!("{}/{slug}.git", gitea_base()),
    };
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

/// Poll the open-review count until it matches `want`. `submit`/`merge` then
/// immediately listing can race the provider's indexing (eventual consistency);
/// without this, a transient mismatch fails the run - and the suite now gates
/// releases, so a flake here blocks a release.
fn wait_for_review_count(provider: Provider, slug: &str, want: usize) -> Result<(), String> {
    let mut last = None;
    for attempt in 0..6 {
        if attempt > 0 {
            sleep(Duration::from_secs(2));
        }
        let count = open_review_count(provider, slug)?;
        if count == want {
            return Ok(());
        }
        last = Some(count);
    }
    Err(format!(
        "expected {want} open reviews, saw {} after retries",
        last.unwrap_or(want)
    ))
}

/// Poll until the branch's open review carries `want` as its title, so the
/// provider's indexing lag after a retitle cannot flake the run.
fn wait_for_review_title(
    provider: Provider,
    slug: &str,
    branch: &str,
    want: &str,
) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..6 {
        if attempt > 0 {
            sleep(Duration::from_secs(2));
        }
        last = review_title(provider, slug, branch)?;
        if last == want {
            return Ok(());
        }
    }
    Err(format!(
        "expected the review for {branch} to be titled {want:?}, saw {last:?}"
    ))
}

/// The title of `branch`'s open review, or an empty string when it has none.
fn review_title(provider: Provider, slug: &str, branch: &str) -> Result<String, String> {
    let (output, branch_field) = match provider {
        Provider::Github => (
            sh(
                "gh",
                &[
                    "pr",
                    "list",
                    "--repo",
                    slug,
                    "--state",
                    "open",
                    "--json",
                    "title,headRefName",
                ],
                None,
            )?,
            "headRefName",
        ),
        Provider::Gitlab => (
            sh(
                "glab",
                &["mr", "list", "-R", slug, "--output", "json"],
                None,
            )?,
            "source_branch",
        ),
        // Gitea nests the head ref, so it is read separately below.
        Provider::Gitea => (
            sh(
                "tea",
                &[
                    "api",
                    "--login",
                    &gitea_login(),
                    &format!("repos/{slug}/pulls?state=open&limit=50"),
                ],
                None,
            )?,
            "head",
        ),
    };
    let value: serde_json::Value =
        serde_json::from_str(&output).map_err(|e| format!("parse review list: {e}: {output}"))?;
    let head = |review: &serde_json::Value| -> String {
        match provider {
            Provider::Gitea => review["head"]["ref"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            _ => review[branch_field].as_str().unwrap_or_default().to_owned(),
        }
    };
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .find(|review| head(review) == branch)
        .and_then(|review| review["title"].as_str())
        .unwrap_or_default()
        .to_owned())
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
        Provider::Gitea => sh(
            "tea",
            &[
                "api",
                "--login",
                &gitea_login(),
                &format!("repos/{slug}/pulls?state=open&limit=50"),
            ],
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
        // Create via the API and read the number straight from the JSON.
        Provider::Gitea => {
            let body = format!(r#"{{"title":"{title}","body":"e2e"}}"#);
            let output = sh(
                "tea",
                &[
                    "api",
                    "--login",
                    &gitea_login(),
                    "-X",
                    "POST",
                    &format!("repos/{slug}/issues"),
                    "-d",
                    &body,
                ],
                None,
            )?;
            let value: serde_json::Value = serde_json::from_str(&output)
                .map_err(|e| format!("parse issue create: {e}: {output}"))?;
            return value["number"]
                .as_u64()
                .ok_or_else(|| format!("no issue number in create output: {output}"));
        }
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
        Provider::Gitea => sh(
            "tea",
            &[
                "api",
                "--login",
                &gitea_login(),
                &format!("repos/{slug}/issues/{number}"),
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
        let status = match self.provider {
            // `--yes` works for both gh and glab repo delete. (GitLab schedules
            // delayed deletion rather than removing immediately - that's its
            // default, and the unique names mean it never blocks a later run.)
            Provider::Github | Provider::Gitlab => Command::new(self.provider.cli())
                .args(["repo", "delete", &self.slug, "--yes"])
                .status(),
            // tea has no scripted repo-delete; DELETE via the API.
            Provider::Gitea => Command::new("tea")
                .args([
                    "api",
                    "--login",
                    &gitea_login(),
                    "-X",
                    "DELETE",
                    &format!("repos/{}", self.slug),
                ])
                .status(),
        };
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
