#[cfg(unix)]
use std::{fs, process::Command};
mod common;

use common::TestRepo;

#[test]
fn completions_bash_includes_git_subcommand_shim() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "-F _clap_complete_git_stk git-stk",
        ))
        .stdout(predicates::str::contains("_git_stk() {"));
}

#[test]
fn completions_zsh_emits_compdef_and_git_shim() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicates::str::contains("#compdef git-stk"))
        .stdout(predicates::str::contains("function _git-stk() {"));
}

#[test]
fn completions_rejects_unknown_shell() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["completions", "tcsh"])
        .assert()
        .failure();
}

#[test]
#[cfg(unix)] // drives a bash completion harness
fn completions_bash_shim_completes_git_stk_subcommands() {
    let repo = TestRepo::new();

    let output = repo.stack_output(["completions", "bash"]).stdout;
    let script_path = repo.path().join("completions.bash");
    fs::write(&script_path, output).expect("write completions script");

    // Simulate `git stk re<TAB>` the way git's completion would: set up the
    // completion environment and invoke the _git_stk shim directly.
    let harness = format!(
        r#"source "{}"
COMP_WORDS=(git stk re)
COMP_CWORD=2
_git_stk
printf '%s\n' "${{COMPREPLY[@]}}"
"#,
        script_path.display()
    );
    let result = Command::new("bash")
        .args(["-c", &harness])
        .output()
        .expect("run bash completion harness");

    assert!(result.status.success());
    let completions = String::from_utf8_lossy(&result.stdout);
    assert!(
        completions.contains("restack") && completions.contains("review"),
        "expected restack/review completions for `git stk re`, got: {completions}"
    );
}

#[test]
#[cfg(unix)] // drives a bash completion harness
fn completions_complete_flags_for_subcommands() {
    let repo = TestRepo::new();

    let completions = repo.complete_git_stk(&["submit", "--"]);
    assert!(
        completions.contains("--dry-run") && completions.contains("--stack"),
        "expected submit flags, got: {completions}"
    );
}

#[test]
#[cfg(unix)] // drives a bash completion harness
fn completions_complete_only_children_for_up() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["new", "feature/alpha"])
        .assert()
        .success();
    repo.stack()
        .args(["new", "feature/beta"])
        .assert()
        .success();
    repo.git(["switch", "feature/alpha"]);

    let completions = repo.complete_git_stk(&["up", ""]);
    assert!(
        completions.contains("feature/beta"),
        "expected child branch, got: {completions}"
    );
    assert!(
        !completions.contains("main"),
        "up must not offer non-children, got: {completions}"
    );
}

#[test]
#[cfg(unix)] // drives a bash completion harness
fn completions_complete_branch_names_with_prefix() {
    let repo = TestRepo::new();

    repo.stack()
        .args(["new", "feature/alpha"])
        .assert()
        .success();
    repo.git(["switch", "main"]);

    let completions = repo.complete_git_stk(&["status", "feat"]);
    assert!(
        completions.contains("feature/alpha"),
        "expected branch completion, got: {completions}"
    );
    assert!(
        !completions.contains("main"),
        "prefix must filter branches, got: {completions}"
    );
}
