//! Behavior when a branch in the stack is checked out in a linked worktree.
//! Git refuses to switch to, rebase, or delete such a branch - but it will
//! happily move the ref underneath it, so the cases that matter most here are
//! the ones that would otherwise fail *silently*.

mod common;

use std::path::Path;

use common::TestRepo;

/// Somewhere outside the repo to put a linked worktree. Adding one *inside*
/// the repo leaves an untracked directory, which trips the clean-tree
/// preconditions under test here - and is not how worktrees are used anyway.
fn worktree_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create worktree parent")
}

/// A two-branch stack whose tips have both moved, plus a linked worktree at
/// `at` holding `feature/b`. Returns b's post-restack tip.
fn stack_with_worktree_on_b(repo: &TestRepo, at: &Path) -> String {
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // Move the trunk so the restack actually rewrites both branches.
    repo.git(["switch", "main"]);
    repo.commit_file("m.txt", "m\n", "trunk moves on");
    repo.git(["switch", "feature/a"]);
    repo.stack().arg("restack").assert().success();

    // Restack leaves HEAD on the last branch; step off it so the worktree can
    // take it.
    repo.git(["switch", "feature/a"]);
    repo.git(["worktree", "add", at.to_str().unwrap(), "feature/b"]);

    repo.git(["rev-parse", "feature/b"])
}

fn is_clean(repo: &TestRepo, worktree: &Path) -> bool {
    repo.git(["-C", worktree.to_str().unwrap(), "status", "--porcelain"])
        .is_empty()
}

#[test]
fn undo_refuses_to_rewind_a_branch_held_by_another_worktree() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");
    let tip = stack_with_worktree_on_b(&repo, &worktree);
    assert!(is_clean(&repo, &worktree), "worktree starts clean");

    // update-ref would succeed here without complaint - that is the bug. The
    // linked worktree would keep an index describing the pre-undo commit and
    // silently show staged changes nobody made.
    repo.stack()
        .arg("undo")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "undo would rewind branches checked out in other worktrees",
        ))
        .stderr(predicates::str::contains("feature/b"))
        .stderr(predicates::str::contains("git worktree remove"));

    // The refusal is total: no ref moved, and the held worktree is untouched.
    assert_eq!(repo.git(["rev-parse", "feature/b"]), tip);
    assert!(
        is_clean(&repo, &worktree),
        "a refused undo must leave the held worktree exactly as it was"
    );
}

#[test]
fn a_refused_undo_keeps_the_snapshot_for_a_retry() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");
    let tip = stack_with_worktree_on_b(&repo, &worktree);

    repo.stack().arg("undo").assert().failure();

    // Freeing the worktree makes the same undo work: the refusal consumed
    // nothing, so the user's recovery is exactly "remove it and re-run".
    repo.git(["worktree", "remove", worktree.to_str().unwrap()]);
    repo.stack()
        .arg("undo")
        .assert()
        .success()
        .stdout(predicates::str::contains("undid"));

    assert_ne!(
        repo.git(["rev-parse", "feature/b"]),
        tip,
        "feature/b should be back at its pre-restack tip"
    );
}

#[test]
fn undo_refuses_when_only_the_head_it_would_return_to_is_held() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-a");

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // Move only feature/a, so the restack rewrites feature/b while feature/a's
    // own tip stays exactly where the snapshot records it. The snapshot's head
    // is feature/a, since that is where the restack ran from.
    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "more a\n", "a moves on");
    repo.stack().arg("restack").assert().success();
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");

    repo.git(["worktree", "add", worktree.to_str().unwrap(), "feature/a"]);
    let b_before = repo.git(["rev-parse", "feature/b"]);

    // feature/a is skipped by the ref loop - its sha never moved - so only the
    // head check can catch it.
    repo.stack()
        .arg("undo")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "undo would rewind branches checked out in other worktrees",
        ))
        .stderr(predicates::str::contains("feature/a"));

    // The discriminating assertion: feature/b is untouched. Without the head
    // check, undo rewinds every ref and only then fails on the final
    // `switch feature/a` - failing either way, but with the damage done.
    assert_eq!(
        repo.git(["rev-parse", "feature/b"]),
        b_before,
        "the refusal must come before the first ref moves"
    );
    assert!(is_clean(&repo, &worktree));
}

#[test]
fn undo_ignores_a_worktree_holding_a_branch_it_would_not_move() {
    let repo = TestRepo::new();

    // A branch outside the stack being undone. Holding it blocks nothing,
    // because the restore never touches its ref.
    repo.git(["branch", "unrelated"]);
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-unrelated");
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "unrelated"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "main"]);
    repo.commit_file("m.txt", "m\n", "trunk moves on");
    repo.git(["switch", "feature/a"]);
    repo.stack().arg("restack").assert().success();

    repo.stack().arg("undo").assert().success();
    assert!(is_clean(&repo, &worktree));
}
