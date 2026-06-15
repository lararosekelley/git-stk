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
