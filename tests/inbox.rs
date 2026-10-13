mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

/// A GitHub search answer holding `reviews`, of `total` found.
fn search(total: u32, reviews: &[&str]) -> String {
    format!(
        r#"{{"data":{{"search":{{"issueCount":{total},"nodes":[{}]}}}}}}"#,
        reviews.join(",")
    )
}

/// A pull request node as the inbox query asks for it.
fn pull(
    number: u32,
    title: &str,
    author: &str,
    rollup: &str,
    decision: &str,
    open_threads: usize,
) -> String {
    let threads = std::iter::repeat_n(r#"{"isResolved":false}"#, open_threads)
        .chain([r#"{"isResolved":true}"#])
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"number":{number},"state":"OPEN","title":"{title}","url":"https://github.com/owner/repo/pull/{number}",
        "isDraft":false,"headRefName":"b{number}","baseRefName":"main","author":{{"login":"{author}"}},
        "createdAt":"2026-10-01T00:00:00Z","updatedAt":"2026-10-12T00:00:00Z","additions":12,"deletions":3,
        "reviewDecision":"{decision}","commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"state":"{rollup}"}}}}}}]}},
        "reviewThreads":{{"nodes":[{threads}]}}}}"#
    )
}

fn github_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo
}

#[test]
fn inbox_lists_what_waits_on_you_first() {
    let repo = github_repo();
    // A rule's needle may be part of a later search, so the narrower comes first.
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"owner/repo"}"#)
        .on(
            "team-review-requested-user:@me",
            &search(
                1,
                &[&pull(7, "Team work", "bo", "PENDING", "REVIEW_REQUIRED", 0)],
            ),
        )
        .on(
            "user-review-requested:@me",
            &search(
                2,
                &[
                    &pull(
                        5,
                        "Fix the parser",
                        "ann",
                        "SUCCESS",
                        "CHANGES_REQUESTED",
                        2,
                    ),
                    &pull(6, "Add docs", "cy", "FAILURE", "REVIEW_REQUIRED", 0),
                ],
            ),
        )
        .on("-review:changes_requested", &search(0, &[]))
        .on(
            "review:changes_requested",
            &search(
                1,
                &[&pull(9, "My fix", "me", "SUCCESS", "CHANGES_REQUESTED", 1)],
            ),
        )
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("inbox")
        .assert()
        .success()
        .stdout(contains("Requested of you (2)"))
        .stdout(contains("#5 Fix the parser"))
        .stdout(contains("ann, "))
        .stdout(contains("changes requested, 2 open threads"))
        .stdout(contains("Returned to you (1)\n  🟢 #9 My fix"))
        .stdout(contains("Requested of your teams (1)"))
        .stdout(contains("Yours (0)\n  nothing here"))
        .stdout(contains(
            "hint: walk through one with `git stk tour <number>`",
        ));

    let out = repo.stack_faked(&fake).arg("inbox").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let order: Vec<usize> = [
        "Requested of you",
        "Returned to you",
        "Requested of your teams",
        "Yours",
    ]
    .iter()
    .map(|heading| text.find(heading).unwrap())
    .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
}

#[test]
fn inbox_asks_only_for_the_lists_filtered_to() {
    let repo = github_repo();
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"owner/repo"}"#)
        .on(
            "-review:changes_requested",
            &search(
                3,
                &[
                    &pull(1, "Small", "me", "SUCCESS", "APPROVED", 0),
                    &pull(2, "Second", "me", "SUCCESS", "REVIEW_REQUIRED", 0),
                    &pull(3, "Third", "me", "SUCCESS", "REVIEW_REQUIRED", 0),
                ],
            ),
        )
        .fallback_fail("unexpected provider call")
        .log_all("calls")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["inbox", "--filter", "mine", "--limit", "2"])
        .assert()
        .success()
        .stdout(contains("Yours (3)"))
        .stdout(contains("approved"))
        .stdout(contains("1 more; raise --limit to see them"))
        .stdout(contains("Requested of you").not())
        .stdout(contains("hint:").not());
    let calls = std::fs::read_to_string(repo.path().join("calls")).unwrap();
    assert_eq!(calls.matches("api graphql").count(), 1, "{calls}");

    repo.stack_faked(&fake)
        .args(["inbox", "--filter", "nonsense"])
        .assert()
        .failure()
        .stderr(contains("invalid value 'nonsense'"));
}

#[test]
fn the_demo_inbox_lists_your_demo_reviews() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack()
        .args(["submit", "--no-push"])
        .assert()
        .success();

    repo.stack()
        .args(["inbox", "--filter", "mine,incoming"])
        .assert()
        .success()
        .stdout(contains("Requested of you (0)"))
        .stdout(contains("Yours (1)"))
        .stdout(contains("#1 add a"));
}
