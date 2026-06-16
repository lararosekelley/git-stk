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
