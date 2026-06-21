use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::git;

use super::json::{
    all_reviews, optional_bool, optional_string, parse_body_field, parse_state, required_string,
};
use super::{
    MergeBlocker, ReviewProvider, ReviewRequest, ReviewState, WaitOutcome, command_output,
    merge_with_retry,
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
        // Fetch the single PR through the API passthrough rather than scanning
        // the listing, so the body is read correctly even past the first page.
        let endpoint = format!("repos/{}/pulls/{}", repo_slug()?, review.id_value());
        let output = command_output("tea", &["api", &endpoint])?;
        parse_body_field(&output, "body")
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

    fn wait_for_checks(&self, _review: &ReviewRequest) -> Result<WaitOutcome> {
        // PR 1 does not yet wire Gitea's per-PR check status, so treat checks as
        // absent and proceed; if checks actually gate the merge, the merge call
        // surfaces the block. (PR 2 reads the `ci` field / commit status.)
        Ok(WaitOutcome::Passed)
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

/// Every PR in the given state (`open`/`closed`/`all`), following pagination.
/// Gitea's `pr list` has no head filter, so callers match client-side; paging
/// keeps a branch's review from falling off the first page on a busy repo.
fn list_pulls(state: &str) -> Result<Vec<ReviewRequest>> {
    const PAGE_SIZE: usize = 200;
    // Bound the walk so a misbehaving server can't loop forever.
    const MAX_PAGES: usize = 50;
    let limit = PAGE_SIZE.to_string();
    let mut reviews = Vec::new();
    for page in 1..=MAX_PAGES {
        let page = page.to_string();
        let output = command_output(
            "tea",
            &[
                "pr", "list", "--state", state, "--output", "json", "--page", &page, "--limit",
                &limit,
            ],
        )?;
        let batch = all_reviews(&output, gitea_review_from)?;
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
