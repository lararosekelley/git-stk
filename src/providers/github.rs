use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::git;

use super::json::{
    all_reviews, first_review, optional_bool, optional_string, parse_body_field, parse_state,
    required_string,
};
use super::{
    CheckStatus, MergeBlocker, ReviewAnnotation, ReviewProvider, ReviewRequest, ReviewState,
    ReviewSummary, WaitOutcome, command_output, generic_annotate, merge_with_retry,
};

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

    fn create_review(
        &self,
        branch: &str,
        base: &str,
        draft: bool,
        title: Option<&str>,
    ) -> Result<String> {
        // Like the glab and tea paths: the branch is already pushed, so set the
        // title and body explicitly from its tip commit. --fill would turn a
        // multi-commit branch into a bulleted dump of every commit subject,
        // which then renders awkwardly under git-stk's template and stack
        // overview; git-stk overwrites the body afterward regardless.
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
            "pr",
            "create",
            "--head",
            branch,
            "--base",
            base,
            "--title",
            title.as_str(),
            "--body",
            description,
        ];
        if draft {
            args.push("--draft");
        }
        command_output("gh", &args)
    }

    fn update_review_base(&self, review: &ReviewRequest, base: &str) -> Result<String> {
        command_output("gh", &["pr", "edit", review.id_value(), "--base", base])
    }

    fn update_review_title(&self, review: &ReviewRequest, title: &str) -> Result<String> {
        command_output("gh", &["pr", "edit", review.id_value(), "--title", title])
    }

    fn review_body(&self, review: &ReviewRequest) -> Result<String> {
        let output = command_output("gh", &["pr", "view", review.id_value(), "--json", "body"])?;
        parse_body_field(&output, "body")
    }

    fn update_review_body(&self, review: &ReviewRequest, body: &str) -> Result<String> {
        command_output("gh", &["pr", "edit", review.id_value(), "--body", body])
    }

    fn review_state(&self, review: &ReviewRequest) -> Result<Option<ReviewState>> {
        let output = command_output("gh", &["pr", "view", review.id_value(), "--json", "state"])?;
        let value: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse gh pr view state")?;
        Ok(value
            .get("state")
            .and_then(serde_json::Value::as_str)
            .map(parse_state))
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

    fn wait_for_checks(&self, review: &ReviewRequest) -> Result<WaitOutcome> {
        // Poll until the checks settle. `gh pr checks` exits 0 when green, 8
        // while pending, and 1 otherwise - but "1 + no checks reported" is
        // ambiguous: a repo with no CI, or a just-pushed branch whose checks
        // have not registered yet (often queued, not running). Tolerate that
        // state for a grace window before concluding there are none, so we
        // neither merge early nor report a false failure.
        let started = Instant::now();
        let timeout = crate::settings::check_timeout()?;
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
                ChecksState::Passed => return Ok(WaitOutcome::Passed),
                ChecksState::Failed => return Ok(WaitOutcome::Failed),
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
                        return Ok(WaitOutcome::Passed);
                    }
                }
                ChecksState::NoneYet => no_checks += 1,
                // A real pending state resets the grace count: checks exist.
                ChecksState::Pending => no_checks = 0,
            }

            if let Some(timeout) = timeout
                && started.elapsed() >= timeout
            {
                return Err(super::checks_timed_out(review, timeout));
            }

            // Some repos leave a merged PR's checks pending instead of
            // cancelling them, so an out-of-band merge would otherwise hang
            // here until checkTimeout. Before sleeping for another poll, stop
            // if the review has already landed and let `sync` reconcile it.
            if super::review_merged_out_of_band(self, review)? {
                return Ok(WaitOutcome::Landed);
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
        // Deliberately lightweight - statusCheckRollup here would fetch every
        // open PR's full check list (huge and slow on a busy repo), so the CI
        // dots come from a per-branch check_status scoped to the shown stack.
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

    fn annotate_branches(
        &self,
        branches: &[String],
        detail: bool,
    ) -> Result<BTreeMap<String, ReviewAnnotation>> {
        if branches.is_empty() {
            return Ok(BTreeMap::new());
        }
        // One GraphQL query fetches number, CI rollup, merge-queue entry, and
        // (with detail) reviews for every branch at once. On any trouble (not a
        // GitHub repo, an over-large --all, a GraphQL hiccup) fall back to the
        // generic per-branch path rather than dropping the annotations.
        match batched_annotate(branches, detail) {
            Ok(annotations) => Ok(annotations),
            Err(_) => generic_annotate(self, branches, detail),
        }
    }

    fn check_status(&self, review: &ReviewRequest) -> Result<CheckStatus> {
        let output = command_output(
            "gh",
            &[
                "pr",
                "view",
                review.id_value(),
                "--json",
                "statusCheckRollup",
            ],
        )?;
        let value: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse gh checks JSON")?;
        Ok(rollup_status(&value))
    }

    fn review_summary(&self, review: &ReviewRequest) -> Result<ReviewSummary> {
        let output = command_output(
            "gh",
            &["pr", "view", review.id_value(), "--json", "latestReviews"],
        )?;
        let value: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse gh reviews JSON")?;
        Ok(count_latest_reviews(value.get("latestReviews")))
    }

    fn mark_ready(&self, review: &ReviewRequest) -> Result<String> {
        command_output("gh", &["pr", "ready", review.id_value()])
    }

    fn request_reviewers(&self, review: &ReviewRequest, reviewers: &[String]) -> Result<String> {
        // gh takes users and `org/team` reviewers alike, comma-separated;
        // --add-reviewer re-requests without clearing anyone already on the PR.
        let list = reviewers.join(",");
        command_output(
            "gh",
            &["pr", "edit", review.id_value(), "--add-reviewer", &list],
        )
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

    fn enqueued_branches(&self, branches: &[String]) -> Result<BTreeSet<String>> {
        Ok(github_enqueued_branches(branches))
    }
}

/// Fetch review annotations for `branches` in a single GraphQL query: one
/// aliased `pullRequests(headRefName:...)` lookup per branch, each returning
/// the number, merge-queue entry, CI rollup state, and (with `detail`) the
/// latest reviews. Collapses what would be ~2N provider calls into one.
fn batched_annotate(
    branches: &[String],
    detail: bool,
) -> Result<BTreeMap<String, ReviewAnnotation>> {
    let (owner, repo) = repo_owner_name().context("could not resolve owner/repo")?;
    let query = build_annotation_query(branches.len(), detail);
    let owner_arg = format!("owner={owner}");
    let repo_arg = format!("repo={repo}");
    let query_arg = format!("query={query}");
    let head_args: Vec<String> = branches
        .iter()
        .enumerate()
        .map(|(index, branch)| format!("h{index}={branch}"))
        .collect();

    let mut args = vec!["api", "graphql", "-f", &owner_arg, "-f", &repo_arg];
    for head_arg in &head_args {
        args.extend(["-f", head_arg]);
    }
    args.extend(["-f", &query_arg]);

    parse_annotation_batch(&command_output("gh", &args)?, detail)
}

/// Build the aliased GraphQL query for [`batched_annotate`]. Kept out of
/// `format!` to dodge brace-escaping; the fields are exactly what
/// [`ReviewAnnotation`] needs, no more.
fn build_annotation_query(count: usize, detail: bool) -> String {
    let reviews_field = if detail {
        "latestReviews(first:100){nodes{state}} "
    } else {
        ""
    };
    let mut vars = String::from("$owner:String!,$repo:String!");
    let mut aliases = String::new();
    for index in 0..count {
        vars.push_str(&format!(",$h{index}:String!"));
        aliases.push_str(&format!(
            "p{index}:pullRequests(headRefName:$h{index},states:OPEN,first:1)"
        ));
        aliases.push_str("{nodes{number headRefName mergeQueueEntry{state} ");
        aliases.push_str(reviews_field);
        aliases.push_str("commits(last:1){nodes{commit{statusCheckRollup{state}}}}}}");
    }
    format!("query({vars}){{repository(owner:$owner,name:$repo){{{aliases}}}}}")
}

/// Parse [`batched_annotate`]'s response. Each aliased entry holds at most one
/// PR node (the open PR for that head, if any); a head with no open PR yields
/// an empty `nodes` list and is skipped.
fn parse_annotation_batch(json: &str, detail: bool) -> Result<BTreeMap<String, ReviewAnnotation>> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("failed to parse gh graphql JSON")?;
    let repository = value
        .pointer("/data/repository")
        .and_then(serde_json::Value::as_object)
        .context("gh graphql response missing repository")?;
    let mut annotations = BTreeMap::new();
    for entry in repository.values() {
        let Some(node) = entry.pointer("/nodes/0") else {
            continue;
        };
        let (Some(branch), Some(number)) = (
            node.get("headRefName").and_then(serde_json::Value::as_str),
            node.get("number").and_then(serde_json::Value::as_i64),
        ) else {
            continue;
        };
        let checks = rollup_state_to_status(
            node.pointer("/commits/nodes/0/commit/statusCheckRollup/state")
                .and_then(serde_json::Value::as_str),
        );
        let queued = node
            .get("mergeQueueEntry")
            .is_some_and(|entry| !entry.is_null());
        let summary = detail.then(|| count_latest_reviews(node.pointer("/latestReviews/nodes")));
        annotations.insert(
            branch.to_owned(),
            ReviewAnnotation {
                id: format!("#{number}"),
                checks,
                queued,
                summary,
            },
        );
    }
    Ok(annotations)
}

/// Map GraphQL's aggregate `StatusState` to a check dot (the GraphQL rollup
/// already reduces every check to one state, unlike the REST array).
fn rollup_state_to_status(state: Option<&str>) -> CheckStatus {
    match state {
        Some("SUCCESS") => CheckStatus::Passing,
        Some("FAILURE" | "ERROR") => CheckStatus::Failing,
        Some("PENDING" | "EXPECTED") => CheckStatus::Pending,
        _ => CheckStatus::None,
    }
}

/// `mergeQueueEntry` exposes merge-queue membership, but only over GraphQL -
/// there is no `gh pr view --json` field for it. Query each branch's open PR by
/// head ref; a non-null entry means it is queued and the branch is locked.
const MERGE_QUEUE_QUERY: &str = "query($owner:String!,$repo:String!,$head:String!){\
repository(owner:$owner,name:$repo){\
pullRequests(headRefName:$head,states:OPEN,first:1){nodes{mergeQueueEntry{state}}}}}";

/// GitHub merge-queue membership for `branches`. Best-effort: any failure (not
/// a GitHub repo, an API hiccup, a query without queue access) warns once and
/// leaves the remaining branches un-frozen rather than blocking the restack -
/// the reactive push-rejection net (`git::push_force_with_lease`) is the
/// backstop, since GitHub *rejects* a push to a queued branch.
fn github_enqueued_branches(branches: &[String]) -> BTreeSet<String> {
    let mut queued = BTreeSet::new();
    if branches.is_empty() {
        return queued;
    }
    let Some((owner, repo)) = repo_owner_name() else {
        return queued;
    };
    for branch in branches {
        match branch_in_merge_queue(&owner, &repo, branch) {
            Ok(true) => {
                queued.insert(branch.clone());
            }
            Ok(false) => {}
            // A failed check is global (auth, network, no queue access) far more
            // often than per-branch, so warn once and stop probing rather than
            // repeating the same warning for every branch.
            Err(error) => {
                anstream::eprintln!(
                    "{}",
                    crate::style::warn(&format!(
                        "could not check merge-queue status: {error}; treating remaining branches as not queued"
                    ))
                );
                break;
            }
        }
    }
    queued
}

/// The current repository's `owner` and `name`, or None when gh cannot resolve
/// them (not a GitHub repo, or gh unavailable).
fn repo_owner_name() -> Option<(String, String)> {
    let output = command_output("gh", &["repo", "view", "--json", "nameWithOwner"]).ok()?;
    let value: serde_json::Value = serde_json::from_str(&output).ok()?;
    let full = value
        .get("nameWithOwner")
        .and_then(serde_json::Value::as_str)?;
    let (owner, repo) = full.split_once('/')?;
    Some((owner.to_owned(), repo.to_owned()))
}

fn branch_in_merge_queue(owner: &str, repo: &str, branch: &str) -> Result<bool> {
    let owner_arg = format!("owner={owner}");
    let repo_arg = format!("repo={repo}");
    let head_arg = format!("head={branch}");
    let query_arg = format!("query={MERGE_QUEUE_QUERY}");
    let output = command_output(
        "gh",
        &[
            "api", "graphql", "-f", &owner_arg, "-f", &repo_arg, "-f", &head_arg, "-f", &query_arg,
        ],
    )?;
    Ok(parse_merge_queue_entry(&output))
}

/// True when the GraphQL response's single PR carries a non-null
/// `mergeQueueEntry` - i.e. it sits in the merge queue. A null entry (not
/// queued), an empty `nodes` list (no open PR for that head), or unparseable
/// output all read as not queued.
fn parse_merge_queue_entry(json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return false;
    };
    value
        .pointer("/data/repository/pullRequests/nodes")
        .and_then(serde_json::Value::as_array)
        .and_then(|nodes| nodes.first())
        .and_then(|node| node.get("mergeQueueEntry"))
        .is_some_and(|entry| !entry.is_null())
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
    first_review(output, github_review_from)
}

fn parse_github_reviews(output: &str) -> Result<Vec<ReviewRequest>> {
    all_reviews(output, github_review_from)
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

/// The check status from a `gh pr view --json statusCheckRollup` object, or
/// [`CheckStatus::None`] when the field is absent or empty.
fn rollup_status(value: &serde_json::Value) -> CheckStatus {
    match value.get("statusCheckRollup") {
        Some(rollup) => aggregate_rollup(rollup),
        None => CheckStatus::None,
    }
}

/// Reduce GitHub's `statusCheckRollup` (a mix of CheckRun nodes, which carry a
/// `status` and `conclusion`, and StatusContext nodes, which carry a `state`)
/// to one status: any failure wins, then any check still in flight is pending,
/// otherwise passing. An empty or absent rollup means there are no checks.
fn aggregate_rollup(rollup: &serde_json::Value) -> CheckStatus {
    let Some(items) = rollup.as_array().filter(|items| !items.is_empty()) else {
        return CheckStatus::None;
    };
    let mut pending = false;
    for item in items {
        let field = |name| {
            item.get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        };
        let conclusion = field("conclusion");
        let status = field("status");
        let state = field("state");
        if matches!(
            conclusion,
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
        ) || matches!(state, "FAILURE" | "ERROR")
        {
            return CheckStatus::Failing;
        }
        // A CheckRun that has not COMPLETED, or a StatusContext still expecting
        // a result, is running.
        if (!status.is_empty() && status != "COMPLETED") || matches!(state, "PENDING" | "EXPECTED")
        {
            pending = true;
        }
    }
    if pending {
        CheckStatus::Pending
    } else {
        CheckStatus::Passing
    }
}

/// Tally a `latestReviews` array (the latest review per reviewer) by state.
fn count_latest_reviews(reviews: Option<&serde_json::Value>) -> ReviewSummary {
    let mut summary = ReviewSummary::default();
    let Some(items) = reviews.and_then(serde_json::Value::as_array) else {
        return summary;
    };
    for item in items {
        match item.get("state").and_then(serde_json::Value::as_str) {
            Some("APPROVED") => summary.approvals += 1,
            Some("CHANGES_REQUESTED") => summary.changes_requested += 1,
            Some("COMMENTED") => summary.comments += 1,
            _ => {}
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{CheckStatus, ReviewRequest, ReviewState};

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

    #[test]
    fn merge_queue_entry_present_means_queued() {
        // gh api graphql nests the data under data.repository.pullRequests.
        assert!(parse_merge_queue_entry(
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"mergeQueueEntry":{"state":"QUEUED"}}]}}}}"#
        ));
    }

    #[test]
    fn a_null_or_absent_merge_queue_entry_is_not_queued() {
        // Open PR, just not in the queue.
        assert!(!parse_merge_queue_entry(
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"mergeQueueEntry":null}]}}}}"#
        ));
        // No open PR for that head at all.
        assert!(!parse_merge_queue_entry(
            r#"{"data":{"repository":{"pullRequests":{"nodes":[]}}}}"#
        ));
        // Unparseable / unexpected shapes never read as queued.
        assert!(!parse_merge_queue_entry("{}"));
        assert!(!parse_merge_queue_entry("not json"));
    }

    #[test]
    fn aggregate_rollup_lets_any_failure_win() {
        let rollup = serde_json::json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"},
        ]);
        assert_eq!(aggregate_rollup(&rollup), CheckStatus::Failing);
        // A red StatusContext counts too.
        let context = serde_json::json!([{"__typename": "StatusContext", "state": "ERROR"}]);
        assert_eq!(aggregate_rollup(&context), CheckStatus::Failing);
    }

    #[test]
    fn aggregate_rollup_is_pending_while_a_check_runs() {
        let rollup = serde_json::json!([
            {"status": "COMPLETED", "conclusion": "SUCCESS"},
            {"status": "IN_PROGRESS", "conclusion": null},
        ]);
        assert_eq!(aggregate_rollup(&rollup), CheckStatus::Pending);
        let context = serde_json::json!([{"state": "PENDING"}]);
        assert_eq!(aggregate_rollup(&context), CheckStatus::Pending);
    }

    #[test]
    fn aggregate_rollup_passes_when_all_green_and_none_when_empty() {
        let green = serde_json::json!([
            {"status": "COMPLETED", "conclusion": "SUCCESS"},
            {"status": "COMPLETED", "conclusion": "SKIPPED"},
            {"state": "SUCCESS"},
        ]);
        assert_eq!(aggregate_rollup(&green), CheckStatus::Passing);
        assert_eq!(aggregate_rollup(&serde_json::json!([])), CheckStatus::None);
        assert_eq!(
            aggregate_rollup(&serde_json::json!("nope")),
            CheckStatus::None
        );
    }

    #[test]
    fn rollup_status_reads_the_view_json_field() {
        // check_status feeds `gh pr view --json statusCheckRollup` here.
        let value = serde_json::json!({
            "statusCheckRollup": [{"status": "COMPLETED", "conclusion": "FAILURE"}]
        });
        assert_eq!(rollup_status(&value), CheckStatus::Failing);
        // No field (or empty) means no dot.
        assert_eq!(rollup_status(&serde_json::json!({})), CheckStatus::None);
    }

    #[test]
    fn rollup_state_to_status_maps_the_graphql_aggregate() {
        assert_eq!(
            rollup_state_to_status(Some("SUCCESS")),
            CheckStatus::Passing
        );
        assert_eq!(
            rollup_state_to_status(Some("FAILURE")),
            CheckStatus::Failing
        );
        assert_eq!(rollup_state_to_status(Some("ERROR")), CheckStatus::Failing);
        assert_eq!(
            rollup_state_to_status(Some("PENDING")),
            CheckStatus::Pending
        );
        assert_eq!(
            rollup_state_to_status(Some("EXPECTED")),
            CheckStatus::Pending
        );
        assert_eq!(rollup_state_to_status(None), CheckStatus::None);
    }

    #[test]
    fn build_annotation_query_includes_reviews_only_with_detail() {
        let plain = build_annotation_query(2, false);
        assert!(plain.contains("$h0:String!") && plain.contains("$h1:String!"));
        assert!(plain.contains("statusCheckRollup"));
        assert!(plain.contains("mergeQueueEntry"));
        assert!(!plain.contains("latestReviews"));
        assert!(build_annotation_query(1, true).contains("latestReviews"));
    }

    #[test]
    fn parse_annotation_batch_reads_each_prs_status_queue_and_reviews() {
        let json = r#"{"data":{"repository":{
            "p0":{"nodes":[{"number":9,"headRefName":"feature/a","mergeQueueEntry":null,
                "latestReviews":{"nodes":[{"state":"APPROVED"},{"state":"APPROVED"}]},
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}]},
            "p1":{"nodes":[{"number":10,"headRefName":"feature/b","mergeQueueEntry":{"state":"QUEUED"},
                "latestReviews":{"nodes":[{"state":"CHANGES_REQUESTED"}]},
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}]}}]},
            "p2":{"nodes":[]}
        }}}"#;
        let annotations = parse_annotation_batch(json, true).expect("parse");

        // p2 had no open PR, so only two branches are annotated.
        assert_eq!(annotations.len(), 2);
        let a = &annotations["feature/a"];
        assert_eq!(a.id, "#9");
        assert_eq!(a.checks, CheckStatus::Passing);
        assert!(!a.queued);
        assert_eq!(a.summary.expect("summary").approvals, 2);
        let b = &annotations["feature/b"];
        assert_eq!(b.id, "#10");
        assert_eq!(b.checks, CheckStatus::Failing);
        assert!(b.queued, "a non-null mergeQueueEntry means queued");
        assert_eq!(b.summary.expect("summary").changes_requested, 1);
    }

    #[test]
    fn parse_annotation_batch_omits_summary_without_detail() {
        let json = r#"{"data":{"repository":{"p0":{"nodes":[{"number":9,"headRefName":"feature/a",
            "mergeQueueEntry":null,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"PENDING"}}}]}}]}}}}"#;
        let annotations = parse_annotation_batch(json, false).expect("parse");
        let a = &annotations["feature/a"];
        assert_eq!(a.checks, CheckStatus::Pending);
        assert!(a.summary.is_none());
    }

    #[test]
    fn count_latest_reviews_tallies_by_state() {
        let reviews = serde_json::json!([
            {"state": "APPROVED"},
            {"state": "APPROVED"},
            {"state": "CHANGES_REQUESTED"},
            {"state": "COMMENTED"},
            {"state": "DISMISSED"},
        ]);
        let summary = count_latest_reviews(Some(&reviews));
        assert_eq!(summary.approvals, 2);
        assert_eq!(summary.changes_requested, 1);
        assert_eq!(summary.comments, 1);
        // No reviews at all -> an empty summary.
        assert_eq!(count_latest_reviews(None), ReviewSummary::default());
    }
}
