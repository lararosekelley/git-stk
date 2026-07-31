//! Behavior when a branch in the stack is checked out in a linked worktree.
//! Git refuses to switch to, rebase, or delete such a branch - but it will
//! happily move the ref underneath it, so the cases that matter most here are
//! the ones that would otherwise fail *silently*.

mod common;

use std::path::Path;

use common::TestRepo;
use predicates::prelude::PredicateBooleanExt;

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
fn restack_refuses_before_rewriting_anything_when_a_branch_is_held() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "main"]);
    repo.commit_file("m.txt", "m\n", "trunk moves on");
    repo.git(["switch", "feature/a"]);
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "feature/b"]);

    let a_before = repo.git(["rev-parse", "feature/a"]);
    let b_before = repo.git(["rev-parse", "feature/b"]);

    repo.stack()
        .arg("restack")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "restack would rebase branches checked out in other worktrees",
        ))
        .stderr(predicates::str::contains("feature/b"))
        // Not a conflict, so it must not offer the conflict recovery path.
        .stderr(predicates::str::contains("git stk continue").not());

    // The point of a preflight: feature/a is untouched. Failing mid-loop would
    // have rewritten it before hitting feature/b.
    assert_eq!(repo.git(["rev-parse", "feature/a"]), a_before);
    assert_eq!(repo.git(["rev-parse", "feature/b"]), b_before);
}

#[test]
fn a_worktree_blocked_restack_leaves_no_state_that_wedges_undo() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");
    let tip = stack_with_worktree_on_b(&repo, &worktree);

    // Give the restack real work to do: without this the stack is already up to
    // date and the preflight rightly lets it through.
    repo.git(["switch", "main"]);
    repo.commit_file("m2.txt", "m2\n", "trunk moves again");
    repo.git(["switch", "feature/a"]);

    // Refused by the preflight, which records nothing - so undo is still
    // reachable, and it is only the worktree standing in the way.
    repo.stack().arg("restack").assert().failure();
    repo.stack()
        .arg("undo")
        .assert()
        .failure()
        .stderr(predicates::str::contains("undo would rewind"));

    repo.git(["worktree", "remove", worktree.to_str().unwrap()]);
    repo.stack().arg("undo").assert().success();
    assert_ne!(repo.git(["rev-parse", "feature/b"]), tip);
}

#[test]
fn restack_is_not_blocked_by_a_held_branch_it_would_not_rebase() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");

    // The helper leaves the stack fully restacked, so a second restack has
    // nothing to rebase. The preflight must let that through: refusing a no-op
    // would make a worktree-per-branch layout unusable, since `restack` is run
    // reflexively and would fail every time.
    stack_with_worktree_on_b(&repo, &worktree);

    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains("already up to date"));
    assert!(is_clean(&repo, &worktree));
}

#[test]
fn continue_refuses_when_a_worktree_appeared_while_the_restack_was_paused() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-c");

    // A stack where rebasing B conflicts, with C above it.
    repo.commit_file("file.txt", "base\n", "base");
    repo.stack().args(["new", "a"]).assert().success();
    repo.commit_file("file.txt", "a\n", "a");
    repo.stack().args(["new", "b"]).assert().success();
    repo.commit_file("file.txt", "b\n", "b");
    repo.stack().args(["new", "c"]).assert().success();
    repo.commit_file("other.txt", "c\n", "c");
    repo.git(["switch", "main"]);
    repo.commit_file("file.txt", "moved\n", "trunk moves on");
    repo.git(["switch", "a"]);

    // Conflicts on a, then on b. Resolve a and continue to reach the state that
    // matters: paused mid-rebase on b, with c still to come.
    repo.stack().arg("restack").assert().failure();
    repo.write("file.txt", "resolved a\n");
    repo.git(["add", "file.txt"]);
    repo.stack().arg("continue").assert().failure();

    // The worktree appears while the restack is paused - the only way to reach
    // this, since the initial preflight would have caught it up front.
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "c"]);
    repo.write("file.txt", "resolved b\n");
    repo.git(["add", "file.txt"]);

    let b_before = repo.git(["rev-parse", "b"]);
    let c_before = repo.git(["rev-parse", "c"]);

    // b's ref is still at its pre-rebase tip while the rebase is paused, so c
    // reads as "up to date" unless the preflight knows b is mid-flight. Without
    // that seed, b gets rewritten and c is stranded behind it.
    repo.stack()
        .arg("continue")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "restack would rebase branches checked out in other worktrees",
        ))
        .stderr(predicates::str::contains("c"));

    assert_eq!(repo.git(["rev-parse", "b"]), b_before, "b must not move");
    assert_eq!(repo.git(["rev-parse", "c"]), c_before, "c must not move");
}

#[test]
fn continue_clears_leftover_state_when_no_rebase_is_in_progress() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // Stand in for a run that died before any rebase started: state on file,
    // nothing to continue. This is what used to trap `continue`, `abort` *and*
    // `undo` with no advertised way out.
    let state = repo.git(["rev-parse", "--git-path", "stack-state"]);
    std::fs::write(
        repo.path().join(&state),
        "branch=feature/a\nparent=main\nremaining=\nupdateRefs=false\npush=false\nall=feature/a\nfrozen=\n",
    )
    .expect("write leftover state");

    repo.stack()
        .arg("continue")
        .assert()
        .failure()
        .stderr(predicates::str::contains("nothing to continue"));

    // Cleared, so the stack is usable again rather than permanently wedged.
    assert!(!repo.path().join(&state).exists());
}

#[test]
fn abort_clears_leftover_state_when_no_rebase_is_in_progress() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let state = repo.git(["rev-parse", "--git-path", "stack-state"]);
    std::fs::write(
        repo.path().join(&state),
        "branch=feature/a\nparent=main\nremaining=\nupdateRefs=false\npush=false\nall=feature/a\nfrozen=\n",
    )
    .expect("write leftover state");

    repo.stack()
        .arg("abort")
        .assert()
        .success()
        .stdout(predicates::str::contains("cleared leftover restack state"));
    assert!(!repo.path().join(&state).exists());
}

#[test]
fn abort_with_nothing_to_abort_still_reports_that() {
    let repo = TestRepo::new();
    repo.stack()
        .arg("abort")
        .assert()
        .failure()
        .stderr(predicates::str::contains("no restack to abort"));
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
