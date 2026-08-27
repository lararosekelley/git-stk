use std::{fs, process::Command};
mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

#[test]
fn submit_creates_github_pr_when_none_exists() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);
    let fake = FakeProvider::new()
        .on("pr list", "[]")
        .on(
            "pr create",
            "https://github.com/lararosekelley/git-stk/pull/12",
        )
        .fallback_fail("unexpected gh args")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("submit")
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/b -> feature/a"))
        .stdout(predicates::str::contains(
            "https://github.com/lararosekelley/git-stk/pull/12",
        ))
        .stdout(predicates::str::contains(
            "submit complete: 1 created, 0 updated, 0 skipped",
        ));
}

#[test]
fn submit_dry_run_reports_create_without_calling_create() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);
    let fake = FakeProvider::new()
        .on("pr list", "[]")
        .fallback_fail("unexpected gh args")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--dry-run"])
        .assert()
        .success()
        .stdout("would create feature/b -> feature/a\nsubmit complete: 1 created, 0 updated, 0 skipped\n");
}

#[test]
fn submit_noops_when_github_pr_already_targets_parent() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);
    let fake = FakeProvider::new()
        .on(
            "pr list",
            r##"[{"number":12,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/lararosekelley/git-stk/pull/12"}]"##,
        )
        .fallback_fail("unexpected gh args")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "feature/b"])
        .assert()
        .success()
        .stdout("feature/b already targets feature/a (#12)\nsubmit complete: 0 created, 0 updated, 1 skipped\n");
}

#[test]
fn submit_updates_gitlab_mr_target_when_parent_changed() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);
    let fake = FakeProvider::new()
        .on(
            "mr list",
            r##"[{"iid":34,"state":"opened","target_branch":"feature/a","source_branch":"feature/b","web_url":"https://gitlab.com/lararosekelley/git-stk-mirror/-/merge_requests/34"}]"##,
        )
        .on("mr update", "updated mr")
        .fallback_fail("unexpected glab args")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains("updated feature/b -> main (!34)"))
        .stdout(predicates::str::contains("updated mr"))
        .stdout(predicates::str::contains(
            "submit complete: 0 created, 1 updated, 0 skipped",
        ));
}

#[test]
fn submit_creates_github_pr_without_fill() {
    // --fill turns a multi-commit branch into a bulleted dump of every commit
    // subject, which then renders awkwardly under git-stk's template and stack
    // overview. git-stk pushes the branch itself and overwrites the body, so it
    // creates the PR with an explicit title and body from the tip commit.
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "the subject line");

    let fake = FakeProvider::new()
        .record(
            "pr create",
            "create-args.txt",
            "https://github.com/owner/repo/pull/7",
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/a -> main"));

    let args = fs::read_to_string(repo.path().join("create-args.txt")).expect("create args");
    assert!(
        !args.contains("--fill"),
        "gh create must not use --fill: {args}"
    );
    assert!(
        args.contains("--title the subject line"),
        "missing title: {args}"
    );
    assert!(args.contains("--body"), "missing body: {args}");
}

#[test]
fn submit_creates_gitlab_mr_without_fill() {
    // glab's --fill re-pushes the current checkout onto the source ref (gh
    // never pushes on create), which clobbers a sibling branch when submitting
    // a stack from its leaf. git-stk pushes branches itself, so it must create
    // the MR non-interactively without --fill.
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "the subject line");

    let fake = FakeProvider::new()
        .record("mr create", "create-args.txt", "created mr")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/a -> main"));

    let args = fs::read_to_string(repo.path().join("create-args.txt")).expect("create args");
    assert!(
        !args.contains("--fill"),
        "glab create must not use --fill: {args}"
    );
    assert!(
        args.contains("--title the subject line"),
        "missing title: {args}"
    );
    assert!(
        args.contains("--description"),
        "missing description: {args}"
    );
    assert!(args.contains("--yes"), "missing --yes: {args}");
}

#[test]
fn submit_wraps_the_github_pr_template_in_the_description_block() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    // The template is a committed working-tree file, like any repo would have.
    fs::create_dir_all(repo.path().join(".github")).unwrap();
    repo.write(
        ".github/pull_request_template.md",
        "## Summary\n\n- [ ] Tests pass\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "add PR template"]);

    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);

    let fake = FakeProvider::new()
        // No review at create time; `pr create` records a marker so the
        // post-create lookup (for seeding) then resolves to #12.
        .record(
            "pr create",
            "created.txt",
            "https://github.com/owner/repo/pull/12",
        )
        .on("pr view 12", r#"{"body":""}"#)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .on_after(
            "feature/b",
            "created.txt",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/12","title":"B work"}]"##,
        )
        .on("feature/b", "[]")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/b -> main"))
        .stdout(predicates::str::contains("seeded the PR template into #12"));

    // No --desc, so the template is wrapped in the managed description block -
    // no stray subject-as-body line, no seam.
    let body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("seeded body");
    assert_eq!(
        body.trim_end(),
        "pr edit 12 --body <!-- git-stk:description -->\n## Summary\n\n- [ ] Tests pass\n<!-- /git-stk:description -->"
    );
}

#[test]
fn submit_seeds_the_gitlab_default_mr_template() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    fs::create_dir_all(repo.path().join(".gitlab/merge_request_templates")).unwrap();
    repo.write(
        ".gitlab/merge_request_templates/Default.md",
        "## What\n\n## Why\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "add MR template"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "the subject");

    let fake = FakeProvider::new()
        .record("mr create", "created.txt", "created mr")
        .on("mr view 7", r#"{"description":""}"#)
        .record("mr update 7 --description", "update-7.txt", "")
        .on_after(
            "feature/a",
            "created.txt",
            r##"[{"iid":7,"state":"opened","source_branch":"feature/a","target_branch":"main","web_url":"https://gitlab.com/o/r/-/merge_requests/7","title":"the subject"}]"##,
        )
        .on("feature/a", "[]")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "feature/a"])
        .assert()
        .success()
        .stdout(predicates::str::contains("seeded the PR template into !7"));

    let body = fs::read_to_string(repo.path().join("update-7.txt")).expect("seeded body");
    assert!(
        body.contains(
            "--description <!-- git-stk:description -->\n## What\n\n## Why\n<!-- /git-stk:description -->"
        ),
        "body: {body}"
    );
}

#[test]
fn submit_wraps_the_template_without_a_seam_when_no_desc() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    fs::create_dir_all(repo.path().join(".github")).unwrap();
    repo.write(
        ".github/pull_request_template.md",
        "## Summary\n\n- [ ] Tests pass\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "add PR template"]);

    // The branch name references an issue, so a `Closes #5` note follows. With
    // no --desc the template is wrapped in the description block, so there is no
    // freeform region and no seam.
    repo.git(["switch", "-c", "5-fix"]);
    repo.git(["config", "branch.5-fix.stkParent", "main"]);

    let fake = FakeProvider::new()
        .record(
            "pr create",
            "created.txt",
            "https://github.com/owner/repo/pull/12",
        )
        .on("pr view 12", r#"{"body":""}"#)
        // Capture every body edit so the seed's own edit is visible even after
        // the later Closes edit.
        .record_append("pr edit 12 --body", "edits-12.log", "")
        .on_after(
            "5-fix",
            "created.txt",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"5-fix","url":"https://github.com/owner/repo/pull/12","title":"Fix"}]"##,
        )
        .on("5-fix", "[]")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "5-fix"])
        .assert()
        .success()
        .stdout(predicates::str::contains("seeded the PR template into #12"));

    let edits = fs::read_to_string(repo.path().join("edits-12.log")).expect("edits log");
    assert!(
        edits.contains("pr edit 12 --body <!-- git-stk:description -->\n## Summary\n\n- [ ] Tests pass\n<!-- /git-stk:description -->"),
        "seed should wrap the template in the description block:\n{edits}"
    );
    assert!(
        !edits.contains("---"),
        "the wrap path lays no seam:\n{edits}"
    );
}

#[test]
fn submit_desc_replaces_the_template() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    fs::create_dir_all(repo.path().join(".github")).unwrap();
    repo.write(
        ".github/pull_request_template.md",
        "## Summary\n\n- [ ] Tests pass\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "add PR template"]);

    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);

    // create_review would seed the body with the commit's subject echo; model
    // that so the seed step has something to drop rather than a no-op.
    let fake = FakeProvider::new()
        .record(
            "pr create",
            "created.txt",
            "https://github.com/owner/repo/pull/12",
        )
        .on("pr view 12", r#"{"body":"add PR template"}"#)
        .record_append("pr edit 12 --body", "edits-12.log", "")
        .on_after(
            "feature/b",
            "created.txt",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/12","title":"B work"}]"##,
        )
        .on("feature/b", "[]")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "feature/b", "-d", "What and why."])
        .assert()
        .success()
        // The template is ignored on a --desc branch, so it is never seeded.
        .stdout(predicates::str::contains("seeded the PR template").not())
        .stdout(predicates::str::contains("set description in #12"));

    // The description replaces the template: no template text, no seam - only
    // the managed description block ends up in the body.
    let edits = fs::read_to_string(repo.path().join("edits-12.log")).expect("edits log");
    assert!(
        !edits.contains("## Summary") && !edits.contains("- [ ] Tests pass"),
        "the template must not be seeded on a --desc branch:\n{edits}"
    );
    assert!(
        !edits.contains("---"),
        "no template means no seam:\n{edits}"
    );
    assert!(
        edits
            .contains("<!-- git-stk:description -->\nWhat and why.\n<!-- /git-stk:description -->"),
        "the --desc block should be written:\n{edits}"
    );
}

#[test]
fn submit_reviewers_requests_reviews_stripping_the_at_prefix() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);

    let fake = FakeProvider::new()
        .record(
            "pr create",
            "created.txt",
            "https://github.com/owner/repo/pull/12",
        )
        .record("pr edit 12 --add-reviewer", "reviewer-args.txt", "")
        .on_after(
            "feature/b",
            "created.txt",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/12","title":"B work"}]"##,
        )
        .on("feature/b", "[]")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        // A team (org/team) rides alongside the users; the `@` is optional.
        .args([
            "submit",
            "feature/b",
            "--reviewers",
            "@foo,bar,@my-org/backend",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "requested reviews from foo, bar, my-org/backend in #12",
        ));

    // gh gets the cleaned, comma-joined list: no `@`, team path intact.
    let args = fs::read_to_string(repo.path().join("reviewer-args.txt")).expect("reviewer args");
    assert!(
        args.contains("--add-reviewer foo,bar,my-org/backend"),
        "reviewers should be passed cleaned and comma-joined:\n{args}"
    );
}

#[test]
fn submit_skips_the_template_when_use_pr_template_is_off() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.usePrTemplate", "false"]);
    fs::create_dir_all(repo.path().join(".github")).unwrap();
    repo.write(".github/pull_request_template.md", "## Summary\n");
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "add PR template"]);

    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);

    // No body lookup/edit should happen: the guard short-circuits before any
    // provider call beyond the create and its pre-check, so a stray call fails.
    let fake = FakeProvider::new()
        .record(
            "pr create",
            "created.txt",
            "https://github.com/owner/repo/pull/12",
        )
        .on("feature/b", "[]")
        .fallback_fail("unexpected gh args")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "feature/b"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/b -> main"))
        .stdout(predicates::str::contains("seeded the PR template").not());
}

#[test]
fn submit_dry_run_reports_update_without_calling_update() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);
    let fake = FakeProvider::new()
        .on(
            "mr list",
            r##"[{"iid":34,"state":"opened","target_branch":"feature/a","source_branch":"feature/b","web_url":"https://gitlab.com/lararosekelley/git-stk-mirror/-/merge_requests/34"}]"##,
        )
        .fallback_fail("unexpected glab args")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--dry-run", "feature/b"])
        .assert()
        .success()
        .stdout("would update feature/b -> main (!34)\nsubmit complete: 0 created, 1 updated, 0 skipped\n");
}

#[test]
fn submit_requires_stack_parent() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack()
        .args(["submit", "feature/b"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("feature/b has no stack parent"));
}

#[test]
fn submit_stack_creates_reviews_parent_first() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    let log_path = repo.path().join("submit.log");
    let fake = FakeProvider::new()
        .log_all("submit.log")
        .on("pr list", "[]")
        .on("pr create", "created url")
        .fallback_fail("unexpected gh args")
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("created feature/a -> main"))
        .stdout(predicates::str::contains("created feature/b -> feature/a"))
        .stdout(predicates::str::contains(
            "submit complete: 2 created, 0 updated, 0 skipped",
        ));

    let log = fs::read_to_string(log_path).expect("read submit log");
    let create_a = log
        .find("pr create --head feature/a --base main --title")
        .expect("feature/a create call");
    let create_b = log
        .find("pr create --head feature/b --base feature/a --title")
        .expect("feature/b create call");
    assert!(create_a < create_b, "parent should submit before child");
}

/// Standing on the parentless root itself: it is the base the line sits on,
/// so the branches above it submit and it does not (#307).
#[test]
fn submit_stack_from_a_parentless_root_submits_the_branches_above_it() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);
    repo.git(["switch", "feature/a"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a is this stack's base; not submitted",
        ))
        .stdout(predicates::str::contains(
            "would create feature/b -> feature/a",
        ));
}

#[test]
fn submit_stack_writes_stack_overview_into_review_bodies() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new()
        .on("pr view 12", r##"{"body":"Parent PR description."}"##)
        .on("pr view 13", r##"{"body":"Child PR description."}"##)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .record("pr edit 13 --body", "edit-body-13.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"Bottom change"}]"##,
        )
        .on(
            "feature/b",
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"Top change"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("updated stack note in #12"))
        .stdout(predicates::str::contains("updated stack note in #13"));

    // The bottom PR's body: full list leaf-first, pointer on itself,
    // trunk in backticks, footer link.
    let bottom = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("bottom body");
    assert!(bottom.contains("Parent PR description."));
    assert!(bottom.contains("<!-- git-stk:data "));
    assert!(
        bottom.contains("- \u{1F7E2} [Top change (#13)](https://github.com/owner/repo/pull/13)")
    );
    assert!(bottom.contains(
        "- \u{1F7E2} [Bottom change (#12)](https://github.com/owner/repo/pull/12) \u{1F448}"
    ));
    assert!(bottom.contains("- `main`"));
    assert!(
        bottom.contains(
            "Stack managed by \
             <img src=\"https://raw.githubusercontent.com/lararosekelley/git-stk/main/assets/logo.svg\" \
             width=\"12\" height=\"12\" alt=\"\" /> \
             [git-stk](https://github.com/lararosekelley/git-stk)"
        )
    );

    // The top PR points at itself instead.
    let top = fs::read_to_string(repo.path().join("edit-body-13.txt")).expect("top body");
    assert!(top.contains(
        "- \u{1F7E2} [Top change (#13)](https://github.com/owner/repo/pull/13) \u{1F448}"
    ));
    assert!(!top.contains("(#12)](https://github.com/owner/repo/pull/12) \u{1F448}"));
}

#[test]
fn submit_links_issue_referenced_by_branch_name() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "5-fix-thing"]);
    repo.git(["config", "branch.5-fix-thing.stkParent", "main"]);
    let fake = FakeProvider::new()
        .on("pr view 12", r##"{"body":"Description."}"##)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .on(
            "5-fix-thing",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"5-fix-thing","url":"https://github.com/owner/repo/pull/12","title":"Fix thing"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    // Dry run announces the link without editing anything.
    repo.stack_faked(&fake)
        .args(["submit", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would link issue #5 in #12"));
    assert!(!repo.path().join("edit-body-12.txt").exists());

    repo.stack_faked(&fake)
        .arg("submit")
        .assert()
        .success()
        .stdout(predicates::str::contains("linked issue #5 in #12"));

    let body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("edited body");
    assert!(body.contains("Description."));
    assert!(body.contains("<!-- git-stk:closes -->\nCloses #5\n<!-- /git-stk:closes -->"));
}

#[test]
fn submit_desc_sets_replaces_and_clears_the_description_block() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);

    // First pass: a body with an existing stack section; the description
    // must land above it.
    let fake = FakeProvider::new()
        .on(
            "pr view 12",
            r##"{"body":"Intro.\n\n<!-- git-stk:stack -->\nstack list\n<!-- /git-stk:stack -->"}"##,
        )
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--dry-run", "-d", "What and why."])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would set the description in #12",
        ));
    assert!(!repo.path().join("edit-body-12.txt").exists());

    repo.stack_faked(&fake)
        .args(["submit", "-d", "What and why."])
        .assert()
        .success()
        .stdout(predicates::str::contains("set description in #12"));

    let body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("edited body");
    assert!(
        body.contains("<!-- git-stk:description -->\nWhat and why.\n<!-- /git-stk:description -->")
    );
    let intro = body.find("Intro.").expect("intro");
    let description = body.find("What and why.").expect("description");
    let stack = body.find("stack list").expect("stack");
    assert!(intro < description && description < stack);

    // Second pass: a body that already carries a description; an empty
    // --desc clears the block and leaves the rest alone.
    let fake = FakeProvider::new()
        .on(
            "pr view 12",
            r##"{"body":"Intro.\n\n<!-- git-stk:description -->\nStale.\n<!-- /git-stk:description -->"}"##,
        )
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "-d", ""])
        .assert()
        .success()
        .stdout(predicates::str::contains("cleared description in #12"));

    let body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("edited body");
    assert!(body.contains("Intro."));
    assert!(!body.contains("git-stk:description"));
    assert!(!body.contains("Stale."));
}

#[test]
fn submit_title_names_a_new_github_pr() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);
    let fake = FakeProvider::new()
        .on("pr list", "[]")
        .record(
            "pr create",
            "create.txt",
            "https://github.com/owner/repo/pull/12",
        )
        .fallback_fail("unexpected gh args")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--title", "Teach the parser about spans"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            r#"created feature/b -> feature/a titled "Teach the parser about spans""#,
        ));

    // The title goes in at create time, so the PR is never published under the
    // commit subject and no follow-up edit is needed.
    let args = fs::read_to_string(repo.path().join("create.txt")).expect("create args");
    assert!(
        args.contains("--title Teach the parser about spans"),
        "create should carry the title: {args}"
    );
}

#[test]
fn submit_title_retitles_an_existing_github_pr() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);
    let fake = FakeProvider::new()
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"wip"}]"##,
        )
        .record("pr edit 12 --title", "edit-title-12.txt", "")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--dry-run", "-t", "A better title"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would set the title in #12"));
    assert!(!repo.path().join("edit-title-12.txt").exists());

    repo.stack_faked(&fake)
        .args(["submit", "-t", "A better title"])
        .assert()
        .success()
        .stdout(predicates::str::contains("set title in #12"));

    let args = fs::read_to_string(repo.path().join("edit-title-12.txt")).expect("edit args");
    assert!(args.contains("--title A better title"), "edit args: {args}");
}

#[test]
fn submit_title_keeps_a_gitlab_draft_prefix() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);
    let fake = FakeProvider::new()
        .commands(&["glab"])
        .on(
            "feature/a",
            r##"[{"iid":34,"state":"opened","draft":true,"target_branch":"main","source_branch":"feature/a","web_url":"https://gitlab.com/owner/repo/-/merge_requests/34","title":"Draft: wip"}]"##,
        )
        .record("mr update 34 --title", "update-title-34.txt", "")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "-t", "A better title"])
        .assert()
        .success()
        .stdout(predicates::str::contains("set title in !34"));

    // GitLab keeps draft state in the title, so the retitle must carry the
    // prefix forward rather than quietly readying the MR.
    let args = fs::read_to_string(repo.path().join("update-title-34.txt")).expect("update args");
    assert!(
        args.contains("--title Draft: A better title"),
        "update args: {args}"
    );
}

#[test]
fn submit_draft_title_does_not_double_the_gitlab_draft_prefix() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);
    let fake = FakeProvider::new()
        .commands(&["glab"])
        .on("mr list", "[]")
        .record(
            "mr create",
            "create.txt",
            "https://gitlab.com/owner/repo/-/merge_requests/34",
        )
        .fallback("[]")
        .install(&repo);

    // glab spells --draft as the `Draft: ` title prefix, so a title that
    // already says so is a draft already; asking for both would stack markers.
    repo.stack_faked(&fake)
        .args(["submit", "--draft", "-t", "Draft: a work"])
        .assert()
        .success();

    let args = std::fs::read_to_string(repo.path().join("create.txt")).expect("create args");
    assert!(
        args.contains("--title Draft: a work"),
        "create args: {args}"
    );
    assert!(!args.contains("--draft"), "create args: {args}");
}

#[test]
fn submit_title_rejects_an_empty_string() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);

    // Unlike --desc, an empty title has no clearing meaning - a review always
    // has one - so it is refused before any provider call.
    repo.stack()
        .args(["submit", "--title", "  "])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--title cannot be empty"));
}

#[test]
fn submit_desc_file_reads_the_description_from_a_file() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);

    // A markdown doc, as an agent might hand off. Surrounding blank lines are
    // trimmed so the block reads cleanly.
    repo.write(
        "desc.md",
        "\n## What\n\nThe change.\n\n## Why\n\nBecause.\n\n",
    );

    let fake = FakeProvider::new()
        .on("pr view 12", r#"{"body":""}"#)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--desc-file", "desc.md"])
        .assert()
        .success()
        .stdout(predicates::str::contains("set description in #12"));

    let body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("edited body");
    assert!(
        body.contains(
            "<!-- git-stk:description -->\n## What\n\nThe change.\n\n## Why\n\nBecause.\n<!-- /git-stk:description -->"
        ),
        "body: {body}"
    );
}

#[test]
fn submit_desc_file_clears_the_block_when_the_file_is_blank() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);

    // A whitespace-only file trims to "", the same as `--desc ""`, so it clears
    // an existing description block and leaves the rest of the body alone.
    repo.write("desc.md", "   \n\n\t\n");

    let fake = FakeProvider::new()
        .on(
            "pr view 12",
            r##"{"body":"Intro.\n\n<!-- git-stk:description -->\nStale.\n<!-- /git-stk:description -->"}"##,
        )
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--desc-file", "desc.md"])
        .assert()
        .success()
        .stdout(predicates::str::contains("cleared description in #12"));

    let body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("edited body");
    assert!(body.contains("Intro."));
    assert!(!body.contains("git-stk:description"));
    assert!(!body.contains("Stale."));
}

#[test]
fn submit_desc_file_expands_a_leading_tilde() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);

    // The file lives at $HOME/pr.md; `~/pr.md` must resolve to it even though
    // the shell never expanded the tilde (here it is a literal argument).
    repo.write("pr.md", "From the tilde path.\n");

    let fake = FakeProvider::new()
        .on("pr view 12", r#"{"body":""}"#)
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .env("HOME", repo.path())
        .args(["submit", "--desc-file", "~/pr.md"])
        .assert()
        .success()
        .stdout(predicates::str::contains("set description in #12"));

    let body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("edited body");
    assert!(
        body.contains(
            "<!-- git-stk:description -->\nFrom the tilde path.\n<!-- /git-stk:description -->"
        ),
        "body: {body}"
    );
}

#[test]
fn submit_desc_drops_the_subject_echo_when_no_template() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    // No PR template in this repo.
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);
    // A subject with no commit body: create_review would echo the subject as
    // the PR body, which must not linger above a supplied description.
    repo.commit_file("a.txt", "a\n", "the subject line");

    let fake = FakeProvider::new()
        .record(
            "pr create",
            "created.txt",
            "https://github.com/owner/repo/pull/12",
        )
        // create_review seeded the body with the echoed subject.
        .on("pr view 12", r#"{"body":"the subject line"}"#)
        .record_append("pr edit 12 --body", "edits-12.log", "")
        .on_after(
            "feature/b",
            "created.txt",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/12","title":"the subject line"}]"##,
        )
        .on("feature/b", "[]")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "feature/b", "-d", "Real description."])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "dropped the commit subject from #12",
        ))
        .stdout(predicates::str::contains("set description in #12"));

    // The seed's edit cleared the echoed subject, and the description landed in
    // its own managed block.
    let edits = fs::read_to_string(repo.path().join("edits-12.log")).expect("edits log");
    assert!(
        edits.contains(
            "<!-- git-stk:description -->\nReal description.\n<!-- /git-stk:description -->"
        ),
        "description block missing:\n{edits}"
    );
}

#[test]
fn submit_desc_file_rejects_a_missing_file() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["config", "branch.feature/a.stkParent", "main"]);

    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--desc-file", "nope.md"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("failed to read description file"));
}

#[test]
fn submit_stack_desc_targets_only_the_current_branch() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new()
        .on("pr view 12", r##"{"body":""}"##)
        .on("pr view 13", r##"{"body":""}"##)
        .record_append("pr edit 12 --body", "edit-body-12.log", "")
        .record_append("pr edit 13 --body", "edit-body-13.log", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"Bottom change"}]"##,
        )
        .on(
            "feature/b",
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"Top change"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    // Standing on the leaf: the description belongs to its review alone.
    repo.stack_faked(&fake)
        .args(["submit", "--stack", "-d", "Top summary."])
        .assert()
        .success()
        .stdout(predicates::str::contains("set description in #13"));

    let top = fs::read_to_string(repo.path().join("edit-body-13.log")).expect("top edits");
    assert!(
        top.contains("<!-- git-stk:description -->\nTop summary.\n<!-- /git-stk:description -->")
    );
    let bottom = fs::read_to_string(repo.path().join("edit-body-12.log")).expect("bottom edits");
    assert!(!bottom.contains("git-stk:description"));
}

#[test]
fn submit_stack_preserves_merged_ledger_entries() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    // The bottom PR's body carries a ledger that remembers #11, a review
    // whose branch merged and was deleted long ago. The top PR has never
    // seen it.
    let fake = FakeProvider::new()
        .on("pr view 11", r##"{"body":"Old description."}"##)
        .on(
            "pr view 12",
            r##"{"body":"Intro.\n\n<!-- git-stk:stack -->\n<!-- git-stk:data [{\"id\":\"#11\",\"url\":\"https://github.com/owner/repo/pull/11\",\"title\":\"Landed\",\"state\":\"merged\"}] -->\n- stale bullets\n<!-- /git-stk:stack -->"}"##,
        )
        .on("pr view 13", r##"{"body":""}"##)
        .record("pr edit 11 --body", "edit-body-11.txt", "")
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .record("pr edit 13 --body", "edit-body-13.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"Bottom change"}]"##,
        )
        .on(
            "feature/b",
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"Top change"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("updated stack note in #12"))
        .stdout(predicates::str::contains("updated stack note in #13"))
        .stdout(predicates::str::contains("updated stack note in #11"));

    // The merged entry survives, restyled, below the live stack.
    let bottom = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("bottom body");
    assert!(bottom.contains("Intro."));
    assert!(bottom.contains(
        "- \u{1F7E3} ~~[Landed (#11)](https://github.com/owner/repo/pull/11)~~ (merged)"
    ));
    let top_at = bottom.find("(#13)").expect("top entry");
    let bottom_at = bottom.find("(#12)").expect("bottom entry");
    let landed_at = bottom.find("(#11)").expect("merged entry");
    assert!(
        top_at < bottom_at && bottom_at < landed_at,
        "leaf-first order"
    );

    // History propagates to bodies that never carried it.
    let top = fs::read_to_string(repo.path().join("edit-body-13.txt")).expect("top body");
    assert!(top.contains("~~[Landed (#11)](https://github.com/owner/repo/pull/11)~~ (merged)"));

    // The merged review's own body gets the refreshed ledger, pointing at
    // itself.
    let landed = fs::read_to_string(repo.path().join("edit-body-11.txt")).expect("merged body");
    assert!(landed.contains("Old description."));
    assert!(landed.contains(
        "- \u{1F7E3} ~~[Landed (#11)](https://github.com/owner/repo/pull/11)~~ (merged) \u{1F448}"
    ));
}

#[test]
fn submit_stack_refreshes_a_carried_forward_row_that_has_since_merged() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    // The bottom PR's ledger remembers #11 as OPEN - it was open when last
    // written - but #11 has since merged. Its branch is gone from the local
    // stack, so only a re-fetch by id can catch the state change. The
    // `--json state` rule is more specific and precedes the generic body rule.
    let fake = FakeProvider::new()
        .on("pr view 11 --json state", r##"{"state":"MERGED"}"##)
        .on("pr view 11", r##"{"body":"Old description."}"##)
        .on(
            "pr view 12",
            r##"{"body":"Intro.\n\n<!-- git-stk:stack -->\n<!-- git-stk:data [{\"id\":\"#11\",\"url\":\"https://github.com/owner/repo/pull/11\",\"title\":\"Landed\",\"state\":\"open\"}] -->\n- stale bullets\n<!-- /git-stk:stack -->"}"##,
        )
        .on("pr view 13", r##"{"body":""}"##)
        .record("pr edit 11 --body", "edit-body-11.txt", "")
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .record("pr edit 13 --body", "edit-body-13.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"Bottom change"}]"##,
        )
        .on(
            "feature/b",
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"Top change"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success();

    // The carried-forward row is now rendered merged, not the stale open green.
    let bottom = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("bottom body");
    assert!(bottom.contains(
        "- \u{1F7E3} ~~[Landed (#11)](https://github.com/owner/repo/pull/11)~~ (merged)"
    ));
    assert!(
        !bottom.contains("- \u{1F7E2} [Landed (#11)]"),
        "the stale open styling must be gone"
    );
    // And the refreshed state is persisted in the machine-readable data line,
    // so later runs see it as terminal and stop re-fetching it.
    assert!(bottom.contains(r#""state":"merged""#));
}

#[test]
fn submit_stack_repairs_mangled_note_markup() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new()
        .on("pr view 12", r##"{"body":"Intro."}"##)
        .on(
            "pr view 13",
            r##"{"body":"Intro.\n\n<!-- git-stk:stack -->\nuser deleted the end marker"}"##,
        )
        .record("pr edit 12 --body", "edit-body-12.txt", "")
        .record("pr edit 13 --body", "edit-body-13.txt", "")
        .on(
            "feature/a",
            r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12","title":"Bottom change"}]"##,
        )
        .on(
            "feature/b",
            r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/13","title":"Top change"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success();

    let top = fs::read_to_string(repo.path().join("edit-body-13.txt")).expect("top body");
    assert_eq!(top.matches("<!-- git-stk:stack -->").count(), 1);
    assert_eq!(top.matches("<!-- /git-stk:stack -->").count(), 1);
    assert!(top.contains("Intro."));
    assert!(top.contains("user deleted the end marker"));
    assert!(top.contains("- \u{1F7E2} [Top change (#13)]"));
}

#[test]
fn submit_stack_writes_overview_into_gitlab_descriptions() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "gitlab"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new()
        .on("mr view 34", r##"{"description":""}"##)
        .on("mr view 35", r##"{"description":""}"##)
        .record("mr update 35 --description", "update-description-args.txt", "")
        .on("mr update 34 --description", "updated 34")
        .on(
            "feature/a",
            r##"[{"iid":34,"state":"opened","target_branch":"main","source_branch":"feature/a","web_url":"https://gitlab.com/owner/repo/-/merge_requests/34","title":"Bottom change"}]"##,
        )
        .on(
            "feature/b",
            r##"[{"iid":35,"state":"opened","target_branch":"feature/a","source_branch":"feature/b","web_url":"https://gitlab.com/owner/repo/-/merge_requests/35","title":"Top change"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("updated stack note in !35"));

    let recorded = fs::read_to_string(repo.path().join("update-description-args.txt"))
        .expect("update description args");
    assert!(recorded.contains(
        "- \u{1F7E2} [Top change (!35)](https://gitlab.com/owner/repo/-/merge_requests/35) \u{1F448}"
    ));
    assert!(recorded.contains(
        "- \u{1F7E2} [Bottom change (!34)](https://gitlab.com/owner/repo/-/merge_requests/34)"
    ));
    assert!(recorded.contains("- `main`"));
}

#[test]
fn submit_stack_push_pushes_branches_before_provider_calls() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "child change");

    // Bare origin with no branches: submit --push must create them remotely.
    let bare = repo.add_bare_origin(&[]);
    let fake = FakeProvider::new()
        .on("pr create", "created review")
        .fallback("[]")
        .install(&repo);

    repo.git(["switch", "feature/a"]);
    let assert = repo
        .stack_faked(&fake)
        .args(["submit", "--stack", "--push"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "pushed feature/a feature/b to origin",
        ));

    // Remote branches exist and match local.
    assert_eq!(
        repo.remote_sha(&bare, "feature/a"),
        repo.git(["rev-parse", "feature/a"])
    );
    assert_eq!(
        repo.remote_sha(&bare, "feature/b"),
        repo.git(["rev-parse", "feature/b"])
    );

    // Push output precedes review creation output.
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let push_at = stdout.find("pushed feature/a").expect("push line");
    let create_at = stdout.find("created feature/a").expect("create line");
    assert!(
        push_at < create_at,
        "push must happen before submit:\n{stdout}"
    );

    // Upstream tracking was set.
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.remote"]),
        "origin"
    );
}

#[test]
fn submit_push_rejected_as_stale_points_at_sync() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");

    let bare = repo.add_bare_origin(&["main", "feature/a"]);

    // Simulate the remote moving on under the stack - the classic "a lower
    // branch merged and the branch advanced upstream" - by rewinding the
    // remote's feature/a out of band, so the local --force-with-lease lease is
    // now stale and the push is rejected.
    let remote_main = repo.remote_sha(&bare, "main");
    let rewind = Command::new("git")
        .args(["update-ref", "refs/heads/feature/a", &remote_main])
        .current_dir(bare.path())
        .output()
        .expect("rewind remote ref");
    assert!(rewind.status.success(), "failed to rewind the remote ref");

    repo.git(["switch", "feature/a"]);
    repo.stack()
        .args(["submit", "--push"])
        .assert()
        .failure()
        // The actionable guidance replaces git's raw plumbing error.
        .stderr(predicates::str::contains("the remote has moved on"))
        .stderr(predicates::str::contains("git stk sync"))
        .stderr(predicates::str::contains("stale info").not());
}

#[test]
fn submit_push_respects_config_and_no_push_overrides_it() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.pushOnSubmit", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");

    let bare = repo.add_bare_origin(&[]);
    let fake = FakeProvider::new()
        .on("pr create", "created review")
        .fallback("[]")
        .install(&repo);

    // Config enables the push.
    repo.stack_faked(&fake)
        .args(["submit"])
        .assert()
        .success()
        .stdout(predicates::str::contains("pushed feature/a to origin"));
    assert_eq!(
        repo.remote_sha(&bare, "feature/a"),
        repo.git(["rev-parse", "feature/a"])
    );

    // --no-push overrides the config.
    repo.commit_file("a2.txt", "a2\n", "more work");
    let stale = repo.remote_sha(&bare, "feature/a");
    repo.stack_faked(&fake)
        .args(["submit", "--no-push"])
        .assert()
        .success();
    assert_eq!(repo.remote_sha(&bare, "feature/a"), stale);
}

#[test]
fn submit_push_dry_run_does_not_push() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "parent change");

    let bare = repo.add_bare_origin(&[]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--push", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would push feature/a to origin"));

    let remote = Command::new("git")
        .args(["rev-parse", "feature/a"])
        .current_dir(bare.path())
        .output()
        .expect("check remote");
    assert!(!remote.status.success(), "dry run must not push");
}

#[test]
fn submit_stack_covers_whole_stack_from_the_leaf() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    // Standing on the LEAF: position must not matter.
    repo.stack_faked(&fake)
        .args(["submit", "--stack", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would create feature/a -> main"))
        .stdout(predicates::str::contains(
            "would create feature/b -> feature/a",
        ));
}

#[test]
fn submit_downstack_stops_at_the_current_branch() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.stack().args(["new", "feature/c"]).assert().success();
    repo.git(["switch", "feature/b"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    // Standing mid-stack: the WIP leaf above stays unsubmitted.
    repo.stack_faked(&fake)
        .args(["submit", "--downstack", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would create feature/a -> main"))
        .stdout(predicates::str::contains(
            "would create feature/b -> feature/a",
        ))
        .stdout(predicates::str::contains("feature/c").not());

    // The scopes are mutually exclusive.
    repo.stack()
        .args(["submit", "--downstack", "--stack"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}

#[test]
fn submit_draft_flag_and_config_control_creation() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "main"]);
    let log_path = repo.path().join("submit.log");
    let fake = FakeProvider::new()
        .log_all("submit.log")
        .on("pr create", "created url")
        .fallback("[]")
        .install(&repo);

    // --draft passes through to creation.
    repo.stack_faked(&fake)
        .args(["submit", "--draft"])
        .assert()
        .success();
    let log = fs::read_to_string(&log_path).expect("submit log");
    assert!(log.contains("pr create --head feature/b --base main --title"));
    assert!(log.contains("--draft"));

    // The config makes drafts the default; --no-draft overrides it.
    fs::remove_file(&log_path).expect("reset log");
    repo.git(["config", "stk.submitDraft", "true"]);
    repo.stack_faked(&fake).arg("submit").assert().success();
    let log = fs::read_to_string(&log_path).expect("submit log");
    assert!(log.contains("--draft"));

    fs::remove_file(&log_path).expect("reset log");
    repo.stack_faked(&fake)
        .args(["submit", "--no-draft"])
        .assert()
        .success();
    let log = fs::read_to_string(&log_path).expect("submit log");
    assert!(log.contains("pr create --head feature/b --base main --title"));
    assert!(!log.contains("--draft"));
}

#[test]
fn submit_ready_marks_draft_reviews_ready() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "demo"]);
    repo.git(["config", "stk.submitDraft", "true"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // Drafted by config, then flipped ready in one stack-wide pass.
    repo.stack().args(["submit", "--stack"]).assert().success();
    repo.stack()
        .args(["submit", "--stack", "--ready"])
        .assert()
        .success()
        .stdout(predicates::str::contains("marked #1 ready"))
        .stdout(predicates::str::contains("marked #2 ready"));

    // Already ready: nothing left to mark.
    repo.stack()
        .args(["submit", "--stack", "--ready"])
        .assert()
        .success()
        .stdout(predicates::str::contains("marked").not());
}

#[test]
fn bare_submit_uses_submit_stack_config() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.submitStack", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    // Bare submit from the leaf: config turns on whole-stack mode.
    repo.stack_faked(&fake)
        .args(["submit", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would create feature/a -> main"))
        .stdout(predicates::str::contains(
            "would create feature/b -> feature/a",
        ));

    // --no-stack overrides the config back to single-branch.
    repo.stack_faked(&fake)
        .args(["submit", "--dry-run", "--no-stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would create feature/b -> feature/a",
        ))
        .stdout(predicates::str::contains("feature/a -> main").not());

    // An explicit branch also means single-branch, config or not.
    repo.stack_faked(&fake)
        .args(["submit", "feature/a", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would create feature/a -> main"))
        .stdout(predicates::str::contains("feature/b").not());
}

/// A stack rooted on a non-trunk branch - a release line, say - keeps that
/// parentless root in its path as the base the branch above targets. Stack
/// mode used to refuse the whole submit over it (#307), even though
/// `--no-stack` handled it and `new`/`adopt` both create the shape.
#[test]
fn submit_stack_treats_a_parentless_root_as_the_base_not_a_branch_to_submit() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    // A release line off the trunk, with no stack metadata of its own.
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "rc-20260817 is this stack's base; not submitted",
        ))
        .stdout(predicates::str::contains(
            "would create fix/shared -> rc-20260817",
        ))
        .stdout(predicates::str::contains(
            "submit complete: 1 created, 0 updated, 0 skipped",
        ));
}

/// Same shape, reached through `stk.submitStack` rather than the flag - the
/// configuration the bug was reported under.
#[test]
fn submit_stack_config_handles_a_parentless_root() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.submitStack", "true"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would create fix/shared -> rc-20260817",
        ));
}

#[test]
fn submit_downstack_treats_a_parentless_root_as_the_base() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.stack().args(["new", "fix/above"]).assert().success();
    repo.git(["switch", "fix/shared"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--downstack", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "would create fix/shared -> rc-20260817",
        ))
        .stdout(predicates::str::contains("fix/above").not());
}

/// The base is not ours to move: it must stay out of the `-u
/// --force-with-lease` push that precedes the provider calls.
#[test]
fn submit_stack_does_not_push_a_parentless_root() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    // The base is on the remote already; it is simply not ours to push.
    let _bare = repo.add_bare_origin(&["main", "rc-20260817"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack", "--push", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("would push fix/shared to origin"))
        .stdout(predicates::str::contains("would push rc-20260817").not());
}

/// A branch with no parent and nothing above it is still an error - there is
/// no base to target - but the advice must not be a bare `git stk adopt`,
/// which silently adopts onto the trunk (#307).
#[test]
fn submit_without_a_stack_parent_names_the_branch_and_a_safe_remedy() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    let log_path = repo.path().join("submit.log");
    let fake = FakeProvider::new()
        .log_all("submit.log")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "rc-20260817 has no stack parent; attach it with \
             `git stk adopt rc-20260817 --parent <parent>`",
        ))
        .stderr(predicates::str::contains("`git stk repair`"));

    assert!(
        !log_path.exists(),
        "provider should not be called after validation failure"
    );
}

/// `--title`/`--desc` target the current branch. Standing on the stack's base,
/// that branch is not part of the stack and its review is not ours to edit -
/// so neither may reach it, even though it has an open review of its own.
#[test]
fn submit_stack_does_not_retitle_or_describe_the_base_review() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.git(["switch", "rc-20260817"]);

    let fake = FakeProvider::new()
        .record("pr edit", "edits.txt", "")
        // The release branch's own PR into the trunk.
        .on("rc-20260817", r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on("fix/shared", "[]")
        .on(
            "pr create",
            "https://github.com/lararosekelley/git-stk/pull/13",
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args([
            "submit",
            "--stack",
            "--title",
            "Retitled",
            "--desc",
            "Described.",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "created fix/shared -> rc-20260817",
        ))
        .stdout(predicates::str::contains(
            "skipped title: rc-20260817 is this stack's base",
        ))
        .stdout(predicates::str::contains(
            "skipped description: rc-20260817 is this stack's base",
        ));

    // Nothing was written to the base's own review.
    let edits = fs::read_to_string(repo.path().join("edits.txt")).unwrap_or_default();
    assert!(
        !edits.contains("99"),
        "the base review must not be edited, got: {edits}"
    );
}

/// The base is the `--base` of the review opened for the branch above it, and
/// git-stk does not push it. One the remote has never seen would make the
/// forge reject the create, so say so first - and before pushing anything.
#[test]
fn submit_stack_rejects_a_base_the_remote_does_not_have() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    // The origin knows the trunk, but never saw the base.
    let bare = repo.add_bare_origin(&["main"]);
    let log_path = repo.path().join("submit.log");
    let fake = FakeProvider::new()
        .log_all("submit.log")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack", "--push"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "rc-20260817 is this stack's base, but origin has no such branch",
        ))
        .stderr(predicates::str::contains(
            "push rc-20260817 to origin first",
        ))
        // The branch to re-root is named: a bare `adopt --parent` would
        // re-root the base itself when run from it.
        .stderr(predicates::str::contains(
            "git stk adopt fix/shared --parent <parent>",
        ));

    assert!(
        !log_path.exists(),
        "provider should not be called after validation failure"
    );
    // The check runs before the push, so nothing reached the remote.
    let pushed = Command::new("git")
        .args(["rev-parse", "fix/shared"])
        .current_dir(bare.path())
        .output()
        .expect("check remote");
    assert!(!pushed.status.success(), "nothing should have been pushed");
}

/// `--downstack` standing on the base submits nothing - everything stacked is
/// above you - but the base is a valid base, not an unstacked branch, so the
/// error must not offer to re-root it onto the trunk.
#[test]
fn submit_downstack_on_the_base_names_it_rather_than_offering_to_re_root() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.git(["switch", "rc-20260817"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--downstack", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "rc-20260817 is this stack's base; there is nothing below it to submit",
        ))
        // The `from <branch>` clause, not just the shared prefix: it is the
        // only evidence the two entrances share one bail, so without it
        // re-specialising `--downstack` leaves the suite green.
        .stderr(predicates::str::contains(
            "`git stk submit --stack` from rc-20260817",
        ))
        .stderr(predicates::str::contains("adopt --parent").not());
}

/// Standing on the base is a supported `--stack` position, so the remote-base
/// remedy must never be a bare `adopt --parent` - `adopt` defaults to the
/// current branch, which would re-root the release line itself.
#[test]
fn submit_remote_base_error_never_offers_to_re_root_the_base_itself() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.git(["switch", "rc-20260817"]);
    let _bare = repo.add_bare_origin(&["main"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack", "--push"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "git stk adopt fix/shared --parent <parent>",
        ))
        .stderr(predicates::str::contains("adopt --parent").not());
}

/// `submit <branch>` acts on a branch other than the one checked out, so its
/// "no stack parent" remedy must name that branch - a bare `adopt --parent`
/// would re-root whatever you happen to be standing on.
#[test]
fn submit_named_branch_error_names_that_branch_in_the_adopt_remedy() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.git(["switch", "-c", "orphan/branch"]);
    repo.commit_file("orphan.txt", "o\n", "orphan work");
    // Standing on the release line, submitting a different branch.
    repo.git(["switch", "rc-20260817"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "orphan/branch", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "git stk adopt orphan/branch --parent <parent>",
        ))
        .stderr(predicates::str::contains("adopt --parent").not());
}

/// `stk.submitStack` is off by default, so a bare `submit` from a base takes
/// the single-branch path, where the base trim never runs. It must still not
/// offer to re-root the branch you are standing on. Unmarked base: the
/// children signal is all there is to go on.
#[test]
fn submit_default_path_from_an_unmarked_base_points_at_stack_mode() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    // A stack rooted before bases were recorded: no marker, only children.
    repo.git(["config", "--unset", "branch.rc-20260817.stkFloor"]);
    repo.git(["switch", "rc-20260817"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "rc-20260817 is this stack's base; there is nothing below it to submit",
        ))
        // Named: `--stack` conflicts with naming a branch, so it has to be run
        // from there rather than pointed at it.
        .stderr(predicates::str::contains(
            "`git stk submit --stack` from rc-20260817",
        ))
        .stderr(predicates::str::contains("adopt").not());
}

/// The trunk has no stack parent, so it reaches the same arm - but it is not a
/// base, and the message it used to get pointed at `--stack`, which refuses on
/// the trunk. A dead end.
#[test]
fn submit_on_the_trunk_gets_the_trunk_message_not_the_base_one() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.submitStack", "false"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "main"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "you are on the trunk (main); check out a stacked branch first",
        ))
        .stderr(predicates::str::contains("this stack's base").not());
}

/// `submit <trunk>` from a stacked checkout must not claim you are standing on
/// the trunk - single-branch mode is the one path that can be pointed at a
/// branch you are not on.
#[test]
fn submit_naming_the_trunk_does_not_claim_you_are_on_it() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.submitStack", "false"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "main", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "main is the trunk, so it is never part of a stack",
        ))
        .stderr(predicates::str::contains("you are on the trunk").not());
}

/// `submit <base>` from a sibling stack must not point at a bare
/// `--stack`, which would resolve the stack you are standing in and submit
/// that one instead - force-pushing it first under `stk.pushOnSubmit`.
#[test]
fn submit_base_from_a_sibling_stack_names_the_branch_in_the_pointer() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    // An unrelated stack off the trunk, and we stand in it.
    repo.git(["switch", "main"]);
    repo.stack().args(["new", "other/work"]).assert().success();
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "rc-20260817", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "`git stk submit --stack` from rc-20260817",
        ));
}

/// `children_of(trunk)` says nothing about where you stand, and a stack rooted
/// off the trunk leaves the trunk childless - so the position check has to be
/// the outer one, or `submit <trunk>` claims the repo has no stacks.
#[test]
fn submit_naming_the_trunk_with_only_an_off_trunk_stack_still_names_it() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.submitStack", "false"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "main", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "main is the trunk, so it is never part of a stack",
        ))
        .stderr(predicates::str::contains("no stacked branches to submit").not());
}

/// "no stacked branches" is about the repo, not about the trunk's children -
/// a stack rooted off the trunk leaves the trunk childless while plainly being
/// a stack.
#[test]
fn submit_on_the_trunk_sees_an_off_trunk_stack() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.submitStack", "false"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.git(["switch", "main"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "you are on the trunk (main); check out a stacked branch first",
        ))
        .stderr(predicates::str::contains("no stacked branches").not());
}

/// The stack-mode trunk guard is a separate chain from the single-branch one -
/// it has no outer position check - so it needs its own off-trunk fixture, or
/// reverting it alone leaves the suite green.
#[test]
fn submit_stack_on_the_trunk_sees_an_off_trunk_stack() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.git(["switch", "main"]);
    let fake = FakeProvider::new().fallback("[]").install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "you are on the trunk (main); check out a stacked branch first",
        ))
        .stderr(predicates::str::contains("no stacked branches").not());
}

/// A stack rooted off the trunk: the overview must end at the base it sits on,
/// not at the trunk - the bottom review targets the release line, and this is
/// the only place the base appears in content git-stk publishes.
#[test]
fn submit_stack_overview_ends_at_an_off_trunk_base() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    let fake = FakeProvider::new()
        .on("pr view 13", r##"{"body":"Shared fix description."}"##)
        .record("pr edit 13 --body", "edit-body-13.txt", "")
        .record("pr edit 99", "edit-body-99.txt", "")
        .on("rc-20260817", r##"[{"number":99,"state":"OPEN","baseRefName":"main","headRefName":"rc-20260817","url":"https://example.com/99","title":"Release 20260817"}]"##)
        .on("fix/shared", r##"[{"number":13,"state":"OPEN","baseRefName":"rc-20260817","headRefName":"fix/shared","url":"https://example.com/13","title":"Shared fix"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("updated stack note in #13"));

    let body = fs::read_to_string(repo.path().join("edit-body-13.txt")).expect("body");
    assert!(
        body.contains("- `rc-20260817`"),
        "overview must end at the base, got: {body}"
    );
    assert!(
        !body.contains("- `main`"),
        "overview must not end at the trunk, got: {body}"
    );
    // The base's own review is still never written to - load-bearing, because
    // the fake records `pr edit 99` above.
    assert!(!repo.path().join("edit-body-99.txt").exists());
}

/// With `stk.githubStacks` on, a stack-wide submit hands the ordered reviews
/// to GitHub so the layers get a stack map and parallel review. Bottom first -
/// the order the stack lands in.
#[test]
fn submit_stack_registers_the_stack_with_github() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    let fake = FakeProvider::new()
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        // `record` carries its own stdout, and matching is first-match-wins -
        // so the POST recorder has to sit ahead of the GET.
        .record("api repos/owner/repo/stacks -X POST", "register.txt", "{}")
        .on("api repos/owner/repo/stacks", "[]")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12"}]"##)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://example.com/13"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains("registered #12 #13 as a stack"));

    let call = fs::read_to_string(repo.path().join("register.txt")).expect("register call");
    assert!(
        call.contains("pull_requests[]=12") && call.contains("pull_requests[]=13"),
        "both reviews, bottom first: {call}"
    );
    assert!(
        call.find("=12") < call.find("=13"),
        "order is the order the stack lands in: {call}"
    );
}

/// Off by default: no stack call, and no mention of one.
#[test]
fn submit_stack_does_not_register_unless_enabled() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    let fake = FakeProvider::new()
        .record("api repos", "register.txt", "")
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12"}]"##)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://example.com/13"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(
            predicates::str::contains("stack")
                .and(predicates::str::contains("registered"))
                .not(),
        );

    assert!(!repo.path().join("register.txt").exists());
}

/// GitHub refuses to retarget a review that belongs to a stack - it moves each
/// layer itself as the one below lands. `submit` must say so rather than claim
/// a change it did not make, or fail against the forge.
#[test]
fn submit_does_not_retarget_a_review_github_owns() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.stack().args(["new", "feature/b"]).assert().success();

    let stacks = r##"[{"number":3,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}},
        {"number":13,"head":{"ref":"feature/b"}}]}]"##;
    let fake = FakeProvider::new()
        // Narrow: `pr edit --body` is the overview, which still happens.
        .record("pr edit 13 --base", "retarget.txt", "")
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("api repos/owner/repo/stacks", stacks)
        .on("feature/a", r##"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://example.com/12"}]"##)
        // Stale base: git-stk would normally retarget this one.
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://example.com/13"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "#13 targets main and is in a stack; the platform moves it as the stack lands",
        ))
        .stdout(predicates::str::contains("updated feature/b").not());

    assert!(
        !repo.path().join("retarget.txt").exists(),
        "no `pr edit --base` may be attempted: GitHub rejects it"
    );
}
