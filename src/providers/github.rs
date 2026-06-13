use anyhow::{Context, Result, bail};

use super::json::{
    first_json_item, json_items, optional_bool, optional_string, parse_body_field, parse_state,
    required_string,
};
use super::{MergeBlocker, ReviewProvider, ReviewRequest, command_output, merge_with_retry};

pub(super) struct GitHubProvider;

impl ReviewProvider for GitHubProvider {
    fn review_for_branch(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        // gh pr list only returns open pull requests by default; check merged
        // ones too so cleanup can see landed reviews.
        if let Some(review) = list_review(branch, None)? {
            return Ok(Some(review));
        }
        list_review(branch, Some("merged"))
    }

    fn review_for_branch_including_closed(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        // Open and merged take precedence: a branch resubmitted after its
        // review was closed should resolve to the fresh review.
        if let Some(review) = self.review_for_branch(branch)? {
            return Ok(Some(review));
        }
        list_review(branch, Some("closed"))
    }

    fn create_review(&self, branch: &str, base: &str, draft: bool) -> Result<String> {
        let mut args = vec!["pr", "create", "--head", branch, "--base", base, "--fill"];
        if draft {
            args.push("--draft");
        }
        command_output("gh", &args)
    }

    fn update_review_base(&self, review: &ReviewRequest, base: &str) -> Result<String> {
        command_output("gh", &["pr", "edit", review.id_value(), "--base", base])
    }

    fn review_body(&self, review: &ReviewRequest) -> Result<String> {
        let output = command_output("gh", &["pr", "view", review.id_value(), "--json", "body"])?;
        parse_body_field(&output, "body")
    }

    fn update_review_body(&self, review: &ReviewRequest, body: &str) -> Result<String> {
        command_output("gh", &["pr", "edit", review.id_value(), "--body", body])
    }

    fn merge_review(&self, review: &ReviewRequest, strategy: &str, auto: bool) -> Result<String> {
        let flag = match strategy {
            "rebase" => "--rebase",
            "merge" => "--merge",
            _ => "--squash",
        };
        let mut args = vec!["pr", "merge", review.id_value(), flag];
        if auto {
            args.push("--auto");
        }
        merge_with_retry(|| command_output("gh", &args))
    }

    fn merge_blocker(&self, review: &ReviewRequest) -> Result<MergeBlocker> {
        let output = command_output(
            "gh",
            &[
                "pr",
                "view",
                review.id_value(),
                "--json",
                "mergeable,mergeStateStatus",
            ],
        )?;
        Ok(classify_github_merge(&output))
    }

    fn wait_for_checks(&self, review: &ReviewRequest) -> Result<bool> {
        // Poll until the checks settle. `gh pr checks` exits 0 when green, 8
        // while pending, and 1 otherwise - but "1 + no checks reported" is
        // ambiguous: a repo with no CI, or a just-pushed branch whose checks
        // have not registered yet (often queued, not running). Tolerate that
        // state for a grace window before concluding there are none, so we
        // neither merge early nor report a false failure.
        let mut no_checks = 0u32;
        let mut polls = 0u32;
        loop {
            let out = std::process::Command::new("gh")
                .args(["pr", "checks", review.id_value()])
                .output()
                .context("failed to run gh")?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            match interpret_checks(out.status.code(), &stdout, &stderr) {
                ChecksState::Passed => return Ok(true),
                ChecksState::Failed => return Ok(false),
                // gh itself failed (network, auth, an API 5xx) - not a check
                // verdict. Surface the real error instead of a false "checks
                // failed"; `merge --all` is rerun-safe, so the user retries
                // once gh recovers.
                ChecksState::Errored => bail!(
                    "could not read checks for {}: {}; rerun `git stk merge --all` once gh recovers",
                    review.id,
                    stderr.trim().lines().next().unwrap_or("gh failed").trim()
                ),
                ChecksState::NoneYet if no_checks >= super::CHECK_GRACE_POLLS => {
                    // Grace exhausted. If branch protection gates the merge the
                    // checks exist but have not registered - keep waiting.
                    if merge_is_gated(review)? {
                        no_checks = 0;
                    } else {
                        return Ok(true);
                    }
                }
                ChecksState::NoneYet => no_checks += 1,
                // A real pending state resets the grace count: checks exist.
                ChecksState::Pending => no_checks = 0,
            }

            polls += 1;
            if polls.is_multiple_of(super::CHECK_GRACE_POLLS) {
                anstream::eprintln!(
                    "{}",
                    crate::style::paint(
                        crate::style::DIM,
                        &format!("still waiting on checks for {}...", review.id)
                    )
                );
            }
            std::thread::sleep(super::check_poll_interval());
        }
    }

    fn open_reviews(&self) -> Result<Vec<ReviewRequest>> {
        let output = command_output(
            "gh",
            &[
                "pr",
                "list",
                "--state",
                "open",
                "--limit",
                "200",
                "--json",
                "number,state,baseRefName,headRefName,url,title,isDraft",
            ],
        )?;
        parse_github_reviews(&output)
    }

    fn mark_ready(&self, review: &ReviewRequest) -> Result<String> {
        command_output("gh", &["pr", "ready", review.id_value()])
    }

    fn close_review(&self, review: &ReviewRequest, delete_branch: bool) -> Result<String> {
        let mut args = vec!["pr", "close", review.id_value()];
        if delete_branch {
            args.push("--delete-branch");
        }
        command_output("gh", &args)
    }

    fn open_review(&self, review: &ReviewRequest) -> Result<String> {
        command_output("gh", &["pr", "view", review.id_value(), "--web"])
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ChecksState {
    Passed,
    Pending,
    /// No checks reported - either no CI, or not registered yet.
    NoneYet,
    /// Checks ran and at least one did not pass.
    Failed,
    /// gh itself errored (network, auth, an API failure) - not a verdict on
    /// the checks.
    Errored,
}

/// Classify a `gh pr checks` run. Exit 0 = passed, 8 = pending. For any other
/// code the streams disambiguate: "no checks reported" (on either stream)
/// means none have registered; otherwise a non-empty stdout is the checks
/// table, so a genuine failure - while an error reported only on stderr (with
/// no table on stdout) is gh itself failing, which must not be mistaken for a
/// failed check.
fn interpret_checks(code: Option<i32>, stdout: &str, stderr: &str) -> ChecksState {
    match code {
        Some(0) => ChecksState::Passed,
        Some(8) => ChecksState::Pending,
        _ => {
            let text = format!("{stdout}{stderr}").to_lowercase();
            if text.contains("no checks") {
                ChecksState::NoneYet
            } else if !stdout.trim().is_empty() {
                ChecksState::Failed
            } else {
                ChecksState::Errored
            }
        }
    }
}

/// Ask GitHub whether branch protection is gating the merge. Used to
/// disambiguate "no checks reported" right after a push (required checks
/// exist but have not registered yet) from a repo with no CI at all.
fn merge_is_gated(review: &ReviewRequest) -> Result<bool> {
    let out = command_output(
        "gh",
        &[
            "pr",
            "view",
            review.id_value(),
            "--json",
            "mergeStateStatus",
        ],
    )?;
    Ok(merge_state_is_gated(&out))
}

/// `BLOCKED` is GitHub's verdict when required checks or reviews are not yet
/// satisfied - i.e. the merge is gated. Any other state (or unparseable
/// output) is treated as not gated.
fn merge_state_is_gated(json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return false;
    };
    value
        .get("mergeStateStatus")
        .and_then(serde_json::Value::as_str)
        == Some("BLOCKED")
}

/// Map GitHub's `mergeable` + `mergeStateStatus` to a blocker. `CONFLICTING`
/// or a `DIRTY` state means conflicts; `BLOCKED` means required checks or
/// reviews are not satisfied. Anything else (or unparseable output) is
/// treated as not-blocked, leaving the caller to fall back to the error text.
fn classify_github_merge(json: &str) -> MergeBlocker {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return MergeBlocker::None;
    };
    let field = |name| value.get(name).and_then(serde_json::Value::as_str);
    if field("mergeable") == Some("CONFLICTING") || field("mergeStateStatus") == Some("DIRTY") {
        MergeBlocker::Conflicts
    } else if field("mergeStateStatus") == Some("BLOCKED") {
        MergeBlocker::ChecksPending
    } else {
        MergeBlocker::None
    }
}

fn list_review(branch: &str, state: Option<&str>) -> Result<Option<ReviewRequest>> {
    let mut args = vec!["pr", "list", "--head", branch];
    if let Some(state) = state {
        args.extend(["--state", state]);
    }
    args.extend([
        "--json",
        "number,state,baseRefName,headRefName,url,title,isDraft",
    ]);

    let output = command_output("gh", &args)?;
    parse_github_review(&output)
}

fn parse_github_review(output: &str) -> Result<Option<ReviewRequest>> {
    first_json_item(output)?
        .as_ref()
        .map(github_review_from)
        .transpose()
}

fn parse_github_reviews(output: &str) -> Result<Vec<ReviewRequest>> {
    json_items(output)?.iter().map(github_review_from).collect()
}

fn github_review_from(review: &serde_json::Value) -> Result<ReviewRequest> {
    Ok(ReviewRequest {
        id: format!("#{}", required_string(review, &["number"])?),
        branch: required_string(review, &["headRefName"])?,
        base: required_string(review, &["baseRefName"])?,
        state: parse_state(&required_string(review, &["state"])?),
        url: required_string(review, &["url"])?,
        title: optional_string(review, "title"),
        draft: optional_bool(review, "isDraft"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ReviewRequest, ReviewState};

    #[test]
    fn parse_github_review_reads_first_array_item() {
        let review = parse_github_review(
            r#"[{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"#,
        )
        .expect("parse review")
        .expect("review exists");

        assert_eq!(
            review,
            ReviewRequest {
                id: "#12".to_owned(),
                branch: "feature/a".to_owned(),
                base: "main".to_owned(),
                state: ReviewState::Open,
                url: "https://github.com/owner/repo/pull/12".to_owned(),
                title: String::new(),
                draft: false,
            }
        );
    }

    #[test]
    fn parse_review_accepts_object_output() {
        let review = parse_github_review(
            r#"{"number":12,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}"#,
        )
        .expect("parse review")
        .expect("review exists");

        assert_eq!(review.id, "#12");
    }

    #[test]
    fn parse_review_errors_on_missing_required_field() {
        let error = parse_github_review(
            r#"[{"number":12,"state":"OPEN","baseRefName":"main","url":"https://github.com/owner/repo/pull/12"}]"#,
        )
        .expect_err("missing head branch should fail");

        assert!(
            error
                .to_string()
                .contains("provider JSON missing required field: headRefName"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn parse_review_preserves_unknown_state() {
        let review = parse_github_review(
            r#"[{"number":12,"state":"READY_FOR_REVIEW","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/12"}]"#,
        )
        .expect("parse review")
        .expect("review exists");

        assert_eq!(
            review.state,
            ReviewState::Unknown("READY_FOR_REVIEW".to_owned())
        );
    }

    #[test]
    fn parse_review_empty_array_returns_none() {
        assert_eq!(parse_github_review("[]").expect("parse review"), None);
    }

    #[test]
    fn parse_github_reviews_reads_every_item() {
        let reviews = parse_github_reviews(
            r#"[{"number":1,"state":"OPEN","baseRefName":"main","headRefName":"feature/a","url":"https://github.com/owner/repo/pull/1"},
                {"number":2,"state":"OPEN","baseRefName":"feature/a","headRefName":"feature/b","url":"https://github.com/owner/repo/pull/2"}]"#,
        )
        .expect("parse reviews");

        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].id, "#1");
        assert_eq!(reviews[0].branch, "feature/a");
        assert_eq!(reviews[1].id, "#2");
        assert_eq!(reviews[1].branch, "feature/b");
    }

    #[test]
    fn interpret_checks_maps_exit_codes() {
        assert_eq!(interpret_checks(Some(0), "", ""), ChecksState::Passed);
        assert_eq!(interpret_checks(Some(8), "", ""), ChecksState::Pending);
    }

    #[test]
    fn interpret_checks_treats_no_checks_as_not_yet_on_either_stream() {
        // The message has landed on stdout in the wild, not just stderr.
        assert_eq!(
            interpret_checks(Some(1), "no checks reported on the 'feat/x' branch", ""),
            ChecksState::NoneYet
        );
        assert_eq!(
            interpret_checks(Some(1), "", "no checks reported on the 'feat/x' branch"),
            ChecksState::NoneYet
        );
    }

    #[test]
    fn interpret_checks_treats_a_reported_failure_as_failed() {
        // A genuine failure prints the checks table to stdout.
        assert_eq!(
            interpret_checks(Some(1), "X  lint  1m  failing", ""),
            ChecksState::Failed
        );
    }

    #[test]
    fn interpret_checks_treats_a_gh_error_as_errored_not_failed() {
        // gh failing operationally writes to stderr and leaves stdout empty;
        // that must not read as a failed check.
        assert_eq!(
            interpret_checks(Some(1), "", "error connecting to api.github.com: timeout"),
            ChecksState::Errored
        );
        assert_eq!(
            interpret_checks(Some(4), "", "gh: authentication required"),
            ChecksState::Errored
        );
        // A blank line on stdout is still no table.
        assert_eq!(
            interpret_checks(Some(1), "  \n", "HTTP 502"),
            ChecksState::Errored
        );
    }

    #[test]
    fn merge_state_blocked_is_gated() {
        assert!(merge_state_is_gated(r#"{"mergeStateStatus":"BLOCKED"}"#));
    }

    #[test]
    fn merge_state_clean_or_unparseable_is_not_gated() {
        assert!(!merge_state_is_gated(r#"{"mergeStateStatus":"CLEAN"}"#));
        assert!(!merge_state_is_gated(r#"{"mergeStateStatus":"UNSTABLE"}"#));
        assert!(!merge_state_is_gated("{}"));
        assert!(!merge_state_is_gated("not json"));
    }

    #[test]
    fn classify_github_merge_reads_structured_status() {
        assert_eq!(
            classify_github_merge(r#"{"mergeable":"CONFLICTING","mergeStateStatus":"DIRTY"}"#),
            MergeBlocker::Conflicts
        );
        // A clean mergeable with a DIRTY state is still a conflict.
        assert_eq!(
            classify_github_merge(r#"{"mergeable":"UNKNOWN","mergeStateStatus":"DIRTY"}"#),
            MergeBlocker::Conflicts
        );
        assert_eq!(
            classify_github_merge(r#"{"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED"}"#),
            MergeBlocker::ChecksPending
        );
        assert_eq!(
            classify_github_merge(r#"{"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN"}"#),
            MergeBlocker::None
        );
        // Conflicts take precedence over a blocked state.
        assert_eq!(
            classify_github_merge(r#"{"mergeable":"CONFLICTING","mergeStateStatus":"BLOCKED"}"#),
            MergeBlocker::Conflicts
        );
    }

    #[test]
    fn classify_github_merge_unparseable_is_not_blocked() {
        assert_eq!(classify_github_merge("{}"), MergeBlocker::None);
        assert_eq!(classify_github_merge("not json"), MergeBlocker::None);
    }
}
