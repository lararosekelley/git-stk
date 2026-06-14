use std::fs;

mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

#[test]
fn status_prints_local_stack_and_review_state() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);
    repo.git(["switch", "-c", "feature/c"]);
    repo.git(["config", "branch.feature/c.stkParent", "feature/b"]);
    let fake = FakeProvider::new()
        .fallback(
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/lararosekelley/git-stk/pull/13"}]"##,
        )
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains("branch: feature/b"))
        .stdout(predicates::str::contains("parent: feature/a"))
        .stdout(predicates::str::contains("children: feature/c"))
        .stdout(predicates::str::contains("provider: github (config)"))
        .stdout(predicates::str::contains(
            "review: #13 open feature/b -> feature/a",
        ))
        .stdout(predicates::str::contains(
            "url: https://github.com/lararosekelley/git-stk/pull/13",
        ));
}

#[test]
fn status_prints_none_when_review_is_missing() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains("branch: feature/b"))
        .stdout(predicates::str::contains("parent: feature/a"))
        .stdout(predicates::str::contains("children: none"))
        .stdout(predicates::str::contains("review: none"));
}

#[test]
fn status_warns_when_review_base_differs_from_parent() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);
    let fake = FakeProvider::new()
        .fallback(
            r##"[{"iid":34,"state":"opened","target_branch":"main","source_branch":"feature/b","web_url":"https://gitlab.com/lararosekelley/git-stk-mirror/-/merge_requests/34"}]"##,
        )
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "review: !34 open feature/b -> main",
        ))
        .stdout(predicates::str::contains(
            "warning: review base is main, local parent is feature/a - run `git stk submit`",
        ));
}

#[test]
fn status_hints_restack_when_behind_parent() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "feature/a"]);
    repo.commit_file("a.txt", "a\nmore\n", "a moves on");
    let fake = FakeProvider::new()
        .on(
            "feature/b",
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "hint: feature/b is 1 commit behind feature/a - run `git stk restack`",
        ));
}

#[test]
fn status_hints_sync_when_parent_review_merged() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "feature/a"]);
    repo.commit_file("a.txt", "a\nmore\n", "a moves on");
    let fake = FakeProvider::new()
        .on(
            "feature/a --state merged",
            r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .on(
            "feature/b",
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    // The sync covers the restack, so only the sync hint shows even though
    // the branch is also behind its parent.
    repo.stack_faked(&fake)
        .args(["status", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "hint: parent review #12 is merged - run `git stk sync`",
        ))
        .stdout(predicates::str::contains("restack").not());
}

#[test]
fn status_surfaces_a_closed_review_with_a_hint() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    let fake = FakeProvider::new()
        .on(
            "feature/a --state closed",
            r##"[{"number":12,"state":"CLOSED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "review: #12 closed feature/a -> main",
        ))
        .stdout(predicates::str::contains(
            "hint: review #12 was closed without merging - \
             `git stk submit` opens a new review",
        ));
}

#[test]
fn status_hints_adopt_when_parent_review_closed() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new()
        .on(
            "feature/a --state closed",
            r##"[{"number":12,"state":"CLOSED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .on("feature/a", "[]")
        .on(
            "feature/b",
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "hint: parent review #12 was closed without merging - \
             retarget feature/b with `git stk adopt`",
        ));
}

#[test]
fn status_hints_sync_when_own_review_merged() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    let fake = FakeProvider::new()
        .on(
            "feature/a --state merged",
            r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "hint: review #12 is merged - run `git stk sync`",
        ));
}

#[test]
fn review_reads_github_pr_for_current_branch() {
    let repo = TestRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "git@github.com:lararosekelley/git-stk",
    ]);
    repo.git(["switch", "-c", "feature/b"]);
    let fake = FakeProvider::new()
        .fallback(
            r##"[{"number":12,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/lararosekelley/git-stk/pull/12"}]"##,
        )
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("review")
        .assert()
        .success()
        .stdout(
            "#12 feature/b -> feature/a open https://github.com/lararosekelley/git-stk/pull/12\n",
        );
}

#[test]
fn review_reads_gitlab_mr_for_explicit_branch() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    let fake = FakeProvider::new()
        .fallback(
            r##"[{"iid":34,"state":"opened","target_branch":"feature/a","source_branch":"feature/b","web_url":"https://gitlab.com/lararosekelley/git-stk-mirror/-/merge_requests/34"}]"##,
        )
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["review", "feature/b"])
        .assert()
        .success()
        .stdout("!34 feature/b -> feature/a open https://gitlab.com/lararosekelley/git-stk-mirror/-/merge_requests/34\n");
}

#[test]
fn review_reports_when_no_review_exists() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["review", "feature/b"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "no github review found for feature/b",
        ));
}

#[test]
fn sync_sets_parent_from_github_pr_base() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "-c", "feature/b"]);
    let fake = FakeProvider::new()
        .on(
            "feature/b",
            r##"[{"number":12,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/lararosekelley/git-stk/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "synced feature/b -> feature/a (#12)",
        ));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
}

#[test]
fn sync_dry_run_reports_parent_without_writing_config() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "-c", "feature/b"]);
    let fake = FakeProvider::new()
        .on(
            "feature/b",
            r##"[{"number":12,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/lararosekelley/git-stk/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["sync", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would sync feature/b -> feature/a (#12)",
        ))
        .stdout(predicates::str::contains(
            "would restack the remaining stack",
        ));

    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/b.stkParent"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn sync_sets_parent_from_gitlab_mr_target() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "-c", "feature/b"]);
    let fake = FakeProvider::new()
        .on(
            "feature/b",
            r##"[{"iid":34,"state":"opened","target_branch":"feature/a","source_branch":"feature/b","web_url":"https://gitlab.com/lararosekelley/git-stk-mirror/-/merge_requests/34"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "synced feature/b -> feature/a (!34)",
        ));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
}

#[test]
fn sync_skips_stack_branches_without_reviews() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "skipped feature/a: no github review found",
        ))
        .stdout(predicates::str::contains(
            "skipped feature/b: no github review found",
        ))
        .stdout(predicates::str::contains(
            "sync complete: 0 synced, 2 skipped",
        ));
}

#[test]
fn config_shows_defaults_and_branch_metadata() {
    let repo = TestRepo::new();

    repo.stack()
        .arg("config")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "stk.provider (default: auto-detect from the remote URL)",
        ))
        .stdout(predicates::str::contains("stk.remote (default: origin)"))
        .stdout(predicates::str::contains("stk.updateRefs (default: false)"))
        .stdout(predicates::str::contains(
            "no branch metadata (no stacked branches)",
        ));

    repo.git(["config", "stk.pushOnRestack", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();

    repo.stack()
        .arg("config")
        .assert()
        .success()
        .stdout(predicates::str::contains("stk.pushOnRestack = true"))
        .stdout(predicates::str::contains(
            "branch.feature/a.stkparent = main",
        ))
        .stdout(predicates::str::contains("branch.feature/a.stkbase = "));
}

#[test]
fn sync_advances_the_merge_loop_end_to_end() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.pushOnRestack", "true"]);

    // Stack: main -> feature/a -> feature/b, with real commits.
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);

    // Simulate GitHub squash-merging feature/a: advance ORIGIN's main, then
    // rewind local main so sync has something to fetch.
    repo.git(["switch", "main"]);
    repo.git(["merge", "--squash", "feature/a"]);
    repo.git(["commit", "-m", "a work (#12)"]);
    repo.git(["push", "origin", "main"]);
    repo.git(["reset", "--hard", "HEAD~1"]);

    // Stand on the MERGED branch: sync must move us off it.
    repo.git(["switch", "feature/a"]);

    let fake = FakeProvider::new()
        .on("pr view 12", r#"{"body":""}"#)
        .on("pr view 13", r#"{"body":""}"#)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .record("pr edit 13 --body", "edit-body-13.txt", "")
        .on(
            "feature/a --state merged",
            r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##,
        )
        .on("feature/a", "[]")
        .on(
            "feature/b",
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"B work"}]"##,
        )
        .on("pr edit", "updated review")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains("feature/a: review #12 is merged"))
        .stdout(predicates::str::contains("updated stack note in #12"))
        .stdout(predicates::str::contains("updated stack note in #13"))
        .stdout(predicates::str::contains(
            "next up: feature/b -> #13 https://github.com/owner/repo/pull/13",
        ));

    // The overview was refreshed mid-loop: the merged entry is restyled in
    // the surviving review (and in its own), not dropped.
    let survivor = fs::read_to_string(repo.path().join("edit-body-13.txt")).expect("survivor");
    assert!(
        survivor.contains(
            "- \u{1F7E2} [B work (#13)](https://github.com/owner/repo/pull/13) \u{1F448}"
        )
    );
    assert!(survivor.contains(
        "- \u{1F7E3} ~~[A work (#12)](https://github.com/owner/repo/pull/12)~~ (merged)"
    ));
    let merged_body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("merged");
    assert!(merged_body.contains(
        "- \u{1F7E3} ~~[A work (#12)](https://github.com/owner/repo/pull/12)~~ (merged) \u{1F448}"
    ));

    // Local main was fetched forward to the squash commit.
    assert_eq!(
        repo.git(["rev-parse", "main"]),
        repo.remote_sha(&bare, "main")
    );
    // The merged branch is gone; we were moved to the survivor.
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");
    assert_eq!(
        repo.git_status(["branch", "--list", "feature/a"])
            .stdout
            .len(),
        0
    );
    // feature/b was retargeted, restacked onto fetched main, and pushed.
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
    assert_eq!(
        repo.git(["merge-base", "main", "feature/b"]),
        repo.git(["rev-parse", "main"])
    );
    assert_eq!(
        repo.remote_sha(&bare, "feature/b"),
        repo.git(["rev-parse", "feature/b"])
    );
}

#[test]
fn sync_styles_closed_reviews_in_the_stack_overview() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    // feature/b's review was closed on the platform: invisible to the sync
    // classification, but the overview must show it red rather than drop it.
    let fake = FakeProvider::new()
        .on("pr view 12", r#"{"body":""}"#)
        .on("pr view 13", r#"{"body":""}"#)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .record("pr edit 13 --body", "edit-body-13.txt", "")
        .on(
            "feature/b --state closed",
            r##"[{"number":13,"state":"CLOSED","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"B work"}]"##,
        )
        .on("feature/b", "[]")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    repo.stack_faked(&fake)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "skipped feature/b: review #13 was closed without merging",
        ))
        .stdout(predicates::str::contains("updated stack note in #12"))
        .stdout(predicates::str::contains("updated stack note in #13"));

    // The closed review never drives metadata: the parent stays put.
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );

    let bottom = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("bottom body");
    assert!(bottom.contains(
        "- \u{1F534} ~~[B work (#13)](https://github.com/owner/repo/pull/13)~~ (closed)"
    ));
    assert!(
        bottom.contains(
            "- \u{1F7E2} [A work (#12)](https://github.com/owner/repo/pull/12) \u{1F448}"
        )
    );
}

#[test]
fn sync_reports_stack_complete_when_everything_merged() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let _bare = repo.add_bare_origin(&["main", "feature/a"]);
    repo.git(["switch", "main"]);
    repo.git(["merge", "--squash", "feature/a"]);
    repo.git(["commit", "-m", "a work (#12)"]);
    repo.git(["push", "origin", "main"]);
    repo.git(["switch", "feature/a"]);

    let fake = FakeProvider::new()
        .on(
            "--state merged",
            r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "stack complete: everything merged into main",
        ));

    assert_eq!(repo.git(["branch", "--show-current"]), "main");
}

#[test]
fn view_opens_the_review_in_the_browser() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);
    let fake = FakeProvider::new()
        .record("pr view 12 --web", "view-args.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["view", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("opening #12"));

    let recorded = std::fs::read_to_string(repo.path().join("view-args.txt")).expect("view args");
    assert_eq!(recorded.trim(), "pr view 12 --web");
}

#[test]
fn provider_command_points_at_auth_login_when_the_cli_is_not_signed_in() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);

    // gh is installed but not authenticated: keep its message, add the hint.
    let fake = FakeProvider::new()
        .fail("pr list", "error: not logged into any GitHub hosts")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["review", "feature/a"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "not logged into any GitHub hosts",
        ))
        .stderr(predicates::str::contains(
            "if you are not signed in, run `gh auth login`",
        ));
}

#[test]
fn status_degrades_without_a_provider() {
    let repo = TestRepo::new();
    // No remote, no stk.provider: status still shows the local stack.
    repo.stack().args(["new", "feature/a"]).assert().success();

    repo.stack()
        .args(["status", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("branch: feature/a"))
        .stdout(predicates::str::contains("parent: main"))
        .stdout(predicates::str::contains("provider: not detected"));
}

#[test]
fn sync_degrades_without_a_remote() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // No remote: nothing to sync against, but no hard error either.
    repo.stack()
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "no remote configured - nothing to sync",
        ))
        .stdout(predicates::str::contains("could not detect provider").not());
}
