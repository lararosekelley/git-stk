mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

// Shared provider responses. The URL host/owner and presence of a title vary
// per test, matching what each assertion inspects.
const MERGED_A: &str = r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/lararosekelley/git-stk/pull/12"}]"##;
const OPEN_B: &str = r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/lararosekelley/git-stk/pull/13"}]"##;
const CLOSED_A: &str = r##"[{"number":12,"state":"CLOSED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/lararosekelley/git-stk/pull/12"}]"##;
const MERGED_A_OWNER: &str = r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##;
const OPEN_B_OWNER: &str = r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13"}]"##;
const MERGED_A_TITLE: &str = r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##;
const OPEN_B_TITLE: &str = r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"B work"}]"##;

#[test]
fn cleanup_retargets_children_and_detaches_merged_branch() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new()
        .on("feature/a --state merged", MERGED_A)
        .on("feature/a", "[]")
        .on("feature/b", OPEN_B)
        .on("pr edit", "updated child review")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("will retarget feature/b -> main"))
        .stdout(predicates::str::contains(
            "will update review feature/b -> main (#13)",
        ))
        .stdout(predicates::str::contains("updated child review"))
        .stdout(predicates::str::contains("will detach feature/a"))
        .stdout(predicates::str::contains(
            "skipped feature/b: review #13 is open",
        ))
        .stdout(predicates::str::contains(
            "cleanup complete: 1 cleaned, 1 skipped",
        ));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/a.stkParent"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn cleanup_dry_run_leaves_stack_metadata_unchanged() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new()
        .on("feature/a --state merged", MERGED_A)
        .on("feature/a", "[]")
        .on("feature/b", OPEN_B)
        // A dry run must never reach a review edit.
        .fail("pr edit", "dry-run should not edit review")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "--dry-run", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would retarget feature/b -> main",
        ))
        .stdout(predicates::str::contains(
            "would update review feature/b -> main (#13)",
        ))
        .stdout(predicates::str::contains("would detach feature/a"))
        .stdout(predicates::str::contains("would update stack note in #12"));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkParent"]),
        "main"
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
}

#[test]
fn cleanup_refreshes_the_stack_overview_ledger() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "main"]);
    let fake = FakeProvider::new()
        .on("pr view 12", r#"{"body":""}"#)
        .on("pr view 13", r#"{"body":""}"#)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .record("pr edit 13 --body", "edit-body-13.txt", "")
        .on("feature/a --state merged", MERGED_A_TITLE)
        .on("feature/a", "[]")
        .on("feature/b", OPEN_B_TITLE)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("updated stack note in #12"))
        .stdout(predicates::str::contains("updated stack note in #13"))
        .stdout(predicates::str::contains("will delete branch feature/a"));

    // The overview was refreshed before the merged branch vanished: its
    // entry is restyled in the survivor, not dropped.
    let survivor =
        std::fs::read_to_string(repo.path().join("edit-body-13.txt")).expect("survivor body");
    assert!(survivor.contains(
        "- \u{1F7E3} ~~[A work (#12)](https://github.com/owner/repo/pull/12)~~ (merged)"
    ));
    assert!(
        survivor.contains(
            "- \u{1F7E2} [B work (#13)](https://github.com/owner/repo/pull/13) \u{1F448}"
        )
    );
}

#[test]
fn cleanup_recovers_base_when_merged_parent_branch_is_gone() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "main"]);
    // The merged parent was deleted out-of-band: feature/b now points at a
    // branch that no longer exists, and only the review remembers its base.
    repo.git(["branch", "-D", "feature/a"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);

    let fake = FakeProvider::new()
        .on("feature/a --state merged", MERGED_A_OWNER)
        .on("feature/a", "[]")
        .record("pr edit 13 --base", "edit-base-13.txt", "")
        .on("feature/b", OPEN_B_OWNER)
        .fallback("[]")
        .install(&repo);

    // Dry run announces the retarget without writing anything.
    repo.stack_faked(&fake)
        .args(["cleanup", "--dry-run", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would retarget feature/b -> main",
        ));
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );

    repo.stack_faked(&fake)
        .args(["cleanup", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/b: parent feature/a is gone, but review #12 merged into main",
        ))
        .stdout(predicates::str::contains("will retarget feature/b -> main"))
        .stdout(predicates::str::contains(
            "will update review feature/b -> main (#13)",
        ))
        .stdout(predicates::str::contains(
            "cleanup complete: 0 cleaned, 1 skipped, 1 retargeted",
        ));

    let recorded =
        std::fs::read_to_string(repo.path().join("edit-base-13.txt")).expect("edit base args");
    assert_eq!(recorded.trim(), "pr edit 13 --base main");
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
}

#[test]
fn cleanup_leaves_a_gone_parent_alone_without_a_merged_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);

    // No review for the missing parent: recovery must defer to repair.
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains("retarget").not());

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
}

#[test]
fn cleanup_skips_closed_reviews_with_their_state() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.git(["switch", "main"]);
    let fake = FakeProvider::new()
        .on("--state closed", CLOSED_A)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "skipped feature/a: review #12 is closed",
        ))
        .stdout(predicates::str::contains(
            "cleanup complete: 0 cleaned, 1 skipped",
        ));

    // Closed work is not in the trunk; the branch must survive.
    assert!(
        repo.git(["branch", "--list", "feature/a"])
            .contains("feature/a")
    );
}

#[test]
fn cleanup_deletes_cleaned_merged_branch_by_default() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    // Real commits + a squash merge: feature/a's commits are NOT
    // ancestry-merged into main afterwards, so `git branch -d` would refuse.
    // Deletion must trust the provider-verified merge state instead.
    repo.commit_file("a.txt", "one\n", "parent change one");
    repo.commit_file("a.txt", "one\ntwo\n", "parent change two");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "main"]);
    repo.git(["merge", "--squash", "feature/a"]);
    repo.git(["commit", "-m", "parent changes (#12)"]);
    let fake = FakeProvider::new()
        .on("feature/a --state merged", MERGED_A)
        .on("feature/a", "[]")
        .on("feature/b", OPEN_B)
        .on("pr edit", "updated child review")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("will delete branch feature/a"))
        .stdout(predicates::str::contains(
            "cleanup complete: 1 cleaned, 1 skipped",
        ));

    assert!(
        !repo
            .git(["branch", "--list", "feature/a"])
            .contains("feature/a")
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
}

#[test]
fn cleanup_deletes_closed_branch_only_with_config_set() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    // Real commits, none of them ancestry-merged into main, so `git branch -d`
    // would refuse: deletion trusts the provider-verified review state.
    repo.commit_file("a.txt", "one\n", "parent change one");
    repo.commit_file("a.txt", "one\ntwo\n", "parent change two");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "main"]);
    repo.git(["merge", "--squash", "feature/a"]);
    repo.git(["commit", "-m", "parent changes (#12)"]);
    let fake = FakeProvider::new()
        .on("feature/a --state closed", CLOSED_A)
        .on("feature/a", "[]")
        .on("feature/b", OPEN_B)
        .on("pr edit", "updated child review")
        .fallback("[]")
        .install(&repo);

    // Initially try without the config option set
    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "skipped feature/a: review #12 is closed",
        ))
        .stdout(predicates::str::contains(
            "cleanup complete: 0 cleaned, 2 skipped",
        ));

    assert!(
        repo.git(["branch", "--list", "feature/a"])
            .contains("feature/a")
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );

    // Then try with the config option set
    repo.git(["config", "stk.cleanClosed", "true"]);
    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "will delete branch feature/a (closed, not merged - `git stk undo` restores it)",
        ))
        .stdout(predicates::str::contains(
            "cleanup complete: 1 cleaned, 1 skipped",
        ));

    assert!(
        !repo
            .git(["branch", "--list", "feature/a"])
            .contains("feature/a")
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
    // No fork point pinned past the closed parent: feature/b's next restack has
    // to replay the closed commits it was built on, not drop them.
    assert!(
        !repo
            .git_status(["config", "--get", "branch.feature/b.stkBase"])
            .status
            .success()
    );
}

#[test]
fn cleanup_dry_run_keeps_branch() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.git(["switch", "main"]);
    let fake = FakeProvider::new()
        .on("--state merged", MERGED_A)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "--dry-run", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would delete branch feature/a"));

    assert!(
        repo.git(["branch", "--list", "feature/a"])
            .contains("feature/a")
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkParent"]),
        "main"
    );
}

#[test]
fn cleanup_keeps_the_checked_out_branch() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    let fake = FakeProvider::new()
        .on("--state merged", MERGED_A)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "kept feature/a: cannot delete the checked out branch",
        ))
        .stdout(predicates::str::contains(
            "cleanup complete: 1 cleaned, 0 skipped",
        ));

    assert!(
        repo.git(["branch", "--list", "feature/a"])
            .contains("feature/a")
    );
}

#[test]
fn cleanup_keep_branch_keeps_cleaned_merged_branch() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.git(["switch", "main"]);
    let fake = FakeProvider::new()
        .on("--state merged", MERGED_A)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "--keep-branch", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("will detach feature/a"))
        .stdout(predicates::str::contains("delete").not());

    assert!(
        repo.git(["branch", "--list", "feature/a"])
            .contains("feature/a")
    );
    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/a.stkParent"])
            .status
            .code(),
        Some(1)
    );
}
