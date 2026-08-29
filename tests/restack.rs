use std::fs;
mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

#[test]
fn restack_rebases_descendants_onto_updated_parent() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "first parent change\n", "add parent change");

    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "child change\n", "add child change");

    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "second parent change\n", "update parent");

    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "rebasing feature/b onto feature/a",
        ))
        .stdout(predicates::str::contains("restack complete"));

    let parent_head = repo.git(["rev-parse", "feature/a"]);
    let merge_base = repo.git(["merge-base", "feature/a", "feature/b"]);
    assert_eq!(merge_base, parent_head);
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");
}

#[test]
fn restack_uses_update_refs_when_git_config_enables_it() {
    let repo = TestRepo::new();
    if !repo.supports_update_refs() {
        return;
    }
    repo.git(["config", "stk.updateRefs", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "parent change\n", "add parent change");

    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "child change\n", "add child change");

    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "second parent change\n", "update parent");

    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains("--update-refs"));
}

#[test]
fn restack_can_force_update_refs() {
    let repo = TestRepo::new();
    if !repo.supports_update_refs() {
        return;
    }
    repo.git(["config", "stk.updateRefs", "false"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "parent change\n", "add parent change");

    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "child change\n", "add child change");

    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "second parent change\n", "update parent");

    repo.stack()
        .args(["restack", "--update-refs"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--update-refs"));
}

#[test]
fn restack_can_opt_out_of_update_refs() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.updateRefs", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "parent change\n", "add parent change");

    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "child change\n", "add child change");

    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "second parent change\n", "update parent");

    let output = repo.stack_output(["restack", "--no-update-refs"]);
    assert!(
        output.status.success(),
        "restack failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("--update-refs"));
    assert!(stdout.contains("restack complete"));
}

#[test]
fn restack_dry_run_prints_the_plan_without_rebasing() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "feature/a"]);
    repo.commit_file("a.txt", "a\nmore\n", "a moves on");

    let before = repo.git(["rev-parse", "feature/b"]);
    repo.stack()
        .args(["restack", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a already up to date with main",
        ))
        .stdout(predicates::str::contains(
            "would rebase feature/b onto feature/a",
        ));

    assert_eq!(repo.git(["rev-parse", "feature/b"]), before);
    assert!(!repo.path().join(".git/stack-state").exists());
}

#[test]
fn restack_dry_run_reports_update_refs_and_push() {
    let repo = TestRepo::new();
    if !repo.supports_update_refs() {
        return;
    }
    repo.git(["config", "stk.updateRefs", "true"]);
    repo.git(["config", "stk.pushOnRestack", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "main"]);
    repo.commit_file("main.txt", "main\n", "main moves");
    repo.git(["switch", "feature/a"]);

    repo.stack()
        .args(["restack", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would rebase feature/a onto main with --update-refs",
        ))
        .stdout(predicates::str::contains("would push feature/a to origin"));
}

#[test]
fn restack_is_quiet_by_default_and_loud_with_verbose() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "main"]);
    repo.commit_file("main.txt", "main\n", "main moves");
    repo.git(["switch", "feature/a"]);

    // Quiet by default: the rebase happens, git's narration does not.
    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains("rebasing feature/a onto main"))
        .stderr(predicates::str::contains("Successfully rebased").not());

    repo.git(["switch", "main"]);
    repo.commit_file("main.txt", "main\nmore\n", "main moves again");
    repo.git(["switch", "feature/a"]);

    // --verbose passes git's own output through.
    repo.stack()
        .args(["restack", "--verbose"])
        .assert()
        .success()
        .stderr(predicates::str::contains("Successfully rebased"));
}

#[test]
fn restack_replays_git_output_when_the_rebase_fails() {
    let repo = TestRepo::new();

    repo.commit_file("conflict.txt", "base\n", "add conflict file");
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("conflict.txt", "parent\n", "parent edits conflict file");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("conflict.txt", "parent\nchild\n", "child edits same file");
    repo.git(["switch", "feature/a"]);
    repo.git(["reset", "--hard", "HEAD~1"]);
    repo.commit_file("conflict.txt", "updated parent\n", "update parent");

    // The captured git output comes back on failure, so the conflict keeps
    // its context.
    let assert = repo.stack().arg("restack").assert().failure();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("CONFLICT"),
        "missing conflict context:\n{combined}"
    );
    assert!(combined.contains("resolve conflicts, then run `git stk continue`"));

    repo.stack().arg("abort").assert().success();
}

#[test]
fn restack_records_state_when_rebase_conflicts() {
    let repo = TestRepo::new();

    repo.commit_file("conflict.txt", "base\n", "add conflict file");
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("conflict.txt", "parent\n", "parent edits conflict file");

    repo.stack().args(["new", "feature/b"]).assert().success();
    // The child's own commit touches the conflicting file, so the conflict
    // survives base-aware restacking (a parent rewrite alone no longer
    // conflicts: only the child's own commits are replayed).
    repo.commit_file("conflict.txt", "parent\nchild\n", "child edits same file");

    repo.git(["switch", "feature/a"]);
    repo.git(["reset", "--hard", "HEAD~1"]);
    repo.commit_file(
        "conflict.txt",
        "updated parent\n",
        "update parent differently",
    );

    repo.stack()
        .arg("restack")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "resolve conflicts, then run `git stk continue`",
        ));

    let state = fs::read_to_string(repo.path().join(".git/stack-state")).expect("read stack state");
    assert!(state.contains("branch=feature/b\n"));
    assert!(state.contains("parent=feature/a\n"));
    assert!(state.contains("updateRefs="));
    assert!(state.contains("remaining=\n"));

    let rebase_head = repo.git_status(["rev-parse", "--verify", "REBASE_HEAD"]);
    assert!(rebase_head.status.success(), "expected active rebase");

    repo.stack().arg("abort").assert().success();
    assert!(!repo.path().join(".git/stack-state").exists());
}

#[test]
fn continue_resumes_restack_after_conflict_resolution() {
    let repo = TestRepo::new();

    repo.commit_file("conflict.txt", "base\n", "add conflict file");
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("conflict.txt", "parent\n", "parent edits conflict file");

    repo.stack().args(["new", "feature/b"]).assert().success();
    // Conflict must come from the child's own commit; see
    // restack_records_state_when_rebase_conflicts.
    repo.commit_file("conflict.txt", "parent\nchild\n", "child edits same file");

    repo.git(["switch", "feature/a"]);
    repo.git(["reset", "--hard", "HEAD~1"]);
    repo.commit_file(
        "conflict.txt",
        "updated parent\n",
        "update parent differently",
    );

    repo.stack().arg("restack").assert().failure();

    repo.write("conflict.txt", "updated parent\nchild\n");
    repo.git(["add", "conflict.txt"]);

    repo.stack()
        .arg("continue")
        .assert()
        .success()
        .stdout(predicates::str::contains("restack complete"));

    assert!(!repo.path().join(".git/stack-state").exists());

    let parent_head = repo.git(["rev-parse", "feature/a"]);
    let merge_base = repo.git(["merge-base", "feature/a", "feature/b"]);
    assert_eq!(merge_base, parent_head);
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");

    let conflict_file = fs::read_to_string(repo.path().join("conflict.txt")).expect("read file");
    assert_eq!(conflict_file, "updated parent\nchild\n");

    // continue must refresh the recorded fork point to the new parent tip
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkBase"]),
        parent_head
    );
}

#[test]
fn restack_after_squash_merge_replays_only_child_commits() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    // Parent stack: two commits on feature/a so git's patch-id auto-skip
    // cannot save a naive rebase after the squash merge.
    repo.commit_file("shared.txt", "base\n", "add shared file");
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("shared.txt", "base\none\n", "parent change one");
    repo.commit_file("shared.txt", "base\none\ntwo\n", "parent change two");

    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("shared.txt", "base\none\ntwo\nchild\n", "child change");

    // Simulate GitHub squash-merging feature/a into main.
    repo.git(["switch", "main"]);
    repo.git(["merge", "--squash", "feature/a"]);
    repo.git(["commit", "-m", "parent changes (#1)"]);

    // cleanup: feature/a merged -> retarget feature/b to main, record fork point.
    let old_parent_tip = repo.git(["rev-parse", "feature/a"]);
    let fake = FakeProvider::new()
        .on(
            "feature/a --state merged",
            r##"[{"number":1,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/lararosekelley/git-stk/pull/1"}]"##,
        )
        .on("feature/a", "[]")
        .on(
            "feature/b",
            r##"[{"number":2,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/lararosekelley/git-stk/pull/2"}]"##,
        )
        .on("pr edit", "updated child review")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["cleanup", "feature/a"])
        .assert()
        .success();

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkBase"]),
        old_parent_tip
    );

    // Without the recorded fork point this rebase replays the parent's
    // commits onto the squashed main and conflicts. With it, only the
    // child's own commit replays, cleanly.
    repo.git(["switch", "feature/b"]);
    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains("restack complete"));

    let own_commits = repo.git(["rev-list", "--count", "main..feature/b"]);
    assert_eq!(own_commits, "1");
    let shared = fs::read_to_string(repo.path().join("shared.txt")).expect("read shared file");
    assert_eq!(shared, "base\none\ntwo\nchild\n");
}

#[test]
fn restack_falls_back_to_plain_rebase_when_base_is_invalid() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "feature work");
    repo.git([
        "config",
        "branch.feature/a.stkBase",
        "0000000000000000000000000000000000000000",
    ]);

    repo.git(["switch", "main"]);
    repo.commit_file("main.txt", "main\n", "main moves on");
    repo.git(["switch", "feature/a"]);

    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains("restack complete"));

    let merge_base = repo.git(["merge-base", "main", "feature/a"]);
    assert_eq!(merge_base, repo.git(["rev-parse", "main"]));
}

#[test]
fn restack_no_ops_when_a_stale_base_still_sits_on_the_trunk_tip() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    // The fork point recorded at creation: main's tip back then.
    let old_base = repo.git(["config", "--get", "branch.feature/a.stkBase"]);

    // Trunk advances several commits.
    repo.git(["switch", "main"]);
    repo.commit_file("m1.txt", "m1\n", "trunk moves");
    repo.commit_file("m2.txt", "m2\n", "trunk moves again");
    let trunk_tip = repo.git(["rev-parse", "main"]);

    // Move feature/a onto the new trunk *outside git-stk*, so stkBase is never
    // refreshed and still points at the old, now-stale fork point.
    repo.git(["switch", "feature/a"]);
    repo.git(["rebase", "--onto", "main", &old_base, "feature/a"]);
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkBase"]),
        old_base,
        "precondition: stkBase is a stale-but-still-ancestor fork point"
    );
    let before = repo.git(["rev-parse", "feature/a"]);

    // feature/a already sits on the trunk tip, so restack must no-op rather
    // than replay the already-upstream trunk commits from the stale base.
    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a already up to date with main",
        ))
        .stdout(predicates::str::contains("rebasing feature/a").not());

    // Untouched: still exactly one commit above the current trunk tip.
    assert_eq!(repo.git(["rev-parse", "feature/a"]), before);
    assert_eq!(repo.git(["rev-parse", "feature/a~1"]), trunk_tip);
}

#[test]
fn restack_push_flag_pushes_rewritten_branches() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);

    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "a2\n", "parent moves");

    repo.stack()
        .args(["restack", "--push"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "pushed feature/a feature/b to origin",
        ));

    assert_eq!(
        repo.remote_sha(&bare, "feature/b"),
        repo.git(["rev-parse", "feature/b"])
    );
}

#[test]
fn restack_leaves_sibling_stacks_sharing_the_trunk_alone() {
    let repo = TestRepo::new();

    // Two independent stacks, both anchored on main.
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "main"]);
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // Advance main so both stacks are behind it (neither is a no-op restack).
    repo.git(["switch", "main"]);
    repo.commit_file("m.txt", "m\n", "trunk moves");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);
    let sibling_remote = repo.remote_sha(&bare, "feature/b");
    let sibling_local = repo.git(["rev-parse", "feature/b"]);

    // Restack from feature/a: only its own line should move.
    repo.git(["switch", "feature/a"]);
    repo.stack()
        .args(["restack", "--push"])
        .assert()
        .success()
        .stdout(predicates::str::contains("rebasing feature/a onto main"))
        .stdout(predicates::str::contains("pushed feature/a to origin"))
        .stdout(predicates::str::contains("feature/b").not());

    // The sibling was neither rebased nor pushed, and we stayed on our own
    // branch rather than landing on the sibling's tip.
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
    assert_eq!(repo.git(["rev-parse", "feature/b"]), sibling_local);
    assert_eq!(repo.remote_sha(&bare, "feature/b"), sibling_remote);
}

#[test]
fn restack_freezes_a_branch_in_a_merge_queue() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);
    let frozen_remote = repo.remote_sha(&bare, "feature/a");

    // Advance the trunk so feature/a would normally be rebased onto it.
    repo.git(["switch", "main"]);
    repo.commit_file("m.txt", "m\n", "trunk moves");
    repo.git(["push", "origin", "main"]);

    // GitHub provider, with feature/a reported as sitting in the merge queue.
    repo.git(["config", "stk.provider", "github"]);
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"higharc/product"}"#)
        .on(
            "head=feature/a",
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"mergeQueueEntry":{"state":"QUEUED"}}]}}}}"#,
        )
        .on(
            "graphql",
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"mergeQueueEntry":null}]}}}}"#,
        )
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    let frozen_local = repo.git(["rev-parse", "feature/a"]);

    repo.stack_faked(&fake)
        .args(["restack", "--push"])
        .assert()
        .success()
        .stdout(predicates::str::contains("frozen feature/a"))
        // Its rebase onto the moved trunk is skipped entirely.
        .stdout(predicates::str::contains("rebasing feature/a").not())
        .stdout(predicates::str::contains("pushed feature/a").not());

    // The queued branch was neither rebased locally nor force-pushed.
    assert_eq!(repo.git(["rev-parse", "feature/a"]), frozen_local);
    assert_eq!(repo.remote_sha(&bare, "feature/a"), frozen_remote);
}

#[test]
fn restack_freezing_a_branch_also_freezes_everything_below_it() {
    // A non-bottom branch in the queue must freeze its base too: rebasing and
    // pushing feature/a would move feature/b's base out from under its locked
    // queue entry.
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);
    let base_remote = repo.remote_sha(&bare, "feature/a");

    // Advance the trunk so feature/a would normally rebase onto it.
    repo.git(["switch", "main"]);
    repo.commit_file("m.txt", "m\n", "trunk moves");
    repo.git(["push", "origin", "main"]);

    // Only feature/b (the upper branch) is in the queue.
    repo.git(["config", "stk.provider", "github"]);
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"higharc/product"}"#)
        .on(
            "head=feature/b",
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"mergeQueueEntry":{"state":"QUEUED"}}]}}}}"#,
        )
        .on(
            "graphql",
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"mergeQueueEntry":null}]}}}}"#,
        )
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    let base_local = repo.git(["rev-parse", "feature/a"]);

    repo.stack_faked(&fake)
        .args(["restack", "--push"])
        .assert()
        .success()
        // The freeze propagates down: feature/a is held even though only
        // feature/b is queued, and nothing is pushed.
        .stdout(predicates::str::contains("frozen feature/a"))
        .stdout(predicates::str::contains("frozen feature/b"))
        .stdout(predicates::str::contains("rebasing feature/a").not())
        .stdout(predicates::str::contains("pushed feature").not())
        .stdout(predicates::str::contains("nothing to push"));

    assert_eq!(repo.git(["rev-parse", "feature/a"]), base_local);
    assert_eq!(repo.remote_sha(&bare, "feature/a"), base_remote);
}

#[test]
fn continue_preserves_frozen_branches_read_back_from_the_state_file() {
    // The freeze must survive `continue` reading it back from .git/stack-state
    // (the `frozen=` round-trip). Build a branching stack where a queued branch
    // (feature/b) freezes its base (feature/a), while a sibling line
    // (feature/d) conflicts on rebase and forces a continue. After resolving,
    // the post-continue push must still exclude the frozen branches - if the
    // deserialization silently returned an empty set, continue would push them.
    let repo = TestRepo::new();

    repo.commit_file("conflict.txt", "base\n", "base");
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("conflict.txt", "parent\n", "a edits conflict file");

    // feature/b: child of feature/a, reported as queued below.
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // feature/d: a sibling of feature/b under feature/a, editing the same file
    // so its rebase onto a diverged feature/a conflicts.
    repo.git(["switch", "feature/a"]);
    repo.stack().args(["new", "feature/d"]).assert().success();
    repo.commit_file("conflict.txt", "parent\nd\n", "d edits conflict file");

    // Diverge feature/a so the rebase of feature/d onto it conflicts.
    repo.git(["switch", "feature/a"]);
    repo.git(["reset", "--hard", "HEAD~1"]);
    repo.commit_file("conflict.txt", "updated parent\n", "a edits differently");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b", "feature/d"]);
    let frozen_a = repo.remote_sha(&bare, "feature/a");
    let frozen_b = repo.remote_sha(&bare, "feature/b");

    repo.git(["config", "stk.provider", "github"]);
    let fake = FakeProvider::new()
        .on("repo view", r#"{"nameWithOwner":"higharc/product"}"#)
        .on(
            "head=feature/b",
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"mergeQueueEntry":{"state":"QUEUED"}}]}}}}"#,
        )
        .on(
            "graphql",
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"mergeQueueEntry":null}]}}}}"#,
        )
        .install(&repo);

    // Initial restack: feature/a and feature/b freeze; feature/d conflicts and
    // the state is saved with frozen=feature/a,feature/b.
    repo.git(["switch", "feature/d"]);
    repo.stack_faked(&fake)
        .args(["restack", "--push"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("frozen feature/a"))
        .stdout(predicates::str::contains("frozen feature/b"));

    // Resolve and continue - which reads the frozen set back from the file.
    repo.write("conflict.txt", "updated parent\nd\n");
    repo.git(["add", "conflict.txt"]);
    repo.stack_faked(&fake)
        .arg("continue")
        .assert()
        .success()
        // Only the non-frozen branch lands; the frozen ones are still excluded.
        .stdout(predicates::str::contains("pushed feature/d to origin"))
        .stdout(predicates::str::contains("feature/a").not())
        .stdout(predicates::str::contains("feature/b").not());

    assert!(!repo.path().join(".git/stack-state").exists());
    // The frozen branches' remotes were never force-updated by the continue.
    assert_eq!(repo.remote_sha(&bare, "feature/a"), frozen_a);
    assert_eq!(repo.remote_sha(&bare, "feature/b"), frozen_b);
}

#[test]
fn restack_prints_push_hint_when_not_pushing() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);
    let stale = repo.remote_sha(&bare, "feature/b");

    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "a2\n", "parent moves");

    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "git push --force-with-lease origin feature/a feature/b",
        ));

    // No push happened.
    assert_eq!(repo.remote_sha(&bare, "feature/b"), stale);
}

#[test]
fn restack_push_respects_config_and_no_push_overrides_it() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.pushOnRestack", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);

    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "a2\n", "parent moves");

    // Config enables the push.
    repo.stack().arg("restack").assert().success();
    assert_eq!(
        repo.remote_sha(&bare, "feature/b"),
        repo.git(["rev-parse", "feature/b"])
    );

    // --no-push overrides the config.
    repo.git(["switch", "feature/a"]);
    repo.commit_file("a3.txt", "a3\n", "parent moves again");
    let before = repo.remote_sha(&bare, "feature/b");

    repo.stack()
        .args(["restack", "--no-push"])
        .assert()
        .success()
        .stdout(predicates::str::contains("remote branches may be stale"));
    assert_eq!(repo.remote_sha(&bare, "feature/b"), before);
}

#[test]
fn restack_fetch_updates_the_trunk_before_rebasing() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let bare = repo.add_bare_origin(&["main", "feature/a"]);

    // The trunk advances on the remote, then the local trunk rewinds behind it:
    // the misleading setup where feature/a reads as up to date with main while
    // origin/main has moved on.
    repo.git(["switch", "main"]);
    repo.commit_file("m.txt", "m\n", "trunk moves on the remote");
    repo.git(["push", "origin", "main"]);
    repo.git(["reset", "--hard", "HEAD~1"]);
    repo.git(["switch", "feature/a"]);

    let trunk_before = repo.git(["rev-parse", "main"]);

    repo.stack()
        .args(["restack", "--fetch"])
        .assert()
        .success()
        .stdout(predicates::str::contains("fetched main from origin"))
        .stdout(predicates::str::contains("rebasing feature/a onto main"));

    // The local trunk fast-forwarded to the remote tip, and feature/a now sits
    // directly on top of it.
    assert_ne!(repo.git(["rev-parse", "main"]), trunk_before);
    assert_eq!(
        repo.git(["rev-parse", "main"]),
        repo.remote_sha(&bare, "main")
    );
    assert_eq!(
        repo.git(["rev-parse", "feature/a~1"]),
        repo.git(["rev-parse", "main"])
    );
}

#[test]
fn restack_warns_when_a_base_is_behind_its_remote() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let _bare = repo.add_bare_origin(&["main", "feature/a"]);

    // Remote trunk advances; the local trunk rewinds behind it.
    repo.git(["switch", "main"]);
    repo.commit_file("m.txt", "m\n", "trunk moves on the remote");
    repo.git(["push", "origin", "main"]);
    repo.git(["reset", "--hard", "HEAD~1"]);
    repo.git(["switch", "feature/a"]);

    // Without --fetch the branch still reads as up to date with the stale local
    // trunk, but the warning names origin/main as the real, moved-on base.
    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains("already up to date with main"))
        .stderr(predicates::str::contains(
            "main is 1 commit behind origin/main",
        ))
        .stderr(predicates::str::contains("git stk restack --fetch"));
}

#[test]
fn continue_after_conflict_pushes_all_restacked_branches() {
    let repo = TestRepo::new();

    repo.commit_file("conflict.txt", "base\n", "add conflict file");
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("conflict.txt", "parent\n", "parent edits conflict file");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("conflict.txt", "parent\nchild\n", "child edits same file");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);

    repo.git(["switch", "feature/a"]);
    repo.git(["reset", "--hard", "HEAD~1"]);
    repo.commit_file(
        "conflict.txt",
        "updated parent\n",
        "update parent differently",
    );
    repo.git(["push", "--force-with-lease", "origin", "feature/a"]);

    repo.stack().args(["restack", "--push"]).assert().failure();

    repo.write("conflict.txt", "updated parent\nchild\n");
    repo.git(["add", "conflict.txt"]);

    repo.stack()
        .arg("continue")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "pushed feature/a feature/b to origin",
        ));

    assert_eq!(
        repo.remote_sha(&bare, "feature/b"),
        repo.git(["rev-parse", "feature/b"])
    );
}

#[test]
fn restack_ignores_rebase_update_refs_git_config() {
    let repo = TestRepo::new();
    if !repo.supports_update_refs() {
        return;
    }
    // Git's own config must no longer influence restack; only stk.updateRefs.
    repo.git(["config", "rebase.updateRefs", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");
    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "a2\n", "parent moves");

    let output = repo.stack_output(["restack"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("--update-refs"),
        "rebase.updateRefs must not enable --update-refs: {stdout}"
    );
}

#[test]
fn restack_covers_whole_stack_from_the_leaf() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    repo.git(["switch", "main"]);
    repo.commit_file("main.txt", "main\n", "main moves on");

    // Standing on the LEAF: the whole stack must rebase, including
    // feature/a below us.
    repo.git(["switch", "feature/b"]);
    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains("rebasing feature/a onto main"))
        .stdout(predicates::str::contains(
            "rebasing feature/b onto feature/a",
        ));

    assert_eq!(
        repo.git(["merge-base", "main", "feature/a"]),
        repo.git(["rev-parse", "main"])
    );
}

#[test]
fn restack_skips_branches_already_on_their_parent() {
    let repo = TestRepo::new();
    // The combination that caused needless rewrites: update-refs forces a
    // full replay unless we skip aligned branches ourselves.
    repo.git(["config", "stk.updateRefs", "true"]);
    if !repo.supports_update_refs() {
        return;
    }

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    repo.git(["switch", "main"]);
    repo.commit_file("main.txt", "m\n", "main moves");
    repo.git(["switch", "feature/b"]);

    // First restack does real work.
    repo.stack().arg("restack").assert().success();
    let a_sha = repo.git(["rev-parse", "feature/a"]);
    let b_sha = repo.git(["rev-parse", "feature/b"]);

    // Second restack must change nothing - same hashes, no replays.
    repo.stack()
        .arg("restack")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a already up to date with main",
        ))
        .stdout(predicates::str::contains(
            "feature/b already up to date with feature/a",
        ));
    assert_eq!(repo.git(["rev-parse", "feature/a"]), a_sha);
    assert_eq!(repo.git(["rev-parse", "feature/b"]), b_sha);
}

/// Helper: push a commit onto `branch`'s remote that the local branch will not
/// have, mimicking a commit made straight on the host (web UI, committed
/// suggestion, a bot). Leaves local `branch` one commit behind its remote, with
/// a genuinely distinct change so it has no local patch-equivalent.
fn add_remote_only_commit(repo: &TestRepo, branch: &str, file: &str, message: &str) {
    repo.git(["switch", branch]);
    repo.commit_file(file, "from the web\n", message);
    repo.git(["push", "origin", branch]);
    repo.git(["reset", "--hard", "HEAD~1"]);
}

#[test]
fn restack_offers_to_incorporate_remote_only_commits_then_pushes() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);

    // A commit lands on feature/a's remote that the local stack never saw.
    add_remote_only_commit(&repo, "feature/a", "web.txt", "web edit on feature/a");
    repo.git(["switch", "feature/b"]);

    // Answer the incorporate prompt with yes.
    repo.stack()
        .args(["restack", "--push"])
        .write_stdin("y\n")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "origin/feature/a has 1 commit not in your local feature/a",
        ))
        .stderr(predicates::str::contains("web edit on feature/a"))
        .stdout(predicates::str::contains(
            "incorporated remote commits into feature/a",
        ))
        .stdout(predicates::str::contains("pushed"));

    // The web commit is now on the local branch and preserved on the remote,
    // and the descendant rebased onto the reconciled tip.
    assert!(
        repo.path().join("web.txt").exists() || {
            repo.git(["switch", "feature/a"]);
            repo.path().join("web.txt").exists()
        }
    );
    assert_eq!(
        repo.remote_sha(&bare, "feature/a"),
        repo.git(["rev-parse", "feature/a"])
    );
    assert_eq!(
        repo.remote_sha(&bare, "feature/b"),
        repo.git(["rev-parse", "feature/b"])
    );
    // feature/b now contains the incorporated web change through its parent.
    assert!(
        repo.git(["log", "--format=%s", "feature/b"])
            .contains("web edit on feature/a")
    );
}

#[test]
fn restack_declining_remote_only_commits_bails_without_pushing() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);
    add_remote_only_commit(&repo, "feature/a", "web.txt", "web edit on feature/a");
    let remote_a = repo.remote_sha(&bare, "feature/a");
    let remote_b = repo.remote_sha(&bare, "feature/b");
    repo.git(["switch", "feature/b"]);

    // Decline the prompt: fail with actionable guidance, no "run sync" loop,
    // and nothing pushed.
    repo.stack()
        .args(["restack", "--push"])
        .write_stdin("n\n")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "remote branches have commits not in your local stack",
        ))
        .stderr(predicates::str::contains("run `git stk sync`").not());

    assert_eq!(repo.remote_sha(&bare, "feature/a"), remote_a);
    assert_eq!(repo.remote_sha(&bare, "feature/b"), remote_b);
    assert!(!repo.path().join("web.txt").exists());
}

#[test]
fn restack_dry_run_reports_remote_only_commits() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);
    add_remote_only_commit(&repo, "feature/a", "web.txt", "web edit on feature/a");
    let remote_a = repo.remote_sha(&bare, "feature/a");
    repo.git(["switch", "feature/b"]);

    repo.stack()
        .args(["restack", "--push", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would offer to cherry-pick these into your local branches",
        ));

    // Dry run changed nothing.
    assert_eq!(repo.remote_sha(&bare, "feature/a"), remote_a);
    assert!(!repo.path().join("web.txt").exists());
}

/// The base a stack sits on is never rebased or force-pushed, even when it has
/// picked up a stack parent from somewhere else - `git stk repair` reading its
/// own release PR, say. The marker outranks the recorded parent (#308).
#[test]
fn restack_never_rewrites_a_stack_base_that_acquired_a_parent() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");

    let bare = repo.add_bare_origin(&["main", "rc-20260817", "fix/shared"]);
    let base_before = repo.remote_sha(&bare, "rc-20260817");

    // What `repair` would write from the base's own release PR into the trunk.
    repo.git(["config", "branch.rc-20260817.stkParent", "main"]);
    // And the trunk moves, so a rebase would have something to replay onto.
    repo.git(["switch", "main"]);
    repo.commit_file("trunk.txt", "t\n", "trunk moves");
    repo.git(["switch", "fix/shared"]);

    repo.stack()
        .args(["restack", "--push"])
        .assert()
        .success()
        // It may be named as the parent above it targets, never as something
        // being rebased.
        .stdout(predicates::str::contains("rc-20260817 onto").not());

    assert_eq!(
        repo.remote_sha(&bare, "rc-20260817"),
        base_before,
        "the base must not have been rewritten or force-pushed"
    );
}

/// The squash-merge shape from #311. The remote branch carries the two commits
/// a PR was squash-merged from; the local branch has been rebased onto a trunk
/// that already contains that work, so those commits are gone on purpose. The
/// squash's patch-id matches neither original, so `--cherry-pick` still reads
/// them as missing - but the trees are identical, and there is nothing to
/// incorporate. With `stk.mergeStrategy = squash` this otherwise fires after
/// every merge in a stack.
#[test]
fn restack_ignores_remote_commits_a_squash_merge_already_landed() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "one\n", "first of the squashed pair");
    repo.commit_file("a2.txt", "two\n", "second of the squashed pair");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);
    let remote_a = repo.remote_sha(&bare, "feature/a");

    // The trunk takes both changes as a single squashed commit, exactly as a
    // squash merge would: same content, one commit, unrelated patch-ids.
    repo.git(["switch", "main"]);
    repo.write("a.txt", "one\n");
    repo.write("a2.txt", "two\n");
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "squashed pair (#1)"]);

    // And the local branch is rebased onto it, dropping the originals - which
    // is what `sync` does, and is correct.
    repo.git(["switch", "feature/a"]);
    repo.git(["reset", "--hard", "main"]);

    repo.stack()
        .args(["restack", "--push", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("cherry-pick").not())
        .stdout(predicates::str::contains("not in your local").not());

    // Nothing was pushed by the dry run, and the remote still has the originals.
    assert_eq!(repo.remote_sha(&bare, "feature/a"), remote_a);
}
