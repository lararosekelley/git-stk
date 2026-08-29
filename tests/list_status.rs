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
        .log_all("calls.txt")
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

    std::fs::remove_file(repo.path().join("calls.txt")).ok();
    repo.stack_faked(&fake)
        .args(["status"])
        .assert()
        .success()
        // Position and size from the same source `list` reads, so the two
        // cannot disagree about one stack's size.
        .stdout(predicates::str::contains("stack: github stack 6 (2 of 3)"));

    // And from the query `status` was already making: no paginated walk of a
    // listing that keeps every stack the repo has ever had.
    let calls = std::fs::read_to_string(repo.path().join("calls.txt")).expect("call log");
    assert!(
        !calls.contains("repos/owner/repo/stacks"),
        "status paid a REST listing call for what the annotate query answers:\n{calls}"
    );
}

/// A host that rejects the preview fields is a lasting property of that host,
/// so the notice lives under `--verbose`: silent by default, like the same
/// fact reaching `status` as a 404, but findable by anyone chasing a `⛁` that
/// never appears.
#[test]
fn list_says_so_under_verbose_when_the_host_rejects_the_stack_fields() {
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

    // Silent by default - the fields are missing on every run, so saying so
    // every time would be permanent noise.
    repo.stack_faked(&fake)
        .args(["list"])
        .assert()
        .success()
        .stderr(predicates::str::contains("stacked-pull-request fields").not())
        .stdout(predicates::str::contains("⛁").not());

    // Under `--verbose` it is there, which is what a missing `⛁` means.
    repo.stack_faked(&fake)
        .args(["-v", "list"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "did not accept the stacked-pull-request fields",
        ));
}

/// And a failure that is not the fields must not be blamed on them. Offline,
/// rate-limited, or no `gh` at all: dropping the fields does not help, so the
/// warning would be a confident wrong diagnosis - and `list` still prints the
/// tree without annotations, as it promises to.
#[test]
fn list_stays_quiet_when_the_provider_fails_for_another_reason() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        // Both attempts fail, with and without the fields.
        .fail("api graphql", "error connecting to api.github.com")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("feature/a"))
        .stderr(predicates::str::contains("stacked-pull-request fields").not());
}

/// `status` asks about one branch, so it must not pay for a listing of every
/// open review. Only GitHub can fold the answer into a batched query; every
/// other provider keeps the per-call path, and `mr list` here would be a walk
/// of the whole project for two facts about one branch.
#[test]
fn status_does_not_list_every_review_on_gitlab() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .log_all("calls.txt")
        .fallback(
            r##"[{"iid":34,"state":"opened","target_branch":"main","source_branch":"feature/a","web_url":"https://gitlab.com/o/r/-/merge_requests/34"}]"##,
        )
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("review: !34 open"));

    let calls = std::fs::read_to_string(repo.path().join("calls.txt")).expect("call log");
    assert!(
        !calls.lines().any(|line| line.contains("mr list")
            && !line.contains("--source-branch")
            && !line.contains("--branch")),
        "status listed every open review to annotate one branch:\n{calls}"
    );
}

/// A merged review still names the stack it landed in - the platform keeps a
/// landed layer listed - and that answer comes from REST, since the annotate
/// query asks `states: OPEN` and cannot match. It must not spend the GraphQL
/// call discovering that.
#[test]
fn status_names_the_stack_of_a_merged_review_without_the_open_query() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":6,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":11,"state":"open","head":{"ref":"below"}},
        {"number":12,"state":"closed","head":{"ref":"feature/a"}},
        {"number":13,"state":"open","head":{"ref":"above"}}]}]"##;
    let fake = FakeProvider::new()
        .log_all("calls.txt")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/a --state merged", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("feature/a", "[]")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("stack: github stack 6 (2 of 3)"));

    let calls = std::fs::read_to_string(repo.path().join("calls.txt")).expect("call log");
    assert!(
        !calls.contains("api graphql"),
        "the annotate query asks states:OPEN and cannot match a merged review:\n{calls}"
    );
}
