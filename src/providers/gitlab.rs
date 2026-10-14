use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};

use crate::git;

use super::json::{
    all_reviews, first_review, optional_bool, optional_string, parse_body_field, parse_issue_state,
    parse_state, required_string,
};
use super::{
    Activity, Anchor, CheckStatus, Comment, Conversation, Decision, FETCH_LIMIT, InboxEntry,
    InboxList, IssueState, Listing, MergeBlocker, ReviewEvent, ReviewProvider, ReviewRequest,
    ReviewState, ReviewSummary, Side, Submission, Thread, Verdict, VerdictState, WaitOutcome,
    command_output, command_output_with_input, merge_with_resettle, merge_with_retry,
};

/// One draft note per comment, positioned on the MR's current diff, then the
/// summary as a note of its own. An unchanged line takes both its numbers.
fn gitlab_draft_notes(submission: &Submission, refs: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut notes: Vec<serde_json::Value> = submission
        .comments
        .iter()
        .map(|comment| {
            let mut position = serde_json::json!({
                "position_type": "text",
                "base_sha": optional_string(refs, "base_sha"),
                "start_sha": optional_string(refs, "start_sha"),
                "head_sha": optional_string(refs, "head_sha"),
                "old_path": if comment.old_path.is_empty() { &comment.path } else { &comment.old_path },
                "new_path": comment.path,
            });
            let (own, other) = match comment.anchor.side {
                Side::New => ("new_line", "old_line"),
                Side::Old => ("old_line", "new_line"),
            };
            position[own] = comment.anchor.line.into();
            if let Some(paired) = comment.paired_line {
                position[other] = paired.into();
            }
            serde_json::json!({ "note": comment.body, "position": position })
        })
        .collect();
    if !submission.body.trim().is_empty() {
        notes.push(serde_json::json!({ "note": submission.body }));
    }
    notes
}

/// The discussion from an MR's discussions and approvals listings. A thread
/// left on another head than `head` is outdated: GitLab keeps its position on
/// the version it was written against. System notes (pushes, label changes)
/// are not discussion and are left out.
fn parse_gitlab_conversation(
    discussions: &str,
    approvals: Option<&str>,
    head: &str,
    web_url: &str,
) -> Result<Conversation> {
    let discussions: serde_json::Value =
        serde_json::from_str(discussions).context("failed to parse glab discussions JSON")?;
    let comment = |note: &serde_json::Value| {
        let id = note
            .get("id")
            .map(|id| id.to_string().trim_matches('"').to_owned())
            .unwrap_or_default();
        Comment {
            url: format!("{web_url}#note_{id}"),
            id,
            author: note
                .pointer("/author/username")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            body: optional_string(note, "body"),
            created_at: optional_string(note, "created_at"),
        }
    };
    let mut conversation = Conversation::default();
    for discussion in discussions.as_array().into_iter().flatten() {
        let notes: Vec<&serde_json::Value> = discussion
            .get("notes")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter(|note| !optional_bool(note, "system"))
            .collect();
        let Some(first) = notes.first() else {
            continue;
        };
        let Some(position) = first.get("position").filter(|position| !position.is_null()) else {
            conversation
                .comments
                .extend(notes.iter().map(|note| comment(note)));
            continue;
        };
        let line = |key: &str| {
            position
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|line| u32::try_from(line).ok())
        };
        let anchor = match (line("new_line"), line("old_line")) {
            _ if optional_string(position, "position_type") == "file" => None,
            (Some(line), _) => Some(Anchor {
                side: Side::New,
                line,
                start_line: position
                    .pointer("/line_range/start/new_line")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|start| u32::try_from(start).ok())
                    .filter(|&start| start != line),
            }),
            (None, Some(line)) => Some(Anchor {
                side: Side::Old,
                line,
                start_line: position
                    .pointer("/line_range/start/old_line")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|start| u32::try_from(start).ok())
                    .filter(|&start| start != line),
            }),
            (None, None) => None,
        };
        let path = match optional_string(position, "new_path") {
            path if path.is_empty() => optional_string(position, "old_path"),
            path => path,
        };
        conversation.threads.push(Thread {
            id: optional_string(discussion, "id"),
            path,
            anchor,
            outdated: optional_string(position, "head_sha") != head,
            resolved: optional_bool(first, "resolvable").then(|| optional_bool(first, "resolved")),
            diff_hunk: None,
            comments: notes.iter().map(|note| comment(note)).collect(),
        });
    }
    if let Some(approvals) = approvals
        && let Ok(approvals) = serde_json::from_str::<serde_json::Value>(approvals)
    {
        conversation.verdicts.extend(
            approvals
                .get("approved_by")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .map(|approval| Verdict {
                    author: approval
                        .pointer("/user/username")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    state: VerdictState::Approved,
                    body: String::new(),
                    submitted_at: String::new(),
                    url: web_url.to_owned(),
                }),
        );
    }
    Ok(conversation)
}

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

    fn review_by_number(&self, number: u64) -> Result<ReviewRequest> {
        let output = command_output(
            "glab",
            &["mr", "view", &number.to_string(), "--output", "json"],
        )?;
        parse_gitlab_review(&output)?.with_context(|| format!("no GitLab merge request !{number}"))
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
        // glab spells a draft as the `Draft: ` title prefix, so a title that
        // already carries one is a draft already - asking for the flag too
        // would stack a second prefix that `--ready` cannot clear in one go.
        if draft && !is_draft_title(&title) {
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

    fn review_head(&self, review: &ReviewRequest) -> Result<Option<String>> {
        let mr = command_output(
            "glab",
            &["mr", "view", review.id_value(), "--output", "json"],
        )?;
        let head = parse_body_field(&mr, "sha")?;
        Ok((!head.is_empty()).then_some(head))
    }

    fn conversation(&self, review: &ReviewRequest) -> Result<Conversation> {
        let iid = review.id_value();
        let mr = command_output("glab", &["mr", "view", iid, "--output", "json"])?;
        let head = parse_body_field(&mr, "sha")?;
        // `--method GET` keeps it a read; `--paginate` joins every page into
        // one array.
        let discussions = command_output(
            "glab",
            &[
                "api",
                "--method",
                "GET",
                "--paginate",
                &format!("projects/:id/merge_requests/{iid}/discussions?per_page=100"),
            ],
        )?;
        // Approvals live on a separate endpoint that not every plan serves.
        let approvals = command_output(
            "glab",
            &[
                "api",
                &format!("projects/:id/merge_requests/{iid}/approvals"),
            ],
        )
        .ok();
        parse_gitlab_conversation(&discussions, approvals.as_deref(), &head, &review.url)
    }

    fn submit_review(&self, review: &ReviewRequest, submission: &Submission) -> Result<String> {
        if submission.event == ReviewEvent::RequestChanges {
            bail!("GitLab reviews here can comment or approve, not request changes");
        }
        let iid = review.id_value();
        let mr: serde_json::Value = serde_json::from_str(&command_output(
            "glab",
            &["mr", "view", iid, "--output", "json"],
        )?)
        .context("failed to parse glab MR JSON")?;
        let refs = mr
            .get("diff_refs")
            .context("glab MR JSON has no diff_refs")?;
        let head = optional_string(refs, "head_sha");
        if head != submission.commit {
            bail!(
                "{} has new commits since these comments were written",
                review.id
            );
        }
        let post = |path: String, body: &serde_json::Value| {
            command_output_with_input(
                "glab",
                &[
                    "api",
                    "--method",
                    "POST",
                    "-H",
                    "Content-Type: application/json",
                    &path,
                    "--input",
                    "-",
                ],
                &body.to_string(),
            )
        };
        // Draft notes publish together, notifying once, as a review does
        // elsewhere. Publishing takes every draft of the user's on the MR,
        // so any left from GitLab's own pages must be dealt with first.
        let drafts = format!("projects/:id/merge_requests/{iid}/draft_notes");
        let existing: serde_json::Value = serde_json::from_str(&command_output(
            "glab",
            &["api", "--method", "GET", &drafts],
        )?)
        .context("failed to parse glab draft notes JSON")?;
        if existing.as_array().is_some_and(|notes| !notes.is_empty()) {
            bail!(
                "you have unpublished draft comments on {} in GitLab, which publishing this review would send too; publish or delete them there first",
                review.id
            );
        }
        let mut created: Vec<String> = Vec::new();
        let discard = |created: &[String]| {
            for id in created {
                let _ = command_output(
                    "glab",
                    &["api", "--method", "DELETE", &format!("{drafts}/{id}")],
                );
            }
        };
        for note in gitlab_draft_notes(submission, refs) {
            let id = post(drafts.clone(), &note).and_then(|output| {
                serde_json::from_str::<serde_json::Value>(&output)
                    .ok()
                    .and_then(|note| note.get("id").and_then(serde_json::Value::as_u64))
                    .map(|id| id.to_string())
                    .context("GitLab created a draft note without saying its id")
            });
            match id {
                Ok(id) => created.push(id),
                Err(error) => {
                    discard(&created);
                    return Err(error);
                }
            }
        }
        // Approving before publishing means a refused approval leaves
        // nothing posted to retry over.
        let approving = submission.event == ReviewEvent::Approve;
        if approving
            && let Err(error) = post(
                format!("projects/:id/merge_requests/{iid}/approve"),
                &serde_json::json!({ "sha": head }),
            )
        {
            discard(&created);
            return Err(error);
        }
        if !created.is_empty()
            && let Err(error) = post(format!("{drafts}/bulk_publish"), &serde_json::json!({}))
        {
            discard(&created);
            return Err(if approving {
                error.context(format!(
                    "{} was approved, but its comments were not posted",
                    review.id
                ))
            } else {
                error
            });
        }
        Ok(review.url.clone())
    }

    fn review_events(&self) -> &'static [ReviewEvent] {
        &[ReviewEvent::Comment, ReviewEvent::Approve]
    }

    fn reply_to_thread(
        &self,
        review: &ReviewRequest,
        thread: &Thread,
        _commit: &str,
        body: &str,
    ) -> Result<()> {
        command_output_with_input(
            "glab",
            &[
                "api",
                "--method",
                "POST",
                "-H",
                "Content-Type: application/json",
                &format!(
                    "projects/:id/merge_requests/{}/discussions/{}/notes",
                    review.id_value(),
                    thread.id
                ),
                "--input",
                "-",
            ],
            &serde_json::json!({ "body": body }).to_string(),
        )?;
        Ok(())
    }

    fn resolves_threads(&self) -> bool {
        true
    }

    fn set_resolved(&self, review: &ReviewRequest, thread: &Thread, resolved: bool) -> Result<()> {
        command_output(
            "glab",
            &[
                "api",
                "--method",
                "PUT",
                &format!(
                    "projects/:id/merge_requests/{}/discussions/{}?resolved={resolved}",
                    review.id_value(),
                    thread.id
                ),
            ],
        )?;
        Ok(())
    }

    fn issue_state(&self, number: u64) -> Result<IssueState> {
        // Merge requests are numbered apart from issues, so `#N` is never one.
        parse_issue_state(&command_output(
            "glab",
            &["api", &format!("projects/:id/issues/{number}")],
        )?)
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
                Some("success") | Some("skipped") => return Ok(WaitOutcome::Passed),
                Some("failed") => return Ok(WaitOutcome::Failed),
                // The same two states the dot calls inconclusive
                // (`map_pipeline_status`): stopped without a verdict, and
                // GitLab holds the merge for both. Merging past them would
                // contradict the dot the user just read.
                Some("canceled") | Some("manual") => return Ok(WaitOutcome::Inconclusive),
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

    fn viewer(&self) -> Result<String> {
        let user: serde_json::Value =
            serde_json::from_str(&command_output("glab", &["api", "user"])?)
                .context("failed to parse glab user JSON")?;
        required_string(&user, &["username"])
    }

    fn inbox(&self, lists: &[InboxList]) -> Result<Vec<(InboxList, Listing)>> {
        let user: serde_json::Value = serde_json::from_str(&super::retry_server_errors(|| {
            command_output("glab", &["api", "user"])
        })?)
        .context("failed to parse glab user JSON")?;
        let me = required_string(&user, &["username"])?;
        let project: serde_json::Value = serde_json::from_str(&super::retry_server_errors(|| {
            command_output("glab", &["api", "projects/:id"])
        })?)
        .context("failed to parse glab project JSON")?;
        let path = required_string(&project, &["path_with_namespace"])?;
        let wants = |list| lists.contains(&list);
        let mut searches = String::new();
        if wants(InboxList::Incoming) {
            searches.push_str(&format!(
                "requested: mergeRequests(state: opened, reviewerUsername: $me, first: {FETCH_LIMIT}, sort: UPDATED_DESC) {{ count nodes {{ ...inboxReview }} }} "
            ));
        }
        if wants(InboxList::Mine) || wants(InboxList::Returned) {
            searches.push_str(&format!(
                "authored: mergeRequests(state: opened, authorUsername: $me, first: {FETCH_LIMIT}, sort: UPDATED_DESC) {{ count nodes {{ ...inboxReview }} }} "
            ));
        }
        let output = if searches.is_empty() {
            r#"{"data":{"project":{}}}"#.to_owned()
        } else {
            let query = format!(
                "query($path: ID!, $me: String!) {{ project(fullPath: $path) {{ {searches}}} }} {GITLAB_INBOX_FRAGMENT}"
            );
            super::retry_server_errors(|| {
                command_output(
                    "glab",
                    &[
                        "api",
                        "graphql",
                        "-f",
                        &format!("query={query}"),
                        "-f",
                        &format!("path={path}"),
                        "-f",
                        &format!("me={me}"),
                    ],
                )
            })?
        };
        parse_gitlab_inbox(&output, lists, &me)
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

    fn all_open_reviews(&self) -> Result<(Vec<ReviewRequest>, bool)> {
        const PER_PAGE: usize = 100;
        // Bound the walk so a misbehaving server can't loop forever.
        const MAX_PAGES: usize = 20;
        let per_page = PER_PAGE.to_string();
        let mut reviews = Vec::new();
        for page in 1..=MAX_PAGES {
            let page = page.to_string();
            let batch = parse_gitlab_reviews(&command_output(
                "glab",
                &[
                    "mr",
                    "list",
                    "--output",
                    "json",
                    "--per-page",
                    &per_page,
                    "--page",
                    &page,
                ],
            )?)?;
            let full_page = batch.len() == PER_PAGE;
            reviews.extend(batch);
            if !full_page {
                return Ok((reviews, true));
            }
        }
        Ok((reviews, false))
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

    fn comment_on_review(&self, review: &ReviewRequest, body: &str) -> Result<String> {
        command_output(
            "glab",
            &["mr", "note", review.id_value(), "--message", body],
        )
    }

    fn enqueued_branches(&self, branches: &[String]) -> Result<BTreeSet<String>> {
        Ok(gitlab_enqueued_branches(branches))
    }
}

/// The fields each inbox merge request is asked for.
const GITLAB_INBOX_FRAGMENT: &str = "fragment inboxReview on MergeRequest { iid title webUrl draft sourceBranch targetBranch createdAt updatedAt author { username } diffStatsSummary { additions deletions } headPipeline { status } approved resolvableDiscussionsCount resolvedDiscussionsCount reviewers { nodes { username mergeRequestInteraction { reviewState } } } notes(last: 30) { nodes { author { username bot } createdAt } } }";

/// Review states that mean the reviewer has had their say, until the review
/// is requested of them again.
const REVIEWED_STATES: [&str; 3] = ["REVIEWED", "REQUESTED_CHANGES", "APPROVED"];

/// The inbox lists from the merge requests requested of `me` and by `me`. A
/// merge request leaves `incoming` once `me` has reviewed it, as on GitHub,
/// and one of `me`'s with changes requested is `returned`, not `mine`.
fn parse_gitlab_inbox(
    json: &str,
    lists: &[InboxList],
    me: &str,
) -> Result<Vec<(InboxList, Listing)>> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("failed to parse glab inbox JSON")?;
    let errors: Vec<&str> = value["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|error| error["message"].as_str())
        .collect();
    if !errors.is_empty() {
        bail!(
            "GitLab could not answer for the inbox: {}",
            errors.join("; ")
        );
    }
    let project = &value["data"]["project"];
    if project.is_null() {
        bail!("GitLab did not find this project for the inbox");
    }
    let nodes = |alias: &str| {
        project[alias]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    let more = |alias: &str, nodes: &[serde_json::Value]| {
        project[alias]["count"].as_u64().unwrap_or(0) > nodes.len() as u64
    };
    let state_of = |node: &serde_json::Value, user: &str| {
        node["reviewers"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|reviewer| user.is_empty() || reviewer["username"] == user)
            .map(|reviewer| {
                reviewer["mergeRequestInteraction"]["reviewState"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };
    let requested = nodes("requested");
    let authored = nodes("authored");
    lists
        .iter()
        .map(|&list| {
            let listing = match list {
                InboxList::Team => {
                    Listing::Unsupported("GitLab has no team review requests".to_owned())
                }
                InboxList::Incoming => {
                    let entries = requested
                        .iter()
                        .filter(|node| node["author"]["username"] != me)
                        .filter(|node| {
                            !state_of(node, me)
                                .iter()
                                .any(|state| REVIEWED_STATES.contains(&state.as_str()))
                        })
                        .map(|node| gitlab_inbox_entry(node, me))
                        .collect::<Result<Vec<_>>>()?;
                    Listing::Found {
                        more: more("requested", &requested),
                        entries,
                    }
                }
                InboxList::Mine | InboxList::Returned => {
                    let returned = list == InboxList::Returned;
                    let entries = authored
                        .iter()
                        .filter(|node| {
                            state_of(node, "")
                                .iter()
                                .any(|state| state == "REQUESTED_CHANGES")
                                == returned
                        })
                        .map(|node| gitlab_inbox_entry(node, me))
                        .collect::<Result<Vec<_>>>()?;
                    Listing::Found {
                        more: more("authored", &authored),
                        entries,
                    }
                }
            };
            Ok((list, listing))
        })
        .collect()
}

fn gitlab_inbox_entry(node: &serde_json::Value, me: &str) -> Result<InboxEntry> {
    let iid = node["iid"]
        .as_str()
        .context("an inbox merge request has no iid")?;
    let text = |key: &str| node[key].as_str().unwrap_or_default().to_owned();
    let states: Vec<&str> = node["reviewers"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reviewer| reviewer["mergeRequestInteraction"]["reviewState"].as_str())
        .collect();
    let decision = if states.contains(&"REQUESTED_CHANGES") {
        Decision::ChangesRequested
    } else if node["approved"] == true {
        Decision::Approved
    } else if states.is_empty() {
        Decision::None
    } else {
        Decision::ReviewRequired
    };
    let count = |key: &str| node[key].as_u64().unwrap_or(0);
    let pipeline = node["headPipeline"]["status"]
        .as_str()
        .map(str::to_ascii_lowercase);
    Ok(InboxEntry {
        review: ReviewRequest {
            id: format!("!{iid}"),
            branch: text("sourceBranch"),
            base: text("targetBranch"),
            state: ReviewState::Open,
            url: text("webUrl"),
            title: text("title"),
            draft: node["draft"].as_bool().unwrap_or(false),
        },
        author: node["author"]["username"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        created_at: text("createdAt"),
        updated_at: text("updatedAt"),
        additions: node["diffStatsSummary"]["additions"].as_u64().unwrap_or(0),
        deletions: node["diffStatsSummary"]["deletions"].as_u64().unwrap_or(0),
        checks: map_pipeline_status(pipeline.as_deref()),
        decision,
        unresolved: Some(
            count("resolvableDiscussionsCount").saturating_sub(count("resolvedDiscussionsCount"))
                as u32,
        ),
        // Notes include GitLab's own, which log pushes and approvals by who
        // made them.
        activity: Activity::of(
            me,
            node["notes"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                // A bot's notes are not news.
                .filter(|note| {
                    note["author"]["bot"] != true
                        && !super::is_bot(note["author"]["username"].as_str().unwrap_or_default())
                })
                .filter_map(|note| {
                    Some((
                        note["author"]["username"].as_str()?,
                        note["createdAt"].as_str()?,
                    ))
                }),
        ),
    })
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
/// green or skipped one passes, one that stopped without a verdict is
/// inconclusive, anything else in flight is pending, and an absent pipeline
/// shows nothing.
///
/// `canceled` and `manual` are the same shape as GitHub's `CANCELLED` and
/// `ACTION_REQUIRED` - nothing failed, but nothing finished either, and a
/// pipeline in that state still holds the merge. The dot means the same thing
/// on every provider or it means nothing.
fn map_pipeline_status(status: Option<&str>) -> CheckStatus {
    match status {
        Some("success" | "skipped") => CheckStatus::Passing,
        Some("failed") => CheckStatus::Failing,
        Some("canceled" | "manual") => CheckStatus::Inconclusive,
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
    fn gitlab_draft_notes_position_each_comment_and_add_the_summary() {
        let submission = Submission {
            event: ReviewEvent::Approve,
            body: "Summary".into(),
            comments: vec![super::super::DraftComment {
                path: "src/a.rs".into(),
                old_path: String::new(),
                anchor: Anchor {
                    side: Side::New,
                    line: 9,
                    start_line: None,
                },
                paired_line: Some(8),
                body: "Why?".into(),
            }],
            commit: "head".into(),
        };
        let refs =
            serde_json::json!({"base_sha": "base", "start_sha": "start", "head_sha": "head"});
        let notes = gitlab_draft_notes(&submission, &refs);
        assert_eq!(
            notes,
            [
                serde_json::json!({"note": "Why?", "position": {
                    "position_type": "text", "base_sha": "base", "start_sha": "start", "head_sha": "head",
                    "old_path": "src/a.rs", "new_path": "src/a.rs", "new_line": 9, "old_line": 8,
                }}),
                serde_json::json!({"note": "Summary"}),
            ]
        );
    }

    #[test]
    fn gitlab_conversation_reads_discussions_and_approvals() {
        let discussions = r#"[
            {"id":"d1","individual_note":true,"notes":[{"id":1,"system":false,"body":"General","author":{"username":"ann"},"created_at":"t1","position":null}]},
            {"id":"d2","individual_note":true,"notes":[{"id":2,"system":true,"body":"added 1 commit","author":{"username":"ann"},"created_at":"t2"}]},
            {"id":"d3","individual_note":false,"notes":[
                {"id":3,"type":"DiffNote","system":false,"body":"Here","author":{"username":"bo"},"created_at":"t3","resolvable":true,"resolved":false,
                 "position":{"head_sha":"HEAD","position_type":"text","new_path":"src/a.rs","old_path":"src/a.rs","new_line":7,"old_line":null,
                   "line_range":{"start":{"new_line":5},"end":{"new_line":7}}}},
                {"id":4,"type":"DiffNote","system":false,"body":"Reply","author":{"username":"ann"},"created_at":"t4","resolvable":true,"resolved":false}]},
            {"id":"d4","individual_note":false,"notes":[
                {"id":5,"type":"DiffNote","system":false,"body":"Old side","author":{"username":"bo"},"created_at":"t5","resolvable":true,"resolved":true,
                 "position":{"head_sha":"EARLIER","position_type":"text","new_path":"src/b.rs","old_path":"src/b.rs","new_line":null,"old_line":3}}]}
        ]"#;
        let approvals = r#"{"approved_by":[{"user":{"username":"cy"}}]}"#;
        let conversation =
            parse_gitlab_conversation(discussions, Some(approvals), "HEAD", "https://gl/mr/1")
                .unwrap();
        assert_eq!(
            conversation.comments.len(),
            1,
            "system notes are not discussion"
        );
        assert_eq!(conversation.comments[0].url, "https://gl/mr/1#note_1");
        assert_eq!(conversation.verdicts[0].author, "cy");
        assert_eq!(conversation.verdicts[0].state, VerdictState::Approved);
        let [current, earlier] = &conversation.threads[..] else {
            panic!("two threads");
        };
        assert_eq!(
            current.anchor,
            Some(Anchor {
                side: Side::New,
                line: 7,
                start_line: Some(5)
            })
        );
        assert_eq!(current.comments.len(), 2);
        assert!(!current.outdated);
        assert_eq!(
            earlier.anchor,
            Some(Anchor {
                side: Side::Old,
                line: 3,
                start_line: None
            })
        );
        assert!(earlier.outdated);
        assert_eq!(earlier.resolved, Some(true));
    }

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
    fn gitlab_activity_reads_notes_but_not_bots() {
        let node = serde_json::json!({
            "iid": "3", "title": "T", "webUrl": "u", "draft": false, "sourceBranch": "b",
            "targetBranch": "main", "createdAt": "2026-10-01T00:00:00Z",
            "updatedAt": "2026-10-02T00:00:00Z", "author": {"username": "ann"},
            "approved": false, "reviewers": {"nodes": []},
            "notes": {"nodes": [
                {"author": {"username": "me", "bot": false}, "createdAt": "2026-10-10T00:00:00Z"},
                {"author": {"username": "ann", "bot": false}, "createdAt": "2026-10-11T00:00:00Z"},
                {"author": {"username": "ann-helper", "bot": true}, "createdAt": "2026-10-12T00:00:00Z"},
                {"author": {"username": "gitlab-bot", "bot": false}, "createdAt": "2026-10-13T00:00:00Z"},
            ]},
        });
        let entry = gitlab_inbox_entry(&node, "me").unwrap();
        assert_eq!(
            entry.activity.yours.as_deref(),
            Some("2026-10-10T00:00:00Z")
        );
        assert_eq!(
            entry.activity.others.as_deref(),
            Some("2026-10-11T00:00:00Z")
        );
    }

    #[test]
    fn the_inbox_drops_reviewed_requests_and_returns_changes_requested() {
        let node = |iid: u32, author: &str, reviewers: &str, approved: bool| {
            format!(
                r#"{{"iid":"{iid}","title":"MR {iid}","webUrl":"u","draft":false,"sourceBranch":"b{iid}","targetBranch":"main",
                "createdAt":"2026-10-01T00:00:00Z","updatedAt":"2026-10-02T00:00:00Z","author":{{"username":"{author}"}},
                "diffStatsSummary":{{"additions":4,"deletions":1}},"headPipeline":{{"status":"SUCCESS"}},"approved":{approved},
                "resolvableDiscussionsCount":3,"resolvedDiscussionsCount":1,"reviewers":{{"nodes":[{reviewers}]}}}}"#
            )
        };
        let reviewer = |user: &str, state: &str| {
            format!(
                r#"{{"username":"{user}","mergeRequestInteraction":{{"reviewState":"{state}"}}}}"#
            )
        };
        let json = format!(
            r#"{{"data":{{"project":{{
                "requested":{{"count":3,"nodes":[{},{},{}]}},
                "authored":{{"count":3,"nodes":[{},{},{}]}}}}}}}}"#,
            node(1, "ann", &reviewer("me", "UNREVIEWED"), false),
            node(2, "bo", &reviewer("me", "APPROVED"), true),
            node(3, "me", &reviewer("me", "UNREVIEWED"), false),
            node(4, "me", &reviewer("ann", "REQUESTED_CHANGES"), false),
            node(5, "me", &reviewer("ann", "APPROVED"), true),
            node(6, "me", "", false),
        );
        let lists = parse_gitlab_inbox(&json, &InboxList::ALL, "me").unwrap();
        let ids = |list: InboxList| -> Vec<String> {
            match &lists.iter().find(|(found, _)| *found == list).unwrap().1 {
                Listing::Found { entries, .. } => entries
                    .iter()
                    .map(|entry| entry.review.id.clone())
                    .collect(),
                Listing::Unsupported(why) | Listing::Failed(why) => vec![why.clone()],
            }
        };
        assert_eq!(ids(InboxList::Incoming), ["!1"]);
        assert_eq!(ids(InboxList::Returned), ["!4"]);
        assert_eq!(ids(InboxList::Mine), ["!5", "!6"]);
        assert_eq!(ids(InboxList::Team), ["GitLab has no team review requests"]);
        let Listing::Found { entries, .. } = &lists[0].1 else {
            panic!("incoming is found");
        };
        assert_eq!(entries[0].unresolved, Some(2));
        assert_eq!(entries[0].checks, CheckStatus::Passing);
        assert_eq!(entries[0].decision, Decision::ReviewRequired);
    }

    #[test]
    fn an_inbox_query_gitlab_rejects_says_why() {
        let error = parse_gitlab_inbox(
            r#"{"errors":[{"message":"Field 'approved' doesn't exist on type 'MergeRequest'"}]}"#,
            &InboxList::ALL,
            "me",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "GitLab could not answer for the inbox: Field 'approved' doesn't exist on type 'MergeRequest'"
        );
    }

    #[test]
    fn map_pipeline_status_maps_gitlab_states() {
        assert_eq!(map_pipeline_status(Some("success")), CheckStatus::Passing);
        assert_eq!(map_pipeline_status(Some("skipped")), CheckStatus::Passing);
        assert_eq!(map_pipeline_status(Some("failed")), CheckStatus::Failing);
        // Stopped without a verdict, like GitHub's CANCELLED/ACTION_REQUIRED:
        // nothing failed, nothing finished, and the merge is still held.
        assert_eq!(
            map_pipeline_status(Some("canceled")),
            CheckStatus::Inconclusive
        );
        assert_eq!(
            map_pipeline_status(Some("manual")),
            CheckStatus::Inconclusive
        );
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
