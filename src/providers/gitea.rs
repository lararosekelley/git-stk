use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::git;

use super::json::{
    all_reviews, optional_bool, optional_string, parse_issue_state, parse_state, required_string,
};
use super::{
    Activity, Anchor, CHECK_GRACE_POLLS, CheckStatus, Comment, Conversation, Decision, FETCH_LIMIT,
    InboxEntry, InboxList, IssueState, Listing, MergeBlocker, ReviewEvent, ReviewProvider,
    ReviewRequest, ReviewState, ReviewSummary, Side, Submission, Thread, Verdict, VerdictState,
    WaitOutcome, check_poll_interval, checks_timed_out, command_output, merge_with_resettle,
    review_merged_out_of_band,
};

pub(super) struct GiteaProvider;

/// Gitea has no draft flag on PR creation; the convention is a `WIP:` title
/// prefix that the server treats as a work-in-progress marker. This is the
/// spelling we write; [`DRAFT_MARKERS`] covers the ones we recognize.
const DRAFT_PREFIX: &str = "WIP: ";

/// Gitea's stock work-in-progress markers, matched case-insensitively as Gitea
/// itself matches them - so `wip: x` is already a draft and must not collect a
/// second prefix. (An instance can configure its own set; these are the
/// defaults, and the only ones we can know about.)
const DRAFT_MARKERS: [&str; 2] = ["wip:", "[wip]"];

impl ReviewProvider for GiteaProvider {
    fn review_for_branch(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        find_review(branch, false)
    }

    fn review_for_branch_including_closed(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        find_review(branch, true)
    }

    fn review_by_number(&self, number: u64) -> Result<ReviewRequest> {
        gitea_review_from(&api_pull(&number.to_string())?)
    }

    fn create_review(
        &self,
        branch: &str,
        base: &str,
        draft: bool,
        title: Option<&str>,
    ) -> Result<String> {
        // Like the glab path: the branch is already pushed, so set title and
        // description explicitly and let git-stk overwrite the body afterward.
        let title = match title {
            Some(title) => title.to_owned(),
            None => git::commit_subject(branch)?,
        };
        let body = git::commit_body(branch)?;
        let description = if body.trim().is_empty() {
            &title
        } else {
            &body
        };
        // Drafts are a `WIP:` title prefix, not a flag - and a title that
        // already carries one is a draft, so prefixing again would only make
        // the title unreadable and leave a marker `mark_ready` has to strip.
        let title = if draft && !is_draft_title(&title) {
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

    fn update_review_title(&self, review: &ReviewRequest, title: &str) -> Result<String> {
        // Drafts are a `WIP:` title prefix here, so keep it on a retitle -
        // dropping it would silently mark the PR ready.
        let title = if review.draft && !is_draft_title(title) {
            format!("{DRAFT_PREFIX}{title}")
        } else {
            title.to_owned()
        };
        command_output(
            "tea",
            &["pr", "edit", review.id_value(), "--title", title.as_str()],
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

    fn conversation(&self, review: &ReviewRequest) -> Result<Conversation> {
        let slug = repo_slug()?;
        let number = review.id_value();
        let api = |endpoint: String| -> Result<Value> {
            serde_json::from_str(&command_output("tea", &["api", &endpoint])?)
                .context("failed to parse gitea JSON")
        };
        let head = api_pull(number)?
            .pointer("/head/sha")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let reviews = api(format!(
            "repos/{slug}/pulls/{number}/reviews?limit={CONVERSATION_PAGE}"
        ))?;
        let mut review_comments = Vec::new();
        for review in reviews.as_array().into_iter().flatten() {
            if review.get("comments_count").and_then(Value::as_u64) == Some(0) {
                continue;
            }
            let id = review.get("id").and_then(Value::as_u64).unwrap_or_default();
            review_comments.push(api(format!(
                "repos/{slug}/pulls/{number}/reviews/{id}/comments"
            ))?);
        }
        let issue_comments = api(format!(
            "repos/{slug}/issues/{number}/comments?limit={CONVERSATION_PAGE}"
        ))?;
        Ok(gitea_conversation(
            &reviews,
            &review_comments,
            &issue_comments,
            &head,
        ))
    }

    fn submit_review(&self, review: &ReviewRequest, submission: &Submission) -> Result<String> {
        let endpoint = format!("repos/{}/pulls/{}/reviews", repo_slug()?, review.id_value());
        let data = gitea_review_request(submission).to_string();
        let value: Value = serde_json::from_str(&command_output(
            "tea",
            &["api", "--method", "POST", &endpoint, "--data", &data],
        )?)
        .context("failed to parse gitea review JSON")?;
        Ok(optional_string(&value, "html_url"))
    }

    fn reply_to_thread(
        &self,
        review: &ReviewRequest,
        thread: &Thread,
        commit: &str,
        body: &str,
    ) -> Result<()> {
        // Gitea has no reply call: a comment on the same line of the same
        // side joins the conversation there, which needs a line on `commit`.
        let Some(anchor) = thread.anchor.filter(|_| !thread.outdated) else {
            bail!(
                "Gitea can only reply to a conversation on a line of the current diff; reply on {} instead",
                review.url
            );
        };
        let submission = Submission {
            event: ReviewEvent::Comment,
            body: String::new(),
            comments: vec![super::DraftComment {
                path: thread.path.clone(),
                old_path: thread.path.clone(),
                anchor,
                paired_line: None,
                body: body.to_owned(),
            }],
            commit: commit.to_owned(),
        };
        self.submit_review(review, &submission)?;
        Ok(())
    }

    fn issue_state(&self, number: u64) -> Result<IssueState> {
        let endpoint = format!("repos/{}/issues/{number}", repo_slug()?);
        parse_issue_state(&command_output("tea", &["api", &endpoint])?)
    }

    fn update_review_body(&self, review: &ReviewRequest, body: &str) -> Result<String> {
        command_output(
            "tea",
            &["pr", "edit", review.id_value(), "--description", body],
        )
    }

    fn admin_merge(&self) -> bool {
        true
    }

    fn merge_review(
        &self,
        review: &ReviewRequest,
        strategy: &str,
        mode: super::MergeMode,
    ) -> Result<String> {
        let style = match strategy {
            "rebase" => "rebase",
            "merge" => "merge",
            _ => "squash",
        };
        // tea has no way to force a merge, so ask the API, which does it for
        // an account allowed to merge past the branch's protection.
        if mode == super::MergeMode::Admin {
            let endpoint = format!("repos/{}/pulls/{}/merge", repo_slug()?, review.id_value());
            let data = serde_json::json!({ "Do": style, "force_merge": true }).to_string();
            return command_output(
                "tea",
                &["api", "--method", "POST", &endpoint, "--data", &data],
            );
        }
        // Gitea/tea has no scheduled ("merge when checks pass") merge, so `auto`
        // falls back to an immediate merge; a real block surfaces as an error.
        let args = vec!["pr", "merge", review.id_value(), "--style", style];
        // A merge right after a force-push (restack, merge --all) can race Gitea
        // recomputing mergeability and get rejected ("is it still open?"). Wait
        // for it to settle, and re-poll between retries rather than guess a delay.
        wait_until_mergeable(review.id_value());
        merge_with_resettle(
            || wait_until_mergeable(review.id_value()),
            || command_output("tea", &args),
        )
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
        Ok(list_pulls("open")?.0)
    }

    fn viewer(&self) -> Result<String> {
        let user: Value = serde_json::from_str(&command_output("tea", &["api", "user"])?)
            .context("failed to parse gitea user JSON")?;
        required_string(&user, &["login"])
    }

    fn inbox(&self, lists: &[InboxList]) -> Result<Vec<(InboxList, Listing)>> {
        let user: Value = serde_json::from_str(&inbox_read("user")?)
            .context("failed to parse gitea user JSON")?;
        let me = required_string(&user, &["login"])?;
        let teams: Vec<u64> = if lists.contains(&InboxList::Team) {
            let value: Value = serde_json::from_str(&inbox_read("user/teams")?)
                .context("failed to parse gitea teams JSON")?;
            value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|team| team["id"].as_u64())
                .collect()
        } else {
            Vec::new()
        };
        let (pulls, complete) = open_pull_values()?;
        let slug = repo_slug()?;
        // The pulls in `list`, up to the fetch limit, each with its reviews.
        let reviewed = |list: InboxList| -> Result<(Vec<(&Value, Value)>, bool)> {
            let mut chosen: Vec<&Value> = pulls
                .iter()
                .filter(|pull| in_gitea_list(pull, list, &me, &teams))
                .collect();
            let more = chosen.len() > FETCH_LIMIT || !complete;
            chosen.truncate(FETCH_LIMIT);
            let reviews = gitea_reviews_of(&slug, &chosen)?;
            Ok((chosen.into_iter().zip(reviews).collect(), more))
        };
        // Returned and mine hold the same pulls, told apart by their reviews,
        // so those are read once for both.
        let authored = (lists.contains(&InboxList::Mine) || lists.contains(&InboxList::Returned))
            .then(|| reviewed(InboxList::Mine).map_err(|error| format!("{error:#}")));
        let listing = |list: InboxList| -> Result<Listing> {
            let asked;
            let (found, more): (&[(&Value, Value)], bool) = match (list, &authored) {
                (InboxList::Mine | InboxList::Returned, Some(Ok((found, more)))) => {
                    (found.as_slice(), *more)
                }
                (InboxList::Mine | InboxList::Returned, Some(Err(why))) => bail!("{why}"),
                _ => {
                    asked = reviewed(list)?;
                    (asked.0.as_slice(), asked.1)
                }
            };
            let kept: Vec<&(&Value, Value)> = found
                .iter()
                .filter(|(_, reviews)| match list {
                    InboxList::Returned => latest_decision(reviews) == Decision::ChangesRequested,
                    InboxList::Mine => latest_decision(reviews) != Decision::ChangesRequested,
                    InboxList::Incoming | InboxList::Team => true,
                })
                .collect();
            let statuses = gitea_statuses(&slug, kept.iter().map(|(pull, _)| *pull))?;
            let sizes = gitea_sizes(kept.iter().map(|(pull, _)| *pull))?;
            let comments = gitea_comments(&slug, kept.iter().map(|(pull, _)| *pull))?;
            let entries = kept
                .into_iter()
                .zip(statuses.into_iter().zip(sizes).zip(comments))
                .map(|((pull, reviews), ((checks, size), comments))| {
                    let mut entry = gitea_inbox_entry(pull, reviews);
                    entry.checks = checks;
                    entry.activity = gitea_activity(&me, reviews, &comments);
                    if let Some((additions, deletions)) = size {
                        entry.additions = additions;
                        entry.deletions = deletions;
                    }
                    entry
                })
                .collect();
            Ok(Listing::Found { entries, more })
        };
        Ok(lists
            .iter()
            .map(|&list| {
                let found =
                    listing(list).unwrap_or_else(|error| Listing::Failed(format!("{error:#}")));
                (list, found)
            })
            .collect())
    }

    fn all_open_reviews(&self) -> Result<(Vec<ReviewRequest>, bool)> {
        list_pulls("open")
    }

    fn check_status(&self, review: &ReviewRequest) -> Result<CheckStatus> {
        let head_sha = api_pull(review.id_value())?
            .get("head")
            .and_then(|head| head.get("sha"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if head_sha.is_empty() {
            return Ok(CheckStatus::None);
        }
        let endpoint = format!("repos/{}/commits/{head_sha}/status", repo_slug()?);
        let value: Value = serde_json::from_str(&command_output("tea", &["api", &endpoint])?)
            .context("failed to parse gitea status JSON")?;
        let total = value
            .get("total_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Ok(map_gitea_state(&optional_string(&value, "state"), total))
    }

    fn review_summary(&self, review: &ReviewRequest) -> Result<ReviewSummary> {
        let endpoint = format!("repos/{}/pulls/{}/reviews", repo_slug()?, review.id_value());
        let value: Value = serde_json::from_str(&command_output("tea", &["api", &endpoint])?)
            .context("failed to parse gitea reviews JSON")?;
        Ok(count_gitea_reviews(&value))
    }

    fn mark_ready(&self, review: &ReviewRequest) -> Result<String> {
        // Clearing the draft state means dropping the `WIP:` title prefix - in
        // whatever case it was written, since Gitea honors it either way.
        let title = without_draft_prefix(&review.title);
        command_output("tea", &["pr", "edit", review.id_value(), "--title", title])
    }

    fn request_reviewers(&self, review: &ReviewRequest, reviewers: &[String]) -> Result<String> {
        // tea has no reviewer flag, so request through the API passthrough
        // (which still uses tea's stored auth). Gitea keeps user and team
        // reviewers in separate arrays; an `org/team` entry is a team, its slug
        // the part after the last slash.
        let mut users = Vec::new();
        let mut teams = Vec::new();
        for name in reviewers {
            match name.rsplit_once('/') {
                Some((_, team)) => teams.push(team),
                None => users.push(name.as_str()),
            }
        }
        let endpoint = format!(
            "repos/{}/pulls/{}/requested_reviewers",
            repo_slug()?,
            review.id_value()
        );
        let data = serde_json::json!({
            "reviewers": users,
            "team_reviewers": teams,
        })
        .to_string();
        command_output(
            "tea",
            &["api", "--method", "POST", &endpoint, "--data", &data],
        )
    }

    fn close_review(&self, review: &ReviewRequest, _delete_branch: bool) -> Result<String> {
        // tea's close has no delete-source-branch flag, so the remote branch may
        // linger; closing is what retires the superseded review.
        command_output("tea", &["pr", "close", review.id_value()])
    }

    fn open_review(&self, review: &ReviewRequest) -> Result<String> {
        command_output("tea", &["open", &format!("pulls/{}", review.id_value())])
    }

    fn comment_on_review(&self, review: &ReviewRequest, body: &str) -> Result<String> {
        // Pull requests share the issue index space, so tea's issue comment
        // reaches them. `--` keeps a flag-shaped body such as `--- done` from
        // reading as a flag. tea prints the comment back rendered rather than a
        // link to it, so there is nothing worth echoing.
        command_output("tea", &["comment", review.id_value(), "--", body])?;
        Ok(String::new())
    }
}

/// The length of the draft marker a title opens with, if any. Gitea compares
/// its prefixes case-insensitively, so `WIP:`, `wip:` and `Wip:` all mark a
/// draft. ASCII-only lowercasing keeps the length usable as a byte offset.
fn draft_marker_len(title: &str) -> Option<usize> {
    let lowered = title.trim_start().to_ascii_lowercase();
    DRAFT_MARKERS
        .iter()
        .find(|marker| lowered.starts_with(*marker))
        .map(|marker| marker.len())
}

/// Whether the title already marks the PR as a draft.
fn is_draft_title(title: &str) -> bool {
    draft_marker_len(title).is_some()
}

/// The title without its leading draft marker, so readying a PR actually clears
/// the state Gitea keys on. Unmarked titles pass through untouched.
fn without_draft_prefix(title: &str) -> &str {
    let trimmed = title.trim_start();
    match draft_marker_len(trimmed) {
        Some(len) => trimmed[len..].trim_start(),
        None => title,
    }
}

/// Find the branch's review, preferring open, then merged, then (only when
/// `include_closed`) closed. Gitea's `pr list` has no head filter, so list and
/// match client-side.
fn find_review(branch: &str, include_closed: bool) -> Result<Option<ReviewRequest>> {
    let mut matches: Vec<ReviewRequest> = list_pulls("all")?
        .0
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

/// The body of a create-review request. Gitea places a comment by one line
/// number on one side, so a multi-line comment sits on its last line.
fn gitea_review_request(submission: &Submission) -> Value {
    serde_json::json!({
        "commit_id": submission.commit,
        "body": submission.body,
        "event": match submission.event {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVED",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        },
        "comments": submission.comments.iter().map(|comment| {
            let mut value = serde_json::json!({ "path": comment.path, "body": comment.body });
            let key = match comment.anchor.side {
                Side::New => "new_position",
                Side::Old => "old_position",
            };
            value[key] = comment.anchor.line.into();
            value
        }).collect::<Vec<_>>(),
    })
}

/// How many reviews or comments one conversation listing asks for; a full
/// page marks the conversation incomplete.
const CONVERSATION_PAGE: usize = 50;

/// The discussion from a pull's reviews, each review's comments, and its
/// issue comments. Gitea has no thread object: comments on the same line of
/// the same side of a file make one conversation, as its own pages show them.
fn gitea_conversation(
    reviews: &Value,
    review_comments: &[Value],
    issue_comments: &Value,
    head: &str,
) -> Conversation {
    let comment = |value: &Value| Comment {
        id: value.get("id").map(|id| id.to_string()).unwrap_or_default(),
        author: value
            .pointer("/user/login")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        body: optional_string(value, "body"),
        created_at: optional_string(value, "created_at"),
        url: optional_string(value, "html_url"),
    };
    let reviews = reviews.as_array().map(Vec::as_slice).unwrap_or_default();
    let issue_comments = issue_comments
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut conversation = Conversation {
        incomplete: reviews.len() >= CONVERSATION_PAGE || issue_comments.len() >= CONVERSATION_PAGE,
        comments: issue_comments.iter().map(comment).collect(),
        ..Conversation::default()
    };
    conversation.verdicts = reviews
        .iter()
        .filter_map(|review| {
            // A dismissed review keeps its original state.
            let state = match review.get("state").and_then(Value::as_str)? {
                _ if optional_bool(review, "dismissed") => VerdictState::Dismissed,
                "APPROVED" => VerdictState::Approved,
                "REQUEST_CHANGES" => VerdictState::ChangesRequested,
                "COMMENT" => VerdictState::Commented,
                _ => return None,
            };
            let body = optional_string(review, "body");
            Some(Verdict {
                author: review
                    .pointer("/user/login")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                state,
                body,
                submitted_at: optional_string(review, "submitted_at"),
                url: optional_string(review, "html_url"),
            })
        })
        .collect();

    for value in review_comments.iter().filter_map(Value::as_array).flatten() {
        let line = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|line| u32::try_from(line).ok())
                .filter(|&line| line > 0)
        };
        let anchor = match (line("position"), line("original_position")) {
            (Some(line), _) => Some(Anchor {
                side: Side::New,
                line,
                start_line: None,
            }),
            (None, Some(line)) => Some(Anchor {
                side: Side::Old,
                line,
                start_line: None,
            }),
            (None, None) => None,
        };
        let path = optional_string(value, "path");
        let existing = conversation
            .threads
            .iter_mut()
            .find(|thread| thread.path == path && thread.anchor == anchor);
        let comment = comment(value);
        match existing {
            Some(thread) => thread.comments.push(comment),
            None => conversation.threads.push(Thread {
                id: comment.id.clone(),
                path,
                anchor,
                outdated: optional_string(value, "commit_id") != head,
                resolved: Some(
                    value
                        .get("resolver")
                        .is_some_and(|resolver| !resolver.is_null()),
                ),
                diff_hunk: Some(optional_string(value, "diff_hunk"))
                    .filter(|hunk| !hunk.is_empty()),
                comments: vec![comment],
            }),
        }
    }
    conversation
}

/// One pull request's canonical API object.
fn api_pull(id_value: &str) -> Result<Value> {
    let endpoint = format!("repos/{}/pulls/{id_value}", repo_slug()?);
    serde_json::from_str(&command_output("tea", &["api", &endpoint])?)
        .context("failed to parse gitea pull JSON")
}

/// One of the inbox's reads, asked again if the server hiccups.
fn inbox_read(endpoint: &str) -> Result<String> {
    super::retry_server_errors(|| command_output("tea", &["api", endpoint]))
}

/// Gitea answers per pull, so the inbox's per-pull calls go this many at once.
const INBOX_CALLS_AT_ONCE: usize = 8;

/// Every open pull as Gitea gives it, most recently updated first, and whether
/// the walk reached the end.
fn open_pull_values() -> Result<(Vec<Value>, bool)> {
    const PAGE_SIZE: usize = 50;
    const MAX_PAGES: usize = 20;
    let slug = repo_slug()?;
    let mut pulls = Vec::new();
    for page in 1..=MAX_PAGES {
        let endpoint = format!(
            "repos/{slug}/pulls?state=open&sort=recentupdate&page={page}&limit={PAGE_SIZE}"
        );
        let batch: Value = serde_json::from_str(&inbox_read(&endpoint)?)
            .context("failed to parse gitea pulls JSON")?;
        let batch = batch.as_array().cloned().unwrap_or_default();
        let full_page = batch.len() == PAGE_SIZE;
        pulls.extend(batch);
        if !full_page {
            return Ok((pulls, true));
        }
    }
    Ok((pulls, false))
}

/// Whether `pull` belongs in `list` for `me`, a member of `teams`. Returned
/// and mine both hold `me`'s pulls; their reviews tell them apart.
fn in_gitea_list(pull: &Value, list: InboxList, me: &str, teams: &[u64]) -> bool {
    let author = pull["user"]["login"].as_str() == Some(me);
    let named = pull["requested_reviewers"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|reviewer| reviewer["login"].as_str() == Some(me));
    let teamed = pull["requested_reviewers_teams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|team| team["id"].as_u64())
        .any(|id| teams.contains(&id));
    match list {
        InboxList::Incoming => named && !author,
        InboxList::Team => teamed && !named && !author,
        InboxList::Mine | InboxList::Returned => author,
    }
}

/// Run `call` on each item, [`INBOX_CALLS_AT_ONCE`] at a time, in order.
fn in_batches<T: Sync, R: Send>(
    items: &[T],
    call: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    let mut results = Vec::with_capacity(items.len());
    for chunk in items.chunks(INBOX_CALLS_AT_ONCE) {
        let answers = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|item| scope.spawn(|| call(item)))
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
                .collect::<Vec<_>>()
        });
        for answer in answers {
            results.push(answer?);
        }
    }
    Ok(results)
}

fn gitea_reviews_of(slug: &str, pulls: &[&Value]) -> Result<Vec<Value>> {
    in_batches(pulls, |pull| {
        // Every page: the latest verdict is the last review, and the pages run
        // oldest first.
        const MAX_PAGES: usize = 10;
        let number = pull["number"].as_u64().unwrap_or(0);
        let mut reviews = Vec::new();
        for page in 1..=MAX_PAGES {
            let endpoint = format!(
                "repos/{slug}/pulls/{number}/reviews?limit={CONVERSATION_PAGE}&page={page}"
            );
            let batch: Value = serde_json::from_str(&inbox_read(&endpoint)?)
                .context("failed to parse gitea reviews JSON")?;
            let batch = batch.as_array().cloned().unwrap_or_default();
            let full = batch.len() == CONVERSATION_PAGE;
            reviews.extend(batch);
            if !full {
                break;
            }
        }
        Ok(Value::Array(reviews))
    })
}

/// Each pull's conversation comments, every page of them.
fn gitea_comments<'a>(slug: &str, pulls: impl Iterator<Item = &'a Value>) -> Result<Vec<Value>> {
    let numbers: Vec<u64> = pulls
        .map(|pull| pull["number"].as_u64().unwrap_or(0))
        .collect();
    in_batches(&numbers, |number| {
        const MAX_PAGES: usize = 10;
        let mut comments = Vec::new();
        for page in 1..=MAX_PAGES {
            let endpoint = format!(
                "repos/{slug}/issues/{number}/comments?limit={CONVERSATION_PAGE}&page={page}"
            );
            let batch: Value = serde_json::from_str(&inbox_read(&endpoint)?)
                .context("failed to parse gitea comments JSON")?;
            let batch = batch.as_array().cloned().unwrap_or_default();
            let full = batch.len() == CONVERSATION_PAGE;
            comments.extend(batch);
            if !full {
                break;
            }
        }
        Ok(Value::Array(comments))
    })
}

/// Reviews and comments by who left them. Gitea gives no cheap way to see
/// when a pull was last pushed, so pushes are not counted.
fn gitea_activity(me: &str, reviews: &Value, comments: &Value) -> Activity {
    // A bot's reviews and comments are not news.
    let user = |item: &Value| {
        item["user"]["login"]
            .as_str()
            .filter(|login| !super::is_bot(login))
            .map(str::to_owned)
    };
    let events: Vec<(String, String)> = reviews
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|review| Some((user(review)?, review["submitted_at"].as_str()?.to_owned())))
        .chain(
            comments
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|comment| {
                    Some((user(comment)?, comment["created_at"].as_str()?.to_owned()))
                }),
        )
        .collect();
    Activity::of(
        me,
        events
            .iter()
            .map(|(who, when)| (who.as_str(), when.as_str())),
    )
}

/// The lines added and deleted by each pull whose listing leaves them out, as
/// Gitea's does (Forgejo's includes them), from the pull itself.
fn gitea_sizes<'a>(pulls: impl Iterator<Item = &'a Value>) -> Result<Vec<Option<(u64, u64)>>> {
    let missing: Vec<Option<u64>> = pulls
        .map(|pull| {
            pull.get("additions")
                .is_none()
                .then(|| pull["number"].as_u64())
                .flatten()
        })
        .collect();
    in_batches(&missing, |number| {
        let Some(number) = number else {
            return Ok(None);
        };
        let pull = super::retry_server_errors(|| api_pull(&number.to_string()))?;
        Ok(Some((
            pull["additions"].as_u64().unwrap_or(0),
            pull["deletions"].as_u64().unwrap_or(0),
        )))
    })
}

fn gitea_statuses<'a>(
    slug: &str,
    pulls: impl Iterator<Item = &'a Value>,
) -> Result<Vec<CheckStatus>> {
    let shas: Vec<String> = pulls
        .map(|pull| pull["head"]["sha"].as_str().unwrap_or_default().to_owned())
        .collect();
    in_batches(&shas, |sha| {
        if sha.is_empty() {
            return Ok(CheckStatus::None);
        }
        let endpoint = format!("repos/{slug}/commits/{sha}/status");
        let value: Value = serde_json::from_str(&inbox_read(&endpoint)?)
            .context("failed to parse gitea status JSON")?;
        let total = value["total_count"].as_u64().unwrap_or(0);
        Ok(map_gitea_state(&optional_string(&value, "state"), total))
    })
}

/// Where a pull's reviewing stands, from each reviewer's latest approval or
/// request for changes; comments and dismissed reviews leave it as it was.
fn latest_decision(reviews: &Value) -> Decision {
    let mut latest: Vec<(&str, &str)> = Vec::new();
    for review in reviews.as_array().into_iter().flatten() {
        let state = review["state"].as_str().unwrap_or_default();
        if !matches!(state, "APPROVED" | "REQUEST_CHANGES") || review["dismissed"] == true {
            continue;
        }
        let reviewer = review["user"]["login"].as_str().unwrap_or_default();
        latest.retain(|(who, _)| *who != reviewer);
        latest.push((reviewer, state));
    }
    if latest.iter().any(|(_, state)| *state == "REQUEST_CHANGES") {
        Decision::ChangesRequested
    } else if latest.is_empty() {
        Decision::None
    } else {
        Decision::Approved
    }
}

fn gitea_inbox_entry(pull: &Value, reviews: &Value) -> InboxEntry {
    let text = |key: &str| pull[key].as_str().unwrap_or_default().to_owned();
    let number = pull["number"].as_u64().unwrap_or(0);
    let requested = pull["requested_reviewers"]
        .as_array()
        .is_some_and(|users| !users.is_empty())
        || pull["requested_reviewers_teams"]
            .as_array()
            .is_some_and(|teams| !teams.is_empty());
    let decision = match latest_decision(reviews) {
        Decision::None if requested => Decision::ReviewRequired,
        decision => decision,
    };
    InboxEntry {
        review: ReviewRequest {
            id: format!("#{number}"),
            branch: pull["head"]["ref"].as_str().unwrap_or_default().to_owned(),
            base: pull["base"]["ref"].as_str().unwrap_or_default().to_owned(),
            state: ReviewState::Open,
            url: text("html_url"),
            title: text("title"),
            draft: pull["draft"].as_bool().unwrap_or(false) || is_draft_title(&text("title")),
        },
        author: pull["user"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        created_at: text("created_at"),
        updated_at: text("updated_at"),
        additions: pull["additions"].as_u64().unwrap_or(0),
        deletions: pull["deletions"].as_u64().unwrap_or(0),
        checks: CheckStatus::None,
        decision,
        // Gitea has no count of open conversations short of reading them all.
        unresolved: None,
        activity: Activity::default(),
    }
}

/// Every PR in `state` (open|closed|all) as ReviewRequests, following
/// pagination. Uses the API passthrough rather than `tea pr list` (whose JSON
/// is a lossy projection: no `merged`, `draft`, or nested refs), and pages so a
/// branch's review can't fall off the first page on a busy repo.
/// The pulls in `state`, and whether that is all of them rather than as many
/// as the page bound allows.
fn list_pulls(state: &str) -> Result<(Vec<ReviewRequest>, bool)> {
    // Gitea caps page size at 50, so request exactly that and page past it.
    const PAGE_SIZE: usize = 50;
    // Bound the walk so a misbehaving server can't loop forever.
    const MAX_PAGES: usize = 50;
    let slug = repo_slug()?;
    let mut reviews = Vec::new();
    for page in 1..=MAX_PAGES {
        let endpoint = format!("repos/{slug}/pulls?state={state}&page={page}&limit={PAGE_SIZE}");
        let batch = all_reviews(
            &command_output("tea", &["api", &endpoint])?,
            gitea_review_from,
        )?;
        let full_page = batch.len() == PAGE_SIZE;
        reviews.extend(batch);
        if !full_page {
            return Ok((reviews, true));
        }
    }
    Ok((reviews, false))
}

/// How long to wait for Gitea to recompute mergeability after a push before a
/// merge (5 polls, 2s apart) - the same shape as the gitlab settle.
const MERGE_SETTLE_POLLS: u32 = 5;
const MERGE_SETTLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Poll until the PR reports mergeable, bounded by the settle budget. An absent
/// or false `mergeable` (still computing, or a genuine block) just exhausts the
/// budget; the merge attempt then surfaces the real reason.
fn wait_until_mergeable(id_value: &str) {
    for attempt in 0..MERGE_SETTLE_POLLS {
        if attempt > 0 {
            std::thread::sleep(MERGE_SETTLE_INTERVAL);
        }
        if api_pull(id_value)
            .ok()
            .and_then(|pull| pull.get("mergeable").and_then(Value::as_bool))
            == Some(true)
        {
            return;
        }
    }
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

/// Map a Gitea combined commit `state` to a check dot. No statuses (an empty
/// state or zero total) means no dot.
fn map_gitea_state(state: &str, total: u64) -> CheckStatus {
    if total == 0 || state.is_empty() {
        return CheckStatus::None;
    }
    match state {
        "success" => CheckStatus::Passing,
        "failure" | "error" => CheckStatus::Failing,
        // pending / warning: statuses exist and are still running.
        _ => CheckStatus::Pending,
    }
}

/// Tally a Gitea PR reviews array by state.
fn count_gitea_reviews(value: &Value) -> ReviewSummary {
    let mut summary = ReviewSummary::default();
    let Some(items) = value.as_array() else {
        return summary;
    };
    for item in items {
        match item.get("state").and_then(Value::as_str) {
            Some("APPROVED") => summary.approvals += 1,
            Some("REQUEST_CHANGES") => summary.changes_requested += 1,
            Some("COMMENT") => summary.comments += 1,
            _ => {}
        }
    }
    summary
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
    fn gitea_activity_reads_reviews_and_comments_but_not_bots() {
        let reviews = serde_json::json!([
            {"user": {"login": "me"}, "submitted_at": "2026-10-10T00:00:00+02:00"},
            {"user": {"login": "ann"}, "submitted_at": "2026-10-09T00:00:00Z"},
        ]);
        let comments = serde_json::json!([
            {"user": {"login": "ann"}, "created_at": "2026-10-11T00:00:00Z"},
            {"user": {"login": "renovate-bot"}, "created_at": "2026-10-12T00:00:00Z"},
        ]);
        let activity = gitea_activity("me", &reviews, &comments);
        assert_eq!(activity.yours.as_deref(), Some("2026-10-10T00:00:00+02:00"));
        assert_eq!(activity.others.as_deref(), Some("2026-10-11T00:00:00Z"));
    }

    #[test]
    fn inbox_lists_sort_pulls_by_who_is_asked() {
        let pull = |author: &str, users: &[&str], teams: &[u64]| {
            serde_json::json!({
                "user": {"login": author},
                "requested_reviewers": users.iter().map(|login| serde_json::json!({"login": login})).collect::<Vec<_>>(),
                "requested_reviewers_teams": teams.iter().map(|id| serde_json::json!({"id": id})).collect::<Vec<_>>(),
            })
        };
        let in_list = |pull: &Value, list| in_gitea_list(pull, list, "me", &[7]);
        let named = pull("ann", &["me"], &[7]);
        assert!(in_list(&named, InboxList::Incoming));
        assert!(
            !in_list(&named, InboxList::Team),
            "named requests are incoming only"
        );
        let teamed = pull("ann", &[], &[7]);
        assert!(in_list(&teamed, InboxList::Team) && !in_list(&teamed, InboxList::Incoming));
        assert!(!in_list(&pull("ann", &[], &[8]), InboxList::Team));
        let mine = pull("me", &["me"], &[]);
        assert!(in_list(&mine, InboxList::Mine) && !in_list(&mine, InboxList::Incoming));
    }

    #[test]
    fn a_reviewers_latest_say_decides() {
        let review = |user: &str, state: &str, dismissed: bool| serde_json::json!({"user": {"login": user}, "state": state, "dismissed": dismissed});
        let decide = |reviews: Vec<Value>| latest_decision(&Value::Array(reviews));
        assert_eq!(decide(vec![]), Decision::None);
        assert_eq!(
            decide(vec![
                review("ann", "REQUEST_CHANGES", false),
                review("ann", "APPROVED", false)
            ]),
            Decision::Approved
        );
        assert_eq!(
            decide(vec![
                review("ann", "APPROVED", false),
                review("bo", "REQUEST_CHANGES", false)
            ]),
            Decision::ChangesRequested
        );
        assert_eq!(
            decide(vec![
                review("bo", "REQUEST_CHANGES", true),
                review("ann", "COMMENT", false)
            ]),
            Decision::None
        );
    }

    #[test]
    fn gitea_review_request_places_comments_by_side() {
        let submission = Submission {
            event: ReviewEvent::Approve,
            body: String::new(),
            comments: vec![
                super::super::DraftComment {
                    path: "a.rs".into(),
                    old_path: "a.rs".into(),
                    anchor: Anchor {
                        side: Side::Old,
                        line: 2,
                        start_line: None,
                    },
                    paired_line: None,
                    body: "old".into(),
                },
                super::super::DraftComment {
                    path: "a.rs".into(),
                    old_path: "a.rs".into(),
                    anchor: Anchor {
                        side: Side::New,
                        line: 5,
                        start_line: Some(3),
                    },
                    paired_line: None,
                    body: "new".into(),
                },
            ],
            commit: "head".into(),
        };
        assert_eq!(
            gitea_review_request(&submission),
            serde_json::json!({
                "commit_id": "head", "body": "", "event": "APPROVED",
                "comments": [
                    {"path": "a.rs", "body": "old", "old_position": 2},
                    {"path": "a.rs", "body": "new", "new_position": 5},
                ],
            })
        );
    }

    #[test]
    fn gitea_conversation_groups_review_comments_by_line() {
        let reviews: Value = serde_json::from_str(
            r#"[{"id":1,"user":{"login":"bo"},"state":"REQUEST_CHANGES","body":"Fix","submitted_at":"t1","html_url":"u1","comments_count":2},
                {"id":2,"user":{"login":"ann"},"state":"COMMENT","body":"","submitted_at":"t2","html_url":"u2","comments_count":1},
                {"id":3,"user":{"login":"cy"},"state":"PENDING","body":"","comments_count":0},
                {"id":4,"user":{"login":"dee"},"state":"APPROVED","body":"Was fine","dismissed":true,"comments_count":0}]"#,
        )
        .unwrap();
        let comments: Vec<Value> = vec![
            serde_json::from_str(
                r#"[{"id":10,"path":"a.rs","position":4,"original_position":0,"commit_id":"HEAD","diff_hunk":"@@ x","body":"One","user":{"login":"bo"},"created_at":"t","html_url":"c1","resolver":null},
                    {"id":11,"path":"a.rs","position":0,"original_position":2,"commit_id":"OLD","diff_hunk":"","body":"Old side","user":{"login":"bo"},"created_at":"t","html_url":"c2","resolver":{"login":"ann"}}]"#,
            )
            .unwrap(),
            serde_json::from_str(
                r#"[{"id":12,"path":"a.rs","position":4,"original_position":0,"commit_id":"HEAD","diff_hunk":"@@ x","body":"Reply","user":{"login":"ann"},"created_at":"t","html_url":"c3","resolver":null}]"#,
            )
            .unwrap(),
        ];
        let issue_comments: Value =
            serde_json::from_str(r#"[{"id":20,"body":"General","user":{"login":"ann"},"created_at":"t","html_url":"i1"}]"#)
                .unwrap();
        let conversation = gitea_conversation(&reviews, &comments, &issue_comments, "HEAD");
        assert_eq!(conversation.comments[0].body, "General");
        let states: Vec<VerdictState> = conversation.verdicts.iter().map(|v| v.state).collect();
        assert_eq!(
            states,
            [
                VerdictState::ChangesRequested,
                VerdictState::Commented,
                VerdictState::Dismissed
            ]
        );
        let [new_side, old_side] = &conversation.threads[..] else {
            panic!("two threads");
        };
        assert_eq!(new_side.comments.len(), 2, "same line, same side");
        assert_eq!(
            new_side.anchor.map(|a| (a.side, a.line)),
            Some((Side::New, 4))
        );
        assert!(!new_side.outdated);
        assert_eq!(
            old_side.anchor.map(|a| (a.side, a.line)),
            Some((Side::Old, 2))
        );
        assert!(old_side.outdated);
        assert_eq!(old_side.resolved, Some(true));
    }

    #[test]
    fn draft_titles_are_recognized_whatever_their_case() {
        // Gitea matches its WIP prefixes case-insensitively, so every spelling
        // here is already a draft and must not collect a second marker.
        for title in [
            "WIP: a", "wip: a", "Wip: a", "  wip: a", "[WIP] a", "[wip] a",
        ] {
            assert!(is_draft_title(title), "{title} should read as a draft");
        }
        for title in ["a", "wipe out the cache", "fix: wip: a"] {
            assert!(!is_draft_title(title), "{title} should not read as a draft");
        }
    }

    #[test]
    fn without_draft_prefix_strips_one_marker_in_any_case() {
        assert_eq!(without_draft_prefix("WIP: a"), "a");
        assert_eq!(without_draft_prefix("wip:a"), "a");
        assert_eq!(without_draft_prefix("[wip] a"), "a");
        // Only the leading marker goes; the rest of the title is the user's.
        assert_eq!(without_draft_prefix("WIP: wip: a"), "wip: a");
        assert_eq!(without_draft_prefix("a"), "a");
    }

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

    #[test]
    fn map_gitea_state_maps_combined_status() {
        assert_eq!(map_gitea_state("success", 2), CheckStatus::Passing);
        assert_eq!(map_gitea_state("failure", 1), CheckStatus::Failing);
        assert_eq!(map_gitea_state("error", 1), CheckStatus::Failing);
        assert_eq!(map_gitea_state("pending", 1), CheckStatus::Pending);
        // No statuses at all -> no dot.
        assert_eq!(map_gitea_state("", 0), CheckStatus::None);
        assert_eq!(map_gitea_state("success", 0), CheckStatus::None);
    }

    #[test]
    fn count_gitea_reviews_tallies_by_state() {
        let reviews = serde_json::json!([
            {"state": "APPROVED"},
            {"state": "REQUEST_CHANGES"},
            {"state": "COMMENT"},
            {"state": "COMMENT"},
            {"state": "PENDING"},
        ]);
        assert_eq!(
            count_gitea_reviews(&reviews),
            ReviewSummary {
                approvals: 1,
                comments: 2,
                changes_requested: 1,
            }
        );
        assert_eq!(
            count_gitea_reviews(&serde_json::json!({})),
            ReviewSummary::default()
        );
    }
}
