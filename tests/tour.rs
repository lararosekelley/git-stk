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
    repo.write("src/ext.rs", "// vendored\n");
    repo.write(
        ".gitattributes",
        "src/gen.rs linguist-generated\nsrc/ext.rs linguist-vendored\n",
    );
    repo.git(["mv", "old name.txt", "new name.txt"]);
    repo.git(["add", "-A"]);
    repo.git(["commit", "-m", "add two"]);
    repo.git(["push", "origin", &format!("contrib:{head_ref}")]);
    repo.git(["switch", "main"]);
    repo.git(["branch", "-D", "contrib"]);
    (repo, origin)
}

const GITHUB_REPO: &str = r#"{"nameWithOwner":"owner/repo"}"#;

const NO_GITHUB_DISCUSSION: &str = r#"{"data":{"repository":{"pullRequest":{"comments":{"totalCount":0,"nodes":[]},"reviews":{"totalCount":0,"nodes":[]},"reviewThreads":{"totalCount":0,"nodes":[]}}}}}"#;

/// An approval, a comment on the review, and a thread on `src/lib.rs`.
const GITHUB_DISCUSSION: &str = r#"{"data":{"repository":{"pullRequest":{
    "comments":{"totalCount":1,"nodes":[{"id":"IC1","author":{"login":"ann"},"body":"Thanks!","createdAt":"2026-10-11T01:00:00Z","url":"https://github.com/owner/repo/pull/12#issuecomment-1"}]},
    "reviews":{"totalCount":1,"nodes":[{"author":{"login":"bo"},"state":"APPROVED","body":"Ship it","submittedAt":"2026-10-11T02:00:00Z","url":"u"}]},
    "reviewThreads":{"totalCount":1,"nodes":[{"id":"T1","isResolved":false,"isOutdated":false,"path":"src/lib.rs","line":2,"originalLine":2,"startLine":null,"originalStartLine":null,"diffSide":"RIGHT","subjectType":"LINE",
        "comments":{"totalCount":1,"nodes":[{"id":"C1","author":{"login":"bo"},"body":"Name it `two`?","createdAt":"2026-10-11T03:00:00Z","url":"u","diffHunk":"@@ -1 +1,2 @@"}]}}]}}}}}"#;

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
        .on("repo view", GITHUB_REPO)
        .on("api graphql", GITHUB_DISCUSSION)
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "#12", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "wrote a tour of Add two (#12) (8 files)",
        ));

    let html = fs::read_to_string(repo.path().join("tour.html")).expect("tour written");
    assert!(html.contains("<title>Tour of Add two (#12)</title>"));
    let data = tour_data(&html);
    assert_eq!(data["current"], 0);
    assert_eq!(data["chapters"].as_array().unwrap().len(), 1);
    assert_eq!(data["served"], false, "a written tour is a snapshot");
    let data = &data["chapters"][0];
    let discussion = &data["conversation"];
    assert_eq!(discussion["verdicts"][0]["state"], "approved");
    assert_eq!(discussion["comments"][0]["bodyHtml"], "<p>Thanks!</p>\n");
    let thread = &discussion["threads"][0];
    assert_eq!(
        (&thread["path"], &thread["side"], &thread["line"]),
        (
            &Value::from("src/lib.rs"),
            &Value::from("new"),
            &Value::from(2)
        )
    );
    assert_eq!(
        thread["comments"][0]["bodyHtml"],
        "<p>Name it <code>two</code>?</p>\n"
    );
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
            ("generated", vec!["Cargo.lock", "src/ext.rs", "src/gen.rs"]),
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

/// A request to the tour server: the status and body it answers with.
fn http(url: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let rest = url.strip_prefix("http://").unwrap();
    let (host, base) = rest.split_once('/').unwrap();
    let mut stream = std::net::TcpStream::connect(host).unwrap();
    write!(
        stream,
        "{method} /{base}{path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split_once("\r\n\r\n").unwrap().1.to_owned();
    (status, body)
}

#[test]
fn tour_serves_the_page_until_its_last_tab_closes() {
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = FakeProvider::new()
        .on("pr view 12 --json body", r#"{"body":""}"#)
        .on("pr view 12 --json", GITHUB_REVIEW)
        .on("pr list", "[]")
        .on("repo view", GITHUB_REPO)
        .on("api graphql", GITHUB_DISCUSSION)
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("git-stk"));
    TestRepo::isolate_git_config(&mut command);
    let mut server = command
        .args(["tour", "12", "--no-open"])
        .current_dir(repo.path())
        .env("PATH", &fake.path)
        .env("STK_FAKE_SPEC", &fake.spec)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first_line = String::new();
    std::io::BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut first_line)
        .unwrap();
    let url = first_line
        .split_whitespace()
        .find(|word| word.starts_with("http://127.0.0.1:"))
        .unwrap_or_else(|| panic!("no address in {first_line:?}"))
        .to_owned();

    let (status, page) = http(&url, "GET", "", "");
    assert_eq!(status, 200);
    assert!(page.contains("<title>Tour of Add two (#12)</title>"));
    assert!(page.contains(r#""served":true"#));
    let (status, discussion) = http(&url, "GET", "api/conversation?chapter=0", "");
    assert_eq!(status, 200);
    let discussion: Value = serde_json::from_str(&discussion).unwrap();
    assert_eq!(discussion["threads"][0]["path"], "src/lib.rs");

    // Without the token there is nothing to find.
    let origin_only = url
        .trim_end_matches('/')
        .rsplit_once('/')
        .unwrap()
        .0
        .to_owned()
        + "/";
    assert_eq!(http(&origin_only, "GET", "", "").0, 404);

    // The server stops once its last tab closes.
    assert_eq!(http(&url, "POST", "api/alive", r#""tab""#).0, 200);
    assert_eq!(http(&url, "POST", "api/closed", r#""tab""#).0, 200);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = server.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        if std::time::Instant::now() > deadline {
            server.kill().unwrap();
            panic!("the server outlived its last tab");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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
        Some(listed) => fake.on(
            "pr list --state open --limit 2000",
            &format!("[{}]", listed.join(",")),
        ),
        None => fake.fail("pr list", "HTTP 502"),
    };
    fake = fake
        .on("repo view", GITHUB_REPO)
        .on("api graphql", NO_GITHUB_DISCUSSION);
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
fn tour_pages_through_every_open_gitlab_mr() {
    let (repo, _origin) = repo_with_review("refs/merge-requests/34/head");
    repo.git(["config", "stk.provider", "gitlab"]);
    let others: Vec<String> = (100..200)
        .map(|iid| {
            format!(
                r#"{{"iid":{iid},"state":"opened","target_branch":"main","source_branch":"other-{iid}","web_url":"https://gitlab.com/group/repo/-/merge_requests/{iid}"}}"#
            )
        })
        .collect();
    let fake = FakeProvider::new()
        .log_all("calls.log")
        .on(
            "mr view 34 --output json",
            r##"{"iid":34,"state":"opened","target_branch":"main","source_branch":"contrib","web_url":"https://gitlab.com/group/repo/-/merge_requests/34","title":"Add two","description":""}"##,
        )
        .on(
            "mr list --output json --per-page 100 --page 1",
            &format!("[{}]", others.join(",")),
        )
        .on("mr list --output json --per-page 100 --page 2", "[]")
        .on("merge_requests/34/discussions", "[]")
        .on("merge_requests/34/approvals", r#"{"approved_by":[]}"#)
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "!34", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stderr(predicates::str::contains("warning").not());

    let calls = fs::read_to_string(repo.path().join("calls.log")).unwrap();
    assert!(
        calls.contains("--page 2"),
        "a full first page is followed by the next:\n{calls}"
    );
    assert!(!calls.contains("--page 3"), "{calls}");
}

#[test]
fn tour_warns_when_the_open_reviews_run_past_the_listing_limit() {
    let (repo, _origin) = stacked_prs();
    let mut listed = stack_listing();
    listed.extend((1000..2996).map(|number| github_pr(number, &format!("other-{number}"), "main")));
    assert_eq!(listed.len(), 2000);
    let fake = stack_fake(Some(&listed))
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "13", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "warning: the repository has more open reviews than could be listed (2000), so the stack may be missing some",
        ));
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
        .on(
            "pr list --state open --limit 2000",
            &format!("[{}]", listed.join(",")),
        )
        .on("pr view 41 --json body", r#"{"body":""}"#)
        .on("pr view 41 --json", &github_pr(41, "feature-x", "develop"))
        .on("repo view", GITHUB_REPO)
        .on("api graphql", NO_GITHUB_DISCUSSION)
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
fn tour_still_writes_a_review_whose_discussion_it_cannot_read() {
    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = FakeProvider::new()
        .on("pr view 12 --json body", r#"{"body":""}"#)
        .on("pr view 12 --json", GITHUB_REVIEW)
        .on("pr list", "[]")
        .on("repo view", GITHUB_REPO)
        .fail("api graphql", "HTTP 502")
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["tour", "12", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "warning: could not read the discussion on #12",
        ));

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert!(
        data["chapters"][0]["conversation"]["error"]
            .as_str()
            .unwrap()
            .contains("HTTP 502")
    );
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
