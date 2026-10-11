use std::fs;

mod common;

use common::{FakeProvider, TestRepo};
use predicates::prelude::*;
use serde_json::Value;

/// The tour data embedded in a written tour page.
fn tour_data(html: &str) -> Value {
    let start_tag = r#"<script id="tour-data" type="application/json">"#;
    let start = html.find(start_tag).expect("tour data script") + start_tag.len();
    let end = start + html[start..].find("</script>").expect("end of tour data");
    serde_json::from_str(&html[start..end]).expect("tour data is JSON")
}

/// A repo whose `origin` has `main` and publishes a review's head only as
/// `head_ref` - like a pull request from a fork, with no branch to fetch.
fn repo_with_review(head_ref: &str) -> (TestRepo, tempfile::TempDir) {
    let repo = TestRepo::new();
    repo.commit_file("old name.txt", "same\ncontent\n", "add a file to rename");
    fs::create_dir_all(repo.path().join("src")).unwrap();
    repo.commit_file("src/lib.rs", "fn one() {}\n", "add lib");
    let origin = repo.add_bare_origin(&["main"]);

    repo.git(["switch", "-c", "contrib"]);
    repo.write("src/lib.rs", "fn one() {}\nfn two() {}\n");
    repo.write("Cargo.toml", "[package]\nname = \"x\"\n");
    repo.write("Cargo.lock", "# lock\n");
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    repo.write("tests/lib_test.rs", "#[test]\nfn t() {}\n");
    repo.write("src/gen.rs", "// generated\n");
    repo.write(".gitattributes", "src/gen.rs linguist-generated\n");
    repo.git(["mv", "old name.txt", "new name.txt"]);
    repo.git(["add", "-A"]);
    repo.git(["commit", "-m", "add two"]);
    repo.git(["push", "origin", &format!("contrib:{head_ref}")]);
    repo.git(["switch", "main"]);
    repo.git(["branch", "-D", "contrib"]);
    (repo, origin)
}

const GITHUB_REVIEW: &str = r##"{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"contrib","url":"https://github.com/owner/repo/pull/12","title":"Add two"}"##;

#[test]
fn tour_walks_a_github_pr_in_role_order() {
    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = FakeProvider::new()
        .on(
            "pr view 12 --json body",
            r#"{"body":"<!-- git-stk:description -->\nWhy it matters.\n<!-- /git-stk:description -->\n\n<!-- git-stk:stack -->\n- ledger\n<!-- /git-stk:stack -->"}"#,
        )
        .on("pr view 12 --json", GITHUB_REVIEW)
        .on("pr list", &format!("[{GITHUB_REVIEW}]"))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "#12", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "wrote a tour of Add two (#12) (7 files)",
        ));

    let html = fs::read_to_string(repo.path().join("tour.html")).expect("tour written");
    assert!(html.contains("<title>Tour of Add two (#12)</title>"));
    let data = tour_data(&html);
    assert_eq!(data["current"], 0);
    assert_eq!(data["chapters"].as_array().unwrap().len(), 1);
    let data = &data["chapters"][0];
    assert_eq!(data["review"]["bodyHtml"], "<p>Why it matters.</p>\n");
    assert_eq!(data["commits"][0]["subject"], "add two");

    let steps: Vec<(&str, Vec<&str>)> = data["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            let files = step["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|file| file["path"].as_str().unwrap())
                .collect();
            (step["id"].as_str().unwrap(), files)
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("config", vec![".gitattributes", "Cargo.toml"]),
            ("source", vec!["src/lib.rs"]),
            ("tests", vec!["tests/lib_test.rs"]),
            ("docs", vec!["new name.txt"]),
            ("generated", vec!["Cargo.lock", "src/gen.rs"]),
        ]
    );
    assert_eq!(data["steps"][4]["collapsed"], true);
    assert_eq!(data["steps"][0]["collapsed"], false);

    let renamed = &data["steps"][3]["files"][0];
    assert_eq!(renamed["status"], "renamed");
    assert_eq!(renamed["oldPath"], "old name.txt");
    assert_eq!(
        renamed["newText"],
        Value::Null,
        "a pure rename has no hunk to expand around"
    );

    let lib = &data["steps"][1]["files"][0];
    assert_eq!(lib["language"], "rust");
    assert_eq!(lib["oldText"], "fn one() {}\n");
    assert_eq!(lib["newText"], "fn one() {}\nfn two() {}\n");
    let added = &data["steps"][2]["files"][0];
    assert_eq!(added["status"], "added");
    assert_eq!(added["oldText"], Value::Null);
    assert_eq!(
        added["newText"],
        Value::Null,
        "an added file's hunk already holds its text"
    );
    let lockfile = &data["steps"][4]["files"][0];
    assert_eq!(
        lockfile["newText"],
        Value::Null,
        "generated files go unembedded"
    );
    assert!(html.contains("Highlight.js v11.11.1"));
    assert!(html.contains("BSD 3-Clause License"));
    assert_eq!(lib["additions"], 1);
    assert_eq!(
        lib["hunks"][0]["lines"],
        serde_json::json!([[" ", "fn one() {}"], ["+", "fn two() {}"]])
    );
}

#[test]
fn tour_saves_under_the_git_dir_by_head_commit() {
    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = FakeProvider::new()
        .on("pr view 12 --json body", r#"{"body":""}"#)
        .on("pr view 12 --json", GITHUB_REVIEW)
        .on("pr list", "[]")
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "12", "--no-open"])
        .assert()
        .success();

    let head = repo.git(["ls-remote", "origin", "refs/pull/12/head"]);
    let path = repo
        .path()
        .join(".git/stk/tours")
        .join(format!("12-{}.html", &head[..12]));
    assert!(path.exists(), "missing {}", path.display());
}

#[test]
fn tour_fetches_a_gitlab_mr_head() {
    let (repo, _origin) = repo_with_review("refs/merge-requests/34/head");
    repo.git(["config", "stk.provider", "gitlab"]);
    let fake = FakeProvider::new()
        .on(
            "mr view 34 --output json",
            r##"{"iid":34,"state":"opened","target_branch":"main","source_branch":"contrib","web_url":"https://gitlab.com/group/repo/-/merge_requests/34","title":"Add two","description":"From GitLab."}"##,
        )
        .on("mr list", "[]")
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "!34", "--no-open", "--output", "tour.html"])
        .assert()
        .success();

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(data["chapters"][0]["review"]["id"], "!34");
    assert_eq!(
        data["chapters"][0]["review"]["bodyHtml"],
        "<p>From GitLab.</p>\n"
    );
}

#[test]
fn tour_reports_a_review_with_no_changes() {
    let repo = TestRepo::new();
    let _origin = repo.add_bare_origin(&["main"]);
    repo.git(["push", "origin", "main:refs/pull/5/head"]);
    repo.git(["config", "stk.provider", "github"]);
    let fake = FakeProvider::new()
        .on(
            "pr view 5 --json",
            r##"{"number":5,"state":"MERGED","baseRefName":"main","headRefName":"done","url":"https://github.com/owner/repo/pull/5"}"##,
        )
        .on("pr list", "[]")
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "5", "--no-open"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("#5 has no changes against main"));
}

fn github_pr(number: u32, branch: &str, base: &str) -> String {
    format!(
        r#"{{"number":{number},"state":"OPEN","baseRefName":"{base}","headRefName":"{branch}","url":"https://github.com/owner/repo/pull/{number}","title":"Part {number}"}}"#
    )
}

/// Three pull requests stacked by base branch alone, as any tool or a person
/// would open them.
fn stacked_prs() -> (TestRepo, tempfile::TempDir) {
    let repo = TestRepo::new();
    let origin = repo.add_bare_origin(&["main"]);
    for (number, branch) in [(12, "part-1"), (13, "part-2"), (14, "part-3")] {
        repo.git(["switch", "-c", branch]);
        repo.commit_file(&format!("{branch}.txt"), "text\n", branch);
        repo.git(["push", "origin", &format!("{branch}:refs/heads/{branch}")]);
        repo.git([
            "push",
            "origin",
            &format!("{branch}:refs/pull/{number}/head"),
        ]);
    }
    repo.git(["switch", "main"]);
    repo.git(["config", "stk.provider", "github"]);
    (repo, origin)
}

const STACKED: [(u32, &str, &str); 3] = [
    (12, "part-1", "main"),
    (13, "part-2", "part-1"),
    (14, "part-3", "part-2"),
];

/// A provider answering for the stacked pull requests, listing `listed` as
/// the open ones, or failing to list when it is None.
fn stack_fake(listed: Option<&[String]>) -> FakeProvider {
    let fake = FakeProvider::new();
    let mut fake = match listed {
        Some(listed) => fake.on("pr list", &format!("[{}]", listed.join(","))),
        None => fake.fail("pr list", "HTTP 502"),
    };
    for (number, branch, base) in STACKED {
        fake = fake
            .on(&format!("pr view {number} --json body"), r#"{"body":""}"#)
            .on(
                &format!("pr view {number} --json"),
                &github_pr(number, branch, base),
            );
    }
    fake
}

fn stack_listing() -> Vec<String> {
    let mut listed: Vec<String> = STACKED
        .iter()
        .rev()
        .map(|(number, branch, base)| github_pr(*number, branch, base))
        .collect();
    listed.push(github_pr(20, "elsewhere", "main"));
    listed
}

fn chapter_ids(data: &Value) -> Vec<&str> {
    data["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chapter| chapter["review"]["id"].as_str().unwrap())
        .collect()
}

#[test]
fn tour_covers_the_stack_found_from_review_bases() {
    let (repo, _origin) = stacked_prs();
    let fake = stack_fake(Some(&stack_listing()))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "13", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "wrote a tour of Part 13 (#13) (1 file, in a stack of 3 reviews)",
        ));

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(data["current"], 1);
    let chapters: Vec<(&str, &str)> = data["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chapter| {
            (
                chapter["review"]["id"].as_str().unwrap(),
                chapter["steps"][0]["files"][0]["path"].as_str().unwrap(),
            )
        })
        .collect();
    // Each review shows only its own change, against the one below it.
    assert_eq!(
        chapters,
        [
            ("#12", "part-1.txt"),
            ("#13", "part-2.txt"),
            ("#14", "part-3.txt")
        ]
    );
}

#[test]
fn tour_no_stack_covers_only_the_review() {
    let (repo, _origin) = stacked_prs();
    let fake = FakeProvider::new()
        .on("pr view 13 --json body", r#"{"body":""}"#)
        .on("pr view 13 --json", &github_pr(13, "part-2", "part-1"))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args([
            "tour",
            "13",
            "--no-stack",
            "--no-open",
            "--output",
            "tour.html",
        ])
        .assert()
        .success();

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(data["chapters"].as_array().unwrap().len(), 1);
    assert_eq!(data["chapters"][0]["review"]["id"], "#13");
}

#[test]
fn tour_leaves_out_a_same_named_review_from_a_fork() {
    let (repo, _origin) = stacked_prs();
    // A fork's pull request whose branch shares #12's name, listed first,
    // and one from the fork's own main.
    repo.commit_file("fork.txt", "fork\n", "fork work");
    repo.git(["push", "origin", "HEAD:refs/pull/30/head"]);
    repo.git(["reset", "--hard", "HEAD~"]);
    let mut listed = vec![
        github_pr(30, "part-1", "main"),
        github_pr(31, "main", "main"),
    ];
    listed.extend(stack_listing());
    let fake = stack_fake(Some(&listed))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "13", "--no-open", "--output", "tour.html"])
        .assert()
        .success();

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    // Only #12's head is the commit part-1 points at.
    assert_eq!(chapter_ids(&data), ["#12", "#13", "#14"]);
    assert_eq!(data["current"], 1);
}

#[test]
fn tour_tours_the_review_alone_when_the_stack_cannot_be_listed() {
    let (repo, _origin) = stacked_prs();
    let fake = stack_fake(None)
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "13", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "warning: could not list open reviews to find the stack, so #13 is toured alone",
        ));

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(chapter_ids(&data), ["#13"]);
}

#[test]
fn tour_cuts_the_stack_at_a_review_it_cannot_read() {
    let (repo, _origin) = stacked_prs();
    let fake = FakeProvider::new()
        .fail("pr view 14 --json body", "HTTP 404")
        .on("pr list", &format!("[{}]", stack_listing().join(",")));
    let mut fake = fake;
    for (number, branch, base) in &STACKED[..2] {
        fake = fake
            .on(&format!("pr view {number} --json body"), r#"{"body":""}"#)
            .on(
                &format!("pr view {number} --json"),
                &github_pr(*number, branch, base),
            );
    }
    let fake = fake
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "12", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "warning: could not read #14, so it is left out with the reviews stacked on it",
        ));

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(chapter_ids(&data), ["#12", "#13"]);
}

#[test]
fn tour_covers_every_branch_of_the_stack() {
    let (repo, _origin) = stacked_prs();
    // A second review built on part-1, beside #13.
    repo.git(["switch", "-c", "part-2b", "part-1"]);
    repo.commit_file("part-2b.txt", "text\n", "part-2b");
    repo.git(["push", "origin", "part-2b:refs/heads/part-2b"]);
    repo.git(["push", "origin", "part-2b:refs/pull/15/head"]);
    repo.git(["switch", "main"]);
    let mut listed = stack_listing();
    listed.push(github_pr(15, "part-2b", "part-1"));
    let fake = stack_fake(Some(&listed))
        .on("pr view 15 --json body", r#"{"body":""}"#)
        .on("pr view 15 --json", &github_pr(15, "part-2b", "part-1"))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "15", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stdout(predicates::str::contains("in a stack of 4 reviews"));

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(chapter_ids(&data), ["#12", "#13", "#14", "#15"]);
    let parents: Vec<&Value> = data["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chapter| &chapter["parent"])
        .collect();
    assert_eq!(
        parents,
        [
            &Value::Null,
            &Value::from(0),
            &Value::from(1),
            &Value::from(0)
        ]
    );
    assert_eq!(data["current"], 3);
    assert_eq!(
        data["chapters"][3]["steps"][0]["files"][0]["path"],
        "part-2b.txt"
    );
}

#[test]
fn tour_leaves_out_a_review_whose_head_cannot_be_fetched() {
    let (repo, origin) = stacked_prs();
    // #14's head ref is gone from the remote, so fetching the whole stack at
    // once fails.
    let status = std::process::Command::new("git")
        .args(["update-ref", "-d", "refs/pull/14/head"])
        .current_dir(origin.path())
        .status()
        .unwrap();
    assert!(status.success());
    let fake = stack_fake(Some(&stack_listing()))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "12", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "warning: could not fetch #14, so it is left out with the reviews stacked on it",
        ));

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(chapter_ids(&data), ["#12", "#13"]);
}

#[test]
fn tour_does_not_stack_reviews_on_a_release_review() {
    let (repo, _origin) = stacked_prs();
    // A develop -> main release review, and a feature review into develop.
    repo.git(["push", "origin", "part-1:refs/heads/develop"]);
    repo.git(["push", "origin", "part-1:refs/pull/40/head"]);
    repo.git(["switch", "-c", "feature-x", "part-1"]);
    repo.commit_file("feature.txt", "text\n", "feature");
    repo.git(["push", "origin", "feature-x:refs/heads/feature-x"]);
    repo.git(["push", "origin", "feature-x:refs/pull/41/head"]);
    repo.git(["switch", "main"]);
    let listed = [
        github_pr(40, "develop", "main"),
        github_pr(41, "feature-x", "develop"),
    ];
    let fake = FakeProvider::new()
        .on("pr list", &format!("[{}]", listed.join(",")))
        .on("pr view 41 --json body", r#"{"body":""}"#)
        .on("pr view 41 --json", &github_pr(41, "feature-x", "develop"))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "41", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stderr(predicates::str::contains("warning").not());

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(chapter_ids(&data), ["#41"]);
}

#[test]
fn tour_reports_its_own_review_that_cannot_be_fetched() {
    let (repo, origin) = stacked_prs();
    // The asked-for review's head is gone; its neighbors' are fine.
    let status = std::process::Command::new("git")
        .args(["update-ref", "-d", "refs/pull/13/head"])
        .current_dir(origin.path())
        .status()
        .unwrap();
    assert!(status.success());
    let fake = stack_fake(Some(&stack_listing()))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "13", "--no-open", "--output", "tour.html"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("refs/pull/13/head"))
        .stderr(predicates::str::contains("refs/pull/12/head").not());
}
