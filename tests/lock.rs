mod common;

use common::TestRepo;

/// Stand in for another git-stk process holding the operation lock.
fn hold_lock(repo: &TestRepo) {
    std::fs::write(repo.path().join(".git/stk-lock"), "99999 merge\n").expect("write lock");
}

#[test]
fn mutating_command_refuses_while_locked() {
    let repo = TestRepo::new();
    hold_lock(&repo);

    repo.stack()
        .args(["new", "feature/x"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "another git stk operation is in progress",
        ))
        // The holder line is surfaced so the message is actionable.
        .stderr(predicates::str::contains("99999 merge"));
}

#[test]
fn linked_worktrees_share_one_lock() {
    let repo = TestRepo::new();

    // A linked worktree of the same repo: it shares the common git dir, and so
    // the same `branch.*` stack metadata the lock guards.
    repo.git(["branch", "wt-branch"]);
    let worktree = repo.path().join("linked-wt");
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "wt-branch"]);

    // The main worktree holds the lock (written under the shared common dir).
    hold_lock(&repo);

    // A mutating command from the linked worktree must see that one lock and
    // refuse - a per-worktree lock would let it clobber the shared metadata.
    repo.stack_in(&worktree)
        .args(["new", "feature/x"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "another git stk operation is in progress",
        ));
}

#[test]
fn run_is_lock_guarded() {
    // `run` rewrites nothing, but it holds the lock for the whole window it
    // walks the stack (see lock_name in main.rs). Pin that: it must refuse
    // while another operation holds the lock, like any mutating command.
    let repo = TestRepo::new();
    hold_lock(&repo);

    repo.stack()
        .args(["run", "--", "true"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "another git stk operation is in progress",
        ));
}

#[test]
fn read_only_command_ignores_the_lock() {
    let repo = TestRepo::new();
    hold_lock(&repo);

    // Navigation/read-only commands are safe to run alongside anything.
    repo.stack().args(["list"]).assert().success();
}

#[test]
fn mutating_command_releases_the_lock_when_done() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/x"]).assert().success();

    assert!(
        !repo.path().join(".git/stk-lock").exists(),
        "the lock file should be gone once the command finishes"
    );
}

#[test]
fn mutating_command_releases_the_lock_on_failure() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/x"]).assert().success();

    // A second `new` of the same branch acquires the lock, then errors. The
    // lock is RAII, so it must still be cleaned up - otherwise a single failed
    // command would wedge the repo for every later one.
    repo.stack().args(["new", "feature/x"]).assert().failure();

    assert!(
        !repo.path().join(".git/stk-lock").exists(),
        "the lock must be released even when the command errors"
    );

    // Proof it is actually free: the next mutating command succeeds.
    repo.stack().args(["new", "feature/y"]).assert().success();
}
