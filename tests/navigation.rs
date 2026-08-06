mod common;

use common::TestRepo;
use predicates::prelude::PredicateBooleanExt;

#[test]
fn new_records_parent_and_supports_navigation() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["new", "feature/a"])
        .assert()
        .success()
        .stdout("created feature/a with parent main\n");

    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkParent"]),
        "main"
    );

    repo.stack()
        .arg("parent")
        .assert()
        .success()
        .stdout("main\n");

    repo.stack()
        .args(["children", "main"])
        .assert()
        .success()
        .stdout("feature/a\n");

    repo.stack()
        .arg("down")
        .assert()
        .success()
        .stdout("switched to main\n");
    assert_eq!(repo.git(["branch", "--show-current"]), "main");

    repo.stack()
        .arg("up")
        .assert()
        .success()
        .stdout("switched to feature/a\n");
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

#[test]
fn adopt_list_and_detach_manage_existing_branches() {
    let repo = TestRepo::new();

    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "main"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["switch", "main"]);

    repo.stack()
        .args(["adopt", "feature/a", "--parent", "main"])
        .assert()
        .success()
        .stdout("attached feature/a to main\n");

    repo.stack()
        .args(["adopt", "feature/b", "--parent", "feature/a"])
        .assert()
        .success()
        .stdout("attached feature/b to feature/a\n");

    repo.stack()
        .arg("list")
        .assert()
        .success()
        .stdout("    \u{25cb} feature/b\n  \u{25cb} feature/a\n\u{25c9} main (trunk)\n");

    repo.stack()
        .args(["detach", "feature/b"])
        .assert()
        .success()
        .stdout("detached feature/b\n");

    repo.stack()
        .args(["children", "feature/a"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn adopt_refuses_to_form_a_cycle() {
    let repo = TestRepo::new();

    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "main"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["switch", "main"]);

    repo.stack()
        .args(["adopt", "feature/a", "--parent", "main"])
        .assert()
        .success();
    repo.stack()
        .args(["adopt", "feature/b", "--parent", "feature/a"])
        .assert()
        .success();

    // feature/b sits above feature/a, so making it feature/a's parent would
    // close a loop. Refuse rather than write cyclic metadata.
    repo.stack()
        .args(["adopt", "feature/a", "--parent", "feature/b"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cycle"));

    // The original parent is untouched.
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkParent"]),
        "main"
    );
}

/// Cyclic metadata can still exist (e.g. written by an older version, or by
/// raw `git config`). Commands that walk descendants must terminate cleanly
/// rather than recurse forever and overflow the stack.
#[test]
fn cyclic_metadata_does_not_crash_descendant_walk() {
    let repo = TestRepo::new();

    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "main"]);
    repo.git(["switch", "-c", "feature/b"]);

    // Forge a 2-cycle directly, bypassing the adopt guard: a <-> b.
    repo.git(["config", "branch.feature/a.stkParent", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);

    repo.git(["switch", "feature/a"]);

    // `list --format markdown` collects via branch_and_descendants; before the
    // visited-set guard this overflowed the stack and core-dumped.
    repo.stack()
        .args(["list", "--format", "markdown"])
        .assert()
        .success();
}

#[test]
fn adopt_with_no_args_uses_current_branch_and_trunk() {
    let repo = TestRepo::new();
    // A branch made with raw git, switched to but not yet in a stack.
    repo.git(["switch", "-c", "feature/x"]);

    repo.stack()
        .arg("adopt")
        .assert()
        .success()
        .stdout("attached feature/x to main\n");

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/x.stkParent"]),
        "main"
    );
}

#[test]
fn new_on_an_existing_branch_points_at_adopt() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "feature/x"]);
    repo.git(["switch", "main"]);

    repo.stack()
        .args(["new", "feature/x"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already exists"))
        .stderr(predicates::str::contains(
            "git stk adopt feature/x --parent main",
        ));
}

#[test]
fn up_requires_branch_when_multiple_children_exist() {
    let repo = TestRepo::new();

    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "main"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["switch", "main"]);

    repo.stack()
        .args(["adopt", "feature/a", "--parent", "main"])
        .assert()
        .success();
    repo.stack()
        .args(["adopt", "feature/b", "--parent", "main"])
        .assert()
        .success();

    repo.stack()
        .arg("up")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "choose one with `git stk up <branch>`",
        ));

    repo.stack().args(["up", "feature/b"]).assert().success();
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");
}

#[test]
fn up_picker_chooses_among_multiple_children() {
    let repo = TestRepo::new();

    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "main"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["switch", "main"]);
    repo.stack()
        .args(["adopt", "feature/a", "--parent", "main"])
        .assert()
        .success();
    repo.stack()
        .args(["adopt", "feature/b", "--parent", "main"])
        .assert()
        .success();

    // Children list alphabetically: 2 picks feature/b.
    repo.stack()
        .arg("up")
        .write_stdin("2\n")
        .assert()
        .success()
        .stderr(predicates::str::contains("1."))
        .stderr(predicates::str::contains("pick [1-2]:"));
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");

    // An answer off the menu falls back to the error.
    repo.git(["switch", "main"]);
    repo.stack()
        .arg("up")
        .write_stdin("9\n")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "choose one with `git stk up <branch>`",
        ));
    assert_eq!(repo.git(["branch", "--show-current"]), "main");
}

#[test]
fn up_and_down_walk_a_given_number_of_branches() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.stack().args(["new", "feature/c"]).assert().success();

    repo.stack()
        .args(["down", "2"])
        .assert()
        .success()
        .stdout("switched to feature/a\n");
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");

    repo.stack()
        .args(["up", "2"])
        .assert()
        .success()
        .stdout("switched to feature/c\n");
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/c");

    // A distance of one is the plain command.
    repo.stack().args(["down", "1"]).assert().success();
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");
}

#[test]
fn up_and_down_reject_a_distance_past_the_end_of_the_stack() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    repo.stack()
        .args(["down", "5"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot go down 5: feature/b is only 2 branches above main",
        ));
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/b");

    repo.git(["switch", "main"]);
    repo.stack()
        .args(["up", "5"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot go up 5: main is only 2 branches below feature/b",
        ));
    assert_eq!(repo.git(["branch", "--show-current"]), "main");
}

#[test]
fn up_and_down_reject_a_distance_below_one() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();

    for args in [
        vec!["up", "0"],
        vec!["down", "0"],
        vec!["up", "-2"],
        vec!["down", "-2"],
    ] {
        repo.stack()
            .args(&args)
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "a distance is 1 or more branches",
            ));
    }
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

#[test]
fn up_with_a_distance_prompts_at_each_fork() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "feature/a"]);
    repo.stack().args(["new", "feature/c"]).assert().success();
    repo.git(["switch", "main"]);

    // main -> feature/a, then the fork picker chooses feature/c.
    repo.stack()
        .args(["up", "2"])
        .write_stdin("2\n")
        .assert()
        .success();
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/c");

    // Declining the pick part-way names the branch the walk stopped on.
    repo.git(["switch", "main"]);
    repo.stack()
        .args(["up", "2"])
        .write_stdin("9\n")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "walk up from feature/a with `git stk up <branch>`",
        ));
    assert_eq!(repo.git(["branch", "--show-current"]), "main");
}

#[test]
fn top_picker_resolves_the_fork_and_keeps_climbing() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "feature/a"]);
    repo.stack().args(["new", "feature/c"]).assert().success();
    repo.stack().args(["new", "feature/d"]).assert().success();
    repo.git(["switch", "main"]);

    // Pick the feature/c side of the fork; the climb continues to its leaf.
    repo.stack()
        .arg("top")
        .write_stdin("2\n")
        .assert()
        .success();
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/d");
}

#[test]
fn top_and_bottom_jump_across_the_stack() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.stack().args(["new", "feature/c"]).assert().success();

    repo.stack().arg("bottom").assert().success();
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");

    repo.stack().arg("top").assert().success();
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/c");

    // Already there: a friendly no-op, not an error.
    repo.stack()
        .arg("top")
        .assert()
        .success()
        .stdout("feature/c is already at the top of the stack\n");
}

#[test]
fn bottom_from_the_trunk_follows_a_single_stack() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "main"]);

    repo.stack().arg("bottom").assert().success();
    assert_eq!(repo.git(["branch", "--show-current"]), "feature/a");
}

#[test]
fn top_stops_at_a_fork_and_lists_the_choices() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "feature/a"]);
    repo.stack().args(["new", "feature/c"]).assert().success();
    repo.git(["switch", "main"]);

    repo.stack()
        .arg("top")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "feature/a has multiple stack children",
        ))
        .stderr(predicates::str::contains(
            "walk up from feature/a with `git stk up <branch>`",
        ));
}

#[test]
fn top_and_bottom_require_a_stack() {
    let repo = TestRepo::new();

    repo.stack()
        .arg("top")
        .assert()
        .failure()
        .stderr(predicates::str::contains("main is not in a stack"));

    repo.stack()
        .arg("bottom")
        .assert()
        .failure()
        .stderr(predicates::str::contains("main has no stacked branches"));
}

#[test]
fn new_and_restack_record_stack_base() {
    let repo = TestRepo::new();

    let main_tip = repo.git(["rev-parse", "main"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkBase"]),
        main_tip
    );

    repo.commit_file("a.txt", "a\n", "feature work");
    repo.git(["switch", "main"]);
    repo.commit_file("main.txt", "main\n", "main moves on");

    repo.git(["switch", "feature/a"]);
    repo.stack().arg("restack").assert().success();

    let new_main_tip = repo.git(["rev-parse", "main"]);
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkBase"]),
        new_main_tip
    );
}

#[test]
fn detach_clears_stack_base() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["detach"]).assert().success();

    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/a.stkBase"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn list_hints_restack_when_a_branch_is_behind() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "feature/a"]);
    repo.commit_file("a.txt", "a\nmore\n", "a moves on");

    repo.stack()
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "hint: feature/b is 1 commit behind feature/a - run `git stk restack`",
        ));
}

#[test]
fn list_prints_leaf_first_without_trunk_label_for_fragments() {
    let repo = TestRepo::new();

    // A stack fragment not rooted at the trunk gets no (trunk) label.
    repo.git(["switch", "-c", "feature/x"]);
    repo.git(["switch", "-c", "feature/y"]);
    repo.stack()
        .args(["adopt", "feature/y", "--parent", "feature/x"])
        .assert()
        .success();

    repo.stack()
        .arg("list")
        .assert()
        .success()
        .stdout("  \u{25c9} feature/y\n\u{25cb} feature/x\n");
}

#[test]
fn list_colors_only_when_the_terminal_wants_it() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();

    // Captured output (not a tty) stays plain; forcing color emits codes.
    repo.stack()
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("\u{1b}[").not());

    repo.stack()
        .arg("list")
        .env("CLICOLOR_FORCE", "1")
        .assert()
        .success()
        .stdout(predicates::str::contains("\u{1b}["));

    // The swept commands speak the style layer too.
    repo.stack()
        .args(["new", "feature/b"])
        .env("CLICOLOR_FORCE", "1")
        .assert()
        .success()
        .stdout(predicates::str::contains("\u{1b}["))
        .stdout(predicates::str::contains("created"));
}

/// A trunk-anchored stack (main <- feature/a) plus a rootless fragment
/// (feature/x <- feature/y, adopted with no trunk anchor).
fn two_stacks() -> TestRepo {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();

    repo.git(["switch", "main"]);
    repo.git(["switch", "-c", "feature/x"]);
    repo.git(["switch", "-c", "feature/y"]);
    repo.stack()
        .args(["adopt", "feature/y", "--parent", "feature/x"])
        .assert()
        .success();

    repo.git(["switch", "feature/a"]);
    repo
}

#[test]
fn list_all_shows_every_stack() {
    let repo = two_stacks();

    let output = repo
        .stack()
        .args(["list", "--all"])
        .output()
        .expect("list --all");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    for needle in ["feature/a", "feature/x", "feature/y", "main (trunk)"] {
        assert!(stdout.contains(needle), "missing {needle} in:\n{stdout}");
    }
    // One trunk-anchored stack (feature/a) and one rootless fragment: the trunk
    // is repeated once per trunk-sharing stack, so here it appears once.
    assert_eq!(stdout.matches("(trunk)").count(), 1);
    // The branch we are on is marked wherever it appears.
    assert!(
        stdout.contains("\u{25c9} feature/a"),
        "current marker:\n{stdout}"
    );
}

#[test]
fn list_all_separates_stacks_sharing_the_trunk() {
    let repo = TestRepo::new();

    // Two independent stacks, both anchored on main; the second forks (c, d).
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "main"]);
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.stack().args(["new", "feature/c"]).assert().success();
    repo.commit_file("c.txt", "c\n", "c work");
    repo.git(["switch", "feature/b"]);
    repo.stack().args(["new", "feature/d"]).assert().success();
    repo.commit_file("d.txt", "d\n", "d work");

    let output = repo
        .stack()
        .args(["list", "--all"])
        .output()
        .expect("list --all");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Two stacks merely share the trunk (feature/a; and feature/b, forked into
    // c and d), so the trunk is repeated as each one's base.
    assert_eq!(
        stdout.matches("(trunk)").count(),
        2,
        "trunk should repeat per trunk-sharing stack:\n{stdout}"
    );
    // The blocks are separated by a blank line.
    assert!(
        stdout.contains("\n\n"),
        "stacks should be blank-line separated:\n{stdout}"
    );
    // The fork stays inside one block: feature/c and feature/d are siblings on
    // feature/b, not split into separate trunk blocks.
    for needle in ["feature/a", "feature/b", "feature/c", "feature/d"] {
        assert!(stdout.contains(needle), "missing {needle} in:\n{stdout}");
    }
}

#[test]
fn list_without_all_shows_only_the_current_stack() {
    let repo = two_stacks();

    // Standing on the trunk-anchored stack, the rootless fragment is hidden.
    repo.stack()
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("feature/a"))
        .stdout(predicates::str::contains("feature/x").not());
}

#[test]
fn list_without_all_hides_sibling_stacks_sharing_the_trunk() {
    let repo = TestRepo::new();

    // Two independent stacks, both anchored on main.
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.git(["switch", "main"]);
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.git(["switch", "feature/a"]);

    // The default view is the one stack you're on, sitting on its trunk - the
    // sibling that only shares the trunk is left for its own `list`.
    repo.stack()
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("feature/a"))
        .stdout(predicates::str::contains("main (trunk)"))
        .stdout(predicates::str::contains("feature/b").not());

    // --all still shows both.
    let output = repo
        .stack()
        .args(["list", "--all"])
        .output()
        .expect("list --all");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("feature/a") && stdout.contains("feature/b"),
        "both stacks expected:\n{stdout}"
    );
}

#[test]
fn list_all_conflicts_with_format() {
    let repo = two_stacks();

    repo.stack()
        .args(["list", "--all", "--format", "markdown"])
        .assert()
        .failure();
}

#[test]
fn list_reports_no_stacked_branches_on_a_fresh_repo() {
    let repo = TestRepo::new();
    // Just the trunk: not a stack. Must not draw a one-node "stack".
    repo.stack()
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("no stacked branches"))
        .stdout(predicates::str::contains("git stk new"))
        .stdout(predicates::str::contains("(trunk)").not());
}

#[test]
fn commands_outside_a_git_repo_give_a_clean_error() {
    let outside = tempfile::tempdir().expect("temp dir");
    // A repo-requiring command run outside any repo: clean message, no raw
    // git "Stopping at filesystem boundary" noise.
    assert_cmd::Command::cargo_bin("git-stk")
        .expect("git-stk binary")
        .arg("status")
        .current_dir(outside.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("not a git repository"))
        .stderr(predicates::str::contains("Stopping at filesystem boundary").not());
}

#[test]
fn help_points_newcomers_at_the_guide() {
    let repo = TestRepo::new();
    repo.stack()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("git stk guide"));
}

#[test]
fn list_commits_shows_each_branch_commits_newest_first() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feat/a"]).assert().success();
    repo.commit_file("a1.txt", "1\n", "add the first thing");
    repo.commit_file("a2.txt", "2\n", "add the second thing");
    repo.stack().args(["new", "feat/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "the b change");

    // Without --commits, commit subjects are not shown.
    repo.stack()
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("add the first thing").not());

    // With --commits, each branch's own commits appear beneath it.
    repo.stack()
        .args(["list", "--commits"])
        .assert()
        .success()
        .stdout(predicates::str::contains("add the first thing"))
        .stdout(predicates::str::contains("add the second thing"))
        .stdout(predicates::str::contains("the b change"));
}

#[test]
fn list_commits_marks_an_empty_branch() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feat/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    // A freshly created branch shares its parent's tip - no commits of its own.
    repo.stack().args(["new", "feat/empty"]).assert().success();

    repo.stack()
        .args(["list", "--commits"])
        .assert()
        .success()
        .stdout(predicates::str::contains("(no commits)"));
}

#[test]
fn list_commits_conflicts_with_format() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feat/a"]).assert().success();

    repo.stack()
        .args(["list", "--commits", "--format", "markdown"])
        .assert()
        .failure();
}
