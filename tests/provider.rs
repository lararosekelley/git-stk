mod common;

use common::TestRepo;
use predicates::prelude::PredicateBooleanExt;

#[test]
fn provider_detects_github_https_remote() {
    let repo = TestRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "https://github.com/lararosekelley/git-stk.git",
    ]);

    repo.stack()
        .arg("provider")
        .assert()
        .success()
        .stdout("github (remote origin (https://github.com/lararosekelley/git-stk.git))\n");
}

#[test]
fn provider_detects_github_ssh_remote() {
    let repo = TestRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "git@github.com:lararosekelley/git-stk.git",
    ]);

    repo.stack()
        .arg("provider")
        .assert()
        .success()
        .stdout("github (remote origin (git@github.com:lararosekelley/git-stk.git))\n");
}

#[test]
fn provider_detects_gitlab_https_remote() {
    let repo = TestRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "https://gitlab.com/lararosekelley/git-stk-mirror.git",
    ]);

    repo.stack()
        .arg("provider")
        .assert()
        .success()
        .stdout("gitlab (remote origin (https://gitlab.com/lararosekelley/git-stk-mirror.git))\n");
}

#[test]
fn provider_detects_gitlab_ssh_remote() {
    let repo = TestRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "git@gitlab.com:lararosekelley/git-stk-mirror.git",
    ]);

    repo.stack()
        .arg("provider")
        .assert()
        .success()
        .stdout("gitlab (remote origin (git@gitlab.com:lararosekelley/git-stk-mirror.git))\n");
}

#[test]
fn provider_does_not_match_lookalike_hosts() {
    // A host that merely embeds github.com/gitlab.com in its name (or a path)
    // must not be detected - only the real host or a subdomain of it.
    for remote in [
        "git@mygithub.com:org/repo.git",
        "https://notgitlab.com/org/repo.git",
        "https://evil.com/github.com/repo.git",
    ] {
        let repo = TestRepo::new();
        repo.git(["remote", "add", "origin", remote]);
        repo.stack()
            .arg("provider")
            .assert()
            .failure()
            .stderr(predicates::str::contains("could not detect provider"));
    }
}

#[test]
fn provider_detects_self_hosted_gitlab_via_config() {
    let repo = TestRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "git@gitlab.example.com:team/git-stk.git",
    ]);
    repo.git(["config", "stk.gitlabHost", "gitlab.example.com"]);

    repo.stack()
        .arg("provider")
        .assert()
        .success()
        .stdout("gitlab (remote origin (git@gitlab.example.com:team/git-stk.git))\n");
}

#[test]
fn provider_self_hosted_gitlab_unset_does_not_detect() {
    let repo = TestRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "git@gitlab.example.com:team/git-stk.git",
    ]);

    repo.stack()
        .arg("provider")
        .assert()
        .failure()
        .stderr(predicates::str::contains("could not detect provider"));
}

#[test]
fn provider_config_override_wins() {
    let repo = TestRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "https://github.com/lararosekelley/git-stk.git",
    ]);
    repo.git(["config", "stk.provider", "gitlab"]);

    repo.stack()
        .arg("provider")
        .assert()
        .success()
        .stdout("gitlab (config)\n");
}

#[test]
fn provider_rejects_invalid_config() {
    let repo = TestRepo::new();
    repo.git(["config", "stk.provider", "bitbucket"]);

    repo.stack()
        .arg("provider")
        .assert()
        .failure()
        .stderr(predicates::str::contains("unsupported stk.provider value"));
}

#[test]
fn provider_redacts_credentials_in_the_remote_url() {
    let repo = TestRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "https://x-access-token:ghp_SUPERSECRET@github.com/lararosekelley/git-stk.git",
    ]);

    repo.stack()
        .arg("provider")
        .assert()
        .success()
        // Detected, host shown, but the token never reaches the terminal.
        .stdout(predicates::str::contains(
            "github.com/lararosekelley/git-stk",
        ))
        .stdout(predicates::str::contains("ghp_SUPERSECRET").not());
}
