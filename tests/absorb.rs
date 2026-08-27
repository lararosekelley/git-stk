mod common;

use common::TestRepo;
use predicates::prelude::PredicateBooleanExt;

/// main <- feature/a (adds foo.txt) <- feature/b (adds bar.txt). Each line is
/// owned by a distinct commit, so blame attribution is unambiguous.
fn stack() -> TestRepo {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("foo.txt", "alpha\n", "add foo");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("bar.txt", "beta\n", "add bar");
    repo
}

#[test]
fn absorb_dry_run_routes_hunks_to_owning_commits() {
    let repo = stack();
    let head = repo.git(["rev-parse", "HEAD"]);

    // Edit a line each commit introduced, two branches down and one down.
    repo.write("foo.txt", "alpha fixed\n");
    repo.write("bar.txt", "beta fixed\n");
    repo.git(["add", "foo.txt", "bar.txt"]);

    repo.stack()
        .args(["absorb", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("foo.txt:1 -> feature/a"))
        .stdout(predicates::str::contains("add foo"))
        .stdout(predicates::str::contains("bar.txt:1 -> feature/b"))
        .stdout(predicates::str::contains("add bar"));

    // --dry-run rewrites nothing.
    assert_eq!(repo.git(["rev-parse", "HEAD"]), head);
}

#[test]
fn absorb_dry_run_leaves_trunk_owned_and_added_lines_unabsorbed() {
    let repo = stack();

    // README is the trunk's; a fresh line belongs to no commit.
    repo.write("README.md", "# changed\n");
    repo.write("foo.txt", "alpha\nbrand new\n");
    repo.git(["add", "README.md", "foo.txt"]);

    repo.stack()
        .args(["absorb", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("unabsorbed (left in place)"))
        .stdout(predicates::str::contains(
            "README.md:1 owned by a commit outside the stack",
        ))
        .stdout(predicates::str::contains(
            "foo.txt:1 added lines - no commit to attribute",
        ));
}

#[test]
fn absorb_without_staged_changes_reports_nothing() {
    let repo = stack();

    repo.stack()
        .args(["absorb", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no staged changes to absorb"));
}

#[test]
fn absorb_include_unstaged_attributes_without_staging() {
    let repo = stack();
    // Edit but do not `git add`.
    repo.write("foo.txt", "alpha fixed\n");

    // Staged-only sees nothing.
    repo.stack()
        .args(["absorb", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no staged changes"));

    // --include-unstaged picks the edit up.
    repo.stack()
        .args(["absorb", "--dry-run", "--include-unstaged"])
        .assert()
        .success()
        .stdout(predicates::str::contains("foo.txt:1 -> feature/a"));
}

#[test]
fn absorb_respects_include_unstaged_config() {
    let repo = stack();
    repo.git(["config", "stk.absorbIncludeUnstaged", "true"]);
    repo.write("foo.txt", "alpha fixed\n");

    repo.stack()
        .args(["absorb", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("foo.txt:1 -> feature/a"));
}

#[test]
fn absorb_folds_a_fix_into_its_owning_commit() {
    let repo = stack();
    repo.write("foo.txt", "alpha fixed\n");
    repo.git(["add", "foo.txt"]);

    repo.stack()
        .args(["absorb"])
        .assert()
        .success()
        .stdout(predicates::str::contains("absorbed 1 hunk into 1 commit"));

    // The fix lands in feature/a's existing commit, not a new one.
    assert_eq!(repo.git(["show", "feature/a:foo.txt"]), "alpha fixed");
    assert_eq!(repo.git(["rev-list", "--count", "main..feature/a"]), "1");
    // Nothing left in the worktree.
    assert!(repo.git(["status", "--porcelain"]).is_empty());
}

#[test]
fn absorb_folds_with_mnemonic_prefix_configured() {
    // diff.mnemonicPrefix (and diff.noprefix) change diff header prefixes from
    // a/ b/; absorb must force them back or it cannot parse or apply the diff.
    let repo = stack();
    repo.git(["config", "diff.mnemonicPrefix", "true"]);
    repo.write("foo.txt", "alpha fixed\n");
    repo.git(["add", "foo.txt"]);

    repo.stack()
        .args(["absorb"])
        .assert()
        .success()
        .stdout(predicates::str::contains("absorbed 1 hunk into 1 commit"));

    assert_eq!(repo.git(["show", "feature/a:foo.txt"]), "alpha fixed");
    assert!(repo.git(["status", "--porcelain"]).is_empty());
}

#[test]
fn absorb_folds_into_multiple_commits_across_branches() {
    let repo = stack();
    repo.write("foo.txt", "alpha fixed\n");
    repo.write("bar.txt", "beta fixed\n");
    repo.git(["add", "foo.txt", "bar.txt"]);

    repo.stack()
        .args(["absorb"])
        .assert()
        .success()
        .stdout(predicates::str::contains("absorbed 2 hunks into 2 commits"));

    assert_eq!(repo.git(["show", "feature/a:foo.txt"]), "alpha fixed");
    assert_eq!(repo.git(["show", "feature/b:bar.txt"]), "beta fixed");
    assert!(repo.git(["status", "--porcelain"]).is_empty());
}

#[test]
fn absorb_leaves_unattributable_changes_in_place() {
    let repo = stack();
    repo.write("foo.txt", "alpha fixed\n"); // -> feature/a
    repo.write("README.md", "# changed\n"); // trunk-owned
    repo.git(["add", "foo.txt", "README.md"]);

    repo.stack()
        .args(["absorb"])
        .assert()
        .success()
        .stdout(predicates::str::contains("absorbed 1 hunk into 1 commit"))
        .stdout(predicates::str::contains(
            "README.md:1 owned by a commit outside the stack",
        ));

    // foo folded; README untouched in history but kept in the worktree.
    assert_eq!(repo.git(["show", "feature/a:foo.txt"]), "alpha fixed");
    assert_eq!(repo.git(["show", "HEAD:README.md"]), "# test repo");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).expect("README"),
        "# changed\n"
    );
}

#[test]
fn absorb_folds_then_restacks_a_branch_forking_above() {
    let repo = stack(); // main <- feature/a (foo) <- feature/b (bar)
    repo.git(["switch", "feature/a"]); // feature/b forks above the current branch

    repo.write("foo.txt", "alpha fixed\n");
    repo.git(["add", "foo.txt"]);

    repo.stack()
        .args(["absorb"])
        .assert()
        .success()
        .stdout(predicates::str::contains("absorbed 1 hunk into 1 commit"))
        // Phase 2 restacks the forked branch onto the rewritten feature/a.
        .stdout(predicates::str::contains("rebasing feature/b"));

    // The fix folded into feature/a's existing commit, not a new one.
    assert_eq!(repo.git(["show", "feature/a:foo.txt"]), "alpha fixed");
    assert_eq!(repo.git(["rev-list", "--count", "main..feature/a"]), "1");
    // feature/b kept its own work and now sits on the rewritten feature/a.
    assert_eq!(repo.git(["show", "feature/b:bar.txt"]), "beta");
    assert_eq!(repo.git(["show", "feature/b:foo.txt"]), "alpha fixed");
    assert!(
        repo.git_status(["merge-base", "--is-ancestor", "feature/a", "feature/b"])
            .status
            .success(),
        "feature/b should still be stacked on feature/a"
    );
    assert!(repo.git(["status", "--porcelain"]).is_empty());
}

#[test]
fn absorb_fork_conflict_is_resumable() {
    // feature/b changes the same line feature/a owns, so restacking it onto
    // the absorbed feature/a conflicts.
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("foo.txt", "alpha\n", "add foo");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("foo.txt", "alpha beta\n", "rework foo on b");

    repo.git(["switch", "feature/a"]);
    repo.write("foo.txt", "alpha fixed\n");
    repo.git(["add", "foo.txt"]);

    // The fold lands; the forked branch's restack then conflicts and stops in
    // the standard resumable state rather than rolling the fold back.
    repo.stack()
        .args(["absorb"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("absorbed 1 hunk into 1 commit"))
        .stderr(predicates::str::contains(
            "conflict while rebasing feature/b",
        ))
        .stderr(predicates::str::contains("git stk continue"));

    // The fold is applied (not rolled back), and a restack is in progress.
    assert_eq!(repo.git(["show", "feature/a:foo.txt"]), "alpha fixed");
    repo.stack().args(["abort"]).assert().success();
}

/// `absorb` spans `base..HEAD` with `--update-refs`, so a stack base carrying
/// a stray `stkParent` would put the release line's commits in range and
/// rewrite it. The marker outranks the recorded parent here too.
#[test]
fn absorb_never_rewrites_a_stack_base_that_acquired_a_parent() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    let base_before = repo.git(["rev-parse", "rc-20260817"]);

    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "one\n", "shared work");
    // What `repair` would write from the base's own release PR - both keys,
    // since a recorded fork point would otherwise put the base's own commits
    // back in the owners map.
    repo.git(["config", "branch.rc-20260817.stkParent", "main"]);
    let fork = repo.git(["rev-parse", "main"]);
    repo.git(["config", "branch.rc-20260817.stkBase", &fork]);

    // An edit that belongs to the layer's own commit.
    repo.write("shared.txt", "two\n");
    repo.git(["add", "shared.txt"]);

    repo.stack()
        .args(["absorb"])
        .assert()
        .success()
        // Nor named in the force-push hint: it is not ours to move.
        .stdout(predicates::str::contains(
            "force-with-lease origin fix/shared",
        ))
        .stdout(predicates::str::contains("rc-20260817 fix/shared").not());

    assert_eq!(
        repo.git(["rev-parse", "rc-20260817"]),
        base_before,
        "the base must not have been rewritten"
    );
    // And the layer's own commit did absorb the edit.
    assert_eq!(
        repo.git(["show", "-s", "--format=%s", "fix/shared"]),
        "shared work"
    );
    assert_eq!(
        repo.git(["rev-list", "--count", "rc-20260817..fix/shared"]),
        "1"
    );
}

/// The fallthrough the previous test passes over. `repair` writes `stkBase`
/// alongside the stray `stkParent`, and that recorded fork point would let a
/// floor own its own commits again, routing a hunk of the base's into it.
#[test]
fn absorb_never_routes_a_hunk_into_the_stack_base() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "release\n", "release commit");
    let base_before = repo.git(["rev-parse", "rc-20260817"]);

    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "one\n", "shared work");
    // Both keys, exactly as `repair` would leave them.
    repo.git(["config", "branch.rc-20260817.stkParent", "main"]);
    let fork = repo.git(["rev-parse", "main"]);
    repo.git(["config", "branch.rc-20260817.stkBase", &fork]);

    // A line the *base's* commit introduced: absorbable only if the base owns
    // commits, which it must not.
    repo.write("rc.txt", "release fixed\n");
    repo.git(["add", "rc.txt"]);

    repo.stack()
        .args(["absorb"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "no changes could be attributed to a stack commit",
        ));

    assert_eq!(repo.git(["rev-parse", "rc-20260817"]), base_before);
}
