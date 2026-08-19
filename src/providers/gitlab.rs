use std::collections::BTreeSet;

use anyhow::{Context, Result};

use crate::git;

use super::json::{
    all_reviews, first_review, optional_bool, optional_string, parse_body_field, parse_state,
    required_string,
};
use super::{
    CheckStatus, MergeBlocker, ReviewProvider, ReviewRequest, ReviewSummary, WaitOutcome,
    command_output, merge_with_resettle, merge_with_retry,
};

/// GitLab keeps draft state in the MR title rather than a flag. `Draft:` is the
/// current marker; `WIP:` is the legacy spelling GitLab still honors.
const DRAFT_PREFIX: &str = "Draft: ";

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

    fn create_review(
        &self,
        branch: &str,
        base: &str,
        draft: bool,
        title: Option<&str>,
    ) -> Result<String> {
        // git-stk has already pushed the branch with the correct per-branch
        // refspec. glab's --fill would re-push the *current* checkout onto the
        // source ref (gh never pushes on create), clobbering a sibling branch
        // when submitting a stack from its leaf. So skip --fill and stay
        // non-interactive with an explicit title and description instead -
        // git-stk overwrites the body with the stack overview afterwards.
        let title = match title {
            Some(title) => title.to_owned(),
            None => git::commit_subject(branch)?,
        };
        let body = git::commit_body(branch)?;
        let description = if body.trim().is_empty() {
            title.as_str()
        } else {
            body.as_str()
        };
        let mut args = vec![
            "mr",
            "create",
            "--source-branch",
            branch,
            "--target-branch",
            base,
            "--title",
            title.as_str(),
            "--description",
            description,
            "--yes",
        ];
        if draft {
            args.push("--draft");
        }
        command_output("glab", &args)
    }

    fn update_review_base(&self, review: &ReviewRequest, base: &str) -> Result<String> {
        command_output(
            "glab",
            &[
                "mr",
                "update",
                review.id_value(),
                "--target-branch",
                base,
                "--yes",
            ],
        )
    }

    fn update_review_title(&self, review: &ReviewRequest, title: &str) -> Result<String> {
        // GitLab keeps draft state in the title, so a bare retitle would ready
        // the MR; re-apply the prefix glab itself writes.
        let title = if review.draft && !is_draft_title(title) {
            format!("{DRAFT_PREFIX}{title}")
        } else {
            title.to_owned()
        };
        command_output(
            "glab",
            &[
                "mr",
                "update",
                review.id_value(),
                "--title",
                title.as_str(),
                "--yes",
            ],
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
            &[
                "mr",
                "update",
                review.id_value(),
                "--description",
                body,
                "--yes",
            ],
        )
    }

    fn merge_review(&self, review: &ReviewRequest, strategy: &str, auto: bool) -> Result<String> {
        // --yes: glab confirms a merge interactively, but git-stk captures its
        // output, so the prompt would error instead of showing.
        let mut args = vec!["mr", "merge", review.id_value(), "--yes"];
        match strategy {
            "rebase" => args.push("--rebase"),
            "merge" => {}
            _ => args.push("--squash"),
        }
        // glab schedules on pending pipelines by default; --auto just makes
        // the intent explicit. A scheduled merge is queued for when checks
        // pass, so it sidesteps the post-push recompute race; a fixed backoff
        // retry is enough.
        if auto {
            args.push("--auto-merge");
            return merge_with_retry(|| command_output("glab", &args));
        }

        // An immediate merge right after a force-push (which restack and
        // merge --all do) can race GitLab recomputing mergeability; wait for it
        // to settle first. If the merge still 405s, the recompute is still in
        // flight, so re-poll until it settles between retries rather than
        // guessing a fixed delay that the recompute can outlast.
        wait_until_settled(review);
        merge_with_resettle(
            || wait_until_settled(review),
            || command_output("glab", &args),
        )
    }

    fn merge_blocker(&self, review: &ReviewRequest) -> Result<MergeBlocker> {
        let output = command_output(
            "glab",
            &["mr", "view", review.id_value(), "--output", "json"],
        )?;
        Ok(classify_gitlab_merge(&output))
    }

    fn wait_for_checks(&self, review: &ReviewRequest) -> Result<WaitOutcome> {
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
                None if no_pipeline >= super::CHECK_GRACE_POLLS => {
                    return Ok(WaitOutcome::Passed);
                }
                None => no_pipeline += 1,
                Some("success") | Some("skipped") | Some("manual") => {
                    return Ok(WaitOutcome::Passed);
                }
                Some("failed") | Some("canceled") => return Ok(WaitOutcome::Failed),
                // Pipeline exists and is running: it registered, so reset.
                _ => no_pipeline = 0,
            }

            if let Some(timeout) = timeout
                && started.elapsed() >= timeout
            {
                return Err(super::checks_timed_out(review, timeout));
            }

            // Some repos leave a merged MR's pipeline running instead of
            // cancelling it, so an out-of-band merge would otherwise hang here
            // until checkTimeout. Before sleeping for another poll, stop if the
            // review has already landed and let `sync` reconcile it.
            if super::review_merged_out_of_band(self, review)? {
                return Ok(WaitOutcome::Landed);
            }
            std::thread::sleep(super::check_poll_interval());
        }
    }

    fn check_status(&self, review: &ReviewRequest) -> Result<CheckStatus> {
        let output = command_output(
            "glab",
            &["mr", "view", review.id_value(), "--output", "json"],
        )?;
        let value: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse glab MR JSON")?;
        Ok(gitlab_pipeline_status(&value))
    }

    fn review_summary(&self, review: &ReviewRequest) -> Result<ReviewSummary> {
        let output = command_output(
            "glab",
            &["mr", "view", review.id_value(), "--output", "json"],
        )?;
        let mr: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse glab MR JSON")?;
        // Approvals live on a separate endpoint; best-effort, so a project
        // without approval access just reports zero.
        let approvals = command_output(
            "glab",
            &[
                "api",
                &format!(
                    "projects/:id/merge_requests/{}/approvals",
                    review.id_value()
                ),
            ],
        )
        .ok();
        Ok(gitlab_summary(&mr, approvals.as_deref()))
    }

    fn open_reviews(&self) -> Result<Vec<ReviewRequest>> {
        // No --opened: it is deprecated, and glab prints the deprecation notice
        // to stdout, which corrupts the JSON we parse. Listing without a state
        // flag already defaults to open merge requests.
        let output = command_output(
            "glab",
            &["mr", "list", "--output", "json", "--per-page", "200"],
        )?;
        parse_gitlab_reviews(&output)
    }

    fn mark_ready(&self, review: &ReviewRequest) -> Result<String> {
        command_output(
            "glab",
            &["mr", "update", review.id_value(), "--ready", "--yes"],
        )
    }

    fn request_reviewers(&self, review: &ReviewRequest, reviewers: &[String]) -> Result<String> {
        // Bare `--reviewer` replaces the set; a `+` prefix on each name adds
        // without dropping anyone already requested. GitLab has no team
        // reviewers, so every entry is a username.
        let list = reviewers
            .iter()
            .map(|name| format!("+{name}"))
            .collect::<Vec<_>>()
            .join(",");
        command_output(
            "glab",
            &["mr", "update", review.id_value(), "--reviewer", &list],
        )
    }

    fn close_review(&self, review: &ReviewRequest, _delete_branch: bool) -> Result<String> {
        // glab has no delete-source-branch flag on close, so the remote branch
        // may linger; closing the MR is what retires the superseded review.
        command_output("glab", &["mr", "close", review.id_value()])
    }

    fn open_review(&self, review: &ReviewRequest) -> Result<String> {
        command_output("glab", &["mr", "view", review.id_value(), "--web"])
    }

    fn enqueued_branches(&self, branches: &[String]) -> Result<BTreeSet<String>> {
        Ok(gitlab_enqueued_branches(branches))
    }
}

/// Whether a title already carries a draft marker, in either spelling, so a
/// retitle does not double up the prefix.
fn is_draft_title(title: &str) -> bool {
    let lowered = title.trim_start().to_ascii_lowercase();
    lowered.starts_with("draft:") || lowered.starts_with("wip:")
}

/// GitLab merge-train membership for `branches`. Unlike GitHub, pushing to a
/// branch on a train is not rejected - it silently *removes* the MR from the
/// train - so this pre-emptive check is the only thing that protects a queued
/// GitLab branch, and it freezes the branch the same way. Lists the project's
/// active trains once and intersects their source branches with `branches`.
/// Silent on failure: merge trains are a Premium feature, so the endpoint 404s
/// on most projects, which simply means nothing is queued.
fn gitlab_enqueued_branches(branches: &[String]) -> BTreeSet<String> {
    if branches.is_empty() {
        return BTreeSet::new();
    }
    // `--method GET` keeps it a read: adding `-f`/`-F` fields would flip glab to
    // POST. `--paginate` concatenates every page of cars into one JSON array.
    let Ok(output) = command_output(
        "glab",
        &[
            "api",
            "--method",
            "GET",
            "--paginate",
            "projects/:id/merge_trains?scope=active",
        ],
    ) else {
        return BTreeSet::new();
    };
    let wanted: BTreeSet<&str> = branches.iter().map(String::as_str).collect();
    active_train_source_branches(&output)
        .into_iter()
        .filter(|branch| wanted.contains(branch.as_str()))
        .collect()
}

/// The source branches of the active merge-train cars in the API response.
/// Each car carries its MR under `merge_request`, whose `source_branch` is the
/// branch the train holds frozen. Unparseable output yields nothing.
fn active_train_source_branches(json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(cars) = value.as_array() else {
        return Vec::new();
    };
    cars.iter()
        .filter_map(|car| {
            car.pointer("/merge_request/source_branch")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect()
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
    first_review(output, gitlab_review_from)
}

fn parse_gitlab_reviews(output: &str) -> Result<Vec<ReviewRequest>> {
    all_reviews(output, gitlab_review_from)
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

/// A GitLab pipeline `status` from an MR object's `head_pipeline`/`pipeline`,
/// mapped to a check dot. The MR list JSON does not always carry a pipeline;
/// when it does not, there is simply no dot.
fn gitlab_pipeline_status(value: &serde_json::Value) -> CheckStatus {
    let status = value
        .get("head_pipeline")
        .or_else(|| value.get("pipeline"))
        .and_then(|pipeline| pipeline.get("status"))
        .and_then(serde_json::Value::as_str);
    map_pipeline_status(status)
}

/// Map GitLab's pipeline status string to a check dot: a red pipeline fails, a
/// green/skipped/manual one passes, anything else in flight is pending, and an
/// absent pipeline shows nothing.
fn map_pipeline_status(status: Option<&str>) -> CheckStatus {
    match status {
        Some("success" | "skipped" | "manual") => CheckStatus::Passing,
        Some("failed" | "canceled") => CheckStatus::Failing,
        Some(_) => CheckStatus::Pending,
        None => CheckStatus::None,
    }
}

/// A GitLab MR's review tallies. Comments come from the MR's `user_notes_count`;
/// approvals from the separate approvals endpoint's `approved_by`; requested
/// changes from any reviewer whose `state` is `requested_changes`. Each source
/// is best-effort - a missing field simply counts as zero.
fn gitlab_summary(mr: &serde_json::Value, approvals_json: Option<&str>) -> ReviewSummary {
    let comments = mr
        .get("user_notes_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let approvals = approvals_json
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|value| {
            value
                .get("approved_by")
                .and_then(serde_json::Value::as_array)
                .map(|list| list.len() as u32)
        })
        .unwrap_or(0);
    let changes_requested = mr
        .get("reviewers")
        .and_then(serde_json::Value::as_array)
        .map(|reviewers| {
            reviewers
                .iter()
                .filter(|reviewer| {
                    reviewer.get("state").and_then(serde_json::Value::as_str)
                        == Some("requested_changes")
                })
                .count() as u32
        })
        .unwrap_or(0);
    ReviewSummary {
        approvals,
        comments,
        changes_requested,
    }
}

/// Map GitLab's `detailed_merge_status` to a blocker. Only the precise
/// detailed status is trusted: the older coarse `merge_status` can't tell a
/// conflict from a failing pipeline, so it (and anything unrecognized) is
/// treated as not-blocked, leaving the caller to fall back to the error text.
/// How long to wait for GitLab to recompute mergeability before an immediate
/// merge (5 polls, 2s apart).
const MERGE_SETTLE_POLLS: u32 = 5;
const MERGE_SETTLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Wait for GitLab to finish recomputing the MR's mergeability. Right after a
/// force-push (which restack and merge --all do), `detailed_merge_status` can
/// read `checking`/`unchecked`/`preparing`, or even a transient `conflict`,
/// before settling to `mergeable`; merging in that window fails spuriously. A
/// genuine blocker that persists past the budget falls through, and the merge
/// then surfaces the real reason.
fn wait_until_settled(review: &ReviewRequest) {
    for attempt in 0..MERGE_SETTLE_POLLS {
        if attempt > 0 {
            std::thread::sleep(MERGE_SETTLE_INTERVAL);
        }
        if !merge_status_settling(detailed_merge_status(review).as_deref()) {
            return;
        }
    }
}

/// GitLab's `detailed_merge_status` for the MR, best-effort.
fn detailed_merge_status(review: &ReviewRequest) -> Option<String> {
    let output = command_output(
        "glab",
        &["mr", "view", review.id_value(), "--output", "json"],
    )
    .ok()?;
    let value: serde_json::Value = serde_json::from_str(&output).ok()?;
    value
        .get("detailed_merge_status")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Whether a `detailed_merge_status` is still settling after a push - so a merge
/// should wait rather than treat it as a verdict. The recompute-window states
/// (`checking`/`unchecked`/`preparing`, and a transient `conflict`) are still
/// settling, as is an absent status: right after a push GitLab often has not
/// populated `detailed_merge_status` yet, and treating that "not computed yet"
/// as settled is what let merges fire into the 405 window. A recognized settled
/// status (`mergeable`, or a definite blocker like `ci_*`/draft/approvals) is
/// not settling. The poll budget in [`wait_until_settled`] bounds the wait, so
/// a status that never populates still falls through rather than hanging.
fn merge_status_settling(status: Option<&str>) -> bool {
    matches!(
        status,
        None | Some("checking" | "unchecked" | "preparing" | "conflict")
    )
}

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
    use crate::providers::{CheckStatus, ReviewRequest, ReviewState};

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
    fn merge_status_settling_waits_through_the_recompute_window() {
        // Post-push recompute states (incl. a transient conflict) -> wait.
        assert!(super::merge_status_settling(Some("checking")));
        assert!(super::merge_status_settling(Some("unchecked")));
        assert!(super::merge_status_settling(Some("preparing")));
        assert!(super::merge_status_settling(Some("conflict")));
        // Not computed yet (no field right after a push) -> wait, not fire.
        assert!(super::merge_status_settling(None));
        // Settled: merge now, or let the real blocker surface.
        assert!(!super::merge_status_settling(Some("mergeable")));
        assert!(!super::merge_status_settling(Some("ci_still_running")));
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

    #[test]
    fn active_train_source_branches_reads_each_cars_source() {
        let json = r#"[
            {"id":1,"status":"idle","merge_request":{"iid":7,"source_branch":"feature/a"}},
            {"id":2,"status":"fresh","merge_request":{"iid":9,"source_branch":"feature/b"}}
        ]"#;
        assert_eq!(
            super::active_train_source_branches(json),
            vec!["feature/a".to_owned(), "feature/b".to_owned()]
        );
    }

    #[test]
    fn no_active_trains_or_unparseable_yields_nothing() {
        // Empty array: the project has trains enabled but none running.
        assert!(super::active_train_source_branches("[]").is_empty());
        // A 404 body / error text (trains are Premium) is not an array.
        assert!(super::active_train_source_branches("not json").is_empty());
        assert!(super::active_train_source_branches(r#"{"message":"404 Not Found"}"#).is_empty());
    }

    #[test]
    fn map_pipeline_status_maps_gitlab_states() {
        assert_eq!(map_pipeline_status(Some("success")), CheckStatus::Passing);
        assert_eq!(map_pipeline_status(Some("manual")), CheckStatus::Passing);
        assert_eq!(map_pipeline_status(Some("failed")), CheckStatus::Failing);
        assert_eq!(map_pipeline_status(Some("canceled")), CheckStatus::Failing);
        assert_eq!(map_pipeline_status(Some("running")), CheckStatus::Pending);
        assert_eq!(map_pipeline_status(None), CheckStatus::None);
    }

    #[test]
    fn gitlab_pipeline_status_reads_head_pipeline_then_pipeline() {
        let head = serde_json::json!({"head_pipeline": {"status": "failed"}});
        assert_eq!(gitlab_pipeline_status(&head), CheckStatus::Failing);
        let legacy = serde_json::json!({"pipeline": {"status": "success"}});
        assert_eq!(gitlab_pipeline_status(&legacy), CheckStatus::Passing);
        assert_eq!(
            gitlab_pipeline_status(&serde_json::json!({})),
            CheckStatus::None
        );
    }

    #[test]
    fn gitlab_summary_gathers_comments_approvals_and_changes() {
        let mr = serde_json::json!({
            "user_notes_count": 3,
            "reviewers": [
                {"username": "a", "state": "requested_changes"},
                {"username": "b", "state": "reviewed"},
            ],
        });
        let approvals = r#"{"approved_by":[{"user":{"username":"c"}},{"user":{"username":"d"}}]}"#;
        let summary = gitlab_summary(&mr, Some(approvals));
        assert_eq!(
            summary,
            ReviewSummary {
                approvals: 2,
                comments: 3,
                changes_requested: 1,
            }
        );
        // No approvals endpoint access and a bare MR -> everything zero.
        assert_eq!(
            gitlab_summary(&serde_json::json!({}), None),
            ReviewSummary::default()
        );
    }
}
