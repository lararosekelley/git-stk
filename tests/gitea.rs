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
fn gitea_submit_title_keeps_the_wip_prefix_on_a_draft() {
    let repo = gitea_repo();
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);

    let fake = FakeProvider::new()
        .commands(&["tea"])
        .on(
            "repos/owner/repo/pulls",
            r##"[{"number":7,"state":"open","merged":false,"draft":true,"head":{"ref":"feature/a"},"base":{"ref":"main"},"html_url":"https://gitea.com/owner/repo/pulls/7","title":"WIP: a work"}]"##,
        )
        .record("pr edit 7 --title", "edit-title-7.txt", "")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "-t", "A better title"])
        .assert()
        .success()
        .stdout(predicates::str::contains("set title in #7"));

    // Gitea encodes draft state as the `WIP:` prefix, so a retitle keeps it.
    let args = std::fs::read_to_string(repo.path().join("edit-title-7.txt")).expect("edit args");
    assert!(
        args.contains("--title WIP: A better title"),
        "edit args: {args}"
    );
}

#[test]
fn gitea_submit_draft_title_does_not_double_the_wip_prefix() {
    let repo = gitea_repo();
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);

    let fake = FakeProvider::new()
        .commands(&["tea"])
        .on("repos/owner/repo/pulls", "[]")
        .record("pr create", "create.txt", "created review")
        .fallback("[]")
        .install(&repo);

    // A title that already carries the marker is a draft as far as Gitea is
    // concerned, so the create must not stack a second one - `WIP: WIP: ...`
    // reads badly and leaves a marker `--ready` cannot strip in one pass.
    repo.stack_faked(&fake)
        .args(["submit", "--draft", "-t", "WIP: a work"])
        .assert()
        .success();

    let args = std::fs::read_to_string(repo.path().join("create.txt")).expect("create args");
    assert!(args.contains("--title WIP: a work"), "create args: {args}");
    assert!(!args.contains("WIP: WIP:"), "create args: {args}");
}

#[test]
fn gitea_submit_title_matches_the_wip_prefix_case_insensitively() {
    let repo = gitea_repo();
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);

    let fake = FakeProvider::new()
        .commands(&["tea"])
        .on(
            "repos/owner/repo/pulls",
            r##"[{"number":7,"state":"open","merged":false,"draft":true,"head":{"ref":"feature/a"},"base":{"ref":"main"},"html_url":"https://gitea.com/owner/repo/pulls/7","title":"WIP: a work"}]"##,
        )
        .record("pr edit 7 --title", "edit-title-7.txt", "")
        .fallback("[]")
        .install(&repo);

    // Gitea honors a lowercase marker too, so the retitle leaves it alone
    // rather than prefixing its own.
    repo.stack_faked(&fake)
        .args(["submit", "-t", "wip: a better title"])
        .assert()
        .success()
        .stdout(predicates::str::contains("set title in #7"));

    let args = std::fs::read_to_string(repo.path().join("edit-title-7.txt")).expect("edit args");
    assert!(
        args.contains("--title wip: a better title"),
        "edit args: {args}"
    );
    assert!(!args.contains("WIP: wip:"), "edit args: {args}");
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
