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
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12","title":"A work"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["merge", "-y", "--auto"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "#12 is in a GitHub stack, which cannot be scheduled with --auto",
        ));

    assert!(
        !repo.path().join("async.txt").exists(),
        "must not merge now"
    );
    assert!(!repo.path().join("sync-merge.txt").exists());
}
