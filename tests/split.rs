mod common;

use common::TestRepo;

#[test]
fn split_per_commit_creates_a_branch_per_commit_reusing_the_leaf() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature"]).assert().success();
    repo.commit_file("f1.txt", "1\n", "first change");
    repo.commit_file("f2.txt", "2\n", "second change");
    repo.commit_file("f3.txt", "3\n", "third change");

    repo.stack()
        .args(["split", "--per-commit"])
        .assert()
        .success()
        .stdout(predicates::str::contains("split feature into 3 branches"));

    // Two new branches beneath; feature is reused as the leaf.
    assert_eq!(
        repo.git(["config", "--get", "branch.first-change.stkParent"]),
        "main"
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.second-change.stkParent"]),
        "first-change"
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature.stkParent"]),
        "second-change"
    );

    // Each new branch points at the matching commit; feature's tip is unchanged.
    assert_eq!(
        repo.git(["rev-parse", "first-change"]),
        repo.git(["rev-parse", "feature~2"])
    );
    assert_eq!(
        repo.git(["rev-parse", "second-change"]),
        repo.git(["rev-parse", "feature~1"])
    );
}

#[test]
fn split_per_commit_dry_run_writes_nothing() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature"]).assert().success();
    repo.commit_file("f1.txt", "1\n", "first change");
    repo.commit_file("f2.txt", "2\n", "second change");

    repo.stack()
        .args(["split", "--per-commit", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would create"));

    // No branch created, and the original parent is untouched.
    assert!(
        !repo
            .git_status(["rev-parse", "--verify", "first-change"])
            .status
            .success()
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature.stkParent"]),
        "main"
    );
}

#[test]
fn split_needs_at_least_two_commits() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature"]).assert().success();
    repo.commit_file("f1.txt", "1\n", "only change");

    repo.stack()
        .args(["split", "--per-commit"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("need at least 2 to split"));
}

#[test]
fn split_interactive_without_a_terminal_points_at_per_commit() {
    // The test harness has no TTY, so the interactive flow can't run; it should
    // bail with a pointer to --per-commit rather than erroring obscurely.
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature"]).assert().success();
    repo.commit_file("f1.txt", "1\n", "first change");
    repo.commit_file("f2.txt", "2\n", "second change");

    repo.stack()
        .args(["split"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("needs a terminal"))
        .stderr(predicates::str::contains("--per-commit"));
}

/// Splitting a stack's base would stamp a `stkParent` on it, which a base has
/// by design not got - leaving that metadata disagreeing with the marker that
/// outranks it everywhere else.
#[test]
fn split_refuses_a_recorded_stack_base() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "one\n", "release one");
    repo.commit_file("rc2.txt", "two\n", "release two");
    // Rooting a stack here records it as the base.
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.git(["switch", "rc-20260817"]);

    repo.stack()
        .args(["split", "--per-commit"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "rc-20260817 is a stack's base, so splitting it would give it a stack parent",
        ))
        // `adopt` first: it clears the base marker and keeps the parent, so a
        // follow-up split still bases on it. `detach` clears both, which would
        // silently re-root the split on the trunk.
        .stderr(predicates::str::contains(
            "git stk adopt rc-20260817 --parent <parent>",
        ))
        .stderr(predicates::str::contains("git stk detach rc-20260817"));

    // And nothing was written to it.
    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkParent"])
            .stdout
            .len(),
        0
    );
}
