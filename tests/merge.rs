use std::fs;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

mod common;

#[test]
fn merge_merges_bottom_review_then_syncs() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.pushOnRestack", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    let bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);

    // Stateful fake: after `pr merge 12` runs, feature/a reports as merged.
    let fake = FakeProvider::new()
        .record("pr merge 12", "merge-args.txt", "")
        .on_after("feature/a --state merged", "merge-args.txt", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##)
        .on("feature/a --state merged", "[]")
        .on_after("feature/a", "merge-args.txt", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"B work"}]"##)
        .on("pr edit", "updated review")
        .fallback("[]")
        .install(&repo);

    // Run from the leaf with -y: position-independent and unprompted.
    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("merged A work (#12)"))
        .stdout(predicates::str::contains("next up: feature/b -> #13"));

    // The provider was asked to squash-merge (the default strategy).
    let recorded = fs::read_to_string(repo.path().join("merge-args.txt")).expect("merge args");
    assert_eq!(recorded.trim(), "pr merge 12 --squash");

    // The sync swept up afterwards: branch gone, child retargeted and pushed.
    assert_eq!(
        repo.git_status(["branch", "--list", "feature/a"])
            .stdout
            .len(),
        0
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
    assert_eq!(
        repo.remote_sha(&bare, "feature/b"),
        repo.git(["rev-parse", "feature/b"])
    );
}

#[test]
fn merge_drops_squashed_parent_commits_when_provider_retargets_the_child() {
    // Regression: feature/a is squash-merged (a new commit on main), and the
    // provider auto-retargets feature/b's review to main (GitLab does this when
    // the parent branch is deleted). sync must pin feature/b's fork point to
    // feature/a's tip so the restack drops the squashed commit instead of
    // replaying it into an add/add conflict.
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.pushOnRestack", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    let _bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);

    // Squash-merge feature/a by hand: main gains a commit adding a.txt with
    // different content, so replaying feature/a's own commit add/add-conflicts.
    repo.git(["switch", "main"]);
    repo.write("a.txt", "a-squashed\n");
    repo.git(["add", "a.txt"]);
    repo.git(["commit", "-m", "squash merge feature/a"]);
    repo.git(["push", "origin", "main"]);

    // feature/a reads merged; feature/b is OPEN and already retargeted to main.
    let fake = FakeProvider::new()
        .record("pr merge 12", "merge-args.txt", "")
        .on_after("feature/a --state merged", "merge-args.txt", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##)
        .on("feature/a --state merged", "[]")
        .on_after("feature/a", "merge-args.txt", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"B work"}]"##)
        .on("pr edit", "updated review")
        .fallback("[]")
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("merged A work (#12)"));

    // feature/b rebased onto main with only its own commit replayed: exactly one
    // commit above main, adding b.txt, while a.txt is main's squashed content -
    // feature/a's original commit was dropped, with no conflict.
    assert_eq!(
        repo.git(["rev-parse", "feature/b~1"]),
        repo.git(["rev-parse", "main"])
    );
    assert_eq!(repo.git(["show", "feature/b:a.txt"]), "a-squashed");
    assert_eq!(repo.git(["show", "feature/b:b.txt"]), "b");
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
}

#[test]
fn merge_respects_strategy_config() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.mergeStrategy", "rebase"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .record("pr merge 12", "merge-args.txt", "")
        .on_after("feature/a", "merge-args.txt", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success();

    let recorded = fs::read_to_string(repo.path().join("merge-args.txt")).expect("merge args");
    assert_eq!(recorded.trim(), "pr merge 12 --rebase");
}

#[test]
fn merge_dry_run_and_decline_merge_nothing() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .record("pr merge", "merged.txt", "")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would merge A work (#12) into main (squash)",
        ));
    assert!(!repo.path().join("merged.txt").exists());

    repo.stack_faked(&fake)
        .args(["merge", "--dry-run", "--auto"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would merge A work (#12) into main (squash, auto)",
        ));
    assert!(!repo.path().join("merged.txt").exists());

    repo.stack_faked(&fake)
        .args(["merge"])
        .write_stdin("n\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("merge cancelled"));
    assert!(!repo.path().join("merged.txt").exists());
}

#[test]
fn merge_all_lands_the_whole_stack() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    let _bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);

    // Stateful fake: each `pr merge` flips its PR to merged, and the
    // retarget from the first sync moves #13's base to main.
    let fake = FakeProvider::new()
        .record("pr merge 12", "merge-args-12.txt", "")
        .record("pr merge 13", "merge-args-13.txt", "")
        .record("pr edit 13 --base", "base-13.txt", "")
        .on_after("feature/a --state merged", "merge-args-12.txt", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("feature/a --state merged", "[]")
        .on_after("feature/a", "merge-args-12.txt", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on_after("feature/b --state merged", "merge-args-13.txt", r##"[{"number":13,"state":"MERGED","baseRefName":"main","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("feature/b --state merged", "[]")
        .on_after("feature/b", "merge-args-13.txt", "[]")
        .on_after("feature/b", "base-13.txt", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("merged A work (#12)"))
        .stdout(predicates::str::contains("merged B work (#13)"))
        .stdout(predicates::str::contains(
            "stack complete: everything merged into main",
        ))
        .stdout(predicates::str::contains(
            "merge complete: 2 of 2 reviews merged",
        ));

    let first = fs::read_to_string(repo.path().join("merge-args-12.txt")).expect("merge 12");
    assert_eq!(first.trim(), "pr merge 12 --squash");
    let second = fs::read_to_string(repo.path().join("merge-args-13.txt")).expect("merge 13");
    assert_eq!(second.trim(), "pr merge 13 --squash");

    assert_eq!(repo.git(["branch", "--show-current"]), "main");
    assert_eq!(
        repo.git_status(["branch", "--list", "feature/a", "feature/b"])
            .stdout
            .len(),
        0
    );
}

#[test]
fn merge_all_dry_run_lists_each_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    let fake = FakeProvider::new()
        .record("pr merge", "merged.txt", "")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would merge A work (#12) into main (squash)",
        ))
        .stdout(predicates::str::contains(
            "would merge B work (#13) into feature/a (squash)",
        ))
        .stdout(predicates::str::contains("would sync after each merge"));

    assert!(!repo.path().join("merged.txt").exists());
}

#[test]
fn merge_all_stops_when_a_merge_only_schedules() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // The first merge only schedules (the PR stays open), so the loop must
    // stop without touching the rest of the stack.
    let fake = FakeProvider::new()
        .record("pr merge 12", "merge-args-12.txt", "")
        .record("pr merge", "unexpected-merge.txt", "")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "merge scheduled for A work (#12)",
        ))
        .stdout(predicates::str::contains(
            "merge complete: 0 of 2 reviews merged",
        ));

    assert!(!repo.path().join("unexpected-merge.txt").exists());
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");
}

#[test]
fn merge_all_resumes_from_the_new_bottom_after_a_partial_landing() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    let _bare = repo.add_bare_origin(&["main", "feature/a", "feature/b"]);

    // Run 1: feature/a merges and syncs (so feature/b retargets onto main),
    // but feature/b's merge only schedules - it stays open - so the loop stops
    // with one of two landed.
    let run1 = FakeProvider::new()
        .record("pr merge 12", "merge-12.txt", "")
        .record("pr edit 13 --base", "base-13.txt", "")
        .record("pr merge 13", "merge-13-run1.txt", "")
        .on_after("feature/a --state merged", "merge-12.txt", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("feature/a --state merged", "[]")
        .on_after("feature/a", "merge-12.txt", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        // feature/b stays open even after its merge (a scheduled merge); its
        // base flips to main once the first sync retargets it.
        .on_after("feature/b", "base-13.txt", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&run1)
        .args(["merge", "--all", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("merged A work (#12)"))
        .stdout(predicates::str::contains(
            "merge scheduled for B work (#13)",
        ))
        .stdout(predicates::str::contains(
            "merge complete: 1 of 2 reviews merged",
        ));

    // feature/a is gone, feature/b survives on main as the new bottom.
    assert_eq!(
        repo.git_status(["branch", "--list", "feature/a"])
            .stdout
            .len(),
        0
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );

    // Run 2: rerun `merge --all`. It picks up from the new bottom (feature/b)
    // and lands the remainder.
    let run2 = FakeProvider::new()
        .record("pr merge 13", "merge-13-run2.txt", "")
        .on_after("feature/b --state merged", "merge-13-run2.txt", r##"[{"number":13,"state":"MERGED","baseRefName":"main","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("feature/b --state merged", "[]")
        .on_after("feature/b", "merge-13-run2.txt", "[]")
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&run2)
        .args(["merge", "--all", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("merged B work (#13)"))
        .stdout(predicates::str::contains(
            "stack complete: everything merged into main",
        ));

    // The whole stack is now landed: both feature branches are gone.
    assert_eq!(
        repo.git_status(["branch", "--list", "feature/a", "feature/b"])
            .stdout
            .len(),
        0
    );
    // Each branch was squash-merged exactly once across the two runs.
    assert_eq!(
        fs::read_to_string(repo.path().join("merge-12.txt"))
            .expect("merge 12")
            .trim(),
        "pr merge 12 --squash"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("merge-13-run2.txt"))
            .expect("merge 13 run 2")
            .trim(),
        "pr merge 13 --squash"
    );
}

#[test]
fn merge_all_wait_gates_each_merge_on_checks() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // Checks are green, so the wait clears and the merge follows.
    let fake = FakeProvider::new()
        .record("pr checks 12", "checks-args.txt", "")
        .record("pr merge 12", "merge-args.txt", "")
        .on_after("feature/a --state merged", "merge-args.txt", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("feature/a --state merged", "[]")
        .on_after("feature/a", "merge-args.txt", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "--wait", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("waiting for checks on #12"))
        .stdout(predicates::str::contains("merged A work (#12)"))
        .stdout(predicates::str::contains(
            "merge complete: 1 of 1 review merged",
        ));

    // The gate ran `gh pr checks` (no `--watch` in the poll model).
    let checks = fs::read_to_string(repo.path().join("checks-args.txt")).expect("checks args");
    assert_eq!(checks.trim(), "pr checks 12");
}

#[test]
fn merge_all_wait_stops_when_checks_fail() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    // The config default turns waiting on without the flag.
    repo.git(["config", "stk.mergeWait", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // A genuine failure prints the checks table to stdout and exits non-zero.
    let fake = FakeProvider::new()
        .fail_with_stdout("pr checks 12", "X  lint  1m  failing", "")
        .record("pr merge", "merged.txt", "")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("checks failed for #12"));
    assert!(!repo.path().join("merged.txt").exists());
}

#[test]
fn merge_all_wait_surfaces_a_gh_error_instead_of_reporting_checks_failed() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.mergeWait", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // gh itself fails operationally: an error on stderr, no table on stdout.
    // This must not be reported as a failed check, and must not merge.
    let fake = FakeProvider::new()
        .fail("pr checks 12", "error connecting to api.github.com: timeout")
        .fail("pr merge", "merge should not run")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("could not read checks for #12"))
        .stderr(predicates::str::contains(
            "error connecting to api.github.com",
        ));
}

#[test]
fn merge_all_no_wait_overrides_the_config() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.mergeWait", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // No `pr checks` handler: a checks call would fall through and fail
    // the wait, so success proves it never ran.
    let fake = FakeProvider::new()
        .fail("pr checks", "checks should not run")
        .record("pr merge 12", "merge-args.txt", "")
        .on_after("feature/a --state merged", "merge-args.txt", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("feature/a --state merged", "[]")
        .on_after("feature/a", "merge-args.txt", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "--no-wait", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("merged A work (#12)"));
}

#[test]
fn merge_all_conflicts_with_auto() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack()
        .args(["merge", "--all", "--auto"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}

#[test]
fn merge_auto_schedules_and_skips_the_sync() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // The PR stays open after `pr merge --auto`: scheduled, not merged.
    let fake = FakeProvider::new()
        .record("pr merge 12", "merge-args.txt", "")
        .on("feature/a --state merged", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y", "--auto"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "merge scheduled for A work (#12)",
        ));

    let recorded = fs::read_to_string(repo.path().join("merge-args.txt")).expect("merge args");
    assert_eq!(recorded.trim(), "pr merge 12 --squash --auto");
    // No sync ran: the branch survives untouched.
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

#[test]
fn merge_hints_when_required_checks_block_it() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .fail(
            "pr merge 12",
            "GraphQL: Required status check \"ci\" is expected. (mergePullRequest)",
        )
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "#12's required checks are not green yet - wait and rerun \
             `git stk merge`, or schedule with `git stk merge --auto`",
        ))
        // The raw gh/GraphQL error is not surfaced on top of the hint.
        .stderr(predicates::str::contains("GraphQL").not());
}

#[test]
fn merge_reports_a_scheduled_gitlab_auto_merge() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // glab exits 0 after scheduling the merge; the MR stays open.
    let fake = FakeProvider::new()
        .on("mr merge 34", "merge scheduled to run when pipeline succeeds")
        .on("feature/a", r##"[{"iid":34,"state":"opened","target_branch":"main","source_branch":"feature/a","web_url":"https://gitlab.com/owner/repo/-/merge_requests/34","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "merge scheduled for A work (!34)",
        ));

    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

#[test]
fn merge_requires_an_open_review_at_the_bottom() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "no github review found for feature/a; submit the stack first",
        ));
}

#[test]
fn merge_reports_pending_checks_from_structured_status() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // The merge is rejected with an error whose text matches neither substring
    // fallback ("status check"/"not mergeable"); only the structured status
    // (mergeStateStatus=BLOCKED) explains why.
    let fake = FakeProvider::new()
        .fail("pr merge 12", "GraphQL: something went wrong")
        .on(
            "mergeStateStatus",
            r#"{"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED"}"#,
        )
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "#12's required checks are not green yet",
        ));
}

#[test]
fn merge_reports_conflicts_from_structured_status() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .fail("pr merge 12", "GraphQL: something went wrong")
        .on(
            "mergeStateStatus",
            r#"{"mergeable":"CONFLICTING","mergeStateStatus":"DIRTY"}"#,
        )
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"A work"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("#12 conflicts with main"));
}

#[test]
fn merge_all_wait_gives_up_when_checks_never_settle() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.mergeWait", "true"]);
    // A 1s ceiling so the test gives up after one poll instead of the 30m default.
    repo.git(["config", "stk.checkTimeout", "1"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    // Checks never register: without a ceiling this would poll forever. The
    // merge must never run.
    let fake = FakeProvider::new()
        .fail("pr checks 12", "no checks reported on the 'feature/a' branch")
        .fail("pr merge", "merge should not run")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "#12's checks have not settled within 1s",
        ))
        .stderr(predicates::str::contains("raise stk.checkTimeout"));
}

#[test]
fn merge_all_wait_stops_when_the_review_is_merged_out_of_band() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    let _bare = repo.add_bare_origin(&["main", "feature/a"]);

    // The checks never settle (exit 8, pending), but the first poll records
    // `checks-ran.txt`, after which the PR reads as merged - someone landed it
    // on the web while we waited. The wait must notice and sync instead of
    // re-merging or hanging until checkTimeout. `pr merge` must never run.
    let fake = FakeProvider::new()
        .fail("pr merge", "merge should not run")
        .record_pending("pr checks 12", "checks-ran.txt", "*  ci  pending  0s")
        .on_after("feature/a --state merged", "checks-ran.txt", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("feature/a --state merged", "[]")
        .on_after("feature/a", "checks-ran.txt", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "--wait", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("waiting for checks on #12"))
        .stdout(predicates::str::contains(
            "#12 was merged outside git-stk; syncing instead",
        ))
        .stdout(predicates::str::contains(
            "merge complete: 1 of 1 review merged",
        ));

    // The sync swept up the branch the out-of-band merge landed.
    assert_eq!(
        repo.git_status(["branch", "--list", "feature/a"])
            .stdout
            .len(),
        0
    );
}

/// A stack rooted on a release line: that base branch is not a layer of the
/// stack, so its own review is never what `merge` lands (#307). Before the
/// fix, the base's release PR was the stack bottom.
#[test]
fn merge_never_lands_the_review_of_a_parentless_base() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();

    let fake = FakeProvider::new()
        .record("pr merge", "merged.txt", "")
        // The release branch has its own open PR into the trunk.
        .on("rc-20260817", r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on("fix/shared", r##"[{"number":13,"state":"OPEN","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would merge Shared fix (#13) into rc-20260817",
        ))
        .stdout(predicates::str::contains("#99").not());

    assert!(!repo.path().join("merged.txt").exists());
}

/// `merge --all` plans and counts the stack's own layers, not the base it
/// sits on.
#[test]
fn merge_all_excludes_a_parentless_base_from_the_plan_and_the_count() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.stack().args(["new", "fix/above"]).assert().success();

    let fake = FakeProvider::new()
        .on("rc-20260817", r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on("fix/shared", r##"[{"number":13,"state":"OPEN","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .on("fix/above", r##"[{"number":14,"state":"OPEN","baseRefName":"fix/shared","headRefName":"fix/above","url":"https://example.com/14","title":"Above fix"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would merge Shared fix (#13) into rc-20260817",
        ))
        .stdout(predicates::str::contains(
            "would merge Above fix (#14) into fix/shared",
        ))
        .stdout(predicates::str::contains("Release 20260817").not());
}

/// Standing on a branch with no stack parent, there is a branch but no base to
/// merge it into. Say which branch, and point at the metadata commands.
#[test]
fn merge_on_an_unstacked_branch_names_it_and_the_remedy() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");

    let fake = FakeProvider::new()
        .record("pr merge", "merged.txt", "")
        .on("rc-20260817", r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "rc-20260817 has no stack parent, so there is no base to merge it into",
        ))
        .stderr(predicates::str::contains("git stk adopt --parent <parent>"))
        .stderr(predicates::str::contains("git stk repair"));

    assert!(!repo.path().join("merged.txt").exists());
}

/// `merge --all` syncs between merges, and `sync` re-records the base's parent
/// from its own review - so the base can become the stack bottom mid-run and
/// land unprompted on a later iteration. The up-front confirmation named it as
/// the destination, so nothing ever offered its review for approval (#307).
#[test]
fn merge_all_never_lands_the_base_review_after_a_sync_readopts_it() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");
    repo.stack().args(["new", "fix/above"]).assert().success();
    repo.commit_file("above.txt", "above\n", "above work");
    repo.git(["switch", "fix/shared"]);
    let _bare = repo.add_bare_origin(&["main", "rc-20260817", "fix/shared", "fix/above"]);

    let fake = FakeProvider::new()
        .record("pr merge 13", "merge-13.txt", "")
        .record("pr merge 14", "merge-14.txt", "")
        .record("pr merge 99", "merge-99.txt", "")
        .record("pr edit 14 --base", "base-14.txt", "")
        // The release line's own PR into the trunk, open throughout.
        .on("rc-20260817", r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on_after("fix/shared --state merged", "merge-13.txt", r##"[{"number":13,"state":"MERGED","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .on("fix/shared --state merged", "[]")
        .on_after("fix/shared", "merge-13.txt", "[]")
        .on("fix/shared", r##"[{"number":13,"state":"OPEN","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .on_after("fix/above --state merged", "merge-14.txt", r##"[{"number":14,"state":"MERGED","baseRefName":"rc-20260817","headRefName":"fix/above","url":"https://example.com/14","title":"Above fix"}]"##)
        .on("fix/above --state merged", "[]")
        .on_after("fix/above", "merge-14.txt", "[]")
        .on_after("fix/above", "base-14.txt", r##"[{"number":14,"state":"OPEN","baseRefName":"rc-20260817","headRefName":"fix/above","url":"https://example.com/14","title":"Above fix"}]"##)
        .on("fix/above", r##"[{"number":14,"state":"OPEN","baseRefName":"fix/shared","headRefName":"fix/above","url":"https://example.com/14","title":"Above fix"}]"##)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .success();

    // The release PR must never be merged, on any iteration.
    assert!(
        !repo.path().join("merge-99.txt").exists(),
        "the base's own review was merged: {}",
        fs::read_to_string(repo.path().join("merge-99.txt")).unwrap_or_default()
    );
    // And the stack's own top layer must not be stranded by it.
    assert!(
        repo.path().join("merge-14.txt").exists(),
        "fix/above never landed"
    );
}

/// Same question, same answer: `merge` from the trunk must see a stack rooted
/// off it rather than reporting the repo has none.
#[test]
fn merge_on_the_trunk_sees_an_off_trunk_stack() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.git(["switch", "main"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "you are on the trunk (main); check out a stacked branch first",
        ))
        .stderr(predicates::str::contains("no stacked branches").not());
}

/// A stacked pull request cannot go through `gh pr merge` - GitHub requires
/// the asynchronous endpoint, because landing a layer also retargets the ones
/// above it. `merge` must take that path and wait for the result.
#[test]
fn merge_uses_githubs_async_endpoint_for_a_stacked_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}}]}]"##;
    let fake = FakeProvider::new()
        .record("pr merge", "sync-merge.txt", "")
        .record(
            "api repos/owner/repo/pulls/12/merge-async -X PUT",
            "async.txt",
            r##"{"status":"merged","details":{"message":"Pull request was merged.","sha":"abc"}}"##,
        )
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success();

    let call = fs::read_to_string(repo.path().join("async.txt")).expect("async merge call");
    assert!(call.contains("merge_method=squash"), "got: {call}");
    assert!(
        !repo.path().join("sync-merge.txt").exists(),
        "`gh pr merge` must not be attempted: GitHub rejects it for a stacked review"
    );
}

/// A merge queue on the base branch decides the merge method itself, and
/// GitHub rejects `merge_method` outright rather than ignoring it - as a
/// `failed` status on a 200, so it arrives looking like a failed merge. Ask
/// again without the parameter and let the queue take the review.
#[test]
fn merge_retries_without_the_method_when_a_merge_queue_refuses_it() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}}]}]"##;
    let refusal = r##"{"status":"failed","details":{"message":"Custom merge params are not supported when merging via a merge queue"}}"##;
    let queued = r##"{"status":"enqueued","details":{"uuid":"u-1"}}"##;
    let fake = FakeProvider::new()
        // Specific rule first: both calls contain the second needle, and the
        // first rule to match wins.
        .record_append(
            "merge-async -X PUT -f merge_method=squash",
            "async.txt",
            refusal,
        )
        .record_append("merge-async -X PUT", "async.txt", queued)
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success()
        .stderr(predicates::str::contains("GitHub refused squash for #12"));

    let calls = fs::read_to_string(repo.path().join("async.txt")).expect("async merge calls");
    let calls: Vec<&str> = calls.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(calls.len(), 2, "got: {calls:?}");
    assert!(calls[0].contains("merge_method=squash"), "got: {calls:?}");
    assert!(!calls[1].contains("merge_method"), "got: {calls:?}");
}

/// The note describes a merge that happened, so a re-send that never lands
/// must not leave it on screen - here with a non-zero exit, and below with the
/// shape that actually matters. `-X PUT` with no `-f` is a distinct call, so
/// the fake can answer it while the first attempt still returns the refusal.
#[test]
fn merge_says_nothing_about_the_method_when_the_retry_errors() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}}]}]"##;
    let refusal = r##"{"status":"failed","details":{"message":"Custom merge params are not supported when merging via a merge queue"}}"##;
    let fake = FakeProvider::new()
        .on("merge-async -X PUT -f merge_method=squash", refusal)
        // Not a transient error, so the enqueue is not retried around this.
        .fail("merge-async -X PUT", "HTTP 422: Unprocessable Entity")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on(
            "pr view 12 --json mergeable,mergeStateStatus",
            r##"{"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"}"##,
        )
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Unprocessable Entity"))
        .stderr(predicates::str::contains("GitHub refused squash").not());
}

/// The failure that matters: an async merge reports `failed` on a `200`, so
/// the re-send exits zero and still merged nothing. Guarding only the non-zero
/// exit above would announce the override right over the error saying the
/// merge did not happen.
#[test]
fn merge_says_nothing_about_the_method_when_the_retry_reports_a_failure() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}}]}]"##;
    let refusal = r##"{"status":"failed","details":{"message":"Custom merge params are not supported when merging via a merge queue"}}"##;
    let failed = r##"{"status":"failed","details":{"message":"Base branch was modified. Review and try the merge again."}}"##;
    let fake = FakeProvider::new()
        .on("merge-async -X PUT -f merge_method=squash", refusal)
        // Exits zero, like every async-merge answer: the verdict is the body.
        .on("merge-async -X PUT", failed)
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on(
            "pr view 12 --json mergeable,mergeStateStatus",
            r##"{"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"}"##,
        )
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Base branch was modified"))
        .stderr(predicates::str::contains("GitHub refused squash").not());
}

/// Only the complaint about the parameter is retried. A merge that failed for
/// a real reason must be reported as it was, not quietly re-sent without the
/// strategy the user asked for.
#[test]
fn merge_reports_a_real_async_failure_without_retrying() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}}]}]"##;
    let failed = r##"{"status":"failed","details":{"message":"Base branch was modified. Review and try the merge again."}}"##;
    let fake = FakeProvider::new()
        .record_append("merge-async -X PUT", "async.txt", failed)
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on(
            "pr view 12 --json mergeable,mergeStateStatus",
            r##"{"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"}"##,
        )
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Base branch was modified"));

    let calls = fs::read_to_string(repo.path().join("async.txt")).expect("async merge calls");
    let calls: Vec<&str> = calls.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(
        calls.len(),
        1,
        "a real failure must not be re-sent: {calls:?}"
    );
}

/// `--auto` asks for a merge *when checks pass*. The async endpoint has no
/// such mode, so a stacked review must refuse rather than merge now - which
/// would land the code early, the opposite of what was asked.
#[test]
fn merge_auto_refuses_a_stacked_review_rather_than_merging_now() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}}]}]"##;
    let fake = FakeProvider::new()
        .record("merge-async", "async.txt", "")
        .record("pr merge", "sync-merge.txt", "")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        // The blocker lookup must be answered, and answered as *blocked*: the
        // refusal has to survive `explain_merge_failure`, which would
        // otherwise re-diagnose it as pending checks and reply by
        // recommending the very flag that was just refused. Without this the
        // fallback answers, no blocker is found, and the test passes for the
        // wrong reason.
        .on(
            "pr view 12 --json mergeable,mergeStateStatus",
            r##"{"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED"}"##,
        )
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y", "--auto"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "#12 is in a GitHub stack, which cannot be scheduled with --auto",
        ))
        // And not answered with the flag it just refused.
        .stderr(predicates::str::contains("--auto once checks").not());

    assert!(
        !repo.path().join("async.txt").exists(),
        "must not merge now"
    );
    assert!(!repo.path().join("sync-merge.txt").exists());
}

/// The wait, not just the enqueue. GitHub answers the `PUT` with `pending` and
/// a result id, and the merge is only done once a poll says so - so this is
/// the path a real stacked merge takes, and the one nothing reached before.
#[test]
fn merge_polls_an_async_merge_until_it_lands() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}},
        {"number":13,"head":{"ref":"above"}}]}]"##;
    let fake = FakeProvider::new()
        .record("pr merge", "sync-merge.txt", "")
        // The enqueue answers `pending` with a uuid; the poll answers `merged`.
        .record(
            "merge-async -X PUT",
            "async.txt",
            r##"{"status":"pending","details":{"message":"Merge request enqueued.","uuid":"u-1"}}"##,
        )
        .record(
            "pulls/12/merge-async/u-1",
            "poll.txt",
            r##"{"status":"merged","details":{"message":"Pull request was merged.","sha":"abc"}}"##,
        )
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("merged #12 (stacked)"));

    assert!(
        repo.path().join("poll.txt").exists(),
        "the result was never polled for"
    );
    assert!(
        !repo.path().join("sync-merge.txt").exists(),
        "`gh pr merge` is rejected for a stacked review and must never be reached"
    );
}

/// And when the polls run out it is not an error: `merge_and_check` re-reads
/// the review and reports the merge as scheduled, which `--all` breaks on
/// cleanly. Returning `Err` would route a mid-merge state through
/// `explain_merge_failure` and be diagnosed as pending checks.
#[test]
fn merge_reports_an_async_merge_that_outlasts_the_polls() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}},
        {"number":13,"head":{"ref":"above"}}]}]"##;
    let pending = r##"{"status":"pending","details":{"message":"Still going.","uuid":"u-1"}}"##;
    let fake = FakeProvider::new()
        .record("pr merge", "sync-merge.txt", "")
        .on("merge-async -X PUT", pending)
        // Never reaches a final status.
        .on("pulls/12/merge-async/u-1", pending)
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("merge scheduled"));
}

/// Polls that never answer are reported as that, not as a merge still
/// running. The merge is still GitHub's to finish, so this is not an error -
/// but two minutes of failed requests summarised as "still merging" names the
/// wrong thing, and the user is about to hit the same failure in `sync`.
#[test]
fn merge_says_so_when_the_async_result_cannot_be_read() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}},
        {"number":13,"head":{"ref":"above"}}]}]"##;
    let fake = FakeProvider::new()
        .record("pr merge", "sync-merge.txt", "")
        .on(
            "merge-async -X PUT",
            r##"{"status":"pending","details":{"message":"Merge request enqueued.","uuid":"u-1"}}"##,
        )
        // The enqueue lands; every poll after it fails.
        .fail("pulls/12/merge-async/u-1", "HTTP 401: Bad credentials")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("its result could not be read"))
        .stdout(predicates::str::contains("Bad credentials"))
        .stdout(predicates::str::contains("is still merging").not());
}

/// `merge --all` over a two-layer stack where GitHub has not retargeted the
/// upper layer yet - the sibling of
/// `merge_all_lands_a_registered_stack_through_the_async_endpoint`, which
/// covers the same loop once the retarget has happened. `cleanup` moves the local parent to the trunk but stands
/// down from the retarget, so for a moment the review's base and the local
/// parent disagree - and the check that notices sends the user to
/// `git stk submit`, which refuses for a review in a stack. The loop must
/// carry on instead: the base is GitHub's to move, and it will.
#[test]
fn merge_all_carries_on_when_github_has_not_retargeted_yet() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "ma/a"]);
    let _bare = repo.add_bare_origin(&["main", "ma/a", "ma/b"]);

    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"ma/a"}},
        {"number":13,"head":{"ref":"ma/b"}}]}]"##;
    let merged =
        r##"{"status":"merged","details":{"message":"Pull request was merged.","sha":"abc"}}"##;
    let fake = FakeProvider::new()
        .record("pr merge", "sync-merge.txt", "")
        .record("pulls/12/merge-async -X PUT", "async-12.txt", merged)
        .record("pulls/13/merge-async -X PUT", "async-13.txt", merged)
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on_after("ma/a --state merged", "async-12.txt", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"ma/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("ma/a --state merged", "[]")
        .on_after("ma/a", "async-12.txt", "[]")
        .on("ma/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"ma/a","url":"https://example.com/12","title":"A work"}]"##)
        .on_after("ma/b --state merged", "async-13.txt", r##"[{"number":13,"state":"MERGED","baseRefName":"ma/a","headRefName":"ma/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("ma/b --state merged", "[]")
        .on_after("ma/b", "async-13.txt", "[]")
        // Still pointing at ma/a after #12 lands: GitHub retargets on its own
        // clock, and this is the window before it does.
        .on("ma/b", r##"[{"number":13,"state":"OPEN","baseRefName":"ma/a","headRefName":"ma/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "merge complete: 2 of 2 reviews merged",
        ))
        .stderr(predicates::str::contains("run `git stk submit` first").not());

    assert!(
        repo.path().join("async-13.txt").exists(),
        "the upper layer never landed"
    );
}

/// The exemption above must not reach the stack's bottom. Nothing lands below
/// it, so GitHub never retargets it - a base that disagrees with the local
/// parent there is the ordinary re-rooted-line bug, and merging anyway would
/// land the branch into the wrong place. Re-root a registered line with
/// `adopt` and the guard has to hold.
#[test]
fn merge_still_refuses_a_stack_bottom_whose_base_went_stale() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);

    // A release line, with the stack re-rooted onto it after submitting.
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["switch", "ma/a"]);

    // ma/a is the stack's bottom, and its review still targets main.
    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"ma/a"}},
        {"number":13,"head":{"ref":"ma/b"}}]}]"##;
    let fake = FakeProvider::new()
        .record("pr merge", "sync-merge.txt", "")
        .record("merge-async", "async.txt", "")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("ma/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"ma/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("ma/b", r##"[{"number":13,"state":"OPEN","baseRefName":"ma/a","headRefName":"ma/b","url":"https://example.com/13","title":"B work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "review #12 targets main, but ma/a's stack parent is rc-20260817",
        ));

    assert!(
        !repo.path().join("async.txt").exists(),
        "the bottom was merged into the wrong branch"
    );
    assert!(!repo.path().join("sync-merge.txt").exists());
}

/// Exemption is temporal, not positional. A landed layer keeps its place in
/// the listing, so "not the first entry" stays true forever - and once the
/// platform has moved this base onto the stack's own base, it will not move it
/// again. A local parent that disagrees after that is a real fault, and
/// merging anyway would land the branch somewhere the prompt did not name.
#[test]
fn merge_refuses_a_layer_whose_retarget_already_happened() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);

    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "ma/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // ma/a landed and stays listed; ma/b was retargeted onto the stack's base
    // by the platform, so nothing further is coming - but the local parent is
    // the release line.
    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"ma/a"}},
        {"number":13,"head":{"ref":"ma/b"}}]}]"##;
    let fake = FakeProvider::new()
        .record("pr merge", "sync-merge.txt", "")
        .record("merge-async", "async.txt", "")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("ma/b", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"ma/b","url":"https://example.com/13","title":"B work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        // And the honest remedy, not `submit`, which refuses here.
        .stderr(predicates::str::contains("git stk unstack"));

    assert!(
        !repo.path().join("async.txt").exists(),
        "the layer was merged into a branch the prompt did not name"
    );
    assert!(!repo.path().join("sync-merge.txt").exists());
}

/// A stack can exist without git-stk registering it - a teammate's
/// `gh stack submit`, the web UI. GitHub refuses the ordinary merge for those
/// pull requests too, so detection must not be gated on `stk.githubStacks`.
#[test]
fn merge_uses_the_async_endpoint_for_a_stack_git_stk_did_not_register() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    // Deliberately NOT set: someone else made this stack.
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}}]}]"##;
    let fake = FakeProvider::new()
        .record("pr merge", "sync-merge.txt", "")
        .record(
            "merge-async -X PUT",
            "async.txt",
            r##"{"status":"merged","details":{"message":"Pull request was merged.","sha":"abc"}}"##,
        )
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .success();

    assert!(
        repo.path().join("async.txt").exists(),
        "a stack we did not register is still a stack"
    );
    assert!(
        !repo.path().join("sync-merge.txt").exists(),
        "`gh pr merge` is rejected by GitHub here, so it must not be attempted"
    );
}

/// `merge --all` over a registered stack, with GitHub having retargeted the
/// upper layer *before* the loop looks: every layer goes through the async
/// endpoint and `gh pr merge` is never reached.
///
/// The sibling case - GitHub not having retargeted yet - is
/// `merge_all_carries_on_when_github_has_not_retargeted_yet`. The two fixtures
/// are the two sides of `NativeStack::can_base_on`: there `#13`'s base is a
/// layer, so the gap is the platform's to close; here it is already the
/// stack's own base, so `open_review_for` finds nothing to reconcile and
/// returns before the predicate is consulted at all.
#[test]
fn merge_all_lands_a_registered_stack_through_the_async_endpoint() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    let _bare = repo.add_bare_origin(&["main", "ma/a", "ma/b"]);

    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"ma/a"}},
        {"number":13,"head":{"ref":"ma/b"}}]}]"##;
    let merged =
        r##"{"status":"merged","details":{"message":"Pull request was merged.","sha":"abc"}}"##;
    let fake = FakeProvider::new()
        .record("pr merge", "sync-merge.txt", "")
        .record("pulls/12/merge-async -X PUT", "async-12.txt", merged)
        .record("pulls/13/merge-async -X PUT", "async-13.txt", merged)
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        // After #12 lands, GitHub has retargeted #13 to main itself.
        .on_after("ma/a --state merged", "async-12.txt", r##"[{"number":12,"state":"MERGED","baseRefName":"main","headRefName":"ma/a","url":"https://example.com/12","title":"A work"}]"##)
        .on("ma/a --state merged", "[]")
        .on_after("ma/a", "async-12.txt", "[]")
        .on("ma/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"ma/a","url":"https://example.com/12","title":"A work"}]"##)
        .on_after("ma/b --state merged", "async-13.txt", r##"[{"number":13,"state":"MERGED","baseRefName":"main","headRefName":"ma/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("ma/b --state merged", "[]")
        .on_after("ma/b", "async-13.txt", "[]")
        .on_after("ma/b", "async-12.txt", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"ma/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("ma/b", r##"[{"number":13,"state":"OPEN","baseRefName":"ma/a","headRefName":"ma/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("pr edit", "edited")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "merge complete: 2 of 2 reviews merged",
        ));

    assert!(repo.path().join("async-12.txt").exists(), "#12 via async");
    assert!(repo.path().join("async-13.txt").exists(), "#13 via async");
    assert!(
        !repo.path().join("sync-merge.txt").exists(),
        "`gh pr merge` is rejected for a stacked review and must never be reached"
    );
}

/// A merge queue taking a stacked review is not a failure and not something to
/// wait on - it lands on its own schedule, and `merge --all` stops there
/// rather than pressing on into a stack that has not moved yet.
#[test]
fn merge_reports_a_stacked_review_taken_by_the_merge_queue() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"ma/a"}},
        {"number":13,"head":{"ref":"ma/b"}}]}]"##;
    let fake = FakeProvider::new()
        .record(
            "pulls/12/merge-async -X PUT",
            "async.txt",
            r##"{"status":"enqueued","details":{"message":"Added to the merge queue."}}"##,
        )
        .record("pulls/13/merge-async -X PUT", "async-13.txt", "{}")
        // GitHub rejects `gh pr merge` for a review in a stack, so reaching it
        // is a bug. Without this, an unmatched rule falls through to the
        // `[]` fallback and the synchronous path looks like a plausible run.
        .record("pr merge", "sync-merge.txt", "")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("ma/b", r##"[{"number":13,"state":"OPEN","baseRefName":"ma/a","headRefName":"ma/b","url":"https://example.com/13","title":"B work"}]"##)
        .on("ma/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"ma/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    // `merge --all` over a two-layer stack: the queue takes the bottom, so
    // the loop must stop there rather than press on over a stack that has not
    // moved. That `break` is the behaviour the CHANGELOG claims.
    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("#12 added to the merge queue"))
        .stdout(predicates::str::contains(
            "merge complete: 0 of 2 reviews merged",
        ))
        .stdout(predicates::str::contains("#13").not());
    assert!(
        !repo.path().join("sync-merge.txt").exists(),
        "`gh pr merge` is rejected for a stacked review and must never be reached"
    );
}

/// A failed async merge is an error, not a message - `merge --all` must stop
/// rather than carry on to the next layer over a stack that did not move.
#[test]
fn merge_surfaces_a_failed_async_merge() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"ma/a"}},
        {"number":13,"head":{"ref":"ma/b"}}]}]"##;
    let fake = FakeProvider::new()
        .record("pulls/13/merge-async -X PUT", "async-13.txt", "{}")
        // See above: `gh pr merge` is refused for a stacked review.
        .record("pr merge", "sync-merge.txt", "")
        .on("ma/b", r##"[{"number":13,"state":"OPEN","baseRefName":"ma/a","headRefName":"ma/b","url":"https://example.com/13","title":"B work"}]"##)
        .record(
            "pulls/12/merge-async -X PUT",
            "async.txt",
            // The `failed` status is read in `await_async_merge`, outside the
            // retry - which wraps only the enqueue, since the `PUT` is not
            // idempotent. So the wording is not about retries at all: it must
            // stay off the "checks are not green" path in
            // `explain_merge_failure`, or the test would pass on a rewritten
            // diagnosis rather than on "failed is an error".
            r##"{"status":"failed","details":{"message":"Required checks have not passed."}}"##,
        )
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("ma/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"ma/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    // And a failure aborts `--all` rather than carrying on to the layer above.
    repo.stack_faked(&fake)
        .args(["merge", "--all", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("#12 could not be merged"));
    assert!(
        !repo.path().join("async-13.txt").exists(),
        "the layer above must not be merged over a stack that did not move"
    );
    assert!(
        !repo.path().join("sync-merge.txt").exists(),
        "`gh pr merge` is rejected for a stacked review and must never be reached"
    );
}

/// Registering a stack was a one-way door: turning `stk.githubStacks` off left
/// every stack it created still registered, with GitHub still refusing the
/// ordinary merge and retarget for those reviews.
#[test]
fn unstack_dissolves_the_platform_stack() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    // Deliberately off: undoing must not need the setting that created it.
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();

    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"ma/a"}},
        {"number":13,"head":{"ref":"ma/b"}}]}]"##;
    let fake = FakeProvider::new()
        .record("stacks/5/unstack -X POST", "unstack.txt", "{}")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .fallback("[]")
        .install(&repo);

    // Dry run says what it would do and dissolves nothing.
    repo.stack_faked(&fake)
        .args(["unstack", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would dissolve stack 5 (#12 #13)",
        ));
    assert!(!repo.path().join("unstack.txt").exists());

    repo.stack_faked(&fake)
        .args(["unstack", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("dissolved stack 5 on GitHub"));
    assert!(repo.path().join("unstack.txt").exists());
}

/// The dissolve is destructive on the platform and has no undo - `undo`
/// restores local metadata, and this is a `POST` - so it asks first, naming
/// every stack it would take apart. A stack reaches past the line that asked,
/// so what is at stake is not visible from where the user is standing.
#[test]
fn unstack_asks_before_dissolving_anything() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();

    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"ma/a"}},
        {"number":13,"head":{"ref":"ma/b"}},
        {"number":14,"head":{"ref":"elsewhere"}}]}]"##;
    let fake = FakeProvider::new()
        .record("stacks/5/unstack -X POST", "unstack.txt", "{}")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .fallback("[]")
        .install(&repo);

    // No `-y`, and nothing on stdin: the prompt reads that as a no.
    repo.stack_faked(&fake)
        .args(["unstack"])
        .assert()
        .success()
        // Named before the question, including the review outside the line.
        .stdout(predicates::str::contains(
            "will dissolve stack 5 (#12 #13 #14)",
        ))
        .stdout(predicates::str::contains("unstack cancelled"));
    assert!(
        !repo.path().join("unstack.txt").exists(),
        "a declined prompt must dissolve nothing"
    );
}

/// Nothing recorded is not an error - it is the ordinary answer for a stack
/// git-stk never registered, and for every provider but GitHub.
#[test]
fn unstack_says_so_when_there_is_no_platform_stack() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", "[]")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["unstack", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no platform stack recorded"));
}

/// The lookup *is* this command, so a failed one must not read as "already
/// dissolved" - that would tell the user the stack is gone while it is still
/// registered.
#[test]
fn unstack_surfaces_a_failed_lookup_rather_than_calling_it_gone() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .fail("repos/owner/repo/stacks", "HTTP 401: Bad credentials")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["unstack", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "could not read this repository's stacks from GitHub",
        ))
        .stderr(predicates::str::contains("nothing to dissolve").not());
}

/// A stack need not begin where the local line does - one made outside git-stk
/// need not align with it at all - so every layer is searched, not the bottom.
#[test]
fn unstack_finds_a_stack_that_does_not_start_at_the_local_bottom() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();

    // The stack holds only the upper layer.
    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"ma/a"},"pull_requests":[
        {"number":13,"head":{"ref":"ma/b"}},
        {"number":14,"head":{"ref":"elsewhere"}}]}]"##;
    let fake = FakeProvider::new()
        .record("stacks/5/unstack -X POST", "unstack.txt", "{}")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["unstack", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("dissolved stack 5"));
    assert!(repo.path().join("unstack.txt").exists());
}

/// The headline case: a teammate's `gh stack submit` over branches this
/// checkout has adopted nothing of. Nothing local records a parent, so a
/// lookup over the branches git-stk tracks finds none - while GitHub still
/// refuses `gh pr merge` and `gh pr edit --base` for the stack holding the
/// branch you are standing on.
#[test]
fn unstack_dissolves_a_stack_over_branches_git_stk_never_adopted() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "ma/b"]);
    repo.commit_file("b.txt", "b\n", "b work");

    let stacks = r##"[{"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"ma/a"}},
        {"number":13,"head":{"ref":"ma/b"}}]}]"##;
    let fake = FakeProvider::new()
        .record("stacks/5/unstack -X POST", "unstack.txt", "{}")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["unstack", "-y"])
        .assert()
        .success()
        // And the reviews are named: the stack reaches past this line.
        .stdout(predicates::str::contains(
            "dissolved stack 5 on GitHub (#12 #13)",
        ));
    assert!(repo.path().join("unstack.txt").exists());
}

/// Two stacks can partition one local line. Dissolving only the first found
/// would report success while leaving the rest of the line blocked.
#[test]
fn unstack_dissolves_every_stack_covering_the_line() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    let stacks = r##"[
        {"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
            {"number":12,"head":{"ref":"ma/a"}},{"number":98,"head":{"ref":"other"}}]},
        {"number":6,"open":true,"base":{"ref":"ma/a"},"pull_requests":[
            {"number":13,"head":{"ref":"ma/b"}},{"number":99,"head":{"ref":"another"}}]}]"##;
    let fake = FakeProvider::new()
        .record("stacks/5/unstack -X POST", "unstack-5.txt", "{}")
        .record("stacks/6/unstack -X POST", "unstack-6.txt", "{}")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["unstack", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains("dissolved stack 5"))
        .stdout(predicates::str::contains("dissolved stack 6"));

    assert!(repo.path().join("unstack-5.txt").exists());
    assert!(repo.path().join("unstack-6.txt").exists());
}

/// One dissolve failing must not strand the rest: the others are still
/// attempted, the ones that went through are still reported, and the error
/// names what is still registered - which is the answer this command exists
/// to give. A `?` on the first failure would leave the user with a success
/// line and no idea the line is still blocked.
#[test]
fn unstack_keeps_going_when_one_dissolve_fails() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "ma/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    let stacks = r##"[
        {"number":5,"open":true,"base":{"ref":"main"},"pull_requests":[
            {"number":12,"head":{"ref":"ma/a"}}]},
        {"number":6,"open":true,"base":{"ref":"ma/a"},"pull_requests":[
            {"number":13,"head":{"ref":"ma/b"}}]}]"##;
    let fake = FakeProvider::new()
        .record("stacks/5/unstack -X POST", "unstack-5.txt", "{}")
        .fail("stacks/6/unstack", "HTTP 404: Not Found")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["unstack", "-y"])
        .assert()
        .failure()
        // The one that went through is still reported - it was not rolled
        // back, and the user needs to know it happened.
        .stdout(predicates::str::contains("dissolved stack 5 on GitHub"))
        .stderr(predicates::str::contains("could not dissolve stack 6"))
        .stderr(predicates::str::contains("1 stack still registered: 6"));

    assert!(repo.path().join("unstack-5.txt").exists());
}

/// `gh repo view` is the first call to fail under an expired token, and its
/// error carries the "run `gh auth login`" hint. Discarding it let a failed
/// lookup read as "nothing to dissolve".
#[test]
fn unstack_surfaces_a_failed_repo_lookup() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "ma/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let fake = FakeProvider::new()
        .fail(
            "repo view",
            "gh: To get started with GitHub CLI, please run: gh auth login",
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["unstack", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "could not resolve the GitHub repository",
        ))
        .stderr(predicates::str::contains("nothing to dissolve").not());
}

/// Checks pending on a review in a platform stack. The refusal git-stk raises
/// is passed through, but the *platform's* refusal is re-diagnosed - and the
/// advice that came with it used to recommend `--auto`, which this same
/// command answers with "cannot be scheduled with --auto".
#[test]
fn merge_does_not_recommend_auto_for_a_stacked_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");

    let stacks = r##"[{"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"state":"open","head":{"ref":"feature/a"}},
        {"number":13,"state":"open","head":{"ref":"above"}}]}]"##;
    let fake = FakeProvider::new()
        // The async merge is rejected while checks are pending.
        .fail("merge-async", "HTTP 405: required status checks have not passed")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on(
            "pr view 12 --json mergeable,mergeStateStatus",
            r##"{"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED"}"##,
        )
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("checks are not green yet"))
        .stderr(predicates::str::contains(
            "`--auto` is not available for a review in a stack",
        ))
        // Never the flag this command would refuse.
        .stderr(predicates::str::contains("schedule with").not());
}
