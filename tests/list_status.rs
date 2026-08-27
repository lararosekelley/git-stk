mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

/// A two-branch GitHub stack (feature/a <- feature/b) with the provider
/// configured. Returns the repo positioned on the top branch.
fn two_branch_stack() -> TestRepo {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo
}

/// A batched-annotation GraphQL response for feature/a (#9) and feature/b (#10)
/// with the given per-branch rollup state, queue entry, and reviews. `list`
/// reads only what it needs, so the same body serves the with/without-reviews
/// cases.
fn graphql_batch(
    a_state: &str,
    a_queue: &str,
    a_reviews: &str,
    b_state: &str,
    b_queue: &str,
    b_reviews: &str,
) -> String {
    format!(
        r#"{{"data":{{"repository":{{
        "p0":{{"nodes":[{{"number":9,"headRefName":"feature/a","mergeQueueEntry":{a_queue},
            "latestReviews":{{"nodes":{a_reviews}}},
            "commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"state":"{a_state}"}}}}}}]}}}}]}},
        "p1":{{"nodes":[{{"number":10,"headRefName":"feature/b","mergeQueueEntry":{b_queue},
            "latestReviews":{{"nodes":{b_reviews}}},
            "commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"state":"{b_state}"}}}}}}]}}}}]}}
    }}}}}}"#
    )
}

fn github_fake(repo: &TestRepo, graphql: &str) -> common::FakeProviderEnv {
    FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"owner/repo"}"#)
        .on("api graphql", graphql)
        .fallback("[]")
        .install(repo)
}

#[test]
fn list_shows_a_status_dot_next_to_the_pr_number() {
    let repo = two_branch_stack();
    let graphql = graphql_batch("SUCCESS", "null", "[]", "FAILURE", "null", "[]");
    let fake = github_fake(&repo, &graphql);

    // A green check for the passing PR, a red one for the failing PR - each
    // next to its number.
    repo.stack_faked(&fake)
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("🟢 #9"))
        .stdout(predicates::str::contains("🔴 #10"));
}

#[test]
fn list_reviews_lists_the_review_tallies_under_each_branch() {
    let repo = two_branch_stack();
    let graphql = graphql_batch(
        "SUCCESS",
        "null",
        r#"[{"state":"APPROVED"},{"state":"APPROVED"}]"#,
        "FAILURE",
        "null",
        r#"[{"state":"CHANGES_REQUESTED"},{"state":"COMMENTED"}]"#,
    );
    let fake = github_fake(&repo, &graphql);

    repo.stack_faked(&fake)
        .args(["list", "--reviews"])
        .assert()
        .success()
        .stdout(predicates::str::contains("2 approvals"))
        .stdout(predicates::str::contains("1 requested change"))
        .stdout(predicates::str::contains("1 comment"));
}

#[test]
fn list_reviews_marks_a_pr_with_no_reviews() {
    let repo = two_branch_stack();
    let graphql = graphql_batch("SUCCESS", "null", "[]", "SUCCESS", "null", "[]");
    let fake = github_fake(&repo, &graphql);

    repo.stack_faked(&fake)
        .args(["list", "--reviews"])
        .assert()
        .success()
        .stdout(predicates::str::contains("(no reviews)"));
}

#[test]
fn list_marks_a_pr_that_sits_in_the_merge_queue() {
    let repo = two_branch_stack();
    // feature/b is queued (🕑), feature/a is not.
    let graphql = graphql_batch(
        "SUCCESS",
        "null",
        "[]",
        "SUCCESS",
        r#"{"state":"QUEUED"}"#,
        "[]",
    );
    let fake = github_fake(&repo, &graphql);

    repo.stack_faked(&fake)
        .arg("list")
        .assert()
        .success()
        // Queued -> just the clock, no CI dot alongside it.
        .stdout(predicates::str::contains("🕑 #10"))
        .stdout(predicates::str::contains("🟢 #10").not())
        // The un-queued branch keeps its normal CI dot.
        .stdout(predicates::str::contains("🟢 #9"));
}

#[test]
fn list_local_makes_no_provider_calls() {
    let repo = two_branch_stack();
    // Any provider invocation fails; --local must still succeed by never making
    // one, and shows no review numbers.
    let fake = FakeProvider::new()
        .fallback_fail("no network allowed under --local")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["list", "--local"])
        .assert()
        .success()
        .stdout(predicates::str::contains("feature/a"))
        .stdout(predicates::str::contains("feature/b"))
        .stdout(predicates::str::contains("#").not());
}

#[test]
fn list_reviews_conflicts_with_commits_format_and_local() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();

    let conflicting: [&[&str]; 3] = [
        &["list", "--reviews", "--commits"],
        &["list", "--reviews", "--format", "markdown"],
        &["list", "--reviews", "--local"],
    ];
    for args in conflicting {
        repo.stack().args(args).assert().failure();
    }
}

#[test]
fn status_shows_a_status_dot_for_the_reviews_pr() {
    let repo = two_branch_stack();
    repo.git(["switch", "feature/a"]);
    let fake = FakeProvider::new()
        .on(
            "pr view 9 --json statusCheckRollup",
            r#"{"statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]}"#,
        )
        .on(
            "--head feature/a",
            r##"[{"number":9,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/9","title":"A"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("🟢 #9"));
}

/// The platform's own stack is distinct from the tree git-stk draws, so `list`
/// marks the layers it holds and `status` names it.
#[test]
fn list_and_status_show_a_platform_stack() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let graphql = r##"{"data":{"repository":{"p0":{"nodes":[{"number":12,"headRefName":"feature/a",
        "mergeQueueEntry":null,"stack":{"number":6,"size":3},"stackEntry":{"position":2},
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}]}}}}"##;
    let stacks = r##"[{"number":6,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":11,"head":{"ref":"below"}},
        {"number":12,"head":{"ref":"feature/a"}},
        {"number":13,"head":{"ref":"above"}}]}]"##;
    let fake = FakeProvider::new()
        .on("api graphql", graphql)
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("⛁2/3"));

    repo.stack_faked(&fake)
        .args(["status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("stack: github stack 6 (2 of 3)"));
}

/// A host that rejects the preview fields must say so once. Retrying silently
/// would make it indistinguishable from a repo with no stacks - the `⛁` marker
/// would just never appear, with nothing to explain why.
#[test]
fn list_says_so_when_the_host_rejects_the_stack_fields() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let without = r##"{"data":{"repository":{"p0":{"nodes":[{"number":12,"headRefName":"feature/a",
        "mergeQueueEntry":null,
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}]}}}}"##;
    let fake = FakeProvider::new()
        // The first query carries the stack fields and is rejected; the retry
        // drops them and succeeds.
        .fail_with_stdout(
            "stackEntry",
            "",
            "Field 'stack' doesn't exist on type 'PullRequest'",
        )
        .on("api graphql", without)
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["list"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "did not accept the stacked-pull-request fields",
        ))
        .stdout(predicates::str::contains("⛁").not());
}
