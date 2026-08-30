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
             retarget with `git stk adopt feature/b --parent <parent>`",
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
fn sync_cleans_up_closed_reviews_in_the_stack_overview_when_configured() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.cleanClosed", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

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
        .stdout(predicates::str::contains("will delete branch feature/b"))
        .stdout(predicates::str::contains("updated stack note in #12"));

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

/// A closed branch's commits are upstream nowhere, so cleaning it up must not
/// pin its child's fork point past them the way a squash merge allows: the
/// child was written on top of that work and has to keep it.
#[test]
fn sync_keeps_a_closed_parents_commits_in_its_child() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.cleanClosed", "true"]);

    // main -> feature/a (a.txt) -> feature/b (b.txt), with real commits.
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // feature/a's review was closed, never merged: nothing of a.txt is in main.
    let fake = FakeProvider::new()
        .on("pr view 12", r#"{"body":""}"#)
        .on("pr view 13", r#"{"body":""}"#)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .record("pr edit 13 --body", "edit-body-13.txt", "")
        .on(
            "feature/a --state closed",
            r##"[{"number":12,"state":"CLOSED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##,
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
        .stdout(predicates::str::contains("feature/a: review #12 is closed"))
        // The deletion says what it is, and where the branch went.
        .stdout(predicates::str::contains(
            "will delete branch feature/a (closed, not merged - `git stk undo` restores it)",
        ));

    // feature/b sits on main now, carrying both its own work and the closed
    // parent's - the whole point.
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
    let files = repo.git(["ls-tree", "-r", "--name-only", "feature/b"]);
    assert!(
        files.contains("a.txt"),
        "closed parent's file gone: {files}"
    );
    assert!(files.contains("b.txt"), "child's own file gone: {files}");
    assert_eq!(repo.git(["show", "feature/b:a.txt"]), "a");
}

/// Nothing merged, so the closing line must not claim a merge.
#[test]
fn sync_does_not_report_a_merge_when_the_stack_was_only_closed() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.cleanClosed", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "main"]);

    let fake = FakeProvider::new()
        .on("pr view 12", r#"{"body":""}"#)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .on(
            "feature/a --state closed",
            r##"[{"number":12,"state":"CLOSED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##,
        )
        .on("feature/a", "[]")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "stack complete: nothing left above main - merged or closed",
        ))
        .stdout(predicates::str::contains("everything merged into").not());
}

#[test]
fn sync_leaves_sibling_stacks_sharing_the_trunk_out_of_the_overview() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    // Two independent stacks that share only the trunk:
    //   main -> feature/a   (PR #12)
    //   main -> feature/x   (PR #99)
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "main"]);
    repo.stack().args(["new", "feature/x"]).assert().success();
    repo.commit_file("x.txt", "x\n", "x work");

    // Stand on feature/a and sync. Its stack is just feature/a; the sibling
    // off the trunk must not be swept in (it gets its own sync).
    repo.git(["switch", "feature/a"]);

    let fake = FakeProvider::new()
        .on("pr view 12", r#"{"body":""}"#)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        // A guard: if the sibling stack leaks in, its body gets written here.
        .record("pr edit 99 --body", "edit-body-99.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##,
        )
        .on(
            "feature/x",
            r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"feature/x","url":"https://github.com/owner/repo/pull/99","title":"X work"}]"##,
        )
        .on("pr edit", "updated review")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains("updated stack note in #12"))
        .stdout(predicates::str::contains("#99").not());

    // feature/a's overview lists only its own review and the trunk - the
    // unrelated sibling stack never leaks into the ledger.
    let body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("body for #12");
    assert!(body.contains("[A work (#12)](https://github.com/owner/repo/pull/12)"));
    assert!(
        !body.contains("#99"),
        "sibling stack leaked into #12:\n{body}"
    );
    assert!(
        !body.contains("X work"),
        "sibling stack leaked into #12:\n{body}"
    );

    // The sibling's review body was never touched by this sync.
    assert!(
        !repo.path().join("edit-body-99.txt").exists(),
        "sync wrote the sibling stack's review body"
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

/// A stack rooted on a release line: `sync` must not adopt that base from its
/// own review. Doing so records `stkParent = <trunk>` on a shared branch,
/// which `restack` then rebases and force-pushes (#308).
#[test]
fn sync_does_not_adopt_the_stack_base_from_its_own_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();

    let fake = FakeProvider::new()
        .on("rc-20260817", r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on("fix/shared", r##"[{"number":13,"state":"OPEN","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["sync", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "skipped rc-20260817: this stack's base",
        ))
        .stdout(predicates::str::contains("rc-20260817 -> main").not())
        // Nor is the base ever what to look at next.
        .stdout(predicates::str::contains("next up: fix/shared"));

    // Not a dry-run artefact: a real sync must not write it either.
    repo.stack_faked(&fake).args(["sync"]).assert().success();
    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkParent"])
            .stdout
            .len(),
        0,
        "the base must not have been adopted into the stack"
    );
}

/// A branch with no parent and nothing stacked on it is not a base - it is a
/// branch whose metadata is missing, which is exactly what `sync` rebuilds.
#[test]
fn sync_still_adopts_a_lone_branch_with_no_parent() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "-c", "feature/b"]);
    let fake = FakeProvider::new()
        .on("feature/b", r##"[{"number":12,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://example.com/12"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake).args(["sync"]).assert().success();

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
}

/// The hole a shape-only rule leaves: once the branches above the base land,
/// nothing about the shape says it is a base any more, and the next `sync`
/// would adopt it. The recorded floor outlives them.
#[test]
fn sync_does_not_adopt_the_base_after_the_stack_above_it_lands() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();

    // Rooting the stack records the base.
    assert_eq!(
        repo.git(["config", "--get", "branch.rc-20260817.stkFloor"]),
        "true"
    );

    // The stack lands and is cleaned up, leaving the base standing alone -
    // indistinguishable, by shape, from a branch whose metadata is missing.
    repo.git(["switch", "rc-20260817"]);
    repo.git(["branch", "-D", "fix/shared"]);

    let fake = FakeProvider::new()
        .on("rc-20260817", r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake).args(["sync"]).assert().success();

    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkParent"])
            .stdout
            .len(),
        0,
        "a recorded base must stay a base once its stack has landed"
    );
}

/// Shape alone cannot tell a base from a stack that is only half rebuilt, so
/// `sync` never records one it merely inferred. Marking `feature/a` here would
/// freeze the bottom of an ordinary stack out of its own restacks for good.
#[test]
fn sync_does_not_mark_a_branch_it_merely_inferred_is_a_base() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "-c", "feature/b"]);
    repo.commit_file("b.txt", "b\n", "b work");

    let fake = FakeProvider::new()
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .fallback("[]")
        .install(&repo);

    // First sync adopts feature/b onto feature/a, which makes feature/a look
    // exactly like a base: parentless, with a layer stacked on it.
    repo.stack_faked(&fake).args(["sync"]).assert().success();
    repo.stack_faked(&fake)
        .args(["sync"])
        .assert()
        .success()
        // And says the skip is a reading of the shape, naming what rebuilds it
        // - not a recorded fact.
        // One assertion spanning the join, so stray indentation inside the
        // literal cannot hide between two half-matches.
        .stdout(predicates::str::contains(
            "reads as the base; `git stk repair` if it is a stacked branch",
        ));

    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/a.stkFloor"])
            .stdout
            .len(),
        0,
        "an inferred base must not be recorded"
    );
    // Unrecorded, it stays an ordinary branch: `repair` still adopts it, and
    // nothing has frozen it out of its own restacks.
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
    repo.stack_faked(&fake).args(["repair"]).assert().success();
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkParent"]),
        "main"
    );
}

/// `detach` is how you say a branch is no longer a stack base.
#[test]
fn detach_clears_the_floor_marker() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.git(["switch", "rc-20260817"]);

    repo.stack()
        .args(["detach"])
        .assert()
        .success()
        // It is the escape every base hint names, so it confirms what it did.
        .stdout(predicates::str::contains(
            "rc-20260817 is no longer a stack base",
        ));

    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkFloor"])
            .stdout
            .len(),
        0
    );
}

/// The other half of holding the base out of `sync`: a merged review on it
/// must not count as finished, or cleanup deletes the branch locally. The
/// skip sits above `landing_for`, which is what makes this hold.
#[test]
fn sync_does_not_clean_up_the_base_when_its_own_review_merges() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");

    let fake = FakeProvider::new()
        // The release line landed in the trunk, on its own schedule.
        .on("rc-20260817", r##"[{"number":99,"state":"MERGED","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on("fix/shared", r##"[{"number":13,"state":"OPEN","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake).args(["sync"]).assert().success();

    assert!(
        !repo
            .git_status(["branch", "--list", "rc-20260817"])
            .stdout
            .is_empty(),
        "the base must not be deleted by cleanup"
    );
}

/// A stack rooted off the trunk lands in its base, not in the trunk - so the
/// closing line must not claim the trunk.
#[test]
fn sync_reports_an_off_trunk_stack_as_complete_into_its_base() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");

    let fake = FakeProvider::new()
        .on("rc-20260817", r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on("fix/shared", r##"[{"number":13,"state":"MERGED","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["sync"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "stack complete: everything merged into rc-20260817",
        ));
}

/// Undoing the command that rooted a stack has to unmark the base it marked,
/// or the branch stays held out of every stack for good.
#[test]
fn undo_clears_a_floor_marker_the_undone_command_set() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");

    // `new --insert` roots a stack on rc, marking it - and snapshots first.
    repo.stack()
        .args(["new", "--insert", "fix/shared"])
        .assert()
        .success();
    assert_eq!(
        repo.git(["config", "--get", "branch.rc-20260817.stkFloor"]),
        "true"
    );

    repo.stack().args(["undo"]).assert().success();

    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkFloor"])
            .stdout
            .len(),
        0,
        "undo should have unmarked the base it marked"
    );
}

/// Adopting a base onto a parent says it is a layer now. Leaving the marker
/// would hold it out of `submit`/`merge` while `restack` treats it as ordinary
/// - and `adopt` is what every "no stack parent" hint points at.
#[test]
fn adopt_clears_the_floor_marker_on_the_branch_it_attaches() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    assert_eq!(
        repo.git(["config", "--get", "branch.rc-20260817.stkFloor"]),
        "true"
    );

    repo.stack()
        .args(["adopt", "rc-20260817", "--parent", "main"])
        .assert()
        .success();

    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkFloor"])
            .stdout
            .len(),
        0,
        "an adopted branch is a layer, not a base"
    );
}

/// The documented migration for a repo an older `sync` already bit. The base
/// carries a stack parent it should never have had, which makes it look like
/// an ordinary branch - so `adopt` alone records nothing and `detach` has to
/// clear the stray parent first.
#[test]
fn detach_then_adopt_records_a_base_an_older_sync_had_adopted() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.git(["switch", "-c", "fix/shared"]);
    repo.commit_file("shared.txt", "shared\n", "shared work");
    // The damage: the base adopted onto the trunk.
    repo.git(["config", "branch.rc-20260817.stkParent", "main"]);

    // Adopt alone cannot tell it from an ordinary trunk-anchored branch.
    repo.stack()
        .args(["adopt", "fix/shared", "--parent", "rc-20260817"])
        .assert()
        .success();
    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkFloor"])
            .stdout
            .len(),
        0,
        "the shapes are identical here; marking would hit ordinary stacks too"
    );

    // The documented two-step does record it.
    repo.stack()
        .args(["detach", "rc-20260817"])
        .assert()
        .success();
    repo.stack()
        .args(["adopt", "fix/shared", "--parent", "rc-20260817"])
        .assert()
        .success();
    assert_eq!(
        repo.git(["config", "--get", "branch.rc-20260817.stkFloor"]),
        "true"
    );
}

/// Neither `merge` nor plain `submit` may suggest `adopt` from a recorded
/// base: `adopt` defaults to the current branch, so following that advice
/// re-roots the release line.
#[test]
fn a_lone_base_is_never_told_to_adopt_itself() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    // The stack lands and is cleaned up, leaving the recorded base alone.
    repo.git(["switch", "rc-20260817"]);
    repo.git(["branch", "-D", "fix/shared"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("nothing to merge"))
        .stderr(predicates::str::contains("adopt").not());

    repo.stack_faked(&fake)
        .args(["submit", "--no-stack", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "rc-20260817 is this stack's base, and nothing is stacked on it",
        ))
        .stderr(predicates::str::contains("adopt").not());
}

/// Stacking on a branch with no parent of its own is ambiguous - a release
/// line and a branch nobody has adopted yet look identical. git-stk records
/// the safe reading, but must say so and name the way back, or an ordinary
/// branch is frozen out of its own restacks with nothing pointing at the fix.
#[test]
fn new_says_when_it_records_the_branch_below_as_a_base() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "feature/a"]);
    repo.commit_file("a.txt", "a\n", "a work");

    repo.stack()
        .args(["new", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "recorded feature/a as this stack's base",
        ))
        .stdout(predicates::str::contains("git stk detach feature/a"));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkFloor"]),
        "true"
    );

    // And the way back works, leaving an ordinary branch behind.
    repo.stack()
        .args(["detach", "feature/a"])
        .assert()
        .success();
    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/a.stkFloor"])
            .stdout
            .len(),
        0
    );
}

/// Stacking on the trunk, or on a branch that is already stacked, is not
/// ambiguous - nothing is recorded and nothing is said.
#[test]
fn new_is_quiet_when_the_branch_below_is_not_a_candidate_base() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["new", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("base").not());
    repo.commit_file("a.txt", "a\n", "a work");

    repo.stack()
        .args(["new", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains("base").not());

    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/a.stkFloor"])
            .stdout
            .len(),
        0
    );
}

/// The recording lands on a branch the command does not name, so a dry run
/// has to preview it - and write nothing.
#[test]
fn new_dry_run_previews_the_base_recording_without_writing_it() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "feature/a"]);
    repo.commit_file("a.txt", "a\n", "a work");

    repo.stack()
        .args(["new", "feature/b", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would record feature/a as this stack's base",
        ));

    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/a.stkFloor"])
            .stdout
            .len(),
        0,
        "a dry run must not write the marker"
    );
}

/// The documented migration, previewed: `adopt --dry-run` marks a third
/// branch, so it must say which.
#[test]
fn adopt_dry_run_previews_the_base_it_would_record() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.git(["switch", "-c", "fix/shared"]);
    repo.commit_file("shared.txt", "shared\n", "shared work");

    repo.stack()
        .args([
            "adopt",
            "fix/shared",
            "--parent",
            "rc-20260817",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("would attach fix/shared"))
        .stdout(predicates::str::contains(
            "would record rc-20260817 as this stack's base",
        ));

    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkFloor"])
            .stdout
            .len(),
        0
    );
}

/// `adopt` removes a base's protection, which is the direction that most wants
/// saying - and `--dry-run` has to preview it, since the write lands on the
/// branch being adopted rather than the one named by `--parent`.
#[test]
fn adopt_announces_and_previews_clearing_a_base() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();

    repo.stack()
        .args(["adopt", "rc-20260817", "--parent", "main", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would record that rc-20260817 is no longer a stack base",
        ));
    assert_eq!(
        repo.git(["config", "--get", "branch.rc-20260817.stkFloor"]),
        "true",
        "a dry run must not clear the marker"
    );

    repo.stack()
        .args(["adopt", "rc-20260817", "--parent", "main"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "recorded that rc-20260817 is no longer a stack base",
        ));
    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkFloor"])
            .stdout
            .len(),
        0
    );
}

/// `status` reads the marker like every other reader: a base carrying a stray
/// `stkParent` has no parent for any purpose, and reporting one produced a
/// "run `git stk restack`" hint that `restack` cannot act on.
#[test]
fn status_on_a_base_names_it_and_offers_no_restack_hint() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");
    // What an older git-stk left behind, plus commits so the trunk is ahead.
    repo.git(["config", "branch.rc-20260817.stkParent", "main"]);
    repo.git(["switch", "main"]);
    repo.commit_file("trunk.txt", "t\n", "trunk moves");
    repo.git(["switch", "rc-20260817"]);

    repo.stack()
        .args(["status"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "parent: none (this stack's base)",
        ))
        .stdout(predicates::str::contains("git stk detach rc-20260817"))
        .stdout(predicates::str::contains("parent: main").not())
        .stdout(predicates::str::contains("git stk restack").not());
}

/// `sync` and `cleanup` both skip a base on purpose, so "run `git stk sync`"
/// can never be satisfied for one - it would reprint every run while the thing
/// the user actually has to do went unnamed.
#[test]
fn status_on_a_base_with_a_merged_review_does_not_send_you_to_sync() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.git(["switch", "rc-20260817"]);

    let fake = FakeProvider::new()
        .on("rc-20260817", r##"[{"number":99,"state":"MERGED","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "git-stk leaves a stack's base alone, so this is yours to finish",
        ))
        .stdout(predicates::str::contains("run `git stk sync`").not());
}

/// The twin of the base's own dead end: a layer stacked on a base whose
/// release PR merged was told to run `git stk sync`, which skips the base
/// before `landing_for` and so never retargets the layer.
#[test]
fn status_on_a_layer_over_a_landed_base_does_not_send_you_to_sync() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");

    let fake = FakeProvider::new()
        .on("rc-20260817", r##"[{"number":99,"state":"MERGED","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on("fix/shared", r##"[{"number":13,"state":"OPEN","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["status"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "rc-20260817 is this stack's base, so git-stk does not retarget off it",
        ))
        .stdout(predicates::str::contains(
            "git stk adopt fix/shared --parent <parent>",
        ))
        .stdout(predicates::str::contains("run `git stk sync`").not());
}

/// `Unknown(_)` is not "landed" - GitLab's `locked` reaches it, and
/// `landing_for` returns `None` for it - so neither base hint may fire on a
/// review that is still running. Both printed nothing before this PR.
#[test]
fn status_says_nothing_about_a_base_whose_review_is_still_running() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");

    let fake = FakeProvider::new()
        .on("rc-20260817", r##"[{"number":99,"state":"LOCKED","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on("fix/shared", r##"[{"number":13,"state":"OPEN","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .fallback("[]")
        .install(&repo);

    // From the layer: no "re-root off the base" advice for a base still running.
    repo.stack_faked(&fake)
        .args(["status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("does not retarget off it").not());

    // And from the base itself: nothing is "yours to finish" yet.
    repo.git(["switch", "rc-20260817"]);
    repo.stack_faked(&fake)
        .args(["status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("yours to finish").not());
}

/// The dot and the merge must agree about the same pipeline. A `manual` one is
/// blocked awaiting a person, which GitLab holds the merge for - so `--wait`
/// stops and says why, rather than merging past a gate the `⚪` just reported.
#[test]
fn merge_wait_stops_on_a_gitlab_pipeline_waiting_on_a_person() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .record("mr merge", "merged.txt", "")
        // `mr view` answers a single object; the listing answers an array.
        .on(
            "mr view 34 --output json",
            r##"{"iid":34,"state":"opened","target_branch":"main","source_branch":"feature/a","head_pipeline":{"status":"manual"}}"##,
        )
        .fallback(
            r##"[{"iid":34,"state":"opened","target_branch":"main","source_branch":"feature/a","web_url":"https://gitlab.com/o/r/-/merge_requests/34"}]"##,
        )
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y", "--wait"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("stopped without a verdict"))
        // Not "failed" - nothing did.
        .stderr(predicates::str::contains("checks failed").not());

    assert!(
        !repo.path().join("merged.txt").exists(),
        "merged past a pipeline the dot reports as held"
    );
}

/// GitHub's gate and its dot must agree too. `gh pr checks` has no exit code
/// for "stopped without a verdict", so a cancelled newest run can land in the
/// green one - and merging there would contradict the `⚪` the user just read.
#[test]
fn merge_wait_stops_on_a_github_check_that_was_cancelled() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .record("pr merge", "merged.txt", "")
        // Exit 0: gh sees nothing failing.
        .on("pr checks 12", "all checks passing")
        // The rollup says the newest run of `plan` was cancelled.
        .on(
            "pr view 12 --json statusCheckRollup",
            r##"{"statusCheckRollup":[{"name":"plan","workflowName":"CI","status":"COMPLETED","conclusion":"CANCELLED","startedAt":"2026-08-29T23:01:56Z"}]}"##,
        )
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y", "--wait"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("stopped without a verdict"))
        .stderr(predicates::str::contains("checks failed").not());

    assert!(
        !repo.path().join("merged.txt").exists(),
        "merged past a check the dot reports as held"
    );
}
