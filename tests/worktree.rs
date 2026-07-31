//! Behavior when a branch in the stack is checked out in a linked worktree.
//! Git refuses to switch to, rebase, or delete such a branch - but it will
//! happily move the ref underneath it, so the cases that matter most here are
//! the ones that would otherwise fail *silently*.

mod common;

use std::path::Path;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

const MERGED_A: &str = r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##;
const MERGED_B: &str = r##"[{"number":13,"state":"MERGED","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13"}]"##;

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

/// A landed two-branch stack with a worktree parked on `feature/a`, viewed from
/// `main` so the worktree can hold it. Returns the worktree's parent tempdir,
/// which must stay alive for the worktree to exist.
fn landed_stack_with_worktree_on_a(repo: &TestRepo) -> (tempfile::TempDir, std::path::PathBuf) {
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "main"]);

    let parent = worktree_dir();
    let worktree = parent.path().join("wt-a");
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "feature/a"]);
    (parent, worktree)
}

fn merged_both(repo: &TestRepo) -> common::FakeProviderEnv {
    FakeProvider::new()
        .on("feature/a --state merged", MERGED_A)
        .on("feature/b --state merged", MERGED_B)
        .on("pr edit", "updated child review")
        .fallback("[]")
        .install(repo)
}

#[test]
fn cleanup_keeps_a_worktree_held_branch_and_finishes_the_rest() {
    let repo = TestRepo::new();
    let (_parent, worktree) = landed_stack_with_worktree_on_a(&repo);
    let fake = merged_both(&repo);

    // `git branch -D` exits 1 on a worktree-held branch. Propagating that would
    // abandon feature/b and its metadata partway through the run.
    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "kept feature/a: checked out in the worktree at",
        ))
        .stdout(predicates::str::contains("will delete branch feature/b"));

    // The held branch survives; the rest of the cleanup still happened.
    repo.git(["rev-parse", "--verify", "feature/a"]);
    assert_eq!(
        repo.git_status(["rev-parse", "--verify", "feature/b"])
            .status
            .code(),
        Some(128),
        "feature/b should still have been deleted"
    );
    assert!(is_clean(&repo, &worktree));
}

#[test]
fn cleanup_dry_run_predicts_the_worktree_skip() {
    let repo = TestRepo::new();
    let (_parent, _worktree) = landed_stack_with_worktree_on_a(&repo);
    let fake = merged_both(&repo);

    // A preview that promised "would delete branch feature/a" would be lying
    // about what the real run does.
    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "kept feature/a: checked out in the worktree at",
        ))
        .stdout(predicates::str::contains("would delete branch feature/a").not());
}

#[test]
fn navigation_to_a_held_branch_explains_where_it_lives() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "feature/a"]);
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "feature/b"]);

    // Previously this leaked `fatal: 'feature/b' is already used by worktree at
    // ...` plus `git exited with status exit status: 128`.
    repo.stack()
        .arg("up")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "feature/b is checked out in the worktree at",
        ))
        .stderr(predicates::str::contains("git worktree remove"))
        .stderr(predicates::str::contains("git exited with status").not());

    // Still on the branch we started from.
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

#[test]
fn a_blocked_restack_never_leaks_gits_raw_fatal() {
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

    // Whichever layer stops this - the restack preflight in practice, the git
    // wrapper as a backstop - the user must never see git's own wording.
    repo.stack()
        .arg("restack")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "restack would rebase branches checked out in other worktrees",
        ))
        .stderr(predicates::str::contains("feature/b"))
        .stderr(predicates::str::contains("already used by worktree").not());
}

#[test]
fn a_free_branch_still_checks_out_normally() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-unrelated");

    // A worktree exists, just not on the branch being navigated to - the
    // collision check must not get in the way of ordinary navigation.
    repo.git(["branch", "unrelated"]);
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "unrelated"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    repo.stack()
        .arg("down")
        .assert()
        .success()
        .stdout(predicates::str::contains("switched to main"));
    assert_eq!(repo.git(["branch", "--show-current"]), "main");
}

#[test]
fn list_annotates_a_branch_living_in_another_worktree() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "feature/a"]);
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "feature/b"]);

    let basename = worktree.file_name().unwrap().to_str().unwrap().to_owned();
    let stdout = String::from_utf8(repo.stack_output(["list", "--local"]).stdout).expect("utf8");

    let line_for = |branch: &str| {
        stdout
            .lines()
            .find(|line| line.contains(branch))
            .unwrap_or_else(|| panic!("no line for {branch} in:\n{stdout}"))
    };

    // feature/b is annotated with where it lives. feature/a is checked out
    // right here, so it gets no annotation - that is what makes the column
    // meaningful rather than noise on every row.
    assert!(
        line_for("feature/b").contains(&basename),
        "feature/b should name its worktree:\n{stdout}"
    );
    assert!(
        !line_for("feature/a").contains(&basename),
        "the branch checked out here must not be annotated:\n{stdout}"
    );
}

#[test]
fn status_names_the_worktree_holding_the_branch() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "feature/a"]);
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "feature/b"]);

    repo.stack()
        .args(["status", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains("worktree: "));

    // The branch we are standing on is not "elsewhere", so no line for it.
    repo.stack()
        .args(["status", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("worktree: ").not());
}

#[test]
fn shareable_formats_leave_out_local_worktree_paths() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "feature/a"]);
    repo.git(["worktree", "add", worktree.to_str().unwrap(), "feature/b"]);

    // markdown/plain output gets pasted into PRs and Slack, where another
    // machine's local paths are noise.
    let basename = worktree.file_name().unwrap().to_str().unwrap().to_owned();
    repo.stack()
        .args(["list", "--format", "markdown"])
        .assert()
        .success()
        .stdout(predicates::str::contains(basename).not());
}

/// `main -> feature/a -> feature/b`, sitting on `feature/a`, with `feature/b`
/// checked out in a linked worktree.
fn two_branch_stack_with_b_elsewhere(repo: &TestRepo, at: &Path) {
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "feature/a"]);
    repo.git(["worktree", "add", at.to_str().unwrap(), "feature/b"]);
}

#[test]
fn path_prints_the_worktree_to_cd_into() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");
    two_branch_stack_with_b_elsewhere(&repo, &worktree);

    // The whole point: `cd "$(git stk up --from-path)"` has to succeed and print a
    // path, where plain `up` can only fail - git cannot check this branch out.
    let output = repo.stack_output(["up", "--from-path"]);
    assert!(output.status.success(), "up --from-path should succeed");

    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert_eq!(
        stdout.lines().count(),
        1,
        "stdout must be exactly the path, nothing else:\n{stdout}"
    );
    let printed = stdout.trim();
    assert!(
        worktree.ends_with(Path::new(printed).file_name().unwrap()),
        "printed path {printed} should point at {}",
        worktree.display()
    );

    // Not a checkout - we are still where we started.
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

#[test]
fn path_checks_out_and_prints_dot_for_a_branch_that_lives_here() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");
    two_branch_stack_with_b_elsewhere(&repo, &worktree);

    // Going down is an ordinary checkout. `.` keeps the caller's cwd - printing
    // the repo root would drag them out of any subdirectory they were in.
    let output = repo.stack_output(["down", "--from-path"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf8").trim(),
        ".",
        "a same-worktree target should print `.`"
    );
    assert_eq!(repo.git(["branch", "--show-current"]), "main");

    // The switch is still reported, just on stderr so stdout stays consumable.
    let stderr = String::from_utf8(repo.stack_output(["up", "--from-path"]).stderr).expect("utf8");
    assert!(
        stderr.contains("switched to"),
        "the switch should be announced on stderr:\n{stderr}"
    );
}

#[test]
fn path_prints_a_destination_even_when_navigation_lands_nowhere() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // Already at the top. `cd "$(...)"` on empty output would fail, so `.` has
    // to come out even when nothing moved.
    let output = repo.stack_output(["top", "--from-path"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).expect("utf8").trim(), ".");
    assert!(
        String::from_utf8(output.stderr)
            .expect("utf8")
            .contains("already at the top")
    );
}

#[test]
fn bottom_from_path_navigates_and_stays_put() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // From the top, `bottom` moves to the branch above the trunk and prints `.`
    // because it lives right here.
    let output = repo.stack_output(["bottom", "--from-path"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).expect("utf8").trim(), ".");
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");

    // Already at the bottom: nothing moves, but a destination still has to come
    // out or the caller's `cd` fails on empty input.
    let output = repo.stack_output(["bottom", "--from-path"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).expect("utf8").trim(), ".");
    assert!(
        String::from_utf8(output.stderr)
            .expect("utf8")
            .contains("already at the bottom")
    );
}

#[test]
fn nav_without_path_still_fails_on_a_held_branch() {
    let repo = TestRepo::new();
    let parent = worktree_dir();
    let worktree = parent.path().join("wt-b");
    two_branch_stack_with_b_elsewhere(&repo, &worktree);

    // Without --from-path there is no destination the caller can act on, so the
    // collision stays an error rather than a silent no-op that exits 0.
    repo.stack()
        .arg("up")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "is checked out in the worktree at",
        ));
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
