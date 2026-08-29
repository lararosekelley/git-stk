mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::PredicateBooleanExt;

/// Run a git plumbing command in `dir`, feeding `stdin`, returning trimmed
/// stdout. For crafting refs the TestRepo helper can't (it has no stdin).
fn git_in(dir: &std::path::Path, args: &[&str], stdin: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t.t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t.t")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("git plumbing");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn repair_reconstructs_wiped_stack_from_ancestry() {
    let repo = TestRepo::new();

    // Build a 3-branch stack with real commits, then wipe all metadata -
    // the config-blown-away incident.
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.stack().args(["new", "feature/c"]).assert().success();
    repo.commit_file("c.txt", "c\n", "c work");

    for branch in ["feature/a", "feature/b", "feature/c"] {
        repo.git(["config", "--unset", &format!("branch.{branch}.stkParent")]);
        repo.git(["config", "--unset", &format!("branch.{branch}.stkBase")]);
    }

    repo.stack()
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a: set parent main (from ancestry)",
        ))
        .stdout(predicates::str::contains(
            "feature/b: set parent feature/a (from ancestry)",
        ))
        .stdout(predicates::str::contains(
            "feature/c: set parent feature/b (from ancestry)",
        ))
        .stdout(predicates::str::contains(
            "repair complete: 3 repaired, 0 verified, 0 unresolved",
        ));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkBase"]),
        repo.git(["rev-parse", "feature/a"])
    );
    // Trunk must never be assigned a parent.
    assert_eq!(
        repo.git_status(["config", "--get", "branch.main.stkParent"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn repair_prefers_provider_review_base_over_ancestry() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");
    repo.git(["config", "--unset", "branch.feature/b.stkParent"]);
    repo.git(["config", "--unset", "branch.feature/b.stkBase"]);

    let fake = FakeProvider::new()
        .on(
            "feature/b",
            r##"[{"number":7,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/7"}]"##,
        )
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/b: set parent feature/a (from github review #7)",
        ));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
}

#[test]
fn repair_re_records_stale_fork_point() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git([
        "config",
        "branch.feature/a.stkBase",
        "0000000000000000000000000000000000000000",
    ]);

    repo.stack()
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a: re-recorded fork point from main",
        ))
        .stdout(predicates::str::contains("1 repaired, 0 verified"));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkBase"]),
        repo.git(["rev-parse", "main"])
    );
}

#[test]
fn repair_re_records_a_stale_but_still_ancestor_fork_point() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    let old_base = repo.git(["config", "--get", "branch.feature/a.stkBase"]);

    // Trunk advances, then feature/a moves onto it *outside git-stk*: the
    // recorded base is still an ancestor of feature/a, but the true fork point
    // has advanced past it. The old "any ancestor" check called this verified;
    // it must now be recognized as stale and re-recorded.
    repo.git(["switch", "main"]);
    repo.commit_file("m.txt", "m\n", "trunk moves");
    repo.git(["switch", "feature/a"]);
    repo.git(["rebase", "--onto", "main", &old_base, "feature/a"]);

    repo.stack()
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a: re-recorded fork point from main",
        ))
        .stdout(predicates::str::contains("1 repaired, 0 verified"));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkBase"]),
        repo.git(["rev-parse", "main"])
    );
}

#[test]
fn repair_verifies_a_current_fork_point() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "b work");

    // A freshly built stack: both fork points are current, so repair verifies
    // them and re-records nothing.
    repo.stack()
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "repair complete: 0 repaired, 2 verified, 0 unresolved",
        ));
}

#[test]
fn repair_dry_run_changes_nothing() {
    let repo = TestRepo::new();

    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["config", "--unset", "branch.feature/a.stkParent"]);
    repo.git(["config", "--unset", "branch.feature/a.stkBase"]);

    repo.stack()
        .args(["repair", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/a: would set parent main (from ancestry)",
        ));

    assert_eq!(
        repo.git_status(["config", "--get", "branch.feature/a.stkParent"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn repair_reports_unrepairable_branches() {
    let repo = TestRepo::new();

    // A branch with no commits of its own and equal tip to main: direction
    // is ambiguous, so repair must not guess.
    repo.git(["switch", "-c", "feature/empty"]);
    repo.git(["switch", "main"]);

    repo.stack()
        .arg("repair")
        .assert()
        .success()
        .stdout(predicates::str::contains("feature/empty: no parent found"))
        .stdout(predicates::str::contains(
            "0 repaired, 0 verified, 1 unresolved",
        ));
}

#[test]
fn repair_from_remote_rebuilds_a_stack_from_the_pushed_metadata() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    repo.stack().args(["new", "feature/b"]).assert().success();
    repo.commit_file("b.txt", "b\n", "add b");

    // Pushing the branches publishes the shared metadata ref alongside them.
    let _origin = repo.add_bare_origin(&["main"]);
    repo.stack().args(["restack", "--push"]).assert().success();

    // A fresh clone would have no local stack metadata; simulate that.
    repo.git(["config", "--unset", "branch.feature/a.stkParent"]);
    repo.git(["config", "--unset", "branch.feature/b.stkParent"]);

    // Rebuild it purely from the metadata ref on the remote.
    repo.stack()
        .args(["repair", "--from-remote"])
        .assert()
        .success()
        .stdout(predicates::str::contains("attached feature/a to main"))
        .stdout(predicates::str::contains("attached feature/b to feature/a"))
        .stdout(predicates::str::contains("rebuilt 2 branches"));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkParent"]),
        "main"
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
}

#[test]
fn repair_from_remote_errors_clearly_when_no_metadata_was_published() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");

    // A remote exists, but nothing ever pushed the metadata ref to it.
    let _origin = repo.add_bare_origin(&["main", "feature/a"]);

    repo.stack()
        .args(["repair", "--from-remote"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no stack metadata on the remote"));
}

#[test]
fn repair_from_remote_conflicts_with_dry_run() {
    let repo = TestRepo::new();

    // The two modes are mutually exclusive; clap must reject the combination.
    repo.stack()
        .args(["repair", "--from-remote", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}

#[test]
fn repair_from_remote_skips_unsafe_metadata_names() {
    let repo = TestRepo::new();
    repo.stack().args(["new", "feature/a"]).assert().success();
    repo.commit_file("a.txt", "a\n", "add a");
    let origin = repo.add_bare_origin(&["main", "feature/a"]);
    let bare = origin.path();

    // Craft a metadata ref on the origin: a malicious name git would parse as
    // an option (`--upload-pack=...`) alongside a legitimate one.
    let json =
        r#"{"trunk":"main","parents":{"--upload-pack=touch PWNED":"main","feature/a":"main"}}"#;
    let blob = git_in(bare, &["hash-object", "-w", "--stdin"], json);
    let tree = git_in(
        bare,
        &["mktree"],
        &format!("100644 blob {blob}\tstack.json\n"),
    );
    let commit = git_in(bare, &["commit-tree", &tree, "-m", "metadata"], "");
    git_in(bare, &["update-ref", "refs/stk/metadata", &commit], "");

    // Fresh-clone state: drop the local metadata so repair rebuilds it.
    repo.git(["config", "--unset", "branch.feature/a.stkParent"]);

    repo.stack()
        .args(["repair", "--from-remote"])
        .assert()
        .success()
        .stdout(predicates::str::contains("attached feature/a to main"))
        .stderr(predicates::str::contains(
            "skipping unsafe stack metadata entry",
        ));

    // The legit branch was attached; the malicious entry did nothing - it was
    // never fetched and no command ran.
    assert_eq!(
        repo.git(["config", "--get", "branch.feature/a.stkParent"]),
        "main"
    );
    assert!(
        !repo.path().join("PWNED").exists(),
        "the malicious metadata name must not have executed anything"
    );
}

/// The branch a stack sits on has no parent, so it is absent from the parent
/// map - and without it the other machine cannot tell a stack's base from a
/// branch whose metadata is missing. It rides the metadata ref too.
#[test]
fn repair_from_remote_restores_the_stack_base() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");

    let _origin = repo.add_bare_origin(&["main", "rc-20260817"]);
    repo.stack().args(["restack", "--push"]).assert().success();

    // A fresh clone: no local stack metadata, and no local base branch either -
    // it is only a parent, so nothing else pulls it down.
    repo.git(["config", "--unset", "branch.fix/shared.stkParent"]);
    repo.git(["config", "--unset", "branch.rc-20260817.stkFloor"]);
    repo.git(["branch", "-D", "rc-20260817"]);

    repo.stack()
        .args(["repair", "--from-remote"])
        .assert()
        .success()
        // Recording a base another machine chose changes what this clone will
        // rebase, so it is said out loud.
        .stdout(predicates::str::contains("rc-20260817 is now a stack base"));

    assert_eq!(
        repo.git(["config", "--get", "branch.fix/shared.stkParent"]),
        "rc-20260817"
    );
    assert!(
        !repo
            .git_status(["branch", "--list", "rc-20260817"])
            .stdout
            .is_empty(),
        "the base should have been fetched: the stack is unusable without it"
    );
    assert_eq!(
        repo.git(["config", "--get", "branch.rc-20260817.stkFloor"]),
        "true",
        "the base should have been restored from the metadata ref"
    );
}

/// The metadata ref has to be able to revoke a base, not only record one: a
/// branch the other machine lists with a parent is a layer there, so a floor
/// held here is stale.
#[test]
fn repair_from_remote_clears_a_base_the_other_machine_adopted() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");

    let _origin = repo.add_bare_origin(&["main", "rc-20260817"]);
    // The other machine decided the base is a layer after all, and pushed that.
    repo.stack()
        .args(["adopt", "rc-20260817", "--parent", "main"])
        .assert()
        .success();
    repo.stack().args(["restack", "--push"]).assert().success();

    // This clone still has it marked.
    repo.git(["config", "branch.rc-20260817.stkFloor", "true"]);

    repo.stack()
        .args(["repair", "--from-remote"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "rc-20260817 is no longer a stack base",
        ));

    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkFloor"])
            .stdout
            .len(),
        0
    );
}

/// A base that picked up a stray `stkParent` must still publish as a base.
/// Published as a layer it arrives with an empty `floors` list, and the
/// revocation on the receiving clone then clears the marker there - the one
/// protected machine exporting the damage.
#[test]
fn publish_metadata_keeps_a_base_a_base_even_with_a_stray_parent() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");
    // What `repair` would write from the base's own release PR.
    repo.git(["config", "branch.rc-20260817.stkParent", "main"]);

    let _origin = repo.add_bare_origin(&["main", "rc-20260817"]);
    repo.stack().args(["restack", "--push"]).assert().success();

    // Simulate the other clone: drop the marker, then rebuild from the ref.
    repo.git(["config", "--unset", "branch.rc-20260817.stkFloor"]);
    repo.stack()
        .args(["repair", "--from-remote"])
        .assert()
        .success();

    assert_eq!(
        repo.git(["config", "--get", "branch.rc-20260817.stkFloor"]),
        "true",
        "a base with a stray parent must publish as a base, not a layer"
    );
}

/// A recorded base is not a branch missing a parent. Inferring one from its
/// own review writes metadata that contradicts the marker and that every
/// rewrite path then ignores.
#[test]
fn repair_leaves_a_recorded_stack_base_alone() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");

    repo.stack()
        .args(["repair"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "rc-20260817: stack base, left alone",
        ));

    assert_eq!(
        repo.git_status(["config", "--get", "branch.rc-20260817.stkParent"])
            .stdout
            .len(),
        0,
        "repair must not invent a parent for a recorded base"
    );
}

/// Metadata written before bases were recorded has no `floors` key at all -
/// which is "this writer did not know about them", not "there are none". An
/// un-upgraded clone must not be able to revoke a marker here.
#[test]
fn repair_from_remote_keeps_a_base_when_the_remote_predates_floors() {
    let repo = TestRepo::new();
    repo.git(["switch", "-c", "rc-20260817"]);
    repo.commit_file("rc.txt", "rc\n", "release commit");
    repo.stack().args(["new", "fix/shared"]).assert().success();
    repo.commit_file("shared.txt", "shared\n", "shared work");

    let _origin = repo.add_bare_origin(&["main", "rc-20260817"]);
    repo.stack().args(["restack", "--push"]).assert().success();

    // An older git-stk publishing the same stack: it adopted the base, so the
    // base appears as an ordinary layer and the document has no floors key.
    let older = r#"{"trunk":"main","parents":{"fix/shared":"rc-20260817","rc-20260817":"main"}}"#;
    repo.write_metadata_ref(older);

    repo.stack()
        .args(["repair", "--from-remote"])
        .assert()
        .success();

    assert_eq!(
        repo.git(["config", "--get", "branch.rc-20260817.stkFloor"]),
        "true",
        "a floors-less document must not revoke a recorded base"
    );
}

/// GitHub's own stack outranks the review base: it is an ordering someone
/// stated rather than one inferred, and it is right even where a review has
/// since been retargeted.
#[test]
fn repair_prefers_githubs_stack_to_the_review_base() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "-c", "feature/b"]);
    repo.commit_file("b.txt", "b\n", "b work");

    // Both layers still open, so the stack's ordering is current: nothing has
    // landed for the platform to have retargeted anything away from.
    let stacks = r##"[{"number":4,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"state":"open","head":{"ref":"feature/a"}},
        {"number":13,"state":"open","head":{"ref":"feature/b"}}]}]"##;
    let fake = FakeProvider::new()
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        // The review targets the trunk - opened against it, then added to the
        // stack - so the stated ordering is the better answer.
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://example.com/13"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["repair"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/b: set parent feature/a (from stack 4 (#13))",
        ));

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "feature/a"
    );
}

/// Off by default: the feature is in public preview, so an unset
/// `stk.githubStacks` must leave `repair` inferring exactly as before.
#[test]
fn repair_ignores_githubs_stack_unless_it_is_enabled() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "-c", "feature/b"]);
    repo.commit_file("b.txt", "b\n", "b work");

    let stacks = r##"[{"number":4,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"head":{"ref":"feature/a"}},
        {"number":13,"head":{"ref":"feature/b"}}]}]"##;
    let fake = FakeProvider::new()
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://example.com/13"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["repair"])
        .assert()
        .success()
        .stdout(predicates::str::contains("from github review #13"))
        .stdout(predicates::str::contains("from stack").not());

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main"
    );
}

/// But not once the layer below has landed. The platform keeps a landed layer
/// listed, so the ordering goes on naming a merged branch as the parent - while
/// the platform itself already retargeted the review away from it, and is the
/// only thing that can, since it refuses a base change by hand. Taking the
/// stale answer would write a `stkParent` pointing at a merged branch for
/// `restack` to rebase and force-push against.
#[test]
fn repair_prefers_the_review_base_once_the_layer_below_has_landed() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.githubStacks", "true"]);
    repo.git(["switch", "-c", "feature/a"]);
    repo.commit_file("a.txt", "a\n", "a work");
    repo.git(["switch", "-c", "feature/b"]);
    repo.commit_file("b.txt", "b\n", "b work");

    // feature/a landed and stays listed; the platform moved #13 onto main.
    let stacks = r##"[{"number":4,"open":true,"base":{"ref":"main"},"pull_requests":[
        {"number":12,"state":"closed","head":{"ref":"feature/a"}},
        {"number":13,"state":"open","head":{"ref":"feature/b"}}]}]"##;
    let fake = FakeProvider::new()
        .on("repo view", r##"{"nameWithOwner":"owner/repo"}"##)
        .on("repos/owner/repo/stacks", stacks)
        .on("feature/b", r##"[{"number":13,"state":"OPEN","baseRefName":"main","headRefName":"feature/b","url":"https://example.com/13"}]"##)
        .fallback("[]")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["repair"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "feature/b: set parent main (from github review #13)",
        ))
        // feature/a still reads from the stack: its parent is the stack's own
        // base, which no layer landing can stale.
        .stdout(predicates::str::contains("feature/b: set parent feature/a").not());

    assert_eq!(
        repo.git(["config", "--get", "branch.feature/b.stkParent"]),
        "main",
        "the parent must be where the platform actually put the base"
    );
}
