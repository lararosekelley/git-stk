mod common;

use common::{FakeProvider, TestRepo};

#[test]
fn list_markdown_prints_summary_and_ordered_pr_list() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new()
        .on(
            "feature/a --state merged",
            r##"[{"number":9,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/9","title":"Bottom change"}]"##,
        )
        .on("feature/a", "[]")
        .on(
            "feature/b",
            r##"[{"number":10,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/10","title":"Top change"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    // Run from the BOTTOM branch: the root walk must find the whole stack.
    repo.git(["switch", "feature/a"]);
    repo.stack_faked(&fake)
        .args(["list", "--format", "markdown"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "2 PRs, base `main`, 1 open / 1 merged",
        ))
        .stdout(predicates::str::contains(
            "1. [Bottom change (#9)](https://github.com/owner/repo/pull/9) - merged",
        ))
        .stdout(predicates::str::contains(
            "2. [Top change (#10)](https://github.com/owner/repo/pull/10) - open",
        ));
}

#[test]
fn list_annotates_gitlab_mr_numbers_without_the_deprecated_opened_flag() {
    // Regression: open_reviews must not pass glab's deprecated --opened flag.
    // glab prints the deprecation notice to stdout, which corrupts the JSON we
    // parse, so the MR numbers silently vanished from `git stk list`.
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .log_all("glab.log")
        .on(
            "mr list",
            r##"[{"iid":34,"state":"opened","target_branch":"main","source_branch":"feature/a","web_url":"https://gitlab.com/owner/repo/-/merge_requests/34","title":"A work"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("!34"));

    let log = std::fs::read_to_string(repo.path().join("glab.log")).expect("glab log");
    assert!(
        log.contains("mr list"),
        "open_reviews should list MRs: {log}"
    );
    assert!(
        !log.contains("--opened"),
        "must not pass deprecated --opened: {log}"
    );
}

#[test]
fn list_markdown_degrades_to_branch_names_without_provider() {
    let repo = TestRepo::new();
    // No remote, no provider config: lookups are impossible.
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    repo.stack()
        .args(["list", "--format", "markdown"])
        .assert()
        .success()
        .stdout(predicates::str::contains("2 branches, base `main`"))
        .stdout(predicates::str::contains("1. `feature/a` (no review)"))
        .stdout(predicates::str::contains("2. `feature/b` (no review)"));
}

#[test]
fn list_markdown_reports_empty_stack() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["list", "--format", "markdown"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no stacked branches"));
}
