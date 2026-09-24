mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

const OPEN_A: &str = r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##;

fn github_repo_on_a() -> TestRepo {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo
}

#[test]
fn comment_posts_on_the_current_branchs_review() {
    let repo = github_repo_on_a();
    let fake = FakeProvider::new()
        .record(
            "pr comment",
            "comment-args.txt",
            "https://github.com/owner/repo/pull/12#issuecomment-1",
        )
        .on("feature/a", OPEN_A)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["comment", "@claude review"])
        .assert()
        .success()
        .stdout(predicates::str::contains("commented on #12"))
        .stdout(predicates::str::contains("#issuecomment-1"));

    let recorded =
        std::fs::read_to_string(repo.path().join("comment-args.txt")).expect("comment args");
    assert_eq!(recorded.trim(), "pr comment 12 --body @claude review");
}

#[test]
fn comment_targets_the_named_branch() {
    let repo = github_repo_on_a();
    repo.git(["switch", "main"]);
    let fake = FakeProvider::new()
        .record("pr comment", "comment-args.txt", "")
        .on("feature/a", OPEN_A)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["comment", "--branch", "feature/a", "ping"])
        .assert()
        .success()
        .stdout(predicates::str::contains("commented on #12"));

    let recorded =
        std::fs::read_to_string(repo.path().join("comment-args.txt")).expect("comment args");
    assert_eq!(recorded.trim(), "pr comment 12 --body ping");
}

#[test]
fn a_comment_may_start_with_a_hyphen() {
    let repo = github_repo_on_a();
    let fake = FakeProvider::new()
        .record("pr comment", "comment-args.txt", "")
        .on("feature/a", OPEN_A)
        .fallback("[]")
        .install(&repo);

    // A markdown bullet or rule, not a flag.
    repo.stack_faked(&fake)
        .args(["comment", "- fixed the parser"])
        .assert()
        .success()
        .stdout(predicates::str::contains("commented on #12"));

    let recorded =
        std::fs::read_to_string(repo.path().join("comment-args.txt")).expect("comment args");
    assert_eq!(recorded.trim(), "pr comment 12 --body - fixed the parser");
}

#[test]
fn comment_dry_run_posts_nothing() {
    let repo = github_repo_on_a();
    let fake = FakeProvider::new()
        .record("pr comment", "comment-args.txt", "")
        .on("feature/a", OPEN_A)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["comment", "--dry-run", "@claude review"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would comment on #12"))
        .stdout(predicates::str::contains("commented on").not());

    assert!(!repo.path().join("comment-args.txt").exists());
}

#[test]
fn comment_without_a_review_says_to_submit_first() {
    let repo = github_repo_on_a();
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["comment", "@claude review"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "no github review found for feature/a; submit it first",
        ));
}

#[test]
fn an_empty_comment_is_refused_before_any_lookup() {
    let repo = github_repo_on_a();
    let fake = FakeProvider::new()
        .log_all("calls.txt")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["comment", "  "])
        .assert()
        .failure()
        .stderr(predicates::str::contains("the comment is empty"));

    assert!(!repo.path().join("calls.txt").exists());
}

#[test]
fn comment_uses_glab_for_a_gitlab_merge_request() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    let fake = FakeProvider::new()
        .record("mr note", "comment-args.txt", "")
        .fallback(
            r##"[{"iid":34,"state":"opened","target_branch":"main","source_branch":"feature/a","web_url":"https://gitlab.com/owner/repo/-/merge_requests/34"}]"##,
        )
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["comment", "@claude review"])
        .assert()
        .success()
        .stdout(predicates::str::contains("commented on !34"));

    let recorded =
        std::fs::read_to_string(repo.path().join("comment-args.txt")).expect("comment args");
    assert_eq!(recorded.trim(), "mr note 34 --message @claude review");
}

#[test]
fn comment_uses_tea_for_a_gitea_pull_request() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitea"]);
    repo.git([
        "remote",
        "add",
        "origin",
        "https://gitea.com/owner/repo.git",
    ]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    let fake = FakeProvider::new()
        .commands(&["tea"])
        .record(
            "comment 7",
            "comment-args.txt",
            "  --------\n\n  **@stk** wrote on 2026-09-24 13:24:\n\n  @claude review",
        )
        .on(
            "repos/owner/repo/pulls",
            r##"[{"number":7,"state":"open","merged":false,"draft":false,"head":{"ref":"feature/a"},"base":{"ref":"main"},"html_url":"https://gitea.com/owner/repo/pulls/7","title":"a work"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["comment", "@claude review"])
        .assert()
        .success()
        .stdout(predicates::str::contains("commented on #7"))
        .stdout(predicates::str::contains("wrote on").not());

    let recorded =
        std::fs::read_to_string(repo.path().join("comment-args.txt")).expect("comment args");
    assert_eq!(recorded.trim(), "comment 7 -- @claude review");
}

#[test]
fn comment_on_a_demo_review_is_stored_with_it() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().arg("submit").assert().success();

    repo.stack()
        .args(["comment", "@claude review"])
        .assert()
        .success()
        .stdout(predicates::str::contains("commented on #1"));

    let state = repo.git(["rev-parse", "--git-path", "stk-demo-reviews"]);
    let state = std::fs::read_to_string(repo.path().join(state)).expect("demo state");
    let state: serde_json::Value = serde_json::from_str(&state).expect("demo json");
    assert_eq!(
        state["reviews"][0]["comments"],
        serde_json::json!(["@claude review"])
    );
}
