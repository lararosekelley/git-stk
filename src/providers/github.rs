use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};

use crate::git;

use super::json::{
    all_reviews, first_review, optional_bool, optional_string, parse_body_field, parse_state,
    required_string,
};
use super::{
    CheckStatus, MergeBlocker, NativeStack, NativeStackLayer, ReviewAnnotation, ReviewProvider,
    ReviewRequest, ReviewState, ReviewSummary, StackPosition, WaitOutcome, command_output,
    generic_annotate, merge_with_retry,
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

    fn merge_review(&self, review: &ReviewRequest, strategy: &str, auto: bool) -> Result<String> {
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
            // Only the enqueue is retried: the `PUT` is not idempotent, so
            // wrapping the wait in the retry too would re-ask GitHub to merge
            // a review that is already merging whenever a poll blipped.
            let (path, output) = merge_with_retry(|| enqueue_async_merge(review, strategy))?;
            return await_async_merge(review, &path, &output);
        }

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

/// Build the aliased GraphQL query for [`batched_annotate`]. Kept out of
/// `format!` to dodge brace-escaping; the fields are exactly what
/// [`ReviewAnnotation`] needs, no more.
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
        aliases.push_str(
            "commits(last:1){nodes{commit{statusCheckRollup{contexts(last:100){nodes{\
             __typename ... on CheckRun{name status conclusion startedAt \
             checkSuite{workflowRun{workflow{name}} app{slug}}} \
             ... on StatusContext{context state createdAt}}}}}}}}",
        );
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
        let checks = node
            .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
            .map_or(CheckStatus::None, aggregate_rollup);
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

/// Ask GitHub to start the merge. Split from the wait because the `PUT` is not
/// idempotent: retrying it after a transient failure *later* in the sequence
/// would ask for a second merge of a review already merging.
fn enqueue_async_merge(review: &ReviewRequest, strategy: &str) -> Result<(String, String)> {
    let (owner, repo) = repo_owner_name().context("could not resolve the GitHub repository")?;
    let number = review_number(&review.id)
        .with_context(|| format!("could not read a pull request number from {}", review.id))?;
    let path = format!("repos/{owner}/{repo}/pulls/{number}/merge-async");
    let method = format!("merge_method={strategy}");
    let output = command_output("gh", &["api", &path, "-X", "PUT", "-f", &method])?;
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
        "failed" => Some(Err(anyhow!("{id} could not be merged: {body}"))),
        "pending" => None,
        other => Some(Err(anyhow!(
            "GitHub reported an unknown merge status {other:?} for {id}: {body}"
        ))),
    }
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
    let result = command_output(
        "gh",
        &["api", "--paginate", &format!("repos/{owner}/{repo}/stacks")],
    );
    // Failures are cached too. The common one is the 404 from a repo without
    // the preview, and not memoising it left a `gh` subprocess per branch on
    // exactly the repos that gain nothing from asking.
    STACKS_LISTING.with(|cell| {
        *cell.borrow_mut() = Some(match &result {
            Ok(output) => Ok(output.clone()),
            Err(error) => Err(error.to_string()),
        });
    });
    result
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
        let workflow = item
            .pointer("/checkSuite/workflowRun/workflow/name")
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
}
