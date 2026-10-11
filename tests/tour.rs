use std::fs;

mod common;

use common::{FakeProvider, TestRepo};
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
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "12", "--no-open"])
        .assert()
        .success();

    let head = repo.git(["rev-parse", "FETCH_HEAD"]);
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
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "!34", "--no-open", "--output", "tour.html"])
        .assert()
        .success();

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(data["review"]["id"], "!34");
    assert_eq!(data["review"]["bodyHtml"], "<p>From GitLab.</p>\n");
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
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "5", "--no-open"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("#5 has no changes against main"));
}
