mod common;

use common::{FakeProvider, TestRepo};

#[test]
fn repair_reconstructs_wiped_stack_from_ancestry() {
    let repo = TestRepo::new();

    // Build a 3-branch stack with real commits, then wipe all metadata -
    // the config-blown-away incident.
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.stack().args(["new", "feature/c"]).assert().success();
    repo.commit_file("c.txt", "c\n", "c work");

    for branch in ["feature/a", "feature/b", "feature/c"] {
        repo.git(["config", "--unset", &format!("branch.{branch}.stkParent")]);
        repo.git(["config", "--unset", &format!("branch.{branch}.stkBase")]);
    }

    repo.stack()
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a: set parent main (from ancestry)",
        ))
        .stdout(predicates::str::contains(
            "feature/b: set parent feature/a (from ancestry)",
        ))
        .stdout(predicates::str::contains(
            "feature/c: set parent feature/b (from ancestry)",
        ))
        .stdout(predicates::str::contains(
            "repair complete: 3 repaired, 0 verified, 0 unresolved",
        ));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkBase"]),
        repo.git(["rev-parse", "feature/a"])
    );
    // Trunk must never be assigned a parent.
    assert_eq!(
        repo.git_status(["config", "--get", "branch.main.stkParent"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn repair_prefers_provider_review_base_over_ancestry() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["config", "--unset", "branch.feature/b.stkParent"]);
    repo.git(["config", "--unset", "branch.feature/b.stkBase"]);

    let fake = FakeProvider::new()
        .on(
            "feature/b",
            r##"[{"number":7,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/7"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/b: set parent feature/a (from github review #7)",
        ));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
}

#[test]
fn repair_re_records_stale_fork_point() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git([
        "config",
        "branch.feature/a.stkBase",
        "0000000000000000000000000000000000000000",
    ]);

    repo.stack()
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a: re-recorded fork point from main",
        ))
        .stdout(predicates::str::contains("1 repaired, 0 verified"));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkBase"]),
        repo.git(["rev-parse", "main"])
    );
}

#[test]
fn repair_dry_run_changes_nothing() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["config", "--unset", "branch.feature/a.stkParent"]);
    repo.git(["config", "--unset", "branch.feature/a.stkBase"]);

    repo.stack()
        .args(["repair", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a: would set parent main (from ancestry)",
        ));

    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/a.stkParent"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn repair_reports_unrepairable_branches() {
    let repo = TestRepo::new();

    // A branch with no commits of its own and equal tip to main: direction
    // is ambiguous, so repair must not guess.
    repo.git(["switch", "-c", "feature/empty"]);
    repo.git(["switch", "main"]);

    repo.stack()
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains("feature/empty: no parent found"))
        .stdout(predicates::str::contains(
            "0 repaired, 0 verified, 1 unresolved",
        ));
}

#[test]
fn repair_from_remote_rebuilds_a_stack_from_the_pushed_metadata() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "add b");

    // Pushing the branches publishes the shared metadata ref alongside them.
    let _origin = repo.add_bare_origin(&["main"]);
    repo.stack().args(["restack", "--push"]).assert().success();

    // A fresh clone would have no local stack metadata; simulate that.
    repo.git(["config", "--unset", "branch.feature/a.stkParent"]);
    repo.git(["config", "--unset", "branch.feature/b.stkParent"]);

    // Rebuild it purely from the metadata ref on the remote.
    repo.stack()
        .args(["repair", "--from-remote"])
        .assert()
        .success()
        .stdout(predicates::str::contains("attached feature/a to main"))
        .stdout(predicates::str::contains("attached feature/b to feature/a"))
        .stdout(predicates::str::contains("rebuilt 2 branches"));

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
fn repair_from_remote_errors_clearly_when_no_metadata_was_published() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");

    // A remote exists, but nothing ever pushed the metadata ref to it.
    let _origin = repo.add_bare_origin(&["main", "feature/a"]);

    repo.stack()
        .args(["repair", "--from-remote"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no stack metadata on the remote"));
}

#[test]
fn repair_from_remote_conflicts_with_dry_run() {
    let repo = TestRepo::new();

    // The two modes are mutually exclusive; clap must reject the combination.
    repo.stack()
        .args(["repair", "--from-remote", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}
