use anyhow::{Context, Result};

use super::json::{
    first_json_item, json_items, optional_bool, optional_string, parse_body_field, parse_state,
    required_string,
};
use super::{MergeBlocker, ReviewProvider, ReviewRequest, command_output, merge_with_retry};

pub(super) struct GitLabProvider;

impl ReviewProvider for GitLabProvider {
    fn review_for_branch(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        // glab mr list only returns open merge requests by default; check
        // merged ones too so cleanup can see landed reviews.
        if let Some(review) = list_review(branch, None)? {
            return Ok(Some(review));
        }
        list_review(branch, Some("--merged"))
    }

    fn review_for_branch_including_closed(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        // Open and merged take precedence: a branch resubmitted after its
        // review was closed should resolve to the fresh review.
        if let Some(review) = self.review_for_branch(branch)? {
            return Ok(Some(review));
        }
        list_review(branch, Some("--closed"))
    }

    fn create_review(&self, branch: &str, base: &str, draft: bool) -> Result<String> {
        let mut args = vec![
            "mr",
            "create",
            "--source-branch",
            branch,
            "--target-branch",
            base,
            "--fill",
        ];
        if draft {
            args.push("--draft");
        }
        command_output("glab", &args)
    }

    fn update_review_base(&self, review: &ReviewRequest, base: &str) -> Result<String> {
        command_output(
            "glab",
            &["mr", "update", review.id_value(), "--target-branch", base],
        )
    }

    fn review_body(&self, review: &ReviewRequest) -> Result<String> {
        let output = command_output(
            "glab",
            &["mr", "view", review.id_value(), "--output", "json"],
        )?;
        parse_body_field(&output, "description")
    }

    fn update_review_body(&self, review: &ReviewRequest, body: &str) -> Result<String> {
        command_output(
            "glab",
            &["mr", "update", review.id_value(), "--description", body],
        )
    }

    fn merge_review(&self, review: &ReviewRequest, strategy: &str, auto: bool) -> Result<String> {
        let mut args = vec!["mr", "merge", review.id_value()];
        match strategy {
            "rebase" => args.push("--rebase"),
            "merge" => {}
            _ => args.push("--squash"),
        }
        // glab schedules on pending pipelines by default; --auto just makes
        // the intent explicit. Either way the caller checks what happened.
        if auto {
            args.push("--auto-merge");
        }
        merge_with_retry(|| command_output("glab", &args))
    }

    fn merge_blocker(&self, review: &ReviewRequest) -> Result<MergeBlocker> {
        let output = command_output(
            "glab",
            &["mr", "view", review.id_value(), "--output", "json"],
        )?;
        Ok(classify_gitlab_merge(&output))
    }

    fn wait_for_checks(&self, review: &ReviewRequest) -> Result<bool> {
        // A just-pushed MR has no pipeline attached for a moment; tolerate
        // that for a grace window before concluding there is none, so we do
        // not merge before the pipeline even starts.
        let started = std::time::Instant::now();
        let timeout = crate::settings::check_timeout()?;
        let mut no_pipeline = 0u32;
        loop {
            let output = command_output(
                "glab",
                &["mr", "view", review.id_value(), "--output", "json"],
            )?;
            let value: serde_json::Value =
                serde_json::from_str(&output).context("failed to parse glab MR JSON")?;
            let status = value
                .get("head_pipeline")
                .or_else(|| value.get("pipeline"))
                .and_then(|pipeline| pipeline.get("status"))
                .and_then(serde_json::Value::as_str);

            match status {
                None if no_pipeline >= super::CHECK_GRACE_POLLS => return Ok(true),
                None => no_pipeline += 1,
                Some("success") | Some("skipped") | Some("manual") => return Ok(true),
                Some("failed") | Some("canceled") => return Ok(false),
                // Pipeline exists and is running: it registered, so reset.
                _ => no_pipeline = 0,
            }

            if let Some(timeout) = timeout
                && started.elapsed() >= timeout
            {
                return Err(super::checks_timed_out(review, timeout));
            }
            std::thread::sleep(super::check_poll_interval());
        }
    }

    fn open_reviews(&self) -> Result<Vec<ReviewRequest>> {
        let output = command_output(
            "glab",
            &[
                "mr",
                "list",
                "--opened",
                "--output",
                "json",
                "--per-page",
                "200",
            ],
        )?;
        parse_gitlab_reviews(&output)
    }

    fn mark_ready(&self, review: &ReviewRequest) -> Result<String> {
        command_output("glab", &["mr", "update", review.id_value(), "--ready"])
    }

    fn close_review(&self, review: &ReviewRequest, _delete_branch: bool) -> Result<String> {
        // glab has no delete-source-branch flag on close, so the remote branch
        // may linger; closing the MR is what retires the superseded review.
        command_output("glab", &["mr", "close", review.id_value()])
    }

    fn open_review(&self, review: &ReviewRequest) -> Result<String> {
        command_output("glab", &["mr", "view", review.id_value(), "--web"])
    }
}

fn list_review(branch: &str, state_flag: Option<&str>) -> Result<Option<ReviewRequest>> {
    let mut args = vec!["mr", "list", "--source-branch", branch];
    if let Some(flag) = state_flag {
        args.push(flag);
    }
    args.extend(["--output", "json"]);

    let output = command_output("glab", &args)?;
    parse_gitlab_review(&output)
}

fn parse_gitlab_review(output: &str) -> Result<Option<ReviewRequest>> {
    first_json_item(output)?
        .as_ref()
        .map(gitlab_review_from)
        .transpose()
}

fn parse_gitlab_reviews(output: &str) -> Result<Vec<ReviewRequest>> {
    json_items(output)?.iter().map(gitlab_review_from).collect()
}

fn gitlab_review_from(review: &serde_json::Value) -> Result<ReviewRequest> {
    Ok(ReviewRequest {
        id: format!("!{}", required_string(review, &["iid", "id"])?),
        branch: required_string(review, &["source_branch", "sourceBranch"])?,
        base: required_string(review, &["target_branch", "targetBranch"])?,
        state: parse_state(&required_string(review, &["state"])?),
        url: required_string(review, &["web_url", "webUrl", "url"])?,
        title: optional_string(review, "title"),
        draft: optional_bool(review, "draft"),
    })
}

/// Map GitLab's `detailed_merge_status` to a blocker. Only the precise
/// detailed status is trusted: the older coarse `merge_status` can't tell a
/// conflict from a failing pipeline, so it (and anything unrecognized) is
/// treated as not-blocked, leaving the caller to fall back to the error text.
fn classify_gitlab_merge(json: &str) -> MergeBlocker {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return MergeBlocker::None;
    };
    match value
        .get("detailed_merge_status")
        .and_then(serde_json::Value::as_str)
    {
        Some("conflict") => MergeBlocker::Conflicts,
        Some("ci_must_pass" | "ci_still_running") => MergeBlocker::ChecksPending,
        _ => MergeBlocker::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ReviewRequest, ReviewState};

    #[test]
    fn parse_gitlab_review_reads_snake_case_fields() {
        let review = parse_gitlab_review(
            r#"[{"iid":34,"state":"merged","target_branch":"feature/a","source_branch":"feature/b","web_url":"https://gitlab.com/owner/repo/-/merge_requests/34"}]"#,
        )
        .expect("parse review")
        .expect("review exists");

        assert_eq!(
            review,
            ReviewRequest {
                id: "!34".to_owned(),
                branch: "feature/b".to_owned(),
                base: "feature/a".to_owned(),
                state: ReviewState::Merged,
                url: "https://gitlab.com/owner/repo/-/merge_requests/34".to_owned(),
                title: String::new(),
                draft: false,
            }
        );
    }

    #[test]
    fn parse_gitlab_review_reads_camel_case_fields() {
        let review = parse_gitlab_review(
            r#"[{"id":34,"state":"closed","targetBranch":"feature/a","sourceBranch":"feature/b","webUrl":"https://gitlab.com/owner/repo/-/merge_requests/34"}]"#,
        )
        .expect("parse review")
        .expect("review exists");

        assert_eq!(review.id, "!34");
        assert_eq!(review.branch, "feature/b");
        assert_eq!(review.base, "feature/a");
        assert_eq!(review.state, ReviewState::Closed);
        assert_eq!(
            review.url,
            "https://gitlab.com/owner/repo/-/merge_requests/34"
        );
    }

    #[test]
    fn parse_gitlab_review_empty_array_returns_none() {
        assert_eq!(parse_gitlab_review("[]").expect("parse review"), None);
    }

    #[test]
    fn classify_gitlab_merge_reads_detailed_status() {
        assert_eq!(
            classify_gitlab_merge(r#"{"detailed_merge_status":"conflict"}"#),
            MergeBlocker::Conflicts
        );
        assert_eq!(
            classify_gitlab_merge(r#"{"detailed_merge_status":"ci_must_pass"}"#),
            MergeBlocker::ChecksPending
        );
        assert_eq!(
            classify_gitlab_merge(r#"{"detailed_merge_status":"ci_still_running"}"#),
            MergeBlocker::ChecksPending
        );
        assert_eq!(
            classify_gitlab_merge(r#"{"detailed_merge_status":"mergeable"}"#),
            MergeBlocker::None
        );
    }

    #[test]
    fn classify_gitlab_merge_ignores_coarse_or_missing_status() {
        // The coarse merge_status can't distinguish conflict from CI, so it's
        // left to the caller's fallback.
        assert_eq!(
            classify_gitlab_merge(r#"{"merge_status":"cannot_be_merged"}"#),
            MergeBlocker::None
        );
        assert_eq!(classify_gitlab_merge("{}"), MergeBlocker::None);
        assert_eq!(classify_gitlab_merge("not json"), MergeBlocker::None);
    }
}
