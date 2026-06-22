mod common;

use common::{FakeProvider, TestRepo};

/// A PR list entry shaped like Gitea's API JSON (nested base/head refs).
const OPEN_PR: &str = r##"[{"number":7,"state":"open","merged":false,"head":{"ref":"feature/a"},"base":{"ref":"main"},"html_url":"https://gitea.com/owner/repo/pulls/7","title":"a work"}]"##;

#[test]
fn gitea_submit_creates_a_pr_via_tea() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitea"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);

    let fake = FakeProvider::new()
        .commands(&["tea"])
        .on("pr list", "[]") // review_for_branch: none yet
        .on("pr create", "https://gitea.com/owner/repo/pulls/12")
        .fallback_fail("unexpected tea args")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("submit")
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/b -> feature/a"))
        .stdout(predicates::str::contains(
            "https://gitea.com/owner/repo/pulls/12",
        ));
}

#[test]
fn gitea_status_shows_the_tea_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitea"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // review_for_branch lists all PRs and matches client-side on the head ref.
    let fake = FakeProvider::new()
        .commands(&["tea"])
        .on("pr list", OPEN_PR)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "review: #7 open feature/a -> main",
        ));
}
