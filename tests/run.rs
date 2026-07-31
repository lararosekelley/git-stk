mod common;

use common::{FakeProvider, TestRepo};

/// Where the fake should log its invocations. Absolute, because the command's
/// working directory is a scratch worktree by default - a relative path would
/// land inside it and be removed with it.
fn log_path(repo: &TestRepo) -> String {
    repo.path()
        .join("runs.log")
        .to_str()
        .expect("utf8 path")
        .to_owned()
}

/// A two-branch stack (feature/a <- feature/b) with real commits.
fn two_branch_stack() -> TestRepo {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo
}

#[test]
fn run_executes_on_each_branch_bottom_up_and_restores_original() {
    let repo = two_branch_stack();
    repo.git(["switch", "feature/a"]);
    // The fake stands in for an arbitrary command: it logs every invocation
    // and succeeds.
    let log = log_path(&repo);
    let fake = FakeProvider::new()
        .commands(&["probe"])
        .log_all(&log)
        .fallback("")
        .install(&repo);

    let output = repo
        .stack_faked(&fake)
        .args(["run", "--", "probe"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Branch headers print bottom-up.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let a = stdout.find("feature/a").expect("feature/a header");
    let b = stdout.find("feature/b").expect("feature/b header");
    assert!(a < b, "bottom-up order: feature/a before feature/b");

    // The command ran once per branch.
    let log = std::fs::read_to_string(repo.path().join("runs.log")).expect("runs log");
    assert_eq!(log.lines().count(), 2);

    // We are returned to where we started.
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

#[test]
fn run_reports_failures_and_exits_nonzero() {
    let repo = two_branch_stack();
    let log = log_path(&repo);
    let fake = FakeProvider::new()
        .commands(&["probe"])
        .log_all(&log)
        .fallback_fail("boom")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["run", "--", "probe"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("FAIL"));

    // Without --fail-fast every branch is still attempted.
    let log = std::fs::read_to_string(repo.path().join("runs.log")).expect("runs log");
    assert_eq!(log.lines().count(), 2);
}

#[test]
fn run_fail_fast_stops_at_the_first_failure() {
    let repo = two_branch_stack();
    let log = log_path(&repo);
    let fake = FakeProvider::new()
        .commands(&["probe"])
        .log_all(&log)
        .fallback_fail("boom")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["run", "--fail-fast", "--", "probe"])
        .assert()
        .failure();

    let log = std::fs::read_to_string(repo.path().join("runs.log")).expect("runs log");
    assert_eq!(
        log.lines().count(),
        1,
        "stopped after the first failing branch"
    );
}

#[test]
fn run_surfaces_a_spawn_error_and_hints_at_a_quoted_command_string() {
    let repo = two_branch_stack();
    repo.git(["switch", "feature/a"]);

    // The classic mistake: the whole command passed as one quoted string, so
    // the "program" is `yarn tsc --noEmit` and no such binary exists. This must
    // not read as every branch failing its build.
    let assert = repo
        .stack()
        .args(["run", "--", "yarn tsc --noEmit"])
        .assert()
        .failure();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("failed to run `yarn tsc --noEmit`"),
        "spawn error not surfaced:\n{stderr}"
    );
    assert!(
        stderr.contains("git stk run -- yarn tsc --noEmit"),
        "missing unquoted-command hint:\n{stderr}"
    );
    // No bogus per-branch FAILs or summary: the branches were never judged.
    assert!(!stdout.contains("FAIL"), "reported a bogus FAIL:\n{stdout}");
    assert!(!stdout.contains("ran on"), "printed a summary:\n{stdout}");
}

#[test]
fn run_surfaces_a_not_found_command_without_the_quoting_hint() {
    let repo = two_branch_stack();
    repo.git(["switch", "feature/a"]);

    // A single-word command that simply isn't installed: still a spawn error,
    // but the quoting hint would be irrelevant, so it is withheld.
    let assert = repo
        .stack()
        .args(["run", "--", "git-stk-no-such-binary-xyz"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);

    assert!(
        stderr.contains("failed to run `git-stk-no-such-binary-xyz`"),
        "spawn error not surfaced:\n{stderr}"
    );
    assert!(!stderr.contains("hint:"), "unexpected hint:\n{stderr}");
}

#[test]
fn run_no_worktree_refuses_a_dirty_worktree() {
    let repo = two_branch_stack();
    // a.txt is tracked (committed on feature/a); dirty it.
    repo.write("a.txt", "uncommitted\n");
    let fake = FakeProvider::new()
        .commands(&["probe"])
        .fallback("")
        .install(&repo);

    // --no-worktree walks the user's own checkout, so it still needs a clean
    // tree - switching branches would drag the changes across the stack.
    repo.stack_faked(&fake)
        .args(["run", "--no-worktree", "--", "probe"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("uncommitted changes"));
}

#[test]
fn run_no_worktree_executes_on_each_branch_and_restores_original() {
    let repo = two_branch_stack();
    repo.git(["switch", "feature/a"]);
    let log = log_path(&repo);
    let fake = FakeProvider::new()
        .commands(&["probe"])
        .log_all(&log)
        .fallback("")
        .install(&repo);

    // The escape hatch has to actually work: this is the path users reach for
    // when the scratch-worktree default breaks their command, and every other
    // happy-path test now exercises the default instead.
    repo.stack_faked(&fake)
        .args(["run", "--no-worktree", "--", "probe"])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("runs log")
            .lines()
            .count(),
        2
    );
    // It walks the user's own checkout, so HEAD must come back where it started.
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

/// A stack whose repo has a `packages/web` subdirectory on every branch, so a
/// command can report where it ran from via `git rev-parse --show-prefix`.
fn stack_with_a_subdirectory() -> TestRepo {
    let repo = TestRepo::new();
    std::fs::create_dir_all(repo.path().join("packages/web")).expect("mkdir");
    repo.commit_file("packages/web/pkg.txt", "pkg\n", "add package");
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo
}

#[test]
fn run_keeps_the_directory_you_started_in() {
    let repo = stack_with_a_subdirectory();
    let subdirectory = repo.path().join("packages/web");

    // The scratch worktree mirrors the directory the user is standing in, so a
    // command run from a monorepo package still runs in that package.
    let output = repo
        .stack_in(&subdirectory)
        .args(["run", "--", "git", "rev-parse", "--show-prefix"])
        .output()
        .expect("run");
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("packages/web/"),
        "command should run in packages/web, got:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn run_no_worktree_keeps_the_directory_you_started_in() {
    let repo = stack_with_a_subdirectory();
    let subdirectory = repo.path().join("packages/web");

    // The old `run` set no working directory at all, inheriting the user's.
    // --no-worktree is advertised as restoring that behavior, so it has to.
    let output = repo
        .stack_in(&subdirectory)
        .args([
            "run",
            "--no-worktree",
            "--",
            "git",
            "rev-parse",
            "--show-prefix",
        ])
        .output()
        .expect("run");
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("packages/web/"),
        "command should run in packages/web, got:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn run_reclaims_a_scratch_worktree_whose_directory_vanished() {
    let repo = two_branch_stack();
    let log = log_path(&repo);
    let fake = FakeProvider::new()
        .commands(&["probe"])
        .log_all(&log)
        .fallback("")
        .install(&repo);

    // A killed run can leave the directory gone but still registered. Git then
    // refuses the path as "a missing but already registered worktree", so a
    // plain add would fail every subsequent run until pruned by hand.
    let scratch = repo.path().join(".git/git-stk-run-worktree");
    repo.git([
        "worktree",
        "add",
        "--detach",
        "--quiet",
        scratch.to_str().unwrap(),
        "HEAD",
    ]);
    std::fs::remove_dir_all(&scratch).expect("remove the scratch directory");

    repo.stack_faked(&fake)
        .args(["run", "--", "probe"])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("runs log")
            .lines()
            .count(),
        2
    );
}

#[test]
fn run_tolerates_a_dirty_worktree_by_default() {
    let repo = two_branch_stack();
    repo.git(["switch", "feature/a"]);
    repo.write("a.txt", "work in progress\n");
    let log = log_path(&repo);
    let fake = FakeProvider::new()
        .commands(&["probe"])
        .log_all(&log)
        .fallback("")
        .install(&repo);

    // The whole point of the scratch worktree: wanting to check the stack is
    // not a reason to be forced to stash.
    repo.stack_faked(&fake)
        .args(["run", "--", "probe"])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("runs log")
            .lines()
            .count(),
        2
    );
    // The uncommitted work is untouched and HEAD never moved.
    assert_eq!(
        std::fs::read_to_string(repo.path().join("a.txt")).expect("a.txt"),
        "work in progress\n"
    );
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

#[test]
fn run_leaves_no_scratch_worktree_behind() {
    let repo = two_branch_stack();
    let log = log_path(&repo);
    let fake = FakeProvider::new()
        .commands(&["probe"])
        .log_all(&log)
        .fallback_fail("boom")
        .install(&repo);

    // Even on failure the worktree is cleaned up, so the next run is not
    // reclaiming an orphan and `git worktree list` stays honest.
    repo.stack_faked(&fake)
        .args(["run", "--", "probe"])
        .assert()
        .failure();

    let worktrees = repo.git(["worktree", "list", "--porcelain"]);
    assert!(
        !worktrees.contains("git-stk-run-worktree"),
        "scratch worktree left behind:\n{worktrees}"
    );
}

#[test]
fn the_scratch_worktree_holds_each_branchs_own_files() {
    let repo = two_branch_stack();

    // b.txt exists only on feature/b, so asking git whether it is tracked
    // succeeds there and fails on feature/a. A worktree parked on one commit for
    // the whole run - or run from the user's checkout - could not produce that
    // split, so the per-branch pass/fail is the proof.
    let assert = repo
        .stack()
        .args(["run", "--", "git", "ls-files", "--error-unmatch", "b.txt"])
        .assert()
        .failure();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    // The summary lines, not the per-branch headers printed above them.
    let line_for = |branch: &str| {
        stdout
            .lines()
            .find(|line| {
                line.trim_start().starts_with(branch)
                    && (line.contains("ok") || line.contains("FAIL"))
            })
            .unwrap_or_else(|| panic!("no summary line for {branch} in:\n{stdout}"))
    };
    assert!(
        line_for("feature/a").contains("FAIL"),
        "b.txt should be absent on feature/a:\n{stdout}"
    );
    assert!(
        line_for("feature/b").contains("ok"),
        "b.txt should be present on feature/b:\n{stdout}"
    );

    // And nothing from the run landed in the user's tree.
    assert!(
        !repo.path().join("git-stk-run-worktree").exists(),
        "the scratch worktree must not sit in the working tree"
    );
}

#[test]
fn run_without_a_stack_reports_nothing_to_do() {
    let repo = TestRepo::new();
    let fake = FakeProvider::new()
        .commands(&["probe"])
        .fallback("")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["run", "--", "probe"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no stacked branches to run on"));
}
