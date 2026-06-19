mod common;

use common::{FakeProvider, TestRepo};

/// One PR shaped like Gitea's canonical API object (what `tea api .../pulls`
/// returns: number, nested base/head refs, html_url).
const OPEN_PR: &str = r##"[{"number":7,"state":"open","merged":false,"draft":false,"head":{"ref":"feature/a"},"base":{"ref":"main"},"html_url":"https://gitea.com/owner/repo/pulls/7","title":"a work"}]"##;

/// A repo configured for the Gitea provider, with a remote so the provider can
/// derive the `owner/repo` slug for its `tea api` calls.
fn gitea_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitea"]);
    repo.git([
        "remote",
        "add",
        "origin",
        "https://gitea.com/owner/repo.git",
    ]);
    repo
}

#[test]
fn gitea_submit_creates_a_pr_via_tea() {
    let repo = gitea_repo();
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);

    let fake = FakeProvider::new()
        .commands(&["tea"])
        // review_for_branch reads canonical PR objects via the API passthrough.
        .on("repos/owner/repo/pulls", "[]")
        .on("pr create", "created review")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("submit")
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/b -> feature/a"));
}

#[test]
fn gitea_status_shows_the_tea_review() {
    let repo = gitea_repo();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .commands(&["tea"])
        .on("repos/owner/repo/pulls", OPEN_PR)
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
