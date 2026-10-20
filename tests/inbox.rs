mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

/// A GitHub search answer holding `reviews`, of `total` found.
fn search(total: u32, reviews: &[&str]) -> String {
    format!(
        r#"{{"data":{{"viewer":{{"login":"me"}},"search":{{"issueCount":{total},"nodes":[{}]}}}}}}"#,
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

#[test]
fn a_list_github_cannot_answer_says_so_and_the_rest_still_show() {
    let repo = github_repo();
    // No rule answers the team search, so it fails on its own.
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"owner/repo"}"#)
        .on("team-review-requested-user:@me", "not json")
        .on("user-review-requested:@me", &search(0, &[]))
        .on("review:changes_requested", &search(0, &[]))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("inbox")
        .assert()
        .success()
        .stdout(contains("Requested of you (0)"))
        .stdout(contains(
            "Requested of your teams\n  could not list these: failed to parse gh inbox JSON",
        ))
        .stdout(contains("Yours (0)"));
}

#[test]
fn a_gitlab_inbox_says_it_has_no_team_list() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    let mr = r#"{"iid":"8","title":"Speed up sync","webUrl":"https://gitlab.com/o/r/-/merge_requests/8",
        "draft":true,"sourceBranch":"fast","targetBranch":"main","createdAt":"2026-10-01T00:00:00Z",
        "updatedAt":"2026-10-02T00:00:00Z","author":{"username":"ann"},
        "diffStatsSummary":{"additions":30,"deletions":2},"headPipeline":{"status":"FAILED"},"approved":false,
        "resolvableDiscussionsCount":0,"resolvedDiscussionsCount":0,
        "reviewers":{"nodes":[{"username":"me","mergeRequestInteraction":{"reviewState":"UNREVIEWED"}}]}}"#;
    let fake = FakeProvider::new()
        .on("api user", r#"{"username":"me"}"#)
        .on("api projects/:id", r#"{"path_with_namespace":"o/r"}"#)
        .on(
            "api graphql",
            &format!(
                r#"{{"data":{{"project":{{"requested":{{"count":1,"nodes":[{mr}]}},"authored":{{"count":0,"nodes":[]}}}}}}}}"#
            ),
        )
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("inbox")
        .assert()
        .success()
        .stdout(contains("Requested of you (1)\n  🔴 !8 Speed up sync"))
        .stdout(contains("ann, "))
        .stdout(contains("draft"))
        .stdout(contains(
            "Requested of your teams\n  GitLab has no team review requests",
        ))
        .stdout(contains("Yours (0)"));
}

#[test]
fn a_gitea_inbox_reads_requests_teams_and_reviews() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitea"]);
    repo.git([
        "remote",
        "add",
        "origin",
        "https://gitea.example.com/o/r.git",
    ]);
    let pull = |number: u32, author: &str, users: &str, teams: &str| {
        format!(
            r#"{{"number":{number},"title":"Pull {number}","html_url":"https://gitea.example.com/o/r/pulls/{number}",
            "draft":false,"user":{{"login":"{author}"}},"created_at":"2026-10-01T00:00:00+02:00",
            "updated_at":"2026-10-02T00:00:00+02:00","additions":7,"deletions":1,
            "head":{{"ref":"b{number}","sha":"sha{number}"}},"base":{{"ref":"main"}},
            "requested_reviewers":[{users}],"requested_reviewers_teams":[{teams}]}}"#
        )
    };
    let pulls = format!(
        "[{},{},{},{},{}]",
        pull(1, "ann", r#"{"login":"me"}"#, ""),
        pull(2, "bo", "", r#"{"id":7}"#),
        // Gitea leaves the line counts out of its listing.
        pull(3, "me", "", "").replace(r#""additions":7,"deletions":1,"#, ""),
        pull(4, "me", r#"{"login":"ann"}"#, ""),
        pull(5, "me", r#"{"login":"bo"}"#, ""),
    );
    // Pull 5's latest verdict is on the second page of its reviews.
    let approvals = format!(
        "[{}]",
        vec![r#"{"user":{"login":"bo"},"state":"APPROVED","dismissed":false}"#; 50].join(",")
    );
    let fake = FakeProvider::new()
        .commands(&["tea"])
        .on("api user/teams", r#"[{"id":7,"name":"core"}]"#)
        .on("api user", r#"{"login":"me"}"#)
        .on("pulls?state=open", &pulls)
        .on(
            "pulls/4/reviews",
            r#"[{"user":{"login":"ann"},"state":"REQUEST_CHANGES","dismissed":false}]"#,
        )
        .on(
            "pulls/5/reviews?limit=50&page=2",
            r#"[{"user":{"login":"bo"},"state":"REQUEST_CHANGES","dismissed":false}]"#,
        )
        .on("pulls/5/reviews?limit=50&page=1", &approvals)
        .on("/reviews", "[]")
        .on("/comments", "[]")
        .on("api repos/o/r/pulls/3", r#"{"additions":40,"deletions":9}"#)
        .on(
            "commits/sha1/status",
            r#"{"state":"failure","total_count":1}"#,
        )
        .on("/status", r#"{"state":"success","total_count":1}"#)
        .fallback_fail("unexpected provider call")
        .log_all("calls")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("inbox")
        .assert()
        .success()
        .stdout(contains("Requested of you (1)\n  🔴 #1 Pull 1"))
        .stdout(contains("Returned to you (2)"))
        .stdout(contains("#4 Pull 4"))
        .stdout(contains("#5 Pull 5"))
        .stdout(contains("changes requested"))
        .stdout(contains("Requested of your teams (1)\n  🟢 #2 Pull 2"))
        .stdout(contains("Yours (1)\n  🟢 #3 Pull 3  +40/-9"));
    let calls = std::fs::read_to_string(repo.path().join("calls")).unwrap();
    assert_eq!(
        calls.matches("pulls/4/reviews").count(),
        1,
        "returned and mine share one read of your pulls' reviews: {calls}"
    );
}

#[test]
fn inbox_marks_a_review_someone_came_back_to_since_you_looked() {
    let repo = github_repo();
    // You reviewed #5, then ann answered; #6 you have never looked at.
    let answered = pull(5, "Answered", "ann", "SUCCESS", "REVIEW_REQUIRED", 0).replacen(
        r#""reviewThreads""#,
        r#""reviews":{"nodes":[{"author":{"login":"me"},"submittedAt":"2026-10-10T00:00:00Z"}]},
        "comments":{"nodes":[{"author":{"login":"ann"},"createdAt":"2026-10-11T00:00:00Z"}]},
        "reviewThreads""#,
        1,
    );
    let untouched = pull(6, "Untouched", "bo", "SUCCESS", "REVIEW_REQUIRED", 0);
    // Only bots came back to #7 after you, one of them a machine user named in
    // stk.inboxBots.
    repo.git(["config", "stk.inboxBots", "@BuildBot, other"]);
    let botted = pull(7, "Botted", "cy", "SUCCESS", "REVIEW_REQUIRED", 0).replacen(
        r#""reviewThreads""#,
        r#""reviews":{"nodes":[{"author":{"__typename":"User","login":"me"},"submittedAt":"2026-10-10T00:00:00Z"}]},
        "comments":{"nodes":[{"author":{"__typename":"Bot","login":"ci"},"createdAt":"2026-10-11T00:00:00Z"},
            {"author":{"__typename":"User","login":"review[bot]"},"createdAt":"2026-10-11T00:00:00Z"},
            {"author":{"__typename":"User","login":"buildbot"},"createdAt":"2026-10-11T00:00:00Z"}]},
        "reviewThreads""#,
        1,
    );
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"owner/repo"}"#)
        .on(
            "user-review-requested:@me",
            &search(3, &[&answered, &untouched, &botted]),
        )
        .fallback_fail("unexpected provider call")
        .install(&repo);

    let output = repo
        .stack_faked(&fake)
        .args(["inbox", "--filter", "incoming"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let line = |id: &str| {
        text.lines()
            .find(|line| line.contains(id))
            .unwrap()
            .to_owned()
    };
    assert!(line("#5 Answered").contains("new, ann"), "{text}");
    assert!(!line("#6 Untouched").contains("new"), "{text}");
    assert!(!line("#7 Botted").contains("new"), "{text}");
}

#[test]
fn a_tour_counts_as_looking_for_the_inbox() {
    let repo = github_repo();
    // You never commented on #6, but you toured it before bo's comment.
    let commented = pull(6, "Toured", "ann", "SUCCESS", "REVIEW_REQUIRED", 0).replacen(
        r#""reviewThreads""#,
        r#""comments":{"nodes":[{"author":{"__typename":"User","login":"bo"},"createdAt":"2026-10-11T00:00:00Z"}]},
        "reviewThreads""#,
        1,
    );
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"owner/repo"}"#)
        .on("user-review-requested:@me", &search(1, &[&commented]))
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let listed = |repo: &TestRepo| {
        let output = repo
            .stack_faked(&fake)
            .args(["inbox", "--filter", "incoming"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    assert!(!listed(&repo).contains("new"), "never looked at");

    let tours = repo.path().join(".git/stk/tours");
    std::fs::create_dir_all(&tours).unwrap();
    // 2026-10-10T00:00:00Z: toured a day before bo came back.
    std::fs::write(tours.join("seen.json"), r##"{"#6":1791590400}"##).unwrap();
    assert!(listed(&repo).contains("new, ann"), "{}", listed(&repo));

    // 2026-10-12T00:00:00Z: toured since.
    std::fs::write(tours.join("seen.json"), r##"{"#6":1791763200}"##).unwrap();
    assert!(!listed(&repo).contains("new"), "{}", listed(&repo));
}

#[test]
fn outside_lists_yours_beyond_you_and_your_organizations() {
    let repo = github_repo();
    let elsewhere = pull(4, "Fix a typo", "me", "SUCCESS", "REVIEW_REQUIRED", 0).replace(
        r#""isDraft""#,
        r#""repository":{"nameWithOwner":"stranger/lib"},"isDraft""#,
    );
    // This repository, when owned by a stranger, is named like any other.
    let here = pull(8, "Fork work", "me", "SUCCESS", "REVIEW_REQUIRED", 0).replace(
        r#""isDraft""#,
        r#""repository":{"nameWithOwner":"owner/repo"},"isDraft""#,
    );
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"owner/repo"}"#)
        .on(
            "organizations",
            r#"{"data":{"viewer":{"login":"me","organizations":{"nodes":[{"login":"acme"},{"login":"widgets"}]}}}}"#,
        )
        .on(
            "-user:me -org:acme -org:widgets",
            &search(2, &[&elsewhere, &here]),
        )
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["inbox", "--filter", "outside"])
        .assert()
        .success()
        .stdout(contains("Yours outside your organizations (2)"))
        .stdout(contains("stranger/lib#4 Fix a typo"))
        .stdout(contains("owner/repo#8   Fork work"))
        .stdout(contains("me,").not())
        .stdout(contains("hint:").not());
}

#[test]
fn org_lists_yours_across_this_repositorys_owner() {
    let repo = github_repo();
    repo.git(["remote", "add", "origin", "git@github.com:acme/repo.git"]);
    let mine = pull(3, "Shared fix", "me", "SUCCESS", "REVIEW_REQUIRED", 0).replace(
        r#""isDraft""#,
        r#""repository":{"nameWithOwner":"acme/tools"},"isDraft""#,
    );
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"acme/repo"}"#)
        .on(
            "repositoryOwner",
            r#"{"data":{"repositoryOwner":{"__typename":"Organization"}}}"#,
        )
        .on("author:@me org:acme", &search(1, &[&mine]))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["inbox", "--filter", "org"])
        .assert()
        .success()
        .stdout(contains("Yours in acme (1)"))
        .stdout(contains("acme/tools#3 Shared fix"))
        .stdout(contains("Requested of you").not());
}

#[test]
fn org_names_a_user_or_organization_to_search_instead() {
    let repo = github_repo();
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"acme/repo"}"#)
        .on(
            "repositoryOwner",
            r#"{"data":{"repositoryOwner":{"__typename":"User"}}}"#,
        )
        .on("author:@me user:lararosekelley", &search(0, &[]))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["inbox", "--org", "lararosekelley"])
        .assert()
        .success()
        .stdout(contains("Yours in lararosekelley (0)"))
        .stdout(contains("Requested of you").not());

    let missing = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"acme/repo"}"#)
        .on("repositoryOwner", r#"{"data":{"repositoryOwner":null}}"#)
        .fallback_fail("unexpected provider call")
        .install(&repo);
    repo.stack_faked(&missing)
        .args(["inbox", "--org", "nobody-here"])
        .assert()
        .success()
        .stdout(contains(
            "could not list these: GitHub has no organization or user named nobody-here",
        ));
}

#[test]
fn org_needs_no_remote_where_the_forge_has_no_such_list() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);

    repo.stack()
        .args(["inbox", "--filter", "org"])
        .assert()
        .success()
        .stdout(contains("Yours in the organization (0)"));
}
