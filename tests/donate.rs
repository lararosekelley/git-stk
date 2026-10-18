mod common;

use common::{FakeProvider, TestRepo};

#[cfg(not(windows))]
#[test]
fn donate_opens_the_ko_fi_page() {
    let repo = TestRepo::new();
    let fake = FakeProvider::new()
        .commands(&["xdg-open", "open"])
        .record("", "opened.txt", "")
        .install(&repo);

    repo.stack_faked(&fake)
        .args(["donate"])
        .assert()
        .success()
        .stdout(predicates::str::contains("https://ko-fi.com/sayonara_lara"));

    let opened = std::fs::read_to_string(repo.path().join("opened.txt")).unwrap();
    assert!(opened.contains("https://ko-fi.com/sayonara_lara"));
}
