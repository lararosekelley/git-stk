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
            "merge scheduled for A work (#12); rerun `git stk sync` once checks pass",
        ))
        .stdout(predicates::str::contains(
            "merge complete: 0 of 2 reviews merged",
        ));

    assert!(!repo.path().join("unexpected-merge.txt").exists());
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");
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
        .stderr(predicates::str::contains(
            "checks failed for #12; fix them and rerun `git stk merge --all`",
        ));
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
            "merge scheduled for A work (#12); rerun `git stk sync` once checks pass",
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
            "merge scheduled for A work (!34); rerun `git stk sync` once checks pass",
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
