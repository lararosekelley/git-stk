use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::git;

use super::json::{all_reviews, optional_bool, optional_string, parse_state, required_string};
use super::{
    CHECK_GRACE_POLLS, MergeBlocker, ReviewProvider, ReviewRequest, ReviewState, WaitOutcome,
    check_poll_interval, checks_timed_out, command_output, merge_with_retry,
    review_merged_out_of_band,
};

pub(super) struct GiteaProvider;

/// Gitea has no draft flag on PR creation; the convention is a `WIP:` title
/// prefix that the server treats as a work-in-progress marker.
const DRAFT_PREFIX: &str = "WIP: ";

impl ReviewProvider for GiteaProvider {
    fn review_for_branch(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        find_review(branch, false)
    }

    fn review_for_branch_including_closed(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        find_review(branch, true)
    }

    fn create_review(&self, branch: &str, base: &str, draft: bool) -> Result<String> {
        // Like the glab path: the branch is already pushed, so set title and
        // description explicitly and let git-stk overwrite the body afterward.
        let title = git::commit_subject(branch)?;
        let body = git::commit_body(branch)?;
        let description = if body.trim().is_empty() {
            &title
        } else {
            &body
        };
        // Drafts are a `WIP:` title prefix, not a flag.
        let title = if draft {
            format!("{DRAFT_PREFIX}{title}")
        } else {
            title.clone()
        };
        command_output(
            "tea",
            &[
                "pr",
                "create",
                "--head",
                branch,
                "--base",
                base,
                "--title",
                &title,
                "--description",
                description,
            ],
        )
    }

    fn update_review_base(&self, review: &ReviewRequest, base: &str) -> Result<String> {
        // `tea pr edit` cannot change the target branch, so PATCH it through
        // the API passthrough (which still uses tea's stored auth).
        let endpoint = format!("repos/{}/pulls/{}", repo_slug()?, review.id_value());
        let data = serde_json::json!({ "base": base }).to_string();
        command_output(
            "tea",
            &["api", "--method", "PATCH", &endpoint, "--data", &data],
        )
    }

    fn review_body(&self, review: &ReviewRequest) -> Result<String> {
        Ok(optional_string(&api_pull(review.id_value())?, "body"))
    }

    fn update_review_body(&self, review: &ReviewRequest, body: &str) -> Result<String> {
        command_output(
            "tea",
            &["pr", "edit", review.id_value(), "--description", body],
        )
    }

    fn merge_review(&self, review: &ReviewRequest, strategy: &str, _auto: bool) -> Result<String> {
        let style = match strategy {
            "rebase" => "rebase",
            "merge" => "merge",
            _ => "squash",
        };
        // Gitea/tea has no scheduled ("merge when checks pass") merge, so `auto`
        // falls back to an immediate merge; a real block surfaces as an error.
        let args = vec!["pr", "merge", review.id_value(), "--style", style];
        merge_with_retry(|| command_output("tea", &args))
    }

    fn merge_blocker(&self, _review: &ReviewRequest) -> Result<MergeBlocker> {
        // Gitea's PR object exposes only a coarse `mergeable` bool, which can't
        // tell a conflict from a pending check - so report nothing structural
        // and let the caller fall back to the merge error text, as the GitLab
        // provider does for its coarse status. (PR 2 may use a richer API.)
        Ok(MergeBlocker::None)
    }

    fn wait_for_checks(&self, review: &ReviewRequest) -> Result<WaitOutcome> {
        // Poll the head commit's combined status. A just-pushed head has no
        // status for a moment; tolerate that for a grace window before
        // concluding there are no checks (mirrors the gitlab path).
        let head_sha = api_pull(review.id_value())?
            .get("head")
            .and_then(|head| head.get("sha"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if head_sha.is_empty() {
            return Ok(WaitOutcome::Passed);
        }
        let endpoint = format!("repos/{}/commits/{head_sha}/status", repo_slug()?);
        let started = std::time::Instant::now();
        let timeout = crate::settings::check_timeout()?;
        let mut no_status = 0u32;
        loop {
            let value: Value = serde_json::from_str(&command_output("tea", &["api", &endpoint])?)
                .context("failed to parse gitea status JSON")?;
            let state = optional_string(&value, "state");
            // No statuses yet (empty state / zero count): wait out the grace
            // window, then treat as "no checks".
            let none_yet =
                value.get("total_count").and_then(Value::as_u64) == Some(0) || state.is_empty();
            match state.as_str() {
                _ if none_yet && no_status >= CHECK_GRACE_POLLS => return Ok(WaitOutcome::Passed),
                _ if none_yet => no_status += 1,
                "success" => return Ok(WaitOutcome::Passed),
                "failure" | "error" => return Ok(WaitOutcome::Failed),
                // pending/warning: statuses exist and are running - reset.
                _ => no_status = 0,
            }

            if let Some(timeout) = timeout
                && started.elapsed() >= timeout
            {
                return Err(checks_timed_out(review, timeout));
            }
            if review_merged_out_of_band(self, review)? {
                return Ok(WaitOutcome::Landed);
            }
            std::thread::sleep(check_poll_interval());
        }
    }

    fn open_reviews(&self) -> Result<Vec<ReviewRequest>> {
        list_pulls("open")
    }

    fn mark_ready(&self, review: &ReviewRequest) -> Result<String> {
        // Clearing the draft state means dropping the `WIP:` title prefix.
        let title = review
            .title
            .strip_prefix(DRAFT_PREFIX)
            .unwrap_or(&review.title);
        command_output("tea", &["pr", "edit", review.id_value(), "--title", title])
    }

    fn close_review(&self, review: &ReviewRequest, _delete_branch: bool) -> Result<String> {
        // tea's close has no delete-source-branch flag, so the remote branch may
        // linger; closing is what retires the superseded review.
        command_output("tea", &["pr", "close", review.id_value()])
    }

    fn open_review(&self, review: &ReviewRequest) -> Result<String> {
        command_output("tea", &["open", &format!("pulls/{}", review.id_value())])
    }
}

/// Find the branch's review, preferring open, then merged, then (only when
/// `include_closed`) closed. Gitea's `pr list` has no head filter, so list and
/// match client-side.
fn find_review(branch: &str, include_closed: bool) -> Result<Option<ReviewRequest>> {
    let mut matches: Vec<ReviewRequest> = list_pulls("all")?
        .into_iter()
        .filter(|review| review.branch == branch)
        .collect();
    matches.sort_by_key(|review| state_rank(&review.state));
    Ok(matches.into_iter().find(|review| match review.state {
        // Open and merged are the live review; closed or unrecognized states
        // surface only when explicitly asked, matching GitHub/GitLab.
        ReviewState::Open | ReviewState::Merged => true,
        ReviewState::Closed | ReviewState::Unknown(_) => include_closed,
    }))
}

fn state_rank(state: &ReviewState) -> u8 {
    match state {
        ReviewState::Open => 0,
        ReviewState::Merged => 1,
        ReviewState::Closed => 2,
        ReviewState::Unknown(_) => 3,
    }
}

/// One pull request's canonical API object.
fn api_pull(id_value: &str) -> Result<Value> {
    let endpoint = format!("repos/{}/pulls/{id_value}", repo_slug()?);
    serde_json::from_str(&command_output("tea", &["api", &endpoint])?)
        .context("failed to parse gitea pull JSON")
}

/// Every PR in `state` (open|closed|all) as ReviewRequests, following
/// pagination. Uses the API passthrough rather than `tea pr list` (whose JSON
/// is a lossy projection: no `merged`, `draft`, or nested refs), and pages so a
/// branch's review can't fall off the first page on a busy repo.
fn list_pulls(state: &str) -> Result<Vec<ReviewRequest>> {
    // Gitea caps page size at 50, so request exactly that and page past it.
    const PAGE_SIZE: usize = 50;
    // Bound the walk so a misbehaving server can't loop forever.
    const MAX_PAGES: usize = 50;
    let slug = repo_slug()?;
    let mut reviews = Vec::new();
    for page in 1..=MAX_PAGES {
        let endpoint = format!("repos/{slug}/pulls?state={state}&page={page}&limit={PAGE_SIZE}");
        let batch = all_reviews(&command_output("tea", &["api", &endpoint])?, gitea_review_from)?;
        let full_page = batch.len() == PAGE_SIZE;
        reviews.extend(batch);
        if !full_page {
            break;
        }
    }
    Ok(reviews)
}

fn gitea_review_from(review: &Value) -> Result<ReviewRequest> {
    // A merged Gitea PR reports state "closed" with merged=true; surface that
    // as Merged rather than Closed.
    let state = if optional_bool(review, "merged") {
        ReviewState::Merged
    } else {
        parse_state(&required_string(review, &["state"])?)
    };

    Ok(ReviewRequest {
        id: format!("#{}", required_string(review, &["number", "index", "id"])?),
        branch: branch_ref(review, &["head", "head_branch", "headBranch"])?,
        base: branch_ref(review, &["base", "base_branch", "baseBranch"])?,
        state,
        url: required_string(review, &["html_url", "htmlUrl", "url"])?,
        title: optional_string(review, "title"),
        draft: optional_bool(review, "draft"),
    })
}

/// A branch name from a PR's base/head, tolerating the shapes tea may emit: a
/// bare string, a nested `{ "ref": ... }`/`{ "label": ... }` object.
fn branch_ref(review: &Value, keys: &[&str]) -> Result<String> {
    for key in keys {
        let Some(field) = review.get(*key) else {
            continue;
        };
        if let Some(value) = field.as_str() {
            return Ok(value.to_owned());
        }
        for nested in ["ref", "name", "label"] {
            if let Some(value) = field.get(nested).and_then(Value::as_str) {
                return Ok(value.to_owned());
            }
        }
    }
    bail!("provider JSON missing branch field: {}", keys.join(" or "));
}

/// The `owner/repo` slug from the configured remote's URL, for API calls that
/// need it in the path.
fn repo_slug() -> Result<String> {
    let remote = crate::settings::remote()?;
    let url = git::remote_url(&remote)?.with_context(|| format!("remote {remote} has no URL"))?;
    slug_from_url(&url).with_context(|| format!("could not parse owner/repo from {url}"))
}

fn slug_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest); // drop userinfo
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    // owner/repo are the last two segments across `host/owner/repo`,
    // `host:port/owner/repo`, and scp `host:owner/repo`.
    let parts: Vec<&str> = rest.split(['/', ':']).filter(|s| !s.is_empty()).collect();
    if parts.len() < 3 {
        return None;
    }
    Some(format!(
        "{}/{}",
        parts[parts.len() - 2],
        parts[parts.len() - 1]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitea_review_from_maps_merged_closed_state() {
        let merged = gitea_review_from(&serde_json::json!({
            "number": 7, "state": "closed", "merged": true,
            "head": {"ref": "feature/b"}, "base": {"ref": "feature/a"},
            "html_url": "https://gitea.com/owner/repo/pulls/7", "title": "B"
        }))
        .expect("parse");
        assert_eq!(merged.id, "#7");
        assert_eq!(merged.branch, "feature/b");
        assert_eq!(merged.base, "feature/a");
        assert_eq!(merged.state, ReviewState::Merged);
        assert_eq!(merged.url, "https://gitea.com/owner/repo/pulls/7");

        let closed = gitea_review_from(&serde_json::json!({
            "number": 8, "state": "closed", "merged": false,
            "head": {"ref": "x"}, "base": {"ref": "main"},
            "html_url": "https://gitea.com/o/r/pulls/8"
        }))
        .expect("parse");
        assert_eq!(closed.state, ReviewState::Closed);
    }

    #[test]
    fn gitea_review_from_tolerates_flat_branch_strings() {
        let review = gitea_review_from(&serde_json::json!({
            "index": 3, "state": "open",
            "head": "feature/x", "base": "main",
            "url": "https://gitea.com/o/r/pulls/3"
        }))
        .expect("parse");
        assert_eq!(review.id, "#3");
        assert_eq!(review.branch, "feature/x");
        assert_eq!(review.base, "main");
        assert_eq!(review.state, ReviewState::Open);
    }

    #[test]
    fn slug_from_url_handles_url_shapes() {
        assert_eq!(
            slug_from_url("https://gitea.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            slug_from_url("git@gitea.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            slug_from_url("ssh://git@gitea.example.com:2222/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            slug_from_url("https://user:token@codeberg.org/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(slug_from_url("https://gitea.com/").as_deref(), None);
    }
}
