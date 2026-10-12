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

/// A served tour of `target`, with its address, stdin for answering the
/// terminal prompt, and the rest of its output.
struct Served {
    child: std::process::Child,
    url: String,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl Served {
    fn start(repo: &TestRepo, fake: &common::FakeProviderEnv, target: &str) -> Self {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("git-stk"));
        TestRepo::isolate_git_config(&mut command);
        let mut child = command
            .args(["tour", target, "--no-open"])
            .current_dir(repo.path())
            .env("PATH", &fake.path)
            .env("STK_FAKE_SPEC", &fake.spec)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut first_line = String::new();
        stdout.read_line(&mut first_line).unwrap();
        let url = first_line
            .split_whitespace()
            .find(|word| word.starts_with("http://127.0.0.1:"))
            .unwrap_or_else(|| panic!("no address in {first_line:?}"))
            .to_owned();
        let stdin = child.stdin.take().unwrap();
        Self {
            child,
            url,
            stdin,
            stdout,
        }
    }

    /// Close the page's tab and wait for the server to stop, returning what
    /// it printed after its address.
    fn close(mut self) -> String {
        use std::io::Read;
        assert_eq!(http(&self.url, "POST", "api/alive", r#""tab""#).0, 200);
        assert_eq!(http(&self.url, "POST", "api/closed", r#""tab""#).0, 200);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{status}");
                break;
            }
            if std::time::Instant::now() > deadline {
                self.child.kill().unwrap();
                panic!("the server outlived its last tab");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mut rest = String::new();
        self.stdout.read_to_string(&mut rest).unwrap();
        rest
    }

    /// Submit `review` from the page, answering the terminal's prompt with
    /// `answer`.
    fn submit(&mut self, review: &str, answer: &str) -> (u16, Value) {
        self.post_confirmed("api/submit", review, answer)
    }

    /// Post `body` to `path` from the page, answering the terminal's prompt
    /// with `answer`.
    fn post_confirmed(&mut self, path: &str, body: &str, answer: &str) -> (u16, Value) {
        use std::io::Write;
        let url = self.url.clone();
        let (path, body) = (path.to_owned(), body.to_owned());
        let request = std::thread::spawn(move || http(&url, "POST", &path, &body));
        std::thread::sleep(std::time::Duration::from_millis(500));
        self.stdin.write_all(answer.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
        let (status, body) = request.join().unwrap();
        (status, serde_json::from_str(&body).unwrap())
    }
}

fn github_tour_fake() -> FakeProvider {
    github_tour_fake_after(FakeProvider::new())
}

/// A GitHub fake for PR #12, after `first`'s rules, which win where both
/// match.
fn github_tour_fake_after(first: FakeProvider) -> FakeProvider {
    first
        .on("pr view 12 --json body", r#"{"body":""}"#)
        .on("pr view 12 --json", GITHUB_REVIEW)
        .on("pr list", "[]")
        .on("repo view", GITHUB_REPO)
        .on("api graphql", GITHUB_DISCUSSION)
}

#[test]
fn tour_serves_the_page_until_its_last_tab_closes() {
    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = github_tour_fake()
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let served = Served::start(&repo, &fake, "12");
    let url = served.url.clone();

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

    served.close();
}

const A_REVIEW: &str = r#"{"chapter":0,"event":"approve","body":"Ship it","comments":[{"path":"src/lib.rs","oldPath":"src/lib.rs","side":"new","line":2,"pairedLine":null,"body":"Name it two?"}]}"#;

#[test]
fn tour_posts_a_review_once_the_terminal_confirms_it() {
    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = github_tour_fake()
        .record(
            "api --method POST repos/owner/repo/pulls/12/reviews --input -",
            "posted",
            r#"{"html_url":"https://github.com/owner/repo/pull/12#pullrequestreview-1"}"#,
        )
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut served = Served::start(&repo, &fake, "12");

    let (status, answer) = served.submit(A_REVIEW, "y\n");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["url"],
        "https://github.com/owner/repo/pull/12#pullrequestreview-1"
    );
    let terminal = served.close();
    assert!(
        terminal.contains("review of Add two (#12): approve, with 1 comment"),
        "{terminal}"
    );
    assert!(terminal.contains("  summary: Ship it"), "{terminal}");
    assert!(
        terminal.contains("  src/lib.rs:2  Name it two?"),
        "{terminal}"
    );
    assert!(repo.path().join("posted").exists(), "the review was posted");
}

#[test]
fn tour_posts_nothing_when_the_terminal_declines() {
    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = github_tour_fake()
        .record("pulls/12/reviews", "posted", "{}")
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut served = Served::start(&repo, &fake, "12");

    let (status, answer) = served.submit(A_REVIEW, "n\n");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer["error"], "not posted: declined in the terminal");
    served.close();
    assert!(!repo.path().join("posted").exists());
}

#[test]
fn tour_refuses_a_review_of_a_head_that_has_moved() {
    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = github_tour_fake()
        .record("pulls/12/reviews", "posted", "{}")
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut served = Served::start(&repo, &fake, "12");
    // A new commit lands on the review after the tour started.
    repo.git(["switch", "-c", "later", "FETCH_HEAD"]);
    repo.commit_file("later.txt", "later\n", "later");
    repo.git(["push", "-f", "origin", "later:refs/pull/12/head"]);

    let (status, answer) = served.submit(A_REVIEW, "");
    assert_eq!(status, 422, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap()
            .contains("#12 has new commits since this tour started"),
        "{answer}"
    );
    served.close();
    assert!(!repo.path().join("posted").exists());
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

/// A GitLab fake for MR !34 at `head`, recording what a review submits.
fn gitlab_review_fake(head: &str, existing_drafts: &str) -> FakeProvider {
    gitlab_review_fake_after(FakeProvider::new(), head, existing_drafts)
}

/// [`gitlab_review_fake`] after `first`'s rules, which win where both match.
fn gitlab_review_fake_after(
    first: FakeProvider,
    head: &str,
    existing_drafts: &str,
) -> FakeProvider {
    let mr = format!(
        r##"{{"iid":34,"state":"opened","target_branch":"main","source_branch":"contrib","web_url":"https://gitlab.com/group/repo/-/merge_requests/34","title":"Add two","description":"","sha":"{head}","diff_refs":{{"base_sha":"base","start_sha":"start","head_sha":"{head}"}}}}"##
    );
    first
        .on("mr view 34 --output json", &mr)
        .on("mr list", "[]")
        .on("merge_requests/34/discussions", "[]")
        .on("merge_requests/34/approvals", r#"{"approved_by":[]}"#)
        .on(
            "api --method GET projects/:id/merge_requests/34/draft_notes",
            existing_drafts,
        )
        .record_append("draft_notes/bulk_publish", "published", "{}")
        .record_append(
            "merge_requests/34/draft_notes --input -",
            "drafted",
            r#"{"id":5}"#,
        )
        .record("merge_requests/34/approve", "approved", "{}")
}

#[test]
fn tour_posts_a_gitlab_review_as_published_drafts() {
    let (repo, _origin) = repo_with_review("refs/merge-requests/34/head");
    repo.git(["config", "stk.provider", "gitlab"]);
    let head = repo.git(["ls-remote", "origin", "refs/merge-requests/34/head"]);
    let fake = gitlab_review_fake(&head[..40], "[]")
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut served = Served::start(&repo, &fake, "!34");

    let (status, answer) = served.submit(A_REVIEW, "y\n");
    assert_eq!(status, 200, "{answer}");
    served.close();
    let drafted = fs::read_to_string(repo.path().join("drafted")).unwrap();
    assert_eq!(
        drafted.lines().count(),
        2,
        "the comment and the summary: {drafted}"
    );
    assert!(repo.path().join("published").exists());
    assert!(repo.path().join("approved").exists());
}

#[test]
fn tour_will_not_publish_someone_elses_gitlab_drafts() {
    let (repo, _origin) = repo_with_review("refs/merge-requests/34/head");
    repo.git(["config", "stk.provider", "gitlab"]);
    let head = repo.git(["ls-remote", "origin", "refs/merge-requests/34/head"]);
    let fake = gitlab_review_fake(&head[..40], r#"[{"id":9,"note":"left in GitLab"}]"#)
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut served = Served::start(&repo, &fake, "!34");

    let (status, answer) = served.submit(A_REVIEW, "y\n");
    assert_eq!(status, 422, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap()
            .contains("unpublished draft comments on !34"),
        "{answer}"
    );
    served.close();
    assert!(!repo.path().join("drafted").exists());
    assert!(!repo.path().join("published").exists());
}

/// A Gitea repository with PR #12 published under `refs/pull/12/head`, on an
/// origin whose path ends in owner/repo, which tea names the repository by.
fn gitea_review_repo() -> (TestRepo, tempfile::TempDir) {
    let repo = TestRepo::new();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    repo.commit_file("src/lib.rs", "fn one() {}\n", "add lib");
    let remote = tempfile::tempdir().unwrap();
    let bare = remote.path().join("owner").join("repo");
    fs::create_dir_all(&bare).unwrap();
    // Forward slashes, which git takes on Windows too, keep the path's
    // owner/repo end readable as a slug.
    let bare = bare.to_str().unwrap().replace('\\', "/");
    repo.git(["init", "--bare", "--initial-branch", "main", &bare]);
    repo.git(["remote", "add", "origin", &bare]);
    repo.git(["push", "-u", "origin", "main"]);
    repo.git(["switch", "-c", "contrib"]);
    repo.write("src/lib.rs", "fn one() {}\nfn two() {}\n");
    repo.git(["commit", "-am", "add two"]);
    repo.git(["push", "origin", "contrib:refs/pull/12/head"]);
    repo.git(["switch", "main"]);
    repo.git(["config", "stk.provider", "gitea"]);
    (repo, remote)
}

/// A tea fake for PR #12, after `first`'s rules, which win where both match.
fn gitea_tour_fake_after(first: FakeProvider) -> FakeProvider {
    let pull = r##"{"number":12,"state":"open","merged":false,"head":{"ref":"contrib","sha":"x"},"base":{"ref":"main"},"html_url":"https://gitea.test/owner/repo/pulls/12","title":"Add two","body":""}"##;
    first
        .commands(&["tea"])
        .record(
            "api --method POST repos/owner/repo/pulls/12/reviews --data",
            "posted",
            r#"{"html_url":"https://gitea.test/owner/repo/pulls/12#review"}"#,
        )
        .on("repos/owner/repo/pulls/12/reviews", "[]")
        .on("repos/owner/repo/issues/12/comments", "[]")
        .on("repos/owner/repo/pulls?state=open", "[]")
        .on("repos/owner/repo/pulls/12", pull)
}

#[test]
fn tour_posts_a_gitea_review() {
    let (repo, _remote) = gitea_review_repo();
    let fake = gitea_tour_fake_after(FakeProvider::new())
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut served = Served::start(&repo, &fake, "12");

    let (status, answer) = served.submit(A_REVIEW, "y\n");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["url"],
        "https://gitea.test/owner/repo/pulls/12#review"
    );
    served.close();
    let posted = fs::read_to_string(repo.path().join("posted")).unwrap();
    assert!(posted.contains(r#""event":"APPROVED""#), "{posted}");
    assert!(posted.contains(r#""new_position":2"#), "{posted}");
}

#[test]
fn tour_discards_its_gitlab_drafts_when_one_fails() {
    let (repo, _origin) = repo_with_review("refs/merge-requests/34/head");
    repo.git(["config", "stk.provider", "gitlab"]);
    let head = repo.git(["ls-remote", "origin", "refs/merge-requests/34/head"]);
    // The comment's draft is made; the summary's then fails.
    let fake = gitlab_review_fake_after(
        FakeProvider::new()
            .fail_after(
                "merge_requests/34/draft_notes --input -",
                "drafted",
                "HTTP 500",
            )
            .record("draft_notes/5", "discarded", ""),
        &head[..40],
        "[]",
    )
    .fallback_fail("unexpected provider call")
    .install(&repo);
    let mut served = Served::start(&repo, &fake, "!34");

    let (status, answer) = served.submit(A_REVIEW, "y\n");
    assert_eq!(status, 422, "{answer}");
    let terminal = served.close();
    let discarded = fs::read_to_string(repo.path().join("discarded")).unwrap_or_else(|_| {
        panic!("nothing was discarded; the page was told {answer}, the terminal said {terminal}")
    });
    assert!(
        discarded.contains("--method DELETE projects/:id/merge_requests/34/draft_notes/5"),
        "{discarded}"
    );
    assert!(!repo.path().join("published").exists());
    assert!(!repo.path().join("approved").exists());
}

#[test]
fn tour_posts_nothing_when_gitlab_refuses_the_approval() {
    let (repo, _origin) = repo_with_review("refs/merge-requests/34/head");
    repo.git(["config", "stk.provider", "gitlab"]);
    let head = repo.git(["ls-remote", "origin", "refs/merge-requests/34/head"]);
    let fake = gitlab_review_fake_after(
        FakeProvider::new()
            .fail(
                "merge_requests/34/approve",
                "HTTP 401: author cannot approve",
            )
            .record_append("draft_notes/5", "discarded", ""),
        &head[..40],
        "[]",
    )
    .fallback_fail("unexpected provider call")
    .install(&repo);
    let mut served = Served::start(&repo, &fake, "!34");

    let (status, answer) = served.submit(A_REVIEW, "y\n");
    assert_eq!(status, 422, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap()
            .contains("author cannot approve"),
        "{answer}"
    );
    served.close();
    let discarded = fs::read_to_string(repo.path().join("discarded")).unwrap();
    assert_eq!(discarded.lines().count(), 2, "both drafts: {discarded}");
    assert!(!repo.path().join("published").exists());
}

const A_THREAD: &str = r#"{"id":"T1","path":"src/lib.rs","side":"new","line":2,"outdated":false}"#;

#[test]
fn tour_replies_comments_and_resolves_on_github() {
    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = github_tour_fake_after(
        FakeProvider::new()
            .record(
                "addPullRequestReviewThreadReply",
                "replied",
                r#"{"data":{"addPullRequestReviewThreadReply":{"comment":{"id":"C9"}}}}"#,
            )
            .record(
                "resolveReviewThread",
                "resolved",
                r#"{"data":{"resolveReviewThread":{"thread":{"isResolved":true}}}}"#,
            )
            .record("pr comment 12 --body", "commented", ""),
    )
    .fallback_fail("unexpected provider call")
    .install(&repo);
    let mut served = Served::start(&repo, &fake, "12");

    let reply = format!(r#"{{"chapter":0,"thread":{A_THREAD},"body":"Agreed"}}"#);
    let (status, answer) = served.post_confirmed("api/reply", &reply, "y\n");
    assert_eq!(status, 200, "{answer}");
    let (status, answer) =
        served.post_confirmed("api/comment", r#"{"chapter":0,"body":"Thanks!"}"#, "y\n");
    assert_eq!(status, 200, "{answer}");
    // Resolving posts nothing, so nothing is asked.
    let resolve = format!(r#"{{"chapter":0,"thread":{A_THREAD},"resolved":true}}"#);
    assert_eq!(http(&served.url, "POST", "api/resolve", &resolve).0, 200);

    let terminal = served.close();
    assert!(
        terminal.contains("reply on Add two (#12) at src/lib.rs:2:\n  Agreed"),
        "{terminal}"
    );
    assert!(
        terminal.contains("comment on Add two (#12):\n  Thanks!"),
        "{terminal}"
    );
    assert!(
        terminal.contains("resolved a thread on #12 at src/lib.rs"),
        "{terminal}"
    );
    let replied = fs::read_to_string(repo.path().join("replied")).unwrap();
    assert!(
        replied.contains("id=T1") && replied.contains("body=Agreed"),
        "{replied}"
    );
    assert!(repo.path().join("resolved").exists());
    assert!(repo.path().join("commented").exists());
}

#[test]
fn tour_reports_a_reply_github_refuses() {
    let (repo, _origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    let fake = github_tour_fake_after(FakeProvider::new().on(
        "addPullRequestReviewThreadReply",
        r#"{"data":null,"errors":[{"message":"Could not resolve to a node"}]}"#,
    ))
    .fallback_fail("unexpected provider call")
    .install(&repo);
    let mut served = Served::start(&repo, &fake, "12");

    let reply = format!(r#"{{"chapter":0,"thread":{A_THREAD},"body":"Agreed"}}"#);
    let (status, answer) = served.post_confirmed("api/reply", &reply, "y\n");
    assert_eq!(status, 422);
    assert_eq!(
        answer["error"],
        "GitHub refused it: Could not resolve to a node"
    );
    served.close();
}

#[test]
fn tour_replies_and_resolves_on_gitlab() {
    let (repo, _origin) = repo_with_review("refs/merge-requests/34/head");
    repo.git(["config", "stk.provider", "gitlab"]);
    let head = repo.git(["ls-remote", "origin", "refs/merge-requests/34/head"]);
    let fake = gitlab_review_fake_after(
        FakeProvider::new()
            .record("merge_requests/34/discussions/d1/notes", "replied", "{}")
            .record(
                "merge_requests/34/discussions/d1?resolved=false",
                "reopened",
                "{}",
            ),
        &head[..40],
        "[]",
    )
    .fallback_fail("unexpected provider call")
    .install(&repo);
    let mut served = Served::start(&repo, &fake, "!34");

    let thread = r#"{"id":"d1","path":"src/lib.rs","side":"new","line":2,"outdated":false}"#;
    let (status, answer) = served.post_confirmed(
        "api/reply",
        &format!(r#"{{"chapter":0,"thread":{thread},"body":"Agreed"}}"#),
        "y\n",
    );
    assert_eq!(status, 200, "{answer}");
    let reopen = format!(r#"{{"chapter":0,"thread":{thread},"resolved":false}}"#);
    assert_eq!(http(&served.url, "POST", "api/resolve", &reopen).0, 200);
    served.close();
    assert!(repo.path().join("replied").exists());
    assert!(repo.path().join("reopened").exists());
}

#[test]
fn tour_replies_on_gitea_by_line() {
    let (repo, _remote) = gitea_review_repo();
    let fake = gitea_tour_fake_after(FakeProvider::new())
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut served = Served::start(&repo, &fake, "12");

    let thread = r#"{"id":"40","path":"src/lib.rs","side":"new","line":2,"outdated":false}"#;
    let (status, answer) = served.post_confirmed(
        "api/reply",
        &format!(r#"{{"chapter":0,"thread":{thread},"body":"Agreed"}}"#),
        "y\n",
    );
    assert_eq!(status, 200, "{answer}");
    served.close();
    // Gitea has no reply call: the reply is a one-comment review on the
    // thread's line at the toured head.
    let posted = fs::read_to_string(repo.path().join("posted")).unwrap();
    let head = repo.git(["ls-remote", "origin", "refs/pull/12/head"]);
    assert!(posted.contains(r#""event":"COMMENT""#), "{posted}");
    assert!(
        posted.contains(r#""body":"","#),
        "a reply is no review summary: {posted}"
    );
    assert!(
        posted.contains(r#"{"body":"Agreed","new_position":2,"path":"src/lib.rs"}"#),
        "{posted}"
    );
    assert!(
        posted.contains(&format!(r#""commit_id":"{}""#, &head[..40])),
        "{posted}"
    );
}

#[test]
fn tour_will_not_reply_to_an_outdated_gitea_thread() {
    let (repo, _remote) = gitea_review_repo();
    let fake = gitea_tour_fake_after(FakeProvider::new())
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut served = Served::start(&repo, &fake, "12");

    let thread = r#"{"id":"40","path":"src/lib.rs","side":"new","line":2,"outdated":true}"#;
    let (status, answer) = served.post_confirmed(
        "api/reply",
        &format!(r#"{{"chapter":0,"thread":{thread},"body":"Agreed"}}"#),
        "y\n",
    );
    assert_eq!(status, 422, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap()
            .contains("Gitea can only reply to a conversation on a line of the current diff"),
        "{answer}"
    );
    served.close();
    assert!(!repo.path().join("posted").exists());
}

#[test]
fn tour_will_not_reply_on_gitea_after_the_head_moves() {
    let (repo, _remote) = gitea_review_repo();
    let fake = gitea_tour_fake_after(FakeProvider::new())
        .fallback_fail("unexpected provider call")
        .install(&repo);
    let mut served = Served::start(&repo, &fake, "12");
    // A new commit lands on the review after the tour started.
    repo.git(["switch", "-c", "later", "FETCH_HEAD"]);
    repo.commit_file("later.txt", "later\n", "later");
    repo.git(["push", "-f", "origin", "later:refs/pull/12/head"]);

    let thread = r#"{"id":"40","path":"src/lib.rs","side":"new","line":2,"outdated":false}"#;
    let (status, answer) = served.post_confirmed(
        "api/reply",
        &format!(r#"{{"chapter":0,"thread":{thread},"body":"Agreed"}}"#),
        "",
    );
    assert_eq!(status, 422, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap()
            .contains("#12 has new commits since this tour started"),
        "{answer}"
    );
    served.close();
    assert!(!repo.path().join("posted").exists());
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

/// A plan the fake tour agent answers with, wrapped in prose as agents do.
const AGENT_PLAN: &str = r#"Here is the plan:
```json
{"version":1,
 "agent":{"provider":"Fake Labs","model":"fake-1","effort":"low"},
 "overview":"Adds `two` and tests it.",
 "steps":[
   {"title":"The new function","summary":"Adds `two` beside `one`.","files":["src/lib.rs"]},
   {"title":"Its test","summary":"Checks it runs.","files":["tests/lib_test.rs"]}],
 "notes":[{"path":"src/lib.rs","side":"new","line":2,"body":"The new function."}]}
```"#;

/// A GitHub repo with PR #12 and a fake `tour-agent` command, allowed to
/// plan tours here.
fn agent_tour_repo(
    agent_rule: impl FnOnce(FakeProvider) -> FakeProvider,
) -> (TestRepo, tempfile::TempDir, common::FakeProviderEnv) {
    let (repo, origin) = repo_with_review("refs/pull/12/head");
    repo.git(["config", "stk.provider", "github"]);
    repo.git(["config", "stk.tourAgent", "tour-agent --print"]);
    repo.git(["config", "stk.tourAgentAllowed", "tour-agent --print"]);
    let fake = github_tour_fake_after(agent_rule(
        FakeProvider::new().commands(&["gh", "tour-agent"]),
    ))
    .fallback_fail("unexpected provider call")
    .install(&repo);
    (repo, origin, fake)
}

#[test]
fn an_agent_tour_follows_the_agents_plan_and_saves_it() {
    let (repo, _origin, fake) =
        agent_tour_repo(|fake| fake.record_append("--print", "asked", AGENT_PLAN));

    repo.stack_faked(&fake)
        .args([
            "tour",
            "12",
            "--style",
            "agentTour",
            "--no-open",
            "--output",
            "tour.html",
        ])
        .assert()
        .success()
        .stderr(predicates::str::contains("left out"));

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    let chapter = &data["chapters"][0];
    let steps: Vec<(&str, &str)> = chapter["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["id"].as_str().unwrap(),
                step["title"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("plan-1", "The new function"),
            ("plan-2", "Its test"),
            ("plan-rest", "Other changes")
        ]
    );
    assert_eq!(
        chapter["steps"][0]["summary"],
        "<p>Adds <code>two</code> beside <code>one</code>.</p>\n"
    );
    let plan = &chapter["plan"];
    assert_eq!(plan["source"], "agent");
    assert_eq!(plan["command"], "tour-agent --print");
    assert_eq!(
        (&plan["provider"], &plan["model"]),
        (&Value::from("Fake Labs"), &Value::from("fake-1"))
    );
    assert!(
        plan.get("effort").is_none(),
        "an agent cannot see its own effort, so its guess is not shown"
    );
    assert_eq!(plan["cached"], false);
    assert_eq!(plan["notes"][0]["line"], 2);

    // The next tour of the same head uses the saved plan.
    repo.stack_faked(&fake)
        .args([
            "tour",
            "12",
            "--style",
            "agentTour",
            "--no-open",
            "--output",
            "again.html",
        ])
        .assert()
        .success();
    let again = tour_data(&fs::read_to_string(repo.path().join("again.html")).unwrap());
    assert_eq!(again["chapters"][0]["plan"]["cached"], true);
    let asked = fs::read_to_string(repo.path().join("asked")).unwrap();
    assert_eq!(asked.lines().count(), 1, "the agent is asked once: {asked}");

    // Unless asked to plan again.
    repo.stack_faked(&fake)
        .args([
            "tour",
            "12",
            "--style",
            "agentTour",
            "--refresh-plan",
            "--no-open",
            "--output",
            "fresh.html",
        ])
        .assert()
        .success();
    let asked = fs::read_to_string(repo.path().join("asked")).unwrap();
    assert_eq!(asked.lines().count(), 2, "{asked}");
}

/// A plan with review findings, for an `agentReview` tour.
const AGENT_REVIEW: &str = r#"{"version":1,
 "agent":{"provider":"Fake Labs","model":"fake-1","effort":"low"},
 "overview":"Adds `two`.",
 "steps":[{"title":"The new function","summary":"S","files":["src/lib.rs"]}],
 "findings":[
   {"path":"tests/lib_test.rs","side":"new","line":2,"severity":"optional","category":"tests",
    "title":"Name the test","body":"Say what it checks.","suggestion":"fn two_runs() {}"},
   {"path":"src/lib.rs","side":"new","line":2,"severity":"required","category":"bug",
    "title":"Unused","body":"`two` is never called."},
   {"path":"src/lib.rs","side":"new","line":40,"severity":"required","title":"Off the diff"}]}"#;

#[test]
fn an_agent_review_shows_its_findings_most_severe_first() {
    let (repo, _origin, fake) =
        agent_tour_repo(|fake| fake.record_append("--print", "asked", AGENT_REVIEW));
    repo.git(["config", "stk.tourStyle", "agentReview"]);

    repo.stack_faked(&fake)
        .args(["tour", "12", "--no-open", "--output", "tour.html"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "found something at src/lib.rs:40, which the diff does not show",
        ));
    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(data["forge"], "github");
    let findings = data["chapters"][0]["plan"]["findings"].as_array().unwrap();
    let shape: Vec<_> = findings
        .iter()
        .map(|finding| {
            (
                finding["path"].as_str().unwrap(),
                finding["severity"].as_str().unwrap(),
                finding["category"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("src/lib.rs", "required", "bug"),
            ("tests/lib_test.rs", "optional", "tests")
        ]
    );
    assert_eq!(
        findings[0]["bodyHtml"],
        "<p><code>two</code> is never called.</p>\n"
    );
    assert_eq!(findings[1]["suggestion"], "fn two_runs() {}");

    // A plan without findings is saved apart from one with them.
    repo.stack_faked(&fake)
        .args([
            "tour",
            "12",
            "--style",
            "agentTour",
            "--no-open",
            "--output",
            "plan.html",
        ])
        .assert()
        .success();
    let plan = tour_data(&fs::read_to_string(repo.path().join("plan.html")).unwrap());
    assert_eq!(
        plan["chapters"][0]["plan"]["findings"],
        serde_json::json!([]),
        "an agentTour shows no findings"
    );
    repo.stack_faked(&fake)
        .args(["tour", "12", "--no-open", "--output", "again.html"])
        .assert()
        .success();
    let asked = fs::read_to_string(repo.path().join("asked")).unwrap();
    assert_eq!(asked.lines().count(), 2, "{asked}");
    let again = tour_data(&fs::read_to_string(repo.path().join("again.html")).unwrap());
    assert_eq!(again["chapters"][0]["plan"]["cached"], true);
    assert_eq!(
        again["chapters"][0]["plan"]["findings"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn an_agent_tour_takes_its_style_from_config_and_the_flag_overrides_it() {
    let (repo, _origin, fake) = agent_tour_repo(|fake| fake.record("--print", "asked", AGENT_PLAN));
    repo.git(["config", "stk.tourStyle", "agentTour"]);

    repo.stack_faked(&fake)
        .args([
            "tour",
            "12",
            "--style",
            "standard",
            "--no-open",
            "--output",
            "tour.html",
        ])
        .assert()
        .success();
    assert!(
        !repo.path().join("asked").exists(),
        "a standard tour asks no agent"
    );
    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    assert_eq!(data["chapters"][0]["plan"], Value::Null);

    repo.stack_faked(&fake)
        .args(["tour", "12", "--no-open", "--output", "tour.html"])
        .assert()
        .success();
    assert!(repo.path().join("asked").exists());

    repo.git(["config", "stk.tourStyle", "fancy"]);
    repo.stack_faked(&fake)
        .args(["tour", "12", "--no-open", "--output", "tour.html"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("stk.tourStyle is \"fancy\""));
}

#[test]
fn an_agent_tour_asks_before_sending_a_repository_anywhere() {
    let (repo, _origin, fake) = agent_tour_repo(|fake| fake.record("--print", "asked", AGENT_PLAN));
    repo.git(["config", "--unset", "stk.tourAgentAllowed"]);

    // Without a terminal to ask at, nothing is sent.
    repo.stack_faked(&fake)
        .args([
            "tour",
            "12",
            "--style",
            "agentTour",
            "--no-open",
            "--output",
            "tour.html",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "set `git config stk.tourAgentAllowed \"tour-agent --print\"`",
        ));
    assert!(!repo.path().join("asked").exists());

    // Agreeing to one command does not send the reviews to another.
    repo.git(["config", "stk.tourAgentAllowed", "some-other-agent"]);
    repo.stack_faked(&fake)
        .args([
            "tour",
            "12",
            "--style",
            "agentTour",
            "--no-open",
            "--output",
            "tour.html",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Agent tours here were allowed with `some-other-agent`",
        ));
    assert!(!repo.path().join("asked").exists());
}

#[test]
fn an_agent_tour_follows_a_plan_given_as_a_file() {
    let (repo, _origin, fake) = agent_tour_repo(|fake| fake);
    repo.git(["config", "--unset", "stk.tourAgent"]);
    repo.write("plan.json", AGENT_PLAN);

    repo.stack_faked(&fake)
        .args([
            "tour",
            "12",
            "--plan",
            "plan.json",
            "--no-open",
            "--output",
            "tour.html",
        ])
        .assert()
        .success();
    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    let plan = &data["chapters"][0]["plan"];
    assert_eq!(plan["source"], "file");
    assert_eq!(plan["command"], Value::Null);
    assert_eq!(data["chapters"][0]["steps"][0]["title"], "The new function");
}

#[test]
fn a_review_its_agent_cannot_plan_is_toured_by_file_role() {
    for (answer, failing) in [("I would rather not.", false), ("", true)] {
        let (repo, _origin, fake) = agent_tour_repo(|fake| {
            if failing {
                fake.fail("--print", "model unavailable")
            } else {
                fake.on("--print", answer)
            }
        });

        repo.stack_faked(&fake)
            .args([
                "tour",
                "12",
                "--style",
                "agentTour",
                "--no-open",
                "--output",
                "tour.html",
            ])
            .assert()
            .success()
            .stderr(predicates::str::contains(
                "could not plan #12 with `tour-agent --print`, so it is toured by file role",
            ));
        let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
        assert_eq!(data["chapters"][0]["plan"], Value::Null);
        assert_eq!(data["chapters"][0]["steps"][0]["id"], "config");
    }
}

#[test]
fn an_agent_tour_plans_every_changeset_in_a_stack() {
    let (repo, _origin) = stacked_prs();
    repo.git(["config", "stk.tourAgent", "tour-agent --print"]);
    repo.git(["config", "stk.tourAgentAllowed", "tour-agent --print"]);
    // One answer for every changeset: each places its own file, and the
    // others' paths are set right.
    let plan = r#"{"version":1,"overview":"O","steps":[{"title":"The part","summary":"S","files":["part-1.txt","part-2.txt","part-3.txt"]}]}"#;
    let fake = stack_fake(Some(&stack_listing()))
        .commands(&["gh", "tour-agent"])
        .record_append("--print", "asked", plan)
        .fallback_fail("unexpected provider call")
        .install(&repo);

    repo.stack_faked(&fake)
        .args([
            "tour",
            "13",
            "--style",
            "agentTour",
            "--no-open",
            "--output",
            "tour.html",
        ])
        .assert()
        .success();

    let data = tour_data(&fs::read_to_string(repo.path().join("tour.html")).unwrap());
    let planned: Vec<(&str, &str)> = data["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chapter| {
            (
                chapter["review"]["id"].as_str().unwrap(),
                chapter["steps"][0]["title"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        planned,
        [
            ("#12", "The part"),
            ("#13", "The part"),
            ("#14", "The part")
        ]
    );
    let asked = fs::read_to_string(repo.path().join("asked")).unwrap();
    assert_eq!(
        asked.lines().count(),
        3,
        "one request per changeset: {asked}"
    );
}
