use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};

use crate::git;

use super::json::{
    all_reviews, first_review, optional_bool, optional_string, parse_body_field, parse_issue_state,
    parse_state, required_string,
};
use super::{
    Activity, Anchor, CheckStatus, Comment, Conversation, Decision, FETCH_LIMIT, InboxEntry,
    InboxList, IssueState, Listing, MergeBlocker, NativeStack, NativeStackLayer, ReviewAnnotation,
    ReviewEvent, ReviewProvider, ReviewRequest, ReviewState, ReviewSummary, Side, StackPosition,
    Submission, Thread, Verdict, VerdictState, WaitOutcome, command_output,
    command_output_with_input, generic_annotate, merge_with_retry,
};
use crate::settings;

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

    fn review_by_number(&self, number: u64) -> Result<ReviewRequest> {
        let output = command_output(
            "gh",
            &[
                "pr",
                "view",
                &number.to_string(),
                "--json",
                "number,state,baseRefName,headRefName,url,title,isDraft",
            ],
        )?;
        parse_github_review(&output)?.with_context(|| format!("no GitHub pull request #{number}"))
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

    fn base_gap(&self, review: &ReviewRequest, parent: &str) -> Result<Option<super::BaseGap>> {
        let Some(stack) = self.native_stack_for(&review.branch)? else {
            return Ok(None);
        };
        Ok(Some(if stack.can_base_on(&review.branch, parent) {
            super::BaseGap::Platform
        } else if stack.parent_landed(&review.branch, parent) {
            // `parent` is this review's own recorded predecessor and it has
            // landed - the exact disjunct `can_base_on` stops accepting, so
            // the two arms are complements. The platform moved this base when
            // that layer landed; the local parent is what has not caught up.
            super::BaseGap::Sync
        } else {
            super::BaseGap::Neither
        }))
    }

    fn update_review_base(&self, review: &ReviewRequest, base: &str) -> Result<String> {
        // GitHub refuses to retarget a pull request that belongs to a stack -
        // it moves the layers itself as each one lands, which is the whole
        // point of registering. Report that rather than fail the submit.
        if self.native_stack_for(&review.branch)?.is_some() {
            return Ok(format!(
                "{} is in a GitHub stack; GitHub retargets it as the stack lands",
                review.id
            ));
        }
        command_output("gh", &["pr", "edit", review.id_value(), "--base", base])
    }

    fn update_review_title(&self, review: &ReviewRequest, title: &str) -> Result<String> {
        command_output("gh", &["pr", "edit", review.id_value(), "--title", title])
    }

    fn review_body(&self, review: &ReviewRequest) -> Result<String> {
        let output = command_output("gh", &["pr", "view", review.id_value(), "--json", "body"])?;
        parse_body_field(&output, "body")
    }

    fn issue_state(&self, number: u64) -> Result<IssueState> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{number}");
        parse_issue_state(&command_output("gh", &["api", &path])?)
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

    fn native_stack_for(&self, branch: &str) -> Result<Option<NativeStack>> {
        // Not gated on `stk.githubStacks`: that setting says whether git-stk
        // *registers* a stack, and a stack can exist without it having done so
        // - a teammate's `gh stack submit`, the web UI, the mobile app. GitHub
        // refuses the ordinary merge and retarget for those pull requests too,
        // so git-stk has to know about a stack whoever created it.
        let Some((owner, repo)) = repo_owner_name() else {
            return Ok(None);
        };
        let Ok(output) = stacks_listing(&owner, &repo) else {
            return Ok(None);
        };
        Ok(parse_native_stack(&output, branch))
    }

    fn register_stack(
        &self,
        reviews: &[String],
        existing: Option<&NativeStack>,
    ) -> Result<Option<String>> {
        if !self.registers_stacks() {
            return Ok(None);
        }
        let Some((owner, repo)) = repo_owner_name() else {
            return Ok(None);
        };
        let Some(plan) = super::plan_stack_registration(reviews, existing) else {
            return Ok(None);
        };
        match plan {
            super::StackPlan::Mismatch { number } => Ok(Some(format!(
                "stack {number} no longer matches this stack, so it was left as recorded; \
                 run `git stk unstack` to re-register it from scratch"
            ))),
            super::StackPlan::Extend { number, fresh } => {
                let numbers = pull_request_numbers(&fresh)?;
                let path = format!("repos/{owner}/{repo}/stacks/{number}/add");
                post_pull_requests(&path, &numbers)?;
                forget_stacks_listing();
                Ok(Some(format!(
                    "extended stack {number} with {}",
                    fresh.join(" ")
                )))
            }
            super::StackPlan::Register(reviews) => {
                let numbers = pull_request_numbers(&reviews)?;
                let path = format!("repos/{owner}/{repo}/stacks");
                post_pull_requests(&path, &numbers)?;
                forget_stacks_listing();
                Ok(Some(format!("registered {} as a stack", reviews.join(" "))))
            }
        }
    }

    fn registers_stacks(&self) -> bool {
        settings::bool_setting(settings::GITHUB_STACKS_KEY).unwrap_or(false)
    }

    fn native_stacks_covering(&self, branches: &[String]) -> Result<Vec<NativeStack>> {
        // Errors propagate all the way here: the caller is acting on the
        // answer, and "no stack" for an expired token would report a stack
        // already gone while it is still registered. `gh repo view` is the
        // first call to fail that way, so it carries its error too.
        let (owner, repo) =
            repo_owner_name_result().context("could not resolve the GitHub repository")?;
        let listing = stacks_listing(&owner, &repo)
            .context("could not read this repository's stacks from GitHub")?;

        let mut found: Vec<NativeStack> = Vec::new();
        for branch in branches {
            if let Some(stack) = parse_native_stack(&listing, branch)
                && !found.iter().any(|seen| seen.number == stack.number)
            {
                found.push(stack);
            }
        }
        Ok(found)
    }

    fn unstack_reviews(&self, stack: &NativeStack) -> Result<Option<String>> {
        // Strict, like `native_stacks_covering`: `Ok(None)` here reaches the
        // user as "this provider does not keep stacks", which is false for
        // GitHub however the lookup failed.
        let (owner, repo) =
            repo_owner_name_result().context("could not resolve the GitHub repository")?;
        let path = format!("repos/{owner}/{repo}/stacks/{}/unstack", stack.number);
        command_output("gh", &["api", &path, "-X", "POST"])
            .with_context(|| format!("could not dissolve stack {} on GitHub", stack.number))?;
        // The listing still holds this stack as open until it is re-read.
        forget_stacks_listing();
        // Name the reviews: a stack is dissolved whole, so it can reach past
        // the line that asked. Saying less than `--dry-run` did would hide
        // exactly that.
        Ok(Some(format!(
            "dissolved stack {} on GitHub ({})",
            stack.number,
            stack
                .layers
                .iter()
                .map(|layer| layer.id.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )))
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
        let auto = mode == super::MergeMode::Auto;
        let admin = mode == super::MergeMode::Admin;
        let flag = match strategy {
            "rebase" => "--rebase",
            "merge" => "--merge",
            _ => "--squash",
        };
        // A pull request in a stack cannot be merged through the synchronous
        // endpoint `gh pr merge` uses - GitHub requires the async one, because
        // landing a layer also retargets the layers above it.
        if self.native_stack_for(&review.branch)?.is_some() {
            // `--auto` schedules a merge for when checks pass, which the async
            // endpoint has no equivalent for - GitHub's auto-merge is a
            // separate mutation and refuses a stacked pull request too. Say so
            // rather than merge now, which is the opposite of what was asked.
            if auto {
                return Err(super::MergeRefused(format!(
                    "{} is in a GitHub stack, which cannot be scheduled with --auto; \
                     rerun without it once checks pass",
                    review.id
                ))
                .into());
            }
            return async_merge(review, strategy, admin);
        }

        let mut args = vec!["pr", "merge", review.id_value(), flag];
        if auto {
            args.push("--auto");
        }
        if admin {
            args.push("--admin");
        }
        merge_with_retry(|| command_output("gh", &args)).or_else(|error| {
            // GitHub knows of a stack the listing did not show: take the
            // endpoint it names.
            if !auto && needs_async_merge(&error) {
                forget_stacks_listing();
                return async_merge(review, strategy, admin);
            }
            Err(error)
        })
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
                // `gh pr checks` buckets every check as pass, fail, pending,
                // skipping or cancel - it has no bucket for "stopped without a
                // verdict", so a cancelled or action-required run lands in one
                // of the others depending on which. Ask the rollup on both
                // settled arms rather than guessing which: it is what the dot
                // reads, and the gate must not contradict it. Only the verdict
                // moves - a rollup that says anything else leaves the arm's own
                // answer alone.
                ChecksState::Passed => {
                    return Ok(match self.check_status(review) {
                        Ok(CheckStatus::Inconclusive) => WaitOutcome::Inconclusive,
                        _ => WaitOutcome::Passed,
                    });
                }
                ChecksState::Failed => {
                    return Ok(match self.check_status(review) {
                        Ok(CheckStatus::Inconclusive) => WaitOutcome::Inconclusive,
                        _ => WaitOutcome::Failed,
                    });
                }
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

    fn viewer(&self) -> Result<String> {
        Ok(command_output("gh", &["api", "user", "--jq", ".login"])?
            .trim()
            .to_owned())
    }

    fn inbox(&self, lists: &[InboxList]) -> Result<Vec<(InboxList, Listing)>> {
        let (owner, repo) = repo_owner_name_result()?;
        let slug = format!("{owner}/{repo}");
        // A query per list, side by side: one query searching for all of them
        // can outlast GitHub's timeout on a busy repository.
        std::thread::scope(|scope| {
            let asked: Vec<_> = lists
                .iter()
                .map(|&list| {
                    let slug = &slug;
                    scope.spawn(move || {
                        let graphql = |query: &str| {
                            let query_arg = format!("query={query}");
                            super::retry_server_errors(|| {
                                command_output("gh", &["api", "graphql", "-f", &query_arg])
                            })
                        };
                        let (search, here) = match list {
                            InboxList::Outside => (
                                graphql(OUTSIDE_OWNERS_QUERY)
                                    .and_then(|output| parse_github_owners(&output))
                                    .map(|owners| outside_search(&owners)),
                                None,
                            ),
                            _ => (
                                Ok(format!("repo:{slug} {}", inbox_search(list))),
                                Some(slug.as_str()),
                            ),
                        };
                        let listing = search
                            .and_then(|search| graphql(&inbox_query(&search)))
                            .and_then(|output| parse_github_inbox(&output, here))
                            .unwrap_or_else(|error| Listing::Failed(format!("{error:#}")));
                        (list, listing)
                    })
                })
                .collect();
            Ok(asked
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
                .collect())
        })
    }

    fn conversation(&self, review: &ReviewRequest) -> Result<Conversation> {
        let (owner, repo) = repo_owner_name_result()?;
        let owner_arg = format!("owner={owner}");
        let repo_arg = format!("name={repo}");
        let number_arg = format!("number={}", review.id_value());
        let query_arg = format!("query={CONVERSATION_QUERY}");
        let output = command_output(
            "gh",
            &[
                "api",
                "graphql",
                "-f",
                &owner_arg,
                "-f",
                &repo_arg,
                "-F",
                &number_arg,
                "-f",
                &query_arg,
            ],
        )?;
        parse_github_conversation(&output)
    }

    fn submit_review(&self, review: &ReviewRequest, submission: &Submission) -> Result<String> {
        let (owner, repo) = repo_owner_name_result()?;
        let path = format!("repos/{owner}/{repo}/pulls/{}/reviews", review.id_value());
        let output = command_output_with_input(
            "gh",
            &["api", "--method", "POST", &path, "--input", "-"],
            &github_review_request(submission).to_string(),
        )?;
        let value: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse gh review JSON")?;
        Ok(optional_string(&value, "html_url"))
    }

    fn reply_to_thread(
        &self,
        _review: &ReviewRequest,
        thread: &Thread,
        _commit: &str,
        body: &str,
    ) -> Result<()> {
        graphql_mutation(
            "mutation($id:ID!,$body:String!){addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$id,body:$body}){comment{id}}}",
            &[("id", &thread.id), ("body", body)],
        )
    }

    fn resolves_threads(&self) -> bool {
        true
    }

    fn set_resolved(&self, _review: &ReviewRequest, thread: &Thread, resolved: bool) -> Result<()> {
        let mutation = if resolved {
            "mutation($id:ID!){resolveReviewThread(input:{threadId:$id}){thread{isResolved}}}"
        } else {
            "mutation($id:ID!){unresolveReviewThread(input:{threadId:$id}){thread{isResolved}}}"
        };
        graphql_mutation(mutation, &[("id", &thread.id)])
    }

    fn all_open_reviews(&self) -> Result<(Vec<ReviewRequest>, bool)> {
        // gh pages through the API itself; a repository under the limit costs
        // a single request.
        const LIMIT: usize = 2000;
        let limit = LIMIT.to_string();
        let output = command_output(
            "gh",
            &[
                "pr",
                "list",
                "--state",
                "open",
                "--limit",
                &limit,
                "--json",
                "number,state,baseRefName,headRefName,url,title,isDraft",
            ],
        )?;
        let reviews = parse_github_reviews(&output)?;
        let complete = reviews.len() < LIMIT;
        Ok((reviews, complete))
    }

    fn annotate_review(&self, review: &ReviewRequest, detail: bool) -> Result<ReviewAnnotation> {
        // One GraphQL call for the queue state, the CI rollup, and the stack
        // position - the same query `list` makes, so the two cannot disagree
        // about a stack's size.
        //
        // Only for an open review: the query asks `states: OPEN`, so for a
        // merged or closed one it cannot match and the call is spent
        // discovering that. Those fall straight to the per-call default,
        // which is also the only way to name the stack a landed layer is
        // still listed in.
        let found = matches!(review.state, ReviewState::Open)
            .then(|| batched_annotate(std::slice::from_ref(&review.branch), detail))
            .and_then(Result::ok)
            .and_then(|mut found| found.remove(&review.branch));
        match found {
            Some(annotation) => Ok(annotation),
            // No entry, or the query failed outright: ask per call instead.
            None => super::generic_annotate_review(self, review, detail),
        }
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

    fn comment_on_review(&self, review: &ReviewRequest, body: &str) -> Result<String> {
        command_output("gh", &["pr", "comment", review.id_value(), "--body", body])
    }

    fn enqueued_branches(&self, branches: &[String]) -> Result<BTreeSet<String>> {
        Ok(github_enqueued_branches(branches))
    }

    fn base_has_merge_queue(&self, branch: &str) -> Result<bool> {
        let (owner, repo) = repo_owner_name().context("could not resolve the GitHub repository")?;
        let owner_arg = format!("owner={owner}");
        let repo_arg = format!("repo={repo}");
        let branch_arg = format!("branch={branch}");
        let query_arg = format!("query={BASE_MERGE_QUEUE_QUERY}");
        let output = command_output(
            "gh",
            &[
                "api",
                "graphql",
                "-f",
                &owner_arg,
                "-f",
                &repo_arg,
                "-f",
                &branch_arg,
                "-f",
                &query_arg,
            ],
        )?;
        Ok(parse_base_merge_queue(&output))
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

    match command_output("gh", &args) {
        Ok(output) => parse_annotation_batch(&output, detail),
        // `stack`/`stackEntry` are preview fields: a host whose schema lacks
        // them rejects the *whole* query, which would silently drop `list` to
        // the per-branch path for everyone on that host. Retry once without
        // them and remember, so the cost is one query rather than two.
        Err(error) if !STACK_FIELDS_UNSUPPORTED.with(std::cell::Cell::get) => {
            STACK_FIELDS_UNSUPPORTED.with(|flag| flag.set(true));
            let retried = batched_annotate(branches, detail);
            if retried.is_err() {
                // Dropping the fields did not help, so they were not what
                // failed - offline, rate-limited, no `gh`. Leave the flag
                // clear and report what actually went wrong, rather than
                // blaming the host's schema for every provider failure.
                STACK_FIELDS_UNSUPPORTED.with(|flag| flag.set(false));
                return Err(error);
            }
            // Under `--verbose` only. A host whose schema lacks the fields
            // lacks them on every run, so warning would be permanent noise on
            // the command run most - and `native_stack_for` already answers
            // the same fact silently for a repo without the preview. This is
            // what a missing `⛁` means, kept where someone looking for it
            // will find it.
            if crate::git::verbose() {
                anstream::eprintln!(
                    "{}",
                    crate::style::dim(&format!(
                        "this host did not accept the stacked-pull-request fields, \
                         so `list` will not mark stack layers: {error}"
                    ))
                );
            }
            retried
        }
        Err(error) => Err(error),
    }
}

thread_local! {
    /// Set once a host rejects the stack fields, so the retry happens at most
    /// once per command rather than per annotate call.
    static STACK_FIELDS_UNSUPPORTED: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

/// A review's place in GitHub's own stack, from the annotate query's
/// `stack`/`stackEntry` fields. `None` when the pull request is in no stack -
/// or when the fields are absent, which is what a host without the preview
/// answers.
fn parse_stack_position(node: &serde_json::Value) -> Option<StackPosition> {
    let stack = node.get("stack")?;
    Some(StackPosition {
        number: stack.get("number")?.as_u64()?,
        position: node.pointer("/stackEntry/position")?.as_u64()? as u32,
        size: stack.get("size")?.as_u64()? as u32,
    })
}

/// Build the aliased GraphQL query for [`batched_annotate`]. The fields are
/// exactly what [`ReviewAnnotation`] needs, no more.
///
/// Assembled by hand, so its braces are the author's to balance - and an
/// unbalanced one is invisible in this repo's tests, which replay canned
/// responses rather than sending the query. `build_annotation_query_balances`
/// counts them, and the live e2e sends it; a query the host rejects otherwise
/// degrades to the per-branch fallback, which looks identical from outside.
fn build_annotation_query(count: usize, detail: bool) -> String {
    build_annotation_query_with(
        count,
        detail,
        !STACK_FIELDS_UNSUPPORTED.with(std::cell::Cell::get),
    )
}

fn build_annotation_query_with(count: usize, detail: bool, stack_fields: bool) -> String {
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
        if stack_fields {
            aliases.push_str("stack{number size} stackEntry{position} ");
        }
        aliases.push_str(reviews_field);
        // The contexts, not the rollup's own `state`: that aggregate counts
        // every run recorded against the head commit, so a superseded
        // cancelled run pins it to FAILURE while the re-run of the same check
        // passed. `aggregate_rollup` keeps the newest per check instead.
        //
        // Bounded by the branches being annotated - this is not the
        // `open_reviews` listing, which covers every open PR in the repo.
        // The cap comes from the constant the truncation guard reads, so the
        // two cannot drift: ask for a different number and the guard fires at
        // the new one.
        aliases.push_str(&format!("commits(last:1){{nodes{{commit{{statusCheckRollup{{state contexts(last:{ROLLUP_CONTEXTS}){{totalCount nodes{{__typename ... on CheckRun{{name status conclusion startedAt checkSuite{{workflowRun{{workflow{{name}}}} app{{slug}}}}}} ... on StatusContext{{context state createdAt}}}}}}}}}}}}}}}}}}"));
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
        let rollup = node.pointer("/commits/nodes/0/commit/statusCheckRollup");
        // The cap counts *runs*, not checks, and keeping every run is the
        // point - a wide matrix restacked a few times passes 100 easily. Past
        // that the oldest fall off, which is usually harmless (they are the
        // superseded ones) but need not be: a check that failed and was never
        // re-run would vanish, and a check with no entry is not a failure, so
        // the dot would go green on a red commit. GitHub's own aggregate sees
        // everything, so fall back to it rather than answer from a slice.
        let truncated = rollup
            .and_then(|rollup| rollup.pointer("/contexts/totalCount"))
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|total| total > ROLLUP_CONTEXTS);
        let checks = match rollup {
            Some(rollup) if truncated => {
                rollup_state_to_status(rollup.get("state").and_then(serde_json::Value::as_str))
            }
            Some(rollup) => rollup
                .pointer("/contexts/nodes")
                .map_or(CheckStatus::None, aggregate_rollup),
            None => CheckStatus::None,
        };
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
                stack: parse_stack_position(node),
            },
        );
    }
    Ok(annotations)
}

/// How many rollup entries the annotate query asks for. Past this the oldest
/// are dropped, and [`parse_annotation_batch`] falls back to GitHub's own
/// aggregate rather than answering from a slice.
const ROLLUP_CONTEXTS: u64 = 100;

/// GitHub's own rollup verdict, for the commits whose check list is too long
/// to read whole. Coarser than [`aggregate_rollup`] - it counts superseded
/// runs, which is the bug this all started with - but it sees every entry,
/// and reporting a stale red beats reporting a green that is not there.
fn rollup_state_to_status(state: Option<&str>) -> CheckStatus {
    match state {
        Some("SUCCESS") => CheckStatus::Passing,
        Some("FAILURE" | "ERROR") => CheckStatus::Failing,
        Some("PENDING" | "EXPECTED") => CheckStatus::Pending,
        _ => CheckStatus::None,
    }
}

/// A non-null `mergeQueue` for the branch. Anything unreadable is `false`:
/// the caller turns a `true` into merging the top of a stack, which is only
/// right when a queue will cascade it.
fn parse_base_merge_queue(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|value| {
            value
                .pointer("/data/repository/mergeQueue")
                .map(|queue| !queue.is_null())
        })
        .unwrap_or(false)
}

/// Run a GraphQL mutation with string variables. `gh api graphql` answers
/// HTTP 200 with an `errors` list when the mutation itself is refused, so
/// that is checked too.
fn graphql_mutation(mutation: &str, variables: &[(&str, &str)]) -> Result<()> {
    let query_arg = format!("query={mutation}");
    let variables: Vec<String> = variables
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    let mut args = vec!["api", "graphql", "-f", query_arg.as_str()];
    for variable in &variables {
        args.extend(["-f", variable.as_str()]);
    }
    let output = command_output("gh", &args)?;
    let value: serde_json::Value =
        serde_json::from_str(&output).context("failed to parse gh GraphQL JSON")?;
    if let Some(message) = value
        .pointer("/errors/0/message")
        .and_then(serde_json::Value::as_str)
    {
        bail!("GitHub refused it: {message}");
    }
    Ok(())
}

/// The body of a create-review request: one review carrying every comment,
/// so the pull request's people are notified once.
fn github_review_request(submission: &Submission) -> serde_json::Value {
    let side = |side: Side| match side {
        Side::Old => "LEFT",
        Side::New => "RIGHT",
    };
    serde_json::json!({
        "commit_id": submission.commit,
        "body": submission.body,
        "event": match submission.event {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        },
        "comments": submission.comments.iter().map(|comment| {
            let mut value = serde_json::json!({
                "path": comment.path,
                "body": comment.body,
                "line": comment.anchor.line,
                "side": side(comment.anchor.side),
            });
            if let Some(start) = comment.anchor.start_line {
                value["start_line"] = start.into();
                value["start_side"] = side(comment.anchor.side).into();
            }
            value
        }).collect::<Vec<_>>(),
    })
}

/// The search behind each list of this repository's reviews. A review
/// requested of you both by name and through a team is only in `incoming`,
/// and one returned to you only in `returned`.
fn inbox_search(list: InboxList) -> &'static str {
    match list {
        InboxList::Incoming => "user-review-requested:@me",
        InboxList::Team => "team-review-requested-user:@me -user-review-requested:@me",
        InboxList::Mine => "author:@me -review:changes_requested",
        InboxList::Returned => "author:@me review:changes_requested",
        InboxList::Outside => "author:@me",
    }
}

/// You and the organizations you belong to, the owners `outside` leaves out.
const OUTSIDE_OWNERS_QUERY: &str =
    "query { viewer { login organizations(first: 100) { nodes { login } } } }";

fn parse_github_owners(json: &str) -> Result<Vec<String>> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("failed to parse gh organizations JSON")?;
    let viewer = &value["data"]["viewer"];
    let me = viewer["login"]
        .as_str()
        .context("gh answered without your login")?;
    Ok(std::iter::once(me)
        .chain(
            viewer["organizations"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|org| org["login"].as_str()),
        )
        .map(str::to_owned)
        .collect())
}

/// Your reviews in every repository not owned by one of `owners`, the first
/// of which is you. Qualifiers do not count toward GitHub's limit on a
/// search's length, so any number of organizations fit.
fn outside_search(owners: &[String]) -> String {
    let mut search = inbox_search(InboxList::Outside).to_owned();
    for (index, owner) in owners.iter().enumerate() {
        let qualifier = if index == 0 { "user" } else { "org" };
        search.push_str(&format!(" -{qualifier}:{owner}"));
    }
    search
}

/// The GraphQL query for the open pull requests matching `search`.
fn inbox_query(search: &str) -> String {
    format!(
        "query {{ viewer {{ login }} search(type: ISSUE, first: {FETCH_LIMIT}, query: \"is:pr is:open sort:updated-desc {search}\") {{ issueCount nodes {{ ...inboxReview }} }} }} fragment inboxReview on PullRequest {{ number state title url isDraft headRefName baseRefName repository {{ nameWithOwner }} author {{ login }} createdAt updatedAt additions deletions reviewDecision commits(last: 1) {{ nodes {{ commit {{ committedDate statusCheckRollup {{ state }} }} }} }} reviewThreads(first: 100) {{ nodes {{ isResolved }} }} comments(last: 20) {{ nodes {{ author {{ __typename login }} createdAt }} }} reviews(last: 20) {{ nodes {{ author {{ __typename login }} submittedAt }} }} }}"
    )
}

/// The reviews an inbox search found, each named with its repository, such
/// as `owner/repo#12`, unless it is in `here`, the repository searched.
fn parse_github_inbox(json: &str, here: Option<&str>) -> Result<Listing> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("failed to parse gh inbox JSON")?;
    let search = &value["data"]["search"];
    let me = value["data"]["viewer"]["login"]
        .as_str()
        .unwrap_or_default();
    let entries = search["nodes"]
        .as_array()
        .context("gh answered the inbox search without results")?
        .iter()
        .map(|node| github_inbox_entry(node, me, here))
        .collect::<Result<Vec<_>>>()?;
    let more = search["issueCount"].as_u64().unwrap_or(0) > entries.len() as u64;
    Ok(Listing::Found { entries, more })
}

fn github_inbox_entry(
    node: &serde_json::Value,
    me: &str,
    here: Option<&str>,
) -> Result<InboxEntry> {
    let number = node["number"]
        .as_u64()
        .context("an inbox review has no number")?;
    let repository = match (node["repository"]["nameWithOwner"].as_str(), here) {
        (Some(repository), Some(here)) if !repository.eq_ignore_ascii_case(here) => repository,
        (Some(repository), None) => repository,
        _ => "",
    };
    let text = |key: &str| node[key].as_str().unwrap_or_default().to_owned();
    let rollup = node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"].as_str();
    let unresolved = node["reviewThreads"]["nodes"].as_array().map(|threads| {
        threads
            .iter()
            .filter(|thread| thread["isResolved"] == false)
            .count() as u32
    });
    Ok(InboxEntry {
        review: ReviewRequest {
            id: format!("{repository}#{number}"),
            branch: text("headRefName"),
            base: text("baseRefName"),
            state: ReviewState::Open,
            url: text("url"),
            title: text("title"),
            draft: node["isDraft"].as_bool().unwrap_or(false),
        },
        author: node["author"]["login"]
            .as_str()
            .unwrap_or("ghost")
            .to_owned(),
        created_at: text("createdAt"),
        updated_at: text("updatedAt"),
        additions: node["additions"].as_u64().unwrap_or(0),
        deletions: node["deletions"].as_u64().unwrap_or(0),
        checks: rollup_state_to_status(rollup),
        decision: match node["reviewDecision"].as_str() {
            Some("APPROVED") => Decision::Approved,
            Some("CHANGES_REQUESTED") => Decision::ChangesRequested,
            Some("REVIEW_REQUIRED") => Decision::ReviewRequired,
            _ => Decision::None,
        },
        unresolved,
        activity: github_activity(node, me),
    })
}

/// Who an actor is, or None for a bot: a bot's comments are not news.
fn login(actor: &serde_json::Value) -> Option<&str> {
    let login = actor["login"].as_str().unwrap_or("ghost");
    (actor["__typename"] != "Bot" && !super::is_bot(login)).then_some(login)
}

/// Comments and reviews by their authors, and the latest commit as the pull
/// request's author's, for what is new since you looked.
fn github_activity(node: &serde_json::Value, me: &str) -> Activity {
    let nodes = |key: &str| node[key]["nodes"].as_array().cloned().unwrap_or_default();
    let comments = nodes("comments");
    let reviews = nodes("reviews");
    let commit = &node["commits"]["nodes"][0]["commit"];
    let events = comments
        .iter()
        .map(|comment| (login(&comment["author"]), comment["createdAt"].as_str()))
        .chain(
            reviews
                .iter()
                .map(|review| (login(&review["author"]), review["submittedAt"].as_str())),
        )
        .chain([(login(&node["author"]), commit["committedDate"].as_str())])
        .filter_map(|(who, when)| Some((who?, when?)));
    Activity::of(me, events)
}

/// A pull request's discussion in one request. Each listing stops at 100,
/// and the counts say whether anything was left out.
const CONVERSATION_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){\
repository(owner:$owner,name:$name){pullRequest(number:$number){\
comments(first:100){totalCount nodes{id author{login} body createdAt url}}\
reviews(first:100){totalCount nodes{author{login} state body submittedAt url}}\
reviewThreads(first:100){totalCount nodes{id isResolved isOutdated path line originalLine \
startLine originalStartLine diffSide subjectType \
comments(first:100){totalCount nodes{id author{login} body createdAt url diffHunk}}}}}}}";

/// The discussion from a [`CONVERSATION_QUERY`] response. Pending reviews are
/// drafts only their author can see, and are left out.
fn parse_github_conversation(output: &str) -> Result<Conversation> {
    let value: serde_json::Value =
        serde_json::from_str(output).context("failed to parse gh GraphQL JSON")?;
    let pr = value
        .pointer("/data/repository/pullRequest")
        .filter(|pr| !pr.is_null())
        .context("gh GraphQL response has no pull request")?;
    let mut incomplete = false;
    let mut nodes = |listing: &serde_json::Value| -> Vec<serde_json::Value> {
        let nodes = listing
            .get("nodes")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = listing
            .get("totalCount")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        incomplete |= total > nodes.len() as u64;
        nodes
    };
    let comment = |node: &serde_json::Value| Comment {
        id: optional_string(node, "id"),
        author: node
            .pointer("/author/login")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("ghost")
            .to_owned(),
        body: optional_string(node, "body"),
        created_at: optional_string(node, "createdAt"),
        url: optional_string(node, "url"),
    };

    let comments = nodes(&pr["comments"]).iter().map(comment).collect();
    let verdicts = nodes(&pr["reviews"])
        .iter()
        .filter_map(|node| {
            let state = match node.get("state").and_then(serde_json::Value::as_str)? {
                "APPROVED" => VerdictState::Approved,
                "CHANGES_REQUESTED" => VerdictState::ChangesRequested,
                "COMMENTED" => VerdictState::Commented,
                "DISMISSED" => VerdictState::Dismissed,
                _ => return None,
            };
            Some(Verdict {
                author: comment(node).author,
                state,
                body: optional_string(node, "body"),
                submitted_at: optional_string(node, "submittedAt"),
                url: optional_string(node, "url"),
            })
        })
        .collect();
    let threads = nodes(&pr["reviewThreads"])
        .iter()
        .map(|node| {
            let outdated = optional_bool(node, "isOutdated");
            let number = |key: &str| {
                node.get(key)
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|line| u32::try_from(line).ok())
            };
            // An outdated thread keeps only its line on the version it was
            // left on.
            let (line, start_line) = if outdated {
                (number("originalLine"), number("originalStartLine"))
            } else {
                (number("line"), number("startLine"))
            };
            let side = match node.get("diffSide").and_then(serde_json::Value::as_str) {
                Some("LEFT") => Side::Old,
                _ => Side::New,
            };
            let file_level =
                node.get("subjectType").and_then(serde_json::Value::as_str) == Some("FILE");
            let comments: Vec<Comment> = nodes(&node["comments"]).iter().map(comment).collect();
            let diff_hunk = node
                .pointer("/comments/nodes/0/diffHunk")
                .and_then(serde_json::Value::as_str)
                .filter(|hunk| !hunk.is_empty())
                .map(str::to_owned);
            Thread {
                id: optional_string(node, "id"),
                path: optional_string(node, "path"),
                anchor: line.filter(|_| !file_level).map(|line| Anchor {
                    side,
                    line,
                    start_line,
                }),
                outdated,
                resolved: Some(optional_bool(node, "isResolved")),
                diff_hunk,
                comments,
            }
        })
        .collect();
    Ok(Conversation {
        verdicts,
        comments,
        threads,
        incomplete,
    })
}

/// `mergeQueueEntry` exposes merge-queue membership, but only over GraphQL -
/// there is no `gh pr view --json` field for it. Query each branch's open PR by
/// head ref; a non-null entry means it is queued and the branch is locked.
const MERGE_QUEUE_QUERY: &str = "query($owner:String!,$repo:String!,$head:String!){\
repository(owner:$owner,name:$repo){\
pullRequests(headRefName:$head,states:OPEN,first:1){nodes{mergeQueueEntry{state}}}}}";

/// Whether a branch has a merge queue at all - `mergeQueue` is null when it
/// does not. Distinct from [`MERGE_QUEUE_QUERY`] above, which asks whether one
/// particular review is *sitting in* a queue.
const BASE_MERGE_QUEUE_QUERY: &str = "query($owner:String!,$repo:String!,$branch:String!){\
repository(owner:$owner,name:$repo){mergeQueue(branch:$branch){id}}}";

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

/// How long to keep polling an enqueued async merge before giving up: two
/// minutes at the interval below. The loop returns the moment a poll reads a
/// final status, so this budget is only what a merge is allowed to take -
/// GitHub does the retargeting after answering, not before.
const ASYNC_MERGE_POLLS: u32 = 60;

fn async_merge_poll_interval() -> std::time::Duration {
    // Under the fake-provider harness the whole merge is answered by a local
    // process, so the wait is pure test latency - and without this the poll
    // loop is untestable at two seconds a turn. Behind the same feature gate
    // as the harness itself, so the check is compiled out of a shipped binary
    // rather than read from its environment.
    #[cfg(feature = "test-fakes")]
    if std::env::var_os("STK_FAKE_SPEC").is_some() {
        return std::time::Duration::from_millis(1);
    }
    std::time::Duration::from_secs(2)
}

/// GitHub refused the merge *method*, not the merge.
///
/// Carried as an error rather than a return value because the refusal can
/// arrive from either end of an async merge and both already report through
/// `Result`. It holds the message it would otherwise have been, so a refusal
/// that survives the retry still reads like any other failed merge.
#[derive(Debug)]
struct MergeMethodRefused(String);

impl std::fmt::Display for MergeMethodRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MergeMethodRefused {}

/// Merge a stacked review, dropping the merge method if GitHub refuses it.
///
/// Something on GitHub's side can own the method - a merge queue on the base
/// branch, in every case seen so far - and GitHub then rejects the whole
/// request rather than ignoring the parameter. The refusal arrives from
/// whichever end evaluates it: the `PUT` runs "only basic pull request state
/// checks", so a rule's claim on the method is a verdict from the background
/// job and surfaces at a poll instead. Nothing merged either way, so running
/// the sequence again without the method is safe wherever it came from.
///
/// Reactive rather than probing for a queue first: a probe that answered wrong
/// in the other direction would drop the method on a repo that does honour it
/// and land the review by the repo's default instead of `stk.mergeStrategy`.
/// Retrying only on GitHub's own refusal drops it exactly when it would not
/// have been obeyed anyway. The match stays broad while the note names the
/// queue: every refusal seen has been one, and the sentence is the clearer for
/// saying so.
///
/// With `bypass`, the merge goes straight into the base past the rules the
/// account may bypass, as `gh pr merge --admin` does. GitHub allows that only
/// for the bottom-most unmerged review of a stack.
fn async_merge(review: &ReviewRequest, strategy: &str, bypass: bool) -> Result<String> {
    let first = attempt_async_merge(review, Some(strategy), bypass);
    match &first {
        Err(error) if error.is::<MergeMethodRefused>() => {}
        _ => return first,
    }
    // `?`: a re-send that never lands leaves nothing to say about which method
    // decided the merge, and a second refusal reports itself in full.
    let outcome = attempt_async_merge(review, None, bypass)?;
    anstream::eprintln!(
        "{}",
        crate::style::warn(&format!(
            "GitHub refused {strategy} for {}: the merge queue decides the merge method",
            review.id
        ))
    );
    Ok(outcome)
}

/// One enqueue-and-wait, with or without a merge method.
fn attempt_async_merge(
    review: &ReviewRequest,
    strategy: Option<&str>,
    bypass: bool,
) -> Result<String> {
    // Only the enqueue is retried: the `PUT` is not idempotent, so wrapping
    // the wait in the retry too would re-ask GitHub to merge a review that is
    // already merging whenever a poll blipped.
    let (path, output) = merge_with_retry(|| enqueue_async_merge(review, strategy, bypass))?;
    await_async_merge(review, &path, &output)
}

/// Ask GitHub to start the merge. Split from the wait because the `PUT` is not
/// idempotent: retrying it after a transient failure *later* in the sequence
/// would ask for a second merge of a review already merging.
fn enqueue_async_merge(
    review: &ReviewRequest,
    strategy: Option<&str>,
    bypass: bool,
) -> Result<(String, String)> {
    let (owner, repo) = repo_owner_name().context("could not resolve the GitHub repository")?;
    let number = review_number(&review.id)
        .with_context(|| format!("could not read a pull request number from {}", review.id))?;
    let path = format!("repos/{owner}/{repo}/pulls/{number}/merge-async");
    let mut args = vec!["api", path.as_str(), "-X", "PUT"];
    let method = strategy.map(|strategy| format!("merge_method={strategy}"));
    if let Some(method) = &method {
        args.extend_from_slice(&["-f", method]);
    }
    if bypass {
        // Straight in, past the queue too, as `gh pr merge --admin` does.
        args.extend_from_slice(&["-F", "bypass_rules=true", "-f", "merge_action=direct_merge"]);
    }
    let output = command_output("gh", &args)?;
    // The PUT is the write: from here GitHub is landing the layer, so a cached
    // listing is a pre-merge snapshot however the wait then returns - merged,
    // enqueued, or still going when the polls run out and the caller finds it
    // landed anyway. Invalidating once here covers every exit.
    //
    // Verified live on a three-layer stack: the stack stays `open` with the
    // landed layer still listed (as `closed`) until every layer has landed, at
    // which point it flips to `open: false`. Those two fields are what this
    // cache holds, both change as a layer lands, and `merge --all` reads them
    // again for every layer after this one through `base_gap`. One request,
    // and the next read is of the stack as it is rather than as it was.
    forget_stacks_listing();
    Ok((path, output))
}

/// Wait for an enqueued merge to reach a final status. Never retried as a
/// whole - only the enqueue above is, and it has already happened here.
fn await_async_merge(review: &ReviewRequest, path: &str, output: &str) -> Result<String> {
    let (status, uuid) = parse_async_merge(output)
        .with_context(|| format!("unexpected response merging {}: {output}", review.id))?;
    if let Some(outcome) = async_merge_outcome(&status, &review.id, output) {
        return outcome;
    }

    let uuid = uuid.with_context(|| format!("{} was enqueued without a result id", review.id))?;
    let result_path = format!("{path}/{uuid}");
    // Kept so a run where *nothing* answered is reported as what it was. The
    // merge is still GitHub's to finish, so this is never an error - but
    // "still merging" for two minutes of failed requests names the wrong
    // thing, and `sync` hits the same failure loudly straight after.
    //
    // Only ever set while no poll has succeeded: a single blip after a run of
    // `pending` answers is a merge we watched running, not an unreachable
    // host, and reporting it as one would be the same wrong name in reverse.
    let mut answered = false;
    let mut unanswered: Option<String> = None;
    for _ in 0..ASYNC_MERGE_POLLS {
        std::thread::sleep(async_merge_poll_interval());
        // A failed poll is not a failed merge. The merge is already running on
        // GitHub, and this loop only asks how it went - so a 502 or a dropped
        // connection is retried like an unreadable body below, not returned.
        // Returning it would reach `explain_merge_failure`, where a mid-merge
        // `BLOCKED` reads as "checks are not green" for a merge that is
        // landing, and `--all` would abort on it.
        let output = match command_output("gh", &["api", &result_path]) {
            Ok(output) => {
                answered = true;
                unanswered = None;
                output
            }
            Err(error) => {
                if !answered {
                    unanswered.get_or_insert_with(|| error.to_string());
                }
                continue;
            }
        };
        let Some((status, _)) = parse_async_merge(&output) else {
            continue;
        };
        if let Some(outcome) = async_merge_outcome(&status, &review.id, &output) {
            return outcome;
        }
    }
    // Not an error: `merge_and_check` re-reads the review after this returns
    // and reports "merge scheduled ... rerun `git stk sync`", which `--all`
    // breaks on cleanly with an accurate count. Returning `Err` instead would
    // route through `explain_merge_failure`, where a mid-merge state can be
    // classified as pending checks - replacing this with a wrong diagnosis and
    // aborting the run.
    Ok(match unanswered {
        Some(error) => format!(
            "{} was handed to GitHub to merge, but its result could not be \
             read: {error}",
            review.id
        ),
        None => format!(
            "{} is still merging on GitHub; `git stk sync` picks it up once it lands",
            review.id
        ),
    })
}

/// What an async-merge status means for the caller: `Some` once it is final,
/// `None` only while the merge is genuinely still running. Shared by the
/// enqueue response and each poll, which see the same statuses and must read
/// them the same way.
///
/// Only `pending` continues. This is a public-preview API, so its vocabulary
/// can grow: an unrecognised status is reported rather than polled against for
/// two minutes, since the likeliest fifth status is a terminal one this
/// version has never heard of.
fn async_merge_outcome(status: &str, id: &str, body: &str) -> Option<Result<String>> {
    match status {
        "merged" => Some(Ok(format!("merged {id} (stacked)"))),
        // A merge queue took it: it lands on its own schedule, and `sync`
        // picks that up later. Not a failure, and not something to wait on.
        "enqueued" => Some(Ok(format!("{id} added to the merge queue"))),
        // A refusal of the method is not a refusal of the merge: it is marked
        // so the caller can drop the method and ask again, and carries the
        // ordinary wording for when that has already been tried.
        "failed" => {
            let message = format!("{id} could not be merged: {body}");
            Some(Err(if merge_method_refused(body) {
                MergeMethodRefused(message).into()
            } else {
                anyhow!(message)
            }))
        }
        "pending" => None,
        other => Some(Err(anyhow!(
            "GitHub reported an unknown merge status {other:?} for {id}: {body}"
        ))),
    }
}

/// Whether GitHub refused the merge *because of* the `merge_method` parameter.
///
/// The refusal arrives as a `failed` status on a 200, not an HTTP error, so it
/// is only visible in the body. Matched on the parameter half of the message
/// rather than the "merge queue" half: the parameter being unsupported is what
/// dropping it answers, whatever GitHub names as the reason.
fn merge_method_refused(body: &str) -> bool {
    if !matches!(parse_async_merge(body), Some((status, _)) if status == "failed") {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    value
        .pointer("/details/message")
        .and_then(serde_json::Value::as_str)
        .map(str::to_lowercase)
        .is_some_and(|message| message.contains("merge param") || message.contains("merge_method"))
}

/// `(status, uuid)` from an async-merge response.
fn parse_async_merge(json: &str) -> Option<(String, Option<String>)> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let status = value.get("status")?.as_str()?.to_owned();
    let uuid = value
        .get("details")
        .and_then(|details| details.get("uuid"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some((status, uuid))
}

/// The repo's stack listing, cached for the life of the command.
///
/// Detection is unconditional (see `native_stack_for`), so this is asked once
/// per branch on `repair` and per retarget on `submit` - and on a repo without
/// the preview every one of those is a 404. One call per command is
/// the right cost for an answer that only changes when the stack does.
/// [`forget_stacks_listing`] owns that list and drops the cache at each, so
/// nothing reads a listing from before the write that invalidated it.
///
/// Best effort: the endpoint is in public preview, and a repo without the
/// feature answers 404. Either way "no stack" is the right read for a caller
/// using this as a hint - never an error that fails the command that asked.
fn stacks_listing(owner: &str, repo: &str) -> Result<String> {
    if let Some(cached) = STACKS_LISTING.with(|cell| cell.borrow().clone()) {
        return cached.map_err(|message| anyhow!("{message}"));
    }
    // `--paginate`: the listing keeps every dissolved and landed stack, so on
    // a long-lived repo the live one is not necessarily on the first page.
    let result = super::retry_server_errors(|| {
        command_output(
            "gh",
            &["api", "--paginate", &format!("repos/{owner}/{repo}/stacks")],
        )
    });
    // The 404 from a repo without the preview is cached too: not memoising it
    // left a `gh` subprocess per branch on exactly the repos that gain nothing
    // from asking. Any other failure may pass, so the next read asks again.
    let cached = match &result {
        Ok(output) => Some(Ok(output.clone())),
        Err(error) if is_not_found(error) => Some(Err(error.to_string())),
        Err(_) => None,
    };
    STACKS_LISTING.with(|cell| *cell.borrow_mut() = cached);
    result
}

fn is_not_found(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    text.contains("404") || text.contains("not found")
}

/// GitHub refused a synchronous merge because the pull request is in a stack.
fn needs_async_merge(error: &anyhow::Error) -> bool {
    format!("{error:#}")
        .to_lowercase()
        .contains("asynchronous merge rest api")
}

thread_local! {
    static STACKS_LISTING: std::cell::RefCell<Option<Result<String, String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Drop the cached listing, so the next read reflects a change to the stack.
/// Called wherever one happens - registering, extending, and landing a layer,
/// which GitHub reshapes the stack for on its own.
fn forget_stacks_listing() {
    STACKS_LISTING.with(|cell| *cell.borrow_mut() = None);
}

/// The digits of a review id like `#13`, for the REST payload.
fn review_number(id: &str) -> Option<String> {
    let digits = id.trim_start_matches('#');
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then(|| digits.to_owned())
}

/// The pull request numbers for `reviews`, or an error naming the id that
/// could not be read - registering a stack we cannot name in full would record
/// a different stack than the one submitted.
fn pull_request_numbers(reviews: &[String]) -> Result<Vec<String>> {
    reviews
        .iter()
        .map(|id| {
            review_number(id)
                .ok_or_else(|| anyhow!("could not read a pull request number from {id}"))
        })
        .collect()
}

/// `POST` an ordered `pull_requests` list to a stacks endpoint. `-F` sends the
/// numbers typed, which the API requires - `-f` would make them strings.
fn post_pull_requests(path: &str, numbers: &[String]) -> Result<String> {
    let fields: Vec<String> = numbers
        .iter()
        .map(|number| format!("pull_requests[]={number}"))
        .collect();
    let mut args = vec!["api", path, "-X", "POST"];
    for field in &fields {
        args.push("-F");
        args.push(field);
    }
    command_output("gh", &args)
}

/// The stack holding `branch`, from the `GET /repos/{owner}/{repo}/stacks`
/// payload. Bottom-first, since that is the order the API lists and the order
/// a stack lands in. Anything unparseable reads as "no stack" - this informs a
/// guess, so a shape we do not recognise must not fail the caller.
fn parse_native_stack(json: &str, branch: &str) -> Option<NativeStack> {
    let stacks: serde_json::Value = serde_json::from_str(json).ok()?;
    for stack in stacks.as_array()? {
        // The listing keeps dissolved and landed stacks - `open` is the only
        // thing separating a live one from history, and a branch that was in a
        // closed stack is not in a stack now.
        //
        // Requires an explicit `true`, so a renamed or absent field fails
        // closed. The two mistakes are not equal: reading a live stack as dead
        // costs a refused merge or retarget, loud and recoverable. Reading a
        // dead one as live makes `cleanup` skip a retarget it needed, and
        // GitHub then closes the child review when its base branch goes -
        // silently, with its comments and approvals. Same asymmetry the
        // whichever answer callers default to when the platform cannot be
        // reached is built on.
        if stack.get("open").and_then(serde_json::Value::as_bool) != Some(true) {
            continue;
        }
        // `continue`, not `?`: one stack with a payload this version cannot
        // read must not end the scan for every stack after it - the live one
        // is often not the first entry, since the listing keeps history.
        let Some(reviews) = stack
            .get("pull_requests")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        let holds_branch = reviews.iter().any(|review| {
            review
                .get("head")
                .and_then(|head| head.get("ref"))
                .and_then(serde_json::Value::as_str)
                == Some(branch)
        });
        if !holds_branch {
            continue;
        }
        return Some(NativeStack {
            number: stack.get("number")?.as_u64()?,
            base: stack.get("base")?.get("ref")?.as_str().map(str::to_owned)?,
            // All layers or none. `parent_of` and `can_base_on` are
            // positional, so dropping one unreadable layer silently shifts
            // every layer above it - and `repair` writes that shifted order as
            // `stkParent`, which `restack` then rebases and force-pushes
            // against. A stack we cannot read whole is not a stack we can
            // answer questions about.
            layers: reviews
                .iter()
                .map(|review| {
                    Some(NativeStackLayer {
                        id: format!("#{}", review.get("number")?.as_u64()?),
                        branch: review
                            .get("head")?
                            .get("ref")?
                            .as_str()
                            .map(str::to_owned)?,
                        // Absent reads as landed, which is the safe side here:
                        // it makes the stack's ordering answer "not current"
                        // and lets `repair` fall through to the review base,
                        // which the platform keeps up to date.
                        open: review.get("state").and_then(serde_json::Value::as_str)
                            == Some("open"),
                    })
                })
                .collect::<Option<Vec<_>>>()?,
        });
    }
    None
}

/// The current repository's `owner` and `name`, or None when gh cannot resolve
/// them (not a GitHub repo, or gh unavailable).
fn repo_owner_name() -> Option<(String, String)> {
    // Cached: it cannot change while a command runs, and it costs a `gh`
    // subprocess on paths that now ask per branch. The stack listing is cached
    // too, but with invalidation - see `stacks_listing` - because git-stk and
    // GitHub both reshape a stack midway through a command.
    repo_owner_name_result().ok()
}

/// [`repo_owner_name`], keeping the failure. `gh repo view` is the first call
/// to fail under an expired token, and discarding its error - which carries
/// `command_output`'s own "run `gh auth login`" hint - is what let a failed
/// lookup read as "no stack".
fn repo_owner_name_result() -> Result<(String, String)> {
    thread_local! {
        static REPO: std::cell::OnceCell<Result<(String, String), String>> =
            const { std::cell::OnceCell::new() };
    }
    REPO.with(|repo| {
        repo.get_or_init(|| resolve_repo_owner_name().map_err(|error| error.to_string()))
            .clone()
            .map_err(|message| anyhow!("{message}"))
    })
}

fn resolve_repo_owner_name() -> Result<(String, String)> {
    let output = command_output("gh", &["repo", "view", "--json", "nameWithOwner"])?;
    let value: serde_json::Value =
        serde_json::from_str(&output).context("failed to parse gh repo view JSON")?;
    let full = value
        .get("nameWithOwner")
        .and_then(serde_json::Value::as_str)
        .context("gh repo view did not report nameWithOwner")?;
    let (owner, repo) = full
        .split_once('/')
        .with_context(|| format!("unexpected repository name {full}"))?;
    Ok((owner.to_owned(), repo.to_owned()))
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

/// Reduce GitHub's `statusCheckRollup` - a mix of CheckRun nodes, which carry
/// a `name`, `status` and `conclusion`, and StatusContext nodes, which carry a
/// `context` and `state` - to one status.
///
/// The rollup holds *every* run recorded against the head commit, superseded
/// ones included. A workflow with a `concurrency` group cancels its in-flight
/// run when the branch is pushed again - which `restack` does to every branch
/// in a stack at once - leaving a cancelled entry beside the successful re-run
/// of the same check, on the same commit, forever. Aggregating the list as-is
/// pins the review red for as long as that commit is the head.
///
/// So: keep only the newest entry per check, which is how GitHub itself
/// decides whether a required check passed, then aggregate those. Any failure
/// wins, then anything still in flight, then anything that finished without a
/// verdict; otherwise passing.
fn aggregate_rollup(rollup: &serde_json::Value) -> CheckStatus {
    let Some(items) = rollup.as_array().filter(|items| !items.is_empty()) else {
        return CheckStatus::None;
    };

    // Newest per check - and a check is a job *within a workflow*, not a job
    // name. `name` on a CheckRun is only the job's name, so two workflows that
    // each define `build`, or an Actions job beside a same-named check from
    // another CI app, would collapse into one and the newer would hide the
    // other's failure. Pair it with the workflow (or the app, for a check with
    // no workflow run behind it), which is stable across re-runs - so the
    // cancelled run and the re-run that replaced it still group together.
    //
    // A StatusContext has no workflow: its `context` is already the full
    // identity. An entry with neither cannot be grouped, so it stands alone
    // under a key that cannot collide.
    let mut newest: BTreeMap<String, ((bool, String), &serde_json::Value)> = BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        let field = |name| item.get(name).and_then(serde_json::Value::as_str);
        // Two shapes for one fact: GraphQL nests the workflow under
        // `checkSuite`, and `gh pr view --json statusCheckRollup` flattens it
        // to `workflowName`. Both feed this function, so read either - missing
        // it entirely would drop the key back to the bare job name.
        let workflow = item
            .pointer("/checkSuite/workflowRun/workflow/name")
            .or_else(|| item.get("workflowName"))
            .or_else(|| item.pointer("/checkSuite/app/slug"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let key = match (field("name"), field("context")) {
            (Some(name), _) => format!("{workflow}\u{1}{name}"),
            (None, Some(context)) => format!("\u{2}{context}"),
            (None, None) => format!("\u{0}{index}"),
        };
        // One clock per comparison: a CheckRun's `startedAt` against another's,
        // a StatusContext's `createdAt` against another's. Mixing in
        // `completedAt` would rank a finished run against an unfinished one's
        // start time, which is how a cancelled entry outranks the re-run that
        // replaced it.
        //
        // A run still in flight wins outright, whatever the stamps say: it
        // cannot have been superseded by one that has already stopped, and a
        // queued run may not have a start time yet.
        let in_flight = field("status").is_some_and(|status| status != "COMPLETED")
            || matches!(field("state"), Some("PENDING" | "EXPECTED"));
        let stamp = field("startedAt")
            .or_else(|| field("createdAt"))
            .unwrap_or("")
            .to_owned();
        let rank = (in_flight, stamp);
        match newest.get(&key) {
            // RFC 3339 stamps from one host sort lexicographically. Ties keep
            // the later entry, which is the order the API returns them in.
            Some((seen, _)) if *seen > rank => {}
            _ => {
                newest.insert(key, (rank, item));
            }
        }
    }

    let mut pending = false;
    let mut inconclusive = false;
    for (_, item) in newest.values() {
        let field = |name| {
            item.get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        };
        let conclusion = field("conclusion");
        let status = field("status");
        let state = field("state");
        // A real failure: the check ran and did not pass.
        if matches!(conclusion, "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE")
            || matches!(state, "FAILURE" | "ERROR")
        {
            return CheckStatus::Failing;
        }
        // Finished without a verdict. Cancelled is not a failure - nothing
        // went wrong, the run was stopped - and `ACTION_REQUIRED` is waiting
        // on a person. Neither is green either: a required check in this state
        // still blocks the merge.
        if matches!(conclusion, "CANCELLED" | "ACTION_REQUIRED") {
            inconclusive = true;
        }
        // A CheckRun that has not COMPLETED, or a StatusContext still
        // expecting a result, is running.
        if (!status.is_empty() && status != "COMPLETED") || matches!(state, "PENDING" | "EXPECTED")
        {
            pending = true;
        }
    }
    if pending {
        CheckStatus::Pending
    } else if inconclusive {
        CheckStatus::Inconclusive
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

    #[test]
    fn githubs_stacked_merge_refusal_reads_as_needing_the_async_endpoint() {
        let refusal = anyhow!(
            "gh failed: GraphQL: This pull request is part of a stack and must be merged using \
             the asynchronous merge REST API. For more information, see \
             https://docs.github.com/rest/pulls/pulls#merge-a-pull-request-asynchronously \
             (mergePullRequest)"
        );
        assert!(needs_async_merge(&refusal));
        assert!(!needs_async_merge(&anyhow!(
            "gh failed: GraphQL: Pull request is not mergeable"
        )));
    }

    #[test]
    fn only_a_missing_stacks_feature_counts_as_not_found() {
        assert!(is_not_found(&anyhow!("gh: Not Found (HTTP 404)")));
        assert!(!is_not_found(&anyhow!("gh: Bad Gateway (HTTP 502)")));
        assert!(!is_not_found(&anyhow!(
            "error connecting to api.github.com"
        )));
    }
    use crate::providers::{CheckStatus, ReviewRequest, ReviewState};

    fn submission(event: ReviewEvent) -> Submission {
        Submission {
            event,
            body: "Summary".into(),
            comments: vec![super::super::DraftComment {
                path: "src/a.rs".into(),
                old_path: "src/a.rs".into(),
                anchor: Anchor {
                    side: Side::Old,
                    line: 9,
                    start_line: Some(7),
                },
                paired_line: None,
                body: "Why?".into(),
            }],
            commit: "abc".into(),
        }
    }

    #[test]
    fn github_review_request_carries_every_comment() {
        let request = github_review_request(&submission(ReviewEvent::RequestChanges));
        assert_eq!(
            request,
            serde_json::json!({
                "commit_id": "abc",
                "body": "Summary",
                "event": "REQUEST_CHANGES",
                "comments": [{
                    "path": "src/a.rs", "body": "Why?", "line": 9, "side": "LEFT",
                    "start_line": 7, "start_side": "LEFT",
                }],
            })
        );
    }

    #[test]
    fn github_conversation_reads_threads_reviews_and_comments() {
        let output = r#"{"data":{"repository":{"pullRequest":{
            "comments":{"totalCount":1,"nodes":[{"id":"IC1","author":{"login":"ann"},"body":"Looks good","createdAt":"2026-10-11T01:00:00Z","url":"u1"}]},
            "reviews":{"totalCount":3,"nodes":[
                {"author":{"login":"bo"},"state":"CHANGES_REQUESTED","body":"Fix it","submittedAt":"2026-10-11T02:00:00Z","url":"u2"},
                {"author":null,"state":"PENDING","body":"","submittedAt":null,"url":"u3"}]},
            "reviewThreads":{"totalCount":3,"nodes":[
                {"id":"T1","isResolved":false,"isOutdated":false,"path":"src/a.rs","line":12,"originalLine":10,"startLine":10,"originalStartLine":8,"diffSide":"LEFT","subjectType":"LINE",
                 "comments":{"totalCount":1,"nodes":[{"id":"C1","author":{"login":"bo"},"body":"Why?","createdAt":"t","url":"u4","diffHunk":"@@ -1 +1 @@"}]}},
                {"id":"T2","isResolved":true,"isOutdated":true,"path":"src/b.rs","line":null,"originalLine":4,"startLine":null,"originalStartLine":null,"diffSide":"RIGHT","subjectType":"LINE",
                 "comments":{"totalCount":1,"nodes":[{"id":"C2","author":{"login":"ann"},"body":"Old","createdAt":"t","url":"u5","diffHunk":""}]}},
                {"id":"T3","isResolved":false,"isOutdated":false,"path":"README.md","line":null,"originalLine":null,"startLine":null,"originalStartLine":null,"diffSide":"RIGHT","subjectType":"FILE",
                 "comments":{"totalCount":1,"nodes":[{"id":"C3","author":{"login":"ann"},"body":"File","createdAt":"t","url":"u6","diffHunk":""}]}}]}}}}}"#;
        let conversation = parse_github_conversation(output).unwrap();
        assert!(
            conversation.incomplete,
            "a review count above what came back"
        );
        assert_eq!(conversation.comments[0].author, "ann");
        assert_eq!(
            conversation.verdicts.len(),
            1,
            "a pending review is a draft"
        );
        assert_eq!(
            conversation.verdicts[0].state,
            VerdictState::ChangesRequested
        );
        let [current, outdated, file] = &conversation.threads[..] else {
            panic!("three threads");
        };
        assert_eq!(
            current.anchor,
            Some(Anchor {
                side: Side::Old,
                line: 12,
                start_line: Some(10)
            })
        );
        assert_eq!(current.resolved, Some(false));
        assert_eq!(current.diff_hunk.as_deref(), Some("@@ -1 +1 @@"));
        assert!(outdated.outdated);
        assert_eq!(outdated.anchor.map(|anchor| anchor.line), Some(4));
        assert_eq!(outdated.resolved, Some(true));
        assert_eq!(outdated.diff_hunk, None);
        assert_eq!(file.anchor, None);
    }

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

    /// The shape from #331: a workflow with a `concurrency` group is cancelled
    /// when the branch is pushed again - which `restack` does to every branch
    /// at once - and re-run seconds later on the same head. Both entries stay
    /// in the rollup for as long as that commit is the head, so aggregating
    /// the list as-is pins the review red forever.
    #[test]
    fn aggregate_rollup_keeps_only_the_newest_run_of_each_check() {
        let superseded = serde_json::json!([
            {"name": "plan", "status": "COMPLETED", "conclusion": "SUCCESS",
             "startedAt": "2026-08-29T23:01:56Z"},
            {"name": "plan", "status": "COMPLETED", "conclusion": "CANCELLED",
             "startedAt": "2026-08-29T23:01:52Z"},
            {"name": "test", "status": "COMPLETED", "conclusion": "SUCCESS",
             "startedAt": "2026-08-29T23:01:50Z"},
        ]);
        assert_eq!(aggregate_rollup(&superseded), CheckStatus::Passing);

        // The other order, since the API does not promise one.
        let reversed = serde_json::json!([
            {"name": "plan", "status": "COMPLETED", "conclusion": "CANCELLED",
             "startedAt": "2026-08-29T23:01:52Z"},
            {"name": "plan", "status": "COMPLETED", "conclusion": "SUCCESS",
             "startedAt": "2026-08-29T23:01:56Z"},
        ]);
        assert_eq!(aggregate_rollup(&reversed), CheckStatus::Passing);

        // A real failure still wins, superseded or not.
        let failed = serde_json::json!([
            {"name": "plan", "status": "COMPLETED", "conclusion": "CANCELLED",
             "startedAt": "2026-08-29T23:01:52Z"},
            {"name": "plan", "status": "COMPLETED", "conclusion": "FAILURE",
             "startedAt": "2026-08-29T23:01:56Z"},
        ]);
        assert_eq!(aggregate_rollup(&failed), CheckStatus::Failing);
    }

    /// Cancelled is not a failure - nothing went wrong, the run was stopped -
    /// and not a pass either, since a required check in that state still
    /// blocks the merge. Only when it is the newest run of its check.
    #[test]
    fn aggregate_rollup_reads_a_current_cancellation_as_inconclusive() {
        let cancelled = serde_json::json!([
            {"name": "plan", "status": "COMPLETED", "conclusion": "CANCELLED"},
            {"name": "test", "status": "COMPLETED", "conclusion": "SUCCESS"},
        ]);
        assert_eq!(aggregate_rollup(&cancelled), CheckStatus::Inconclusive);

        // Waiting on a person is the same kind of answer.
        let manual = serde_json::json!([
            {"name": "deploy", "status": "COMPLETED", "conclusion": "ACTION_REQUIRED"},
        ]);
        assert_eq!(aggregate_rollup(&manual), CheckStatus::Inconclusive);

        // Something still running outranks it: it may yet resolve.
        let running = serde_json::json!([
            {"name": "plan", "status": "COMPLETED", "conclusion": "CANCELLED"},
            {"name": "test", "status": "IN_PROGRESS"},
        ]);
        assert_eq!(aggregate_rollup(&running), CheckStatus::Pending);

        // And the re-run *of the same check* that replaced the cancelled one,
        // still in flight and with no start time of its own yet. Ranking it by
        // the cancelled entry's completion time would hide a running build.
        let replaced = serde_json::json!([
            {"name": "plan", "status": "COMPLETED", "conclusion": "CANCELLED",
             "startedAt": "2026-08-29T23:01:52Z", "completedAt": "2026-08-29T23:01:59Z"},
            {"name": "plan", "status": "QUEUED"},
        ]);
        assert_eq!(aggregate_rollup(&replaced), CheckStatus::Pending);
    }

    /// Past the cap the oldest entries fall off, and a check with no entry is
    /// not a failure - so answering from the slice could paint a red commit
    /// green. GitHub's own aggregate sees everything, superseded runs and all.
    #[test]
    fn parse_annotation_batch_falls_back_to_the_aggregate_when_truncated() {
        let truncated = r#"{"data":{"repository":{
            "p0":{"nodes":[{"number":9,"headRefName":"feature/a","mergeQueueEntry":null,
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE",
                    "contexts":{"totalCount":140,"nodes":[
                        {"name":"build","workflowName":"CI","status":"COMPLETED","conclusion":"SUCCESS"}
                    ]}}}}]}}]}
        }}}"#;
        let annotations = parse_annotation_batch(truncated, false).expect("parse");
        // The slice says passing; the aggregate says otherwise, and it is the
        // one that saw the whole list.
        assert_eq!(annotations["feature/a"].checks, CheckStatus::Failing);

        // Within the cap, the deduplicated read stands.
        let whole = truncated.replace(r#""totalCount":140"#, r#""totalCount":1"#);
        let annotations = parse_annotation_batch(&whole, false).expect("parse");
        assert_eq!(annotations["feature/a"].checks, CheckStatus::Passing);
    }

    /// A job name is not a check identity. Two workflows can each define
    /// `build`, as can an Actions job beside a same-named check from another
    /// CI app - grouping on the name alone would keep only the newer and hide
    /// the other's failure, which is worse than the red dot this all started
    /// with.
    #[test]
    fn aggregate_rollup_does_not_merge_the_same_job_name_across_workflows() {
        let two_workflows = serde_json::json!([
            {"name": "build", "status": "COMPLETED", "conclusion": "SUCCESS",
             "startedAt": "2026-08-29T23:01:56Z",
             "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}},
            {"name": "build", "status": "COMPLETED", "conclusion": "FAILURE",
             "startedAt": "2026-08-29T23:01:52Z",
             "checkSuite": {"workflowRun": {"workflow": {"name": "Release"}}}},
        ]);
        assert_eq!(aggregate_rollup(&two_workflows), CheckStatus::Failing);

        // The same rollup as `gh pr view --json statusCheckRollup` sends it,
        // which flattens the workflow to `workflowName` and omits
        // `checkSuite` entirely. `check_status` feeds that shape in.
        let flattened = serde_json::json!([
            {"name": "build", "status": "COMPLETED", "conclusion": "SUCCESS",
             "startedAt": "2026-08-29T23:01:56Z", "workflowName": "CI"},
            {"name": "build", "status": "COMPLETED", "conclusion": "FAILURE",
             "startedAt": "2026-08-29T23:01:52Z", "workflowName": "Release"},
        ]);
        assert_eq!(aggregate_rollup(&flattened), CheckStatus::Failing);

        // A check with no workflow run behind it is identified by its app.
        let other_app = serde_json::json!([
            {"name": "build", "status": "COMPLETED", "conclusion": "SUCCESS",
             "startedAt": "2026-08-29T23:01:56Z",
             "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}},
            {"name": "build", "status": "COMPLETED", "conclusion": "FAILURE",
             "startedAt": "2026-08-29T23:01:52Z",
             "checkSuite": {"app": {"slug": "buildkite"}}},
        ]);
        assert_eq!(aggregate_rollup(&other_app), CheckStatus::Failing);

        // But the same job in the same workflow, run twice, is still one
        // check - which is the whole point of the deduplication.
        let same_workflow = serde_json::json!([
            {"name": "build", "status": "COMPLETED", "conclusion": "SUCCESS",
             "startedAt": "2026-08-29T23:01:56Z",
             "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}},
            {"name": "build", "status": "COMPLETED", "conclusion": "CANCELLED",
             "startedAt": "2026-08-29T23:01:52Z",
             "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}},
        ]);
        assert_eq!(aggregate_rollup(&same_workflow), CheckStatus::Passing);
    }

    /// Legacy commit statuses identify and timestamp themselves differently,
    /// and are deduplicated on their own terms.
    #[test]
    fn aggregate_rollup_deduplicates_status_contexts_too() {
        let contexts = serde_json::json!([
            {"context": "ci/travis", "state": "SUCCESS", "createdAt": "2026-08-29T23:01:56Z"},
            {"context": "ci/travis", "state": "FAILURE", "createdAt": "2026-08-29T23:01:52Z"},
        ]);
        assert_eq!(aggregate_rollup(&contexts), CheckStatus::Passing);

        // An entry with neither name nor context cannot be grouped, so it
        // stands alone rather than colliding with its neighbours.
        let anonymous = serde_json::json!([
            {"state": "SUCCESS"},
            {"state": "FAILURE"},
        ]);
        assert_eq!(aggregate_rollup(&anonymous), CheckStatus::Failing);
    }

    #[test]
    fn build_annotation_query_includes_reviews_only_with_detail() {
        let plain = build_annotation_query(2, false);
        assert!(plain.contains("$h0:String!") && plain.contains("$h1:String!"));
        assert!(plain.contains("statusCheckRollup"));
        assert!(plain.contains("mergeQueueEntry"));
        assert!(!plain.contains("latestReviews"));
        assert!(build_annotation_query(1, true).contains("latestReviews"));

        // The cap the truncation guard reads is the cap the query asks for.
        assert!(plain.contains(&format!("contexts(last:{ROLLUP_CONTEXTS})")));
        assert!(plain.contains("totalCount"));
    }

    /// The query is assembled by hand and never sent by these tests, so an
    /// unbalanced brace reaches the host - where it degrades to the per-branch
    /// fallback and looks like an ordinary answer. Counting is cheap and would
    /// have caught it.
    #[test]
    fn build_annotation_query_balances() {
        for (count, detail) in [(1, false), (2, false), (1, true), (3, true)] {
            let query = build_annotation_query(count, detail);
            assert_eq!(
                query.chars().filter(|c| *c == '{').count(),
                query.chars().filter(|c| *c == '}').count(),
                "unbalanced braces for ({count}, {detail}): {query}"
            );
        }
    }

    #[test]
    fn parse_annotation_batch_reads_each_prs_status_queue_and_reviews() {
        let json = r#"{"data":{"repository":{
            "p0":{"nodes":[{"number":9,"headRefName":"feature/a","mergeQueueEntry":null,
                "latestReviews":{"nodes":[{"state":"APPROVED"},{"state":"APPROVED"}]},
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
                    {"name":"build","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-01-01T00:00:02Z"},
                    {"name":"build","status":"COMPLETED","conclusion":"CANCELLED","startedAt":"2026-01-01T00:00:01Z"}]}}}}]}}]},
            "p1":{"nodes":[{"number":10,"headRefName":"feature/b","mergeQueueEntry":{"state":"QUEUED"},
                "latestReviews":{"nodes":[{"state":"CHANGES_REQUESTED"}]},
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
                    {"name":"build","status":"COMPLETED","conclusion":"FAILURE"}]}}}}]}}]},
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
            "mergeQueueEntry":null,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
                {"name":"build","status":"IN_PROGRESS"}]}}}}]}}]}}}}"#;
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

    #[test]
    fn parse_native_stack_finds_the_stack_holding_a_branch_bottom_first() {
        let json = r#"[
          {"number": 4, "open": true, "base": {"ref": "develop"},
           "pull_requests": [{"number": 13, "head": {"ref": "fix/shared"}},
                             {"number": 14, "head": {"ref": "fix/above"}}]},
          {"number": 7, "open": true, "base": {"ref": "main"},
           "pull_requests": [{"number": 20, "head": {"ref": "other/work"}}]}
        ]"#;

        let stack = parse_native_stack(json, "fix/above").expect("stack");
        assert_eq!(stack.number, 4);
        assert_eq!(stack.base, "develop");
        assert_eq!(
            stack
                .layers
                .iter()
                .map(|l| l.id.as_str())
                .collect::<Vec<_>>(),
            vec!["#13", "#14"]
        );
        // The parent each layer stacks on, per the platform.
        assert_eq!(stack.parent_of("fix/shared"), Some("develop"));
        assert_eq!(stack.parent_of("fix/above"), Some("fix/shared"));
        assert_eq!(stack.parent_of("not/in/it"), None);

        // The other stack is found by its own branch, not by position.
        assert_eq!(
            parse_native_stack(json, "other/work")
                .expect("stack")
                .number,
            7
        );
    }

    #[test]
    fn parse_native_stack_ignores_a_stack_that_is_no_longer_open() {
        // The listing keeps dissolved and landed stacks. A branch that was in
        // one is not in a stack now, and treating it as stacked would send its
        // merge down the async path for a review GitHub no longer stacks.
        let json = r#"[
          {"number": 3, "open": false, "base": {"ref": "main"},
           "pull_requests": [{"number": 1, "head": {"ref": "feature/a"}},
                             {"number": 2, "head": {"ref": "feature/b"}}]},
          {"number": 9, "open": true, "base": {"ref": "main"},
           "pull_requests": [{"number": 20, "head": {"ref": "live/one"}},
                             {"number": 21, "head": {"ref": "live/two"}}]}
        ]"#;
        assert_eq!(parse_native_stack(json, "feature/a"), None);
        assert_eq!(
            parse_native_stack(json, "live/two").expect("stack").number,
            9
        );

        // Absent fails closed: reading a dead stack as live makes `cleanup`
        // skip a retarget, after which GitHub closes the child review
        // silently. A live stack read as dead only costs a refused merge.
        let no_field = r#"[{"number": 4, "base": {"ref": "main"},
            "pull_requests": [{"number": 13, "head": {"ref": "fix/shared"}}]}]"#;
        assert_eq!(parse_native_stack(no_field, "fix/shared"), None);
    }

    #[test]
    fn parse_native_stack_reads_an_unknown_branch_or_shape_as_no_stack() {
        let json = r#"[{"number": 4, "open": true, "base": {"ref": "develop"},
                        "pull_requests": [{"number": 13, "head": {"ref": "fix/shared"}}]}]"#;
        assert_eq!(parse_native_stack(json, "not/in/it"), None);

        // Empty listing, and payloads this version does not recognise: all
        // "no stack" rather than an error, since this only informs a guess.
        assert_eq!(parse_native_stack("[]", "fix/shared"), None);
        assert_eq!(
            parse_native_stack(r#"{"message":"Not Found"}"#, "fix/shared"),
            None
        );
        assert_eq!(parse_native_stack("not json", "fix/shared"), None);
    }

    /// A stack this version cannot read must cost only itself. Both routes end
    /// in a wrong `stkParent` otherwise: an unreadable entry aborting the scan
    /// hides the live stack behind it, and an unreadable *layer* silently
    /// shifts every layer above it, since `parent_of` is positional.
    #[test]
    fn parse_native_stack_skips_an_unreadable_stack_rather_than_the_rest() {
        // The live stack sits behind one with no `pull_requests` - the listing
        // keeps history, so it is routinely not first.
        let odd_entry = r#"[
          {"number": 2, "open": true, "base": {"ref": "main"}},
          {"number": 9, "open": true, "base": {"ref": "main"},
           "pull_requests": [{"number": 20, "head": {"ref": "live/one"}},
                             {"number": 21, "head": {"ref": "live/two"}}]}
        ]"#;
        let stack = parse_native_stack(odd_entry, "live/two").expect("the stack behind it");
        assert_eq!(stack.number, 9);
        assert_eq!(stack.parent_of("live/two"), Some("live/one"));

        // A layer that cannot be read makes the whole stack unanswerable
        // rather than shifting the ones above it: dropping #21 here would say
        // `live/three` stacks on `live/one`, which is a layer too low.
        let odd_layer = r#"[
          {"number": 9, "open": true, "base": {"ref": "main"},
           "pull_requests": [{"number": 20, "head": {"ref": "live/one"}},
                             {"head": {"ref": "live/two"}},
                             {"number": 22, "head": {"ref": "live/three"}}]}
        ]"#;
        assert_eq!(parse_native_stack(odd_layer, "live/three"), None);
    }

    #[test]
    fn parse_async_merge_reads_status_and_the_result_id() {
        let enqueued = r#"{"status":"pending","details":{"message":"Merge request enqueued.",
            "uuid":"5f4d46b3-d67d-4310-91d2-d2e2717f0341","merge_method":"squash"}}"#;
        assert_eq!(
            parse_async_merge(enqueued),
            Some((
                "pending".to_owned(),
                Some("5f4d46b3-d67d-4310-91d2-d2e2717f0341".to_owned())
            ))
        );

        // The polled result carries no uuid - the status is the whole answer.
        let done = r#"{"status":"merged","details":{"message":"Pull request was merged.",
            "sha":"77775602411442f6864e4dd7c0823a7cfb957464"}}"#;
        assert_eq!(parse_async_merge(done), Some(("merged".to_owned(), None)));

        assert_eq!(parse_async_merge("{}"), None);
        assert_eq!(parse_async_merge("not json"), None);
    }

    #[test]
    fn review_number_reads_a_pull_request_id() {
        assert_eq!(review_number("#13"), Some("13".to_owned()));
        assert_eq!(review_number("13"), Some("13".to_owned()));
        // A GitLab-style `!13` or anything non-numeric is not ours to send.
        assert_eq!(review_number("!13"), None);
        assert_eq!(review_number("#"), None);
        assert_eq!(review_number(""), None);
    }

    /// `true` only for a queue that is really there: the caller turns it into
    /// merging the top of a stack, which lands that review into the layer
    /// below unless a queue cascades it.
    #[test]
    fn parse_base_merge_queue_says_yes_only_to_a_queue() {
        assert!(parse_base_merge_queue(
            r#"{"data":{"repository":{"mergeQueue":{"id":"MQ_1"}}}}"#
        ));
        assert!(!parse_base_merge_queue(
            r#"{"data":{"repository":{"mergeQueue":null}}}"#
        ));
        // A GraphQL error carries `data: null` beside `errors`, and an absent
        // key reads the same way: no queue was reported, so none is assumed.
        assert!(!parse_base_merge_queue(
            r#"{"data":null,"errors":[{"message":"Could not resolve to a Repository"}]}"#
        ));
        assert!(!parse_base_merge_queue(r#"{"data":{"repository":{}}}"#));
        assert!(!parse_base_merge_queue("not json"));
        assert!(!parse_base_merge_queue(""));
    }

    /// The merge-queue refusal arrives as a `failed` status on a 200, so it is
    /// only distinguishable from a real merge failure by its message.
    #[test]
    fn merge_method_refused_reads_the_parameter_complaint_only() {
        let refusal = r#"{"status":"failed","details":{"message":"Custom merge params are not supported when merging via a merge queue"}}"#;
        assert!(merge_method_refused(refusal));
        assert!(merge_method_refused(
            r#"{"status":"failed","details":{"message":"merge_method may not be set here"}}"#
        ));
        // A real merge failure must still be reported, not silently retried.
        assert!(!merge_method_refused(
            r#"{"status":"failed","details":{"message":"Base branch was modified"}}"#
        ));
        assert!(!merge_method_refused(r#"{"status":"failed"}"#));
        // Only a refusal is a refusal: a merge that succeeded is not retried
        // however its message reads.
        assert!(!merge_method_refused(
            r#"{"status":"merged","details":{"message":"custom merge params"}}"#
        ));
        assert!(!merge_method_refused("not json"));
    }

    #[test]
    fn async_merge_outcome_continues_only_while_pending() {
        assert_eq!(
            async_merge_outcome("merged", "#12", "{}")
                .expect("final")
                .unwrap(),
            "merged #12 (stacked)"
        );
        assert_eq!(
            async_merge_outcome("enqueued", "#12", "{}")
                .expect("final")
                .unwrap(),
            "#12 added to the merge queue"
        );
        assert!(
            async_merge_outcome("failed", "#12", "{}")
                .expect("final")
                .unwrap_err()
                .to_string()
                .contains("#12 could not be merged")
        );

        // Only `pending` keeps polling.
        assert!(async_merge_outcome("pending", "#12", "{}").is_none());

        // A public-preview vocabulary can grow: an unrecognised status is
        // reported, not polled against for two minutes.
        let unknown = async_merge_outcome("conflicted", "#12", "{}")
            .expect("final")
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("unknown merge status"), "got: {unknown}");
        assert!(
            unknown.contains("conflicted"),
            "names the status: {unknown}"
        );
    }

    #[test]
    fn parse_stack_position_reads_a_layer_or_nothing() {
        let stacked = serde_json::json!({
            "stack": {"number": 6, "size": 3},
            "stackEntry": {"position": 2}
        });
        assert_eq!(
            parse_stack_position(&stacked),
            Some(StackPosition {
                number: 6,
                position: 2,
                size: 3
            })
        );

        // No stack, and an older GitHub that does not know the fields: both
        // read as "not in a stack" rather than as an error.
        assert_eq!(
            parse_stack_position(&serde_json::json!({"stack": null})),
            None
        );
        assert_eq!(parse_stack_position(&serde_json::json!({})), None);
    }

    #[test]
    fn build_annotation_query_asks_for_the_stack_fields() {
        let query = build_annotation_query(1, false);
        assert!(query.contains("stack{number size}"), "got: {query}");
        assert!(query.contains("stackEntry{position}"), "got: {query}");
    }

    #[test]
    fn build_annotation_query_drops_the_stack_fields_when_unsupported() {
        // A host whose schema lacks them rejects the whole query, so the
        // retry has to be able to ask without them.
        let plain = build_annotation_query_with(1, false, false);
        assert!(!plain.contains("stack{number size}"), "got: {plain}");
        assert!(!plain.contains("stackEntry"), "got: {plain}");
        // Everything else the annotations need is still there.
        assert!(plain.contains("mergeQueueEntry"));
        assert!(plain.contains("statusCheckRollup"));

        let full = build_annotation_query_with(1, false, true);
        assert!(full.contains("stack{number size}"));
        assert!(full.contains("stackEntry{position}"));
    }

    #[test]
    fn outside_leaves_out_you_then_each_organization() {
        let owners = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(outside_search(&owners(&["me"])), "author:@me -user:me");
        assert_eq!(
            outside_search(&owners(&["me", "acme"])),
            "author:@me -user:me -org:acme"
        );
        assert_eq!(
            outside_search(&owners(&["me", "acme", "widgets"])),
            "author:@me -user:me -org:acme -org:widgets"
        );
    }

    #[test]
    fn the_owners_put_your_login_first() {
        let owners = parse_github_owners(
            r#"{"data":{"viewer":{"login":"me","organizations":{"nodes":[{"login":"acme"},{"login":"widgets"}]}}}}"#,
        )
        .unwrap();
        assert_eq!(owners, ["me", "acme", "widgets"]);
        let alone = parse_github_owners(r#"{"data":{"viewer":{"login":"me"}}}"#).unwrap();
        assert_eq!(alone, ["me"]);
        assert!(parse_github_owners(r#"{"data":{"viewer":{}}}"#).is_err());
    }
}
