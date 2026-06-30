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
fn submit_seeds_the_github_pr_template_above_the_managed_body() {
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

    // The fresh body is the template, replacing whatever create set.
    let body = fs::read_to_string(repo.path().join("edit-body-12.txt")).expect("seeded body");
    assert_eq!(
        body.trim_end(),
        "pr edit 12 --body ## Summary\n\n- [ ] Tests pass"
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
        body.contains("--description ## What\n\n## Why"),
        "body: {body}"
    );
}

#[test]
fn submit_seeds_a_seam_below_the_template_when_managed_content_follows() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    fs::create_dir_all(repo.path().join(".github")).unwrap();
    repo.write(
        ".github/pull_request_template.md",
        "## Summary\n\n- [ ] Tests pass\n",
    );
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "add PR template"]);

    // The branch name references an issue, so a `Closes #5` note will follow
    // the template - the seed should lay a horizontal-rule seam for it.
    repo.git(["switch", "-c", "5-fix"]);
    repo.git(["config", "branch.5-fix.stkParent", "main"]);

    let fake = FakeProvider::new()
        .record(
            "pr create",
            "created.txt",
            "https://github.com/owner/repo/pull/12",
        )
        .on("pr view 12", r#"{"body":""}"#)
        // Capture every body edit so the seed's own edit (with the seam) is
        // visible even after the later Closes edit.
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

    // The seed wrote the template followed by a `---` seam, so the managed
    // content that follows reads as a distinct block beneath it.
    let edits = fs::read_to_string(repo.path().join("edits-12.log")).expect("edits log");
    assert!(
        edits.contains("pr edit 12 --body ## Summary\n\n- [ ] Tests pass\n\n---"),
        "seed should append a seam rule:\n{edits}"
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

#[test]
fn submit_stack_validates_parents_before_provider_calls() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.git(["switch", "-c", "feature/b"]);
    repo.git(["config", "branch.feature/b.stkParent", "feature/a"]);
    repo.git(["switch", "feature/a"]);
    let log_path = repo.path().join("submit.log");
    let fake = FakeProvider::new()
        .log_all("submit.log")
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["submit", "--stack"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("feature/a has no stack parent"));

    assert!(
        !log_path.exists(),
        "provider should not be called after validation failure"
    );
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
