mod common;

use common::TestRepo;
use predicates::prelude::PredicateBooleanExt;

#[test]
fn demo_provider_runs_the_full_merge_loop_offline() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);

    repo.stack()
        .args(["new", "feature/login"])
        .assert()
        .success();
    repo.commit_file("login.txt", "login form\n", "add login form");
    repo.stack()
        .args(["new", "feature/avatar"])
        .assert()
        .success();
    repo.commit_file("avatar.txt", "avatars\n", "add avatars");

    // Submit opens demo reviews and writes the stack overview into them.
    repo.stack()
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/login -> main"))
        .stdout(predicates::str::contains(
            "created feature/avatar -> feature/login",
        ))
        .stdout(predicates::str::contains("updated stack note in #1"))
        .stdout(predicates::str::contains("updated stack note in #2"));

    repo.stack()
        .args(["status", "feature/login"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "review: #1 open feature/login -> main",
        ))
        .stdout(predicates::str::contains("url: demo://review/1"));

    // The whole landing, no network: real squashes onto main, real cleanup.
    repo.stack()
        .args(["merge", "--all", "-y"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "squashed feature/login into main",
        ))
        .stdout(predicates::str::contains("merged add login form (#1)"))
        .stdout(predicates::str::contains(
            "squashed feature/avatar into main",
        ))
        .stdout(predicates::str::contains("merged add avatars (#2)"))
        .stdout(predicates::str::contains(
            "stack complete: everything merged into main",
        ))
        .stdout(predicates::str::contains(
            "merge complete: 2 of 2 reviews merged",
        ));

    // The squashed work is genuinely on main.
    assert_eq!(repo.git(["branch", "--show-current"]), "main");
    assert_eq!(repo.git(["show", "main:login.txt"]), "login form");
    assert_eq!(repo.git(["show", "main:avatar.txt"]), "avatars");
    assert_eq!(
        repo.git_status(["branch", "--list", "feature/login", "feature/avatar"])
            .stdout
            .len(),
        0
    );
}

#[test]
fn demo_provider_is_never_auto_detected() {
    let repo = TestRepo::new();
    repo.git(["remote", "add", "origin", "git@github.com:owner/repo.git"]);

    repo.stack()
        .arg("provider")
        .assert()
        .success()
        .stdout(predicates::str::contains("github"))
        .stdout(predicates::str::contains("demo").not());
}

#[test]
fn list_plain_format_uses_plain_text_and_bare_urls() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add feature a");
    repo.stack().args(["submit"]).assert().success();

    repo.stack()
        .args(["list", "--format", "plain"])
        .assert()
        .success()
        // Unquoted base; bare URL on its own line for chat apps to auto-link.
        .stdout(predicates::str::contains("1 PR, base main"))
        .stdout(predicates::str::contains(
            "1. add feature a (#1, +1/-0) - open",
        ))
        .stdout(predicates::str::contains("   demo://review/1"))
        // No markdown link syntax.
        .stdout(predicates::str::contains("](").not());
}

#[test]
fn undo_restores_branch_tips_after_a_restack() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    let before = repo.git(["rev-parse", "feature/b"]);

    // Move the parent so the restack actually rewrites feature/b.
    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "more\n", "a moves on");
    repo.stack().arg("restack").assert().success();
    repo.git(["switch", "feature/b"]);
    assert_ne!(repo.git(["rev-parse", "feature/b"]), before);

    repo.stack()
        .arg("undo")
        .assert()
        .success()
        .stdout(predicates::str::contains("undid restack"))
        .stdout(predicates::str::contains(
            "pushes and merged reviews are not reverted",
        ));

    assert_eq!(repo.git(["rev-parse", "feature/b"]), before);
    // One-shot: a second undo has nothing to restore.
    repo.stack()
        .arg("undo")
        .assert()
        .failure()
        .stderr(predicates::str::contains("nothing to undo"));
}

#[test]
fn undo_recreates_a_branch_cleanup_deleted() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["submit"]).assert().success();
    let sha = repo.git(["rev-parse", "feature/a"]);

    // Merge lands feature/a on main and cleanup deletes it.
    repo.stack().args(["merge", "-y"]).assert().success();
    assert_eq!(
        repo.git_status(["rev-parse", "--verify", "feature/a"])
            .status
            .code(),
        Some(128),
        "feature/a should be gone after merge"
    );

    repo.stack().arg("undo").assert().success();
    // The deleted branch is back at its pre-merge tip.
    assert_eq!(repo.git(["rev-parse", "feature/a"]), sha);
}

#[test]
fn undo_refuses_with_a_dirty_worktree() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "feature/a"]);
    repo.commit_file("a2.txt", "more\n", "a moves on");
    repo.git(["switch", "main"]); // make feature/a the stack, restack a no-op-free case
    repo.git(["switch", "feature/a"]);
    repo.stack().arg("restack").assert().success();

    // Dirty the tree, then undo must refuse rather than reset over it.
    repo.write("uncommitted.txt", "work in progress\n");
    repo.git(["add", "uncommitted.txt"]);
    repo.stack()
        .arg("undo")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "worktree has uncommitted changes",
        ));
}

#[test]
fn errors_are_prefixed_and_colored() {
    let repo = TestRepo::new();

    // A failing command: no stack to merge. Captured (non-tty) stderr is
    // plain, but carries the `error:` prefix and exits nonzero.
    repo.stack()
        .arg("merge")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "error: no stacked branches to merge",
        ))
        .stderr(predicates::str::contains("\u{1b}[").not());

    // Forced color paints the prefix red.
    repo.stack()
        .arg("merge")
        .env("CLICOLOR_FORCE", "1")
        .assert()
        .failure()
        .stderr(predicates::str::contains("\u{1b}["))
        .stderr(predicates::str::contains("error:"));
}

#[test]
fn view_reports_no_review_without_one() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.stack().args(["new", "feature/a"]).assert().success();

    repo.stack()
        .args(["view", "feature/a"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "no demo review found for feature/a; submit it first with `git stk submit`",
        ));
}

#[test]
fn view_opens_the_current_branch_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["submit"]).assert().success();

    // The demo has no browser, but the command resolves the review, prints
    // the opening line, and the provider's graceful note.
    repo.stack()
        .args(["view"])
        .assert()
        .success()
        .stdout(predicates::str::contains("opening #1"))
        .stdout(predicates::str::contains("demo reviews have no web page"));
}

#[test]
fn guide_requires_a_terminal() {
    let repo = TestRepo::new();

    repo.stack()
        .arg("guide")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "the guide is interactive; run it from a terminal",
        ));

    // A named tour still needs the terminal.
    repo.stack()
        .args(["guide", "conflicts"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "the guide is interactive; run it from a terminal",
        ));
}

// The two scripted tours run interactively, so their exact recipes are
// proven here instead: same commands, same files, same expected outcomes.

#[test]
fn guide_conflicts_recipe_conflicts_and_continues() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["new", "feature/payment"])
        .assert()
        .success();
    repo.commit_file("notes.txt", "use stripe\n", "choose payment provider");
    repo.stack()
        .args(["new", "feature/receipts"])
        .assert()
        .success();
    repo.commit_file("notes.txt", "use stripe with receipts\n", "email receipts");
    repo.git(["switch", "feature/payment"]);
    repo.commit_file("notes.txt", "use paypal\n", "switch to paypal");

    repo.stack()
        .arg("restack")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "resolve conflicts, then run `git stk continue`",
        ));

    repo.write("notes.txt", "use paypal with receipts\n");
    repo.git(["add", "notes.txt"]);
    repo.stack()
        .arg("continue")
        .assert()
        .success()
        .stdout(predicates::str::contains("restack complete"));

    assert_eq!(
        repo.git(["show", "feature/receipts:notes.txt"]),
        "use paypal with receipts"
    );
}

#[test]
fn guide_repair_recipe_recovers_from_the_demo_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);

    repo.stack().args(["new", "feature/api"]).assert().success();
    repo.commit_file("api.txt", "endpoints\n", "add api");
    repo.stack().args(["new", "feature/ui"]).assert().success();
    repo.commit_file("ui.txt", "buttons\n", "add ui");
    repo.stack().args(["submit", "--stack"]).assert().success();

    repo.git(["config", "--unset", "branch.feature/ui.stkParent"]);

    repo.stack()
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/ui: set parent feature/api (from demo review #2)",
        ));
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/ui.stkParent"]),
        "feature/api"
    );
}

/// Every tour in the menu is reachable by name. A topic added to `TOPICS` but
/// not to the argument's value list parses as invalid, which is a failure only
/// someone running that exact tour would ever see.
#[test]
fn guide_accepts_every_listed_topic() {
    let repo = TestRepo::new();

    for topic in [
        "intro",
        "conflicts",
        "repair",
        "absorb",
        "adopt",
        "split",
        "undo",
        "worktrees",
        "github",
    ] {
        // Not a terminal here, so each stops at the same guard - but an
        // unknown topic fails at parsing instead, with a different message.
        repo.stack()
            .args(["guide", topic])
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "the guide is interactive; run it from a terminal",
            ));
    }
}

#[test]
fn guide_rejects_unknown_topics() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["guide", "bogus"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("invalid value"))
        .stderr(predicates::str::contains("intro"));
}

#[test]
fn submit_stack_does_not_sweep_sibling_stacks_on_the_trunk() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);

    // Two independent stacks rooted on the trunk.
    repo.stack().args(["new", "feature/x"]).assert().success();
    repo.commit_file("x.txt", "x\n", "add x");
    repo.git(["switch", "main"]);
    repo.stack().args(["new", "feature/y"]).assert().success();
    repo.commit_file("y.txt", "y\n", "add y");

    // Submitting feature/x's stack opens a review for it alone - feature/y is
    // a separate stack that merely shares the trunk.
    repo.git(["switch", "feature/x"]);
    repo.stack()
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/x -> main"))
        .stdout(predicates::str::contains("feature/y").not());

    // feature/y still has no review until its own stack is submitted.
    repo.git(["switch", "feature/y"]);
    repo.stack()
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/y -> main"));
}

#[test]
fn stack_wide_commands_from_the_trunk_refuse_instead_of_sweeping() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);

    // Two independent stacks rooted on the trunk.
    repo.stack().args(["new", "feature/x"]).assert().success();
    repo.commit_file("x.txt", "x\n", "add x");
    repo.git(["switch", "main"]);
    repo.stack().args(["new", "feature/y"]).assert().success();
    repo.commit_file("y.txt", "y\n", "add y");

    // Standing on the trunk, a stack-wide submit must point you at a stacked
    // branch rather than open reviews for every sibling stack.
    repo.git(["switch", "main"]);
    repo.stack()
        .args(["submit", "--stack"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("you are on the trunk"))
        .stderr(predicates::str::contains("feature/x").not());

    // Same for `merge --all`: no sibling stack gets swept.
    repo.stack()
        .args(["merge", "--all", "-y"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("you are on the trunk"));

    // Nothing was submitted: feature/x still has no review.
    repo.git(["switch", "feature/x"]);
    repo.stack()
        .args(["view"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "no demo review found for feature/x",
        ));
}

#[test]
fn rename_then_submit_replaces_and_prunes_the_old_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "add b");
    repo.stack().args(["submit", "--stack"]).assert().success();

    // Rename the leaf; its open review (#2) still heads the old name, so the
    // rename records the supersession for the next submit to resolve.
    repo.stack()
        .args(["rename", "feature/b2"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "opens a fresh review for feature/b2 and closes #2",
        ));
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b2.stkRenamedFrom"]),
        "feature/b"
    );

    // Resubmit opens a fresh review for feature/b2, then retires the stale #2.
    repo.stack()
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/b2 -> feature/a"))
        .stdout(predicates::str::contains(
            "closed superseded review #2 for feature/b",
        ));

    // The marker is consumed once handled.
    assert!(
        repo.git_status(["config", "--get", "branch.feature/b2.stkRenamedFrom"])
            .stdout
            .is_empty()
    );

    // feature/a's overview now lists the replacement review, not the stale one.
    let raw = std::fs::read_to_string(repo.path().join(".git/stk-demo-reviews"))
        .expect("demo review state");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("parse demo state");
    let body_a = state["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .find(|review| review["id"] == 1)
        .expect("review #1")["body"]
        .as_str()
        .unwrap();
    assert!(
        body_a.contains("demo://review/3"),
        "overview should list the renamed branch's fresh review"
    );
    assert!(
        !body_a.contains("demo://review/2"),
        "overview should drop the superseded review"
    );
}

/// A single-branch submit cannot prune the superseded row from the other
/// reviews' overviews, so it must leave the rename marker set rather than close
/// the old review and orphan that row forever. A later `submit --stack` then
/// resolves it cleanly.
#[test]
fn single_branch_submit_defers_rename_supersession_to_stack_submit() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "add b");
    repo.stack().args(["submit", "--stack"]).assert().success();

    // Rename the leaf; its open review (#2) still heads the old name.
    repo.stack()
        .args(["rename", "feature/b2"])
        .assert()
        .success();
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b2.stkRenamedFrom"]),
        "feature/b"
    );

    // Single-branch submit: opens #3 for the new name but must NOT close #2 or
    // clear the marker, since it never prunes feature/a's overview.
    repo.stack()
        .args(["submit", "feature/b2"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/b2 -> feature/a"))
        .stdout(predicates::str::contains("superseded").not());

    // Marker survives for a later stack submit to consume.
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b2.stkRenamedFrom"]),
        "feature/b"
    );

    // Now the stack submit closes #2 and prunes it from feature/a's overview.
    repo.stack()
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "closed superseded review #2 for feature/b",
        ));
    assert!(
        repo.git_status(["config", "--get", "branch.feature/b2.stkRenamedFrom"])
            .stdout
            .is_empty()
    );

    let body_a = demo_review_body(&repo, 1);
    assert!(
        body_a.contains("demo://review/3"),
        "overview should list the renamed branch's fresh review"
    );
    assert!(
        !body_a.contains("demo://review/2"),
        "overview should drop the superseded review once the stack submit prunes it"
    );
}

/// A 2-branch stack with both reviews submitted, then the leaf renamed so its
/// open review is superseded. Leaves the repo on feature/b2 with the marker set.
fn renamed_leaf_awaiting_supersession() -> TestRepo {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "add b");
    repo.stack().args(["submit", "--stack"]).assert().success();

    repo.stack()
        .args(["rename", "feature/b2"])
        .assert()
        .success();
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b2.stkRenamedFrom"]),
        "feature/b"
    );
    repo
}

#[test]
fn declining_the_supersession_close_keeps_the_marker_for_a_later_submit() {
    let repo = renamed_leaf_awaiting_supersession();

    // Decline the close: the old review is kept, and the marker must survive
    // so a later submit re-offers rather than orphaning the stale review.
    repo.stack()
        .args(["submit", "--stack"])
        .write_stdin("n\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("kept review #2 for feature/b"));
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b2.stkRenamedFrom"]),
        "feature/b"
    );

    // A later submit re-offers; accepting closes it and consumes the marker.
    repo.stack()
        .args(["submit", "--stack"])
        .write_stdin("y\n")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "closed superseded review #2 for feature/b",
        ));
    assert!(
        repo.git_status(["config", "--get", "branch.feature/b2.stkRenamedFrom"])
            .stdout
            .is_empty()
    );
}

#[test]
fn a_confirm_reprompts_past_unrecognized_input() {
    let repo = renamed_leaf_awaiting_supersession();

    // A stray line (e.g. type-ahead) before the real answer must not be misread
    // as the answer: the prompt re-asks and honors the "y" that follows.
    repo.stack()
        .args(["submit", "--stack"])
        .write_stdin("git stk status\ny\n")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "closed superseded review #2 for feature/b",
        ));
}

#[test]
fn sync_from_the_trunk_keeps_sibling_stack_notes_separate() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);

    // Two independent stacks, each a direct child of main.
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack().args(["submit", "--stack"]).assert().success(); // review #1
    repo.git(["switch", "main"]);
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "add b");
    repo.stack().args(["submit", "--stack"]).assert().success(); // review #2

    // Sync from the trunk sees both sibling stacks; each PR's note must stay
    // scoped to its own stack rather than listing the other one.
    repo.git(["switch", "main"]);
    repo.stack().args(["sync"]).assert().success();

    let body_a = demo_review_body(&repo, 1);
    let body_b = demo_review_body(&repo, 2);
    assert!(
        body_a.contains("demo://review/1") && !body_a.contains("demo://review/2"),
        "feature/a's note leaked the sibling stack:\n{body_a}"
    );
    assert!(
        body_b.contains("demo://review/2") && !body_b.contains("demo://review/1"),
        "feature/b's note leaked the sibling stack:\n{body_b}"
    );
}

/// The stored review body for demo review `id`, for asserting overview content.
fn demo_review_body(repo: &TestRepo, id: u64) -> String {
    let raw = std::fs::read_to_string(repo.path().join(".git/stk-demo-reviews"))
        .expect("demo review state");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("parse demo state");
    state["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .find(|review| review["id"].as_u64() == Some(id))
        .unwrap_or_else(|| panic!("review #{id}"))["body"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn rebuild_overview_drops_orphaned_rows_keeping_the_live_stack() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "add b");
    repo.stack().args(["submit", "--stack"]).assert().success();

    // Drift the ledger like a pre-fix rename would: drop the marker so #2 is
    // left orphaned in the overview instead of being closed and pruned.
    repo.stack()
        .args(["rename", "feature/b2"])
        .assert()
        .success();
    repo.git(["config", "--unset", "branch.feature/b2.stkRenamedFrom"]);
    repo.stack().args(["submit", "--stack"]).assert().success();

    // Dry run reports the drifted row it would drop.
    repo.stack()
        .args(["submit", "--stack", "--rebuild-overview", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would drop drifted entry #2"));

    repo.stack()
        .args(["submit", "--stack", "--rebuild-overview"])
        .assert()
        .success();

    // feature/a's overview now lists only the live stack, not the orphaned #2.
    let body = demo_review_body(&repo, 1);
    assert!(body.contains("demo://review/1"));
    assert!(body.contains("demo://review/3"));
    assert!(
        !body.contains("demo://review/2"),
        "rebuild should drop the orphaned entry"
    );
}

#[test]
fn rebuild_overview_keeps_merged_history() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "add b");
    repo.stack().args(["submit", "--stack"]).assert().success();

    // Land feature/a so #1 becomes merged history and a leaves the stack.
    repo.git(["switch", "feature/a"]);
    repo.stack().args(["merge", "-y"]).assert().success();

    // Rebuilding from the survivor keeps the merged #1 alongside the live #2.
    repo.git(["switch", "feature/b"]);
    repo.stack()
        .args(["submit", "--stack", "--rebuild-overview"])
        .assert()
        .success();

    let body = demo_review_body(&repo, 2);
    assert!(
        body.contains("demo://review/1"),
        "rebuild should keep merged history"
    );
    assert!(body.contains("demo://review/2"));
}

#[test]
fn list_shows_review_numbers_for_branches_with_reviews() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "add b");

    // No reviews yet: no numbers, but each branch shows its diff size against
    // its parent (one added line apiece).
    repo.stack()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("feature/a (+1/-0)"))
        .stdout(predicates::str::contains("(#1)").not());

    repo.stack().args(["submit", "--stack"]).assert().success();

    // Each branch now shows its open review number alongside the diff size.
    repo.stack()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("feature/a (#1, +1/-0)"))
        .stdout(predicates::str::contains("feature/b (#2, +1/-0)"))
        // The trunk has neither a review nor a parent to diff against.
        .stdout(predicates::str::contains("main (trunk)"));
}
