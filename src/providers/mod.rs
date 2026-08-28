use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use std::{fmt, process::Command};

use anyhow::{Context, Result, anyhow, bail};

use crate::git;
use crate::settings;

/// How long to keep polling a "no checks / no pipeline yet" result before
/// concluding there genuinely are none. A just-pushed branch's checks take a
/// moment to register, so concluding too early would either merge without
/// waiting or report a false failure.
pub(super) const CHECK_GRACE_POLLS: u32 = 6;

/// Delay between `wait_for_checks` polls.
pub(super) fn check_poll_interval() -> Duration {
    Duration::from_secs(5)
}

/// The error a `wait_for_checks` loop returns when its `stk.checkTimeout`
/// ceiling elapses with the checks still unsettled - so a pipeline that never
/// reports does not block `merge --wait` forever.
pub(super) fn checks_timed_out(review: &ReviewRequest, timeout: Duration) -> anyhow::Error {
    anyhow!(
        "{}'s checks have not settled within {}; rerun `git stk merge` once they pass, \
         or raise stk.checkTimeout",
        review.id,
        humanize(timeout),
    )
}

/// A whole-minute duration as "30m"; otherwise plain seconds.
fn humanize(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

mod demo;
mod gitea;
mod github;
mod gitlab;
mod json;

use demo::DemoProvider;
use gitea::GiteaProvider;
use github::GitHubProvider;
use gitlab::GitLabProvider;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ProviderKind {
    GitHub,
    GitLab,
    Gitea,
    /// Offline stand-in: reviews in `.git`, merges as local squashes. Only
    /// ever selected explicitly via `stk.provider = demo`.
    Demo,
}

impl ProviderKind {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "github" | "gh" => Some(Self::GitHub),
            "gitlab" | "glab" => Some(Self::GitLab),
            "gitea" | "tea" => Some(Self::Gitea),
            "demo" => Some(Self::Demo),
            _ => None,
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitHub => write!(formatter, "github"),
            Self::GitLab => write!(formatter, "gitlab"),
            Self::Gitea => write!(formatter, "gitea"),
            Self::Demo => write!(formatter, "demo"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct DetectedProvider {
    pub kind: ProviderKind,
    pub source: ProviderSource,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ProviderSource {
    Config,
    Remote { remote: String, url: String },
}

impl fmt::Display for ProviderSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config => write!(formatter, "config"),
            Self::Remote { remote, url } => {
                write!(formatter, "remote {remote} ({})", redact_url(url))
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReviewState {
    Open,
    Merged,
    Closed,
    Unknown(String),
}

/// A structural reason the platform won't merge a review, read from its API
/// rather than its error text - so a wording change can't silently reclassify
/// a real failure. `None` means nothing structural blocks the merge, or the
/// platform did not say (the caller falls back to matching the error text).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MergeBlocker {
    /// Required checks or reviews have not passed yet.
    ChecksPending,
    /// The review conflicts with its base branch.
    Conflicts,
    /// Nothing structural blocks the merge, or the platform did not say.
    None,
}

/// One review in a platform-recorded stack.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NativeStackLayer {
    pub id: String,
    pub branch: String,
}

/// A stack as the platform records it: its layers bottom first - the order it
/// lands in - and the branch the bottom one targets. Read-only here;
/// registering one is a separate step, gated behind `stk.githubStacks`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NativeStack {
    /// The platform's own number for the stack, for messages.
    pub number: u64,
    /// The branch the bottom review targets.
    pub base: String,
    /// Layers bottom first.
    ///
    /// Verified live rather than assumed: registering three chained pull
    /// requests (`main <- p <- q <- r`) lists them in exactly that order, and
    /// `POST /stacks/{n}/add` appends - a fourth review arrives last. That
    /// append-only behaviour is why [`plan_stack_registration`] will only
    /// extend a stack the submitted line grew on top of; `/add` carries no
    /// position, so it cannot express anything else.
    pub layers: Vec<NativeStackLayer>,
}

impl NativeStack {
    /// Whether this stack can still bring `branch`'s base to `parent` on its
    /// own - `parent` is somewhere the platform puts a base: the stack's own
    /// base, or a layer still between them.
    ///
    /// This is the question every caller has, and it needs the local parent
    /// to answer. A base and a parent that disagree while the stack can still
    /// close the gap is a chain part-way through unwinding: the platform
    /// retargets each layer onto the stack's base as the one below it lands,
    /// and `cleanup` walks the local parents the same way. A parent the stack
    /// cannot reach - a line re-rooted onto a release branch, say - is a
    /// disagreement nothing will resolve, and callers say so instead of
    /// waiting for it.
    pub fn can_base_on(&self, branch: &str, parent: &str) -> bool {
        // Two destinations, not "any layer": the platform sets a layer's base
        // to the one recorded below it, and moves it to the stack's own base
        // once that lands. A layer *above* this one is neither - accepting it
        // would put a reordered stack's bottom back inside the exemption, and
        // the bottom is the one layer the platform never retargets.
        self.layers.iter().any(|layer| layer.branch == branch)
            && (self.base == parent || self.parent_of(branch) == Some(parent))
    }

    /// What `branch` stacks on according to the platform: the branch below it,
    /// or the stack's base when it is the bottom. `None` when the stack does
    /// not hold `branch` at all.
    pub fn parent_of(&self, branch: &str) -> Option<&str> {
        let index = self
            .layers
            .iter()
            .position(|layer| layer.branch == branch)?;
        Some(match index.checked_sub(1) {
            Some(below) => &self.layers[below].branch,
            None => &self.base,
        })
    }

    /// The review id recorded for `branch`, for messages.
    pub fn review_id_for(&self, branch: &str) -> Option<&str> {
        self.layers
            .iter()
            .find(|layer| layer.branch == branch)
            .map(|layer| layer.id.as_str())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ReviewRequest {
    pub id: String,
    pub branch: String,
    pub base: String,
    pub state: ReviewState,
    pub url: String,
    pub title: String,
    pub draft: bool,
}

/// A review's CI check rollup, reduced to one at-a-glance dot for `list` and
/// `status`. `None` means no checks ran, or the provider could not report
/// them - either way, no dot is shown.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CheckStatus {
    Passing,
    Failing,
    Pending,
    None,
}

impl CheckStatus {
    /// The status dot, with a trailing space so it sits before the review id -
    /// or empty when there is nothing to show.
    pub fn dot(self) -> &'static str {
        match self {
            Self::Passing => "🟢 ",
            Self::Failing => "🔴 ",
            Self::Pending => "🟡 ",
            Self::None => "",
        }
    }
}

/// Tallies of a review's latest reviews, for `list --reviews`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub struct ReviewSummary {
    pub approvals: u32,
    pub comments: u32,
    pub changes_requested: u32,
}

impl ReviewSummary {
    /// One line per non-zero category (`"2 approvals"`, `"1 requested change"`),
    /// mirroring the `--commits` list. Empty when nothing has been reviewed, so
    /// the caller can show a `(no reviews)` placeholder instead.
    pub fn lines(&self) -> Vec<String> {
        let count =
            |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut lines = Vec::new();
        if self.approvals > 0 {
            lines.push(count(self.approvals, "approval", "approvals"));
        }
        if self.comments > 0 {
            lines.push(count(self.comments, "comment", "comments"));
        }
        if self.changes_requested > 0 {
            lines.push(count(
                self.changes_requested,
                "requested change",
                "requested changes",
            ));
        }
        lines
    }
}

/// The marker shown before a review that sits in a merge queue (GitHub) or
/// merge train (GitLab) - it is waiting its turn to land. Includes a trailing
/// space so it sits before the CI dot / id.
pub const QUEUED_MARK: &str = "🕑 ";

/// Per-branch review data threaded into the `list` tree: the id (e.g. `#12`),
/// its CI dot, whether it sits in a merge queue/train, and - only with
/// `--reviews` - the review tallies.
pub struct ReviewAnnotation {
    pub id: String,
    pub checks: CheckStatus,
    pub queued: bool,
    pub summary: Option<ReviewSummary>,
}

/// The result of waiting on a review's checks before merging it.
pub enum WaitOutcome {
    /// Checks passed, or there are none - go ahead and merge.
    Passed,
    /// A required check failed - stop the run.
    Failed,
    /// The review merged out-of-band while we waited (an admin merge on the
    /// web, say). Skip the redundant merge and let `sync` reconcile it.
    Landed,
}

pub trait ReviewProvider {
    fn review_for_branch(&self, branch: &str) -> Result<Option<ReviewRequest>>;

    /// Like review_for_branch, but also finds closed reviews. Kept separate
    /// so flows that act on a review (submit, sync, cleanup) never mistake a
    /// dead review for a live one; only the stack-notes ledger wants closed
    /// state, to restyle the entry rather than drop it.
    fn review_for_branch_including_closed(&self, branch: &str) -> Result<Option<ReviewRequest>>;

    /// Open a review for the branch; with `draft`, as a draft. `title` sets the
    /// review's title, defaulting to the branch tip's commit subject.
    fn create_review(
        &self,
        branch: &str,
        base: &str,
        draft: bool,
        title: Option<&str>,
    ) -> Result<String>;

    fn update_review_base(&self, review: &ReviewRequest, base: &str) -> Result<String>;
    /// Whether the platform refuses a base change on this review, whoever
    /// asks. GitHub rejects `pr edit --base` for every pull request in a
    /// stack, its bottom layer included - so a bottom whose base has gone
    /// stale is a state neither side will fix, and the honest answer is to
    /// say so rather than attempt a call that is refused or promise a
    /// retarget that never arrives.
    ///
    /// Default `false`: only GitHub keeps stacks.
    fn platform_refuses_base_change(&self, review: &ReviewRequest) -> Result<bool> {
        let _ = review;
        Ok(false)
    }

    /// Whether the platform can still bring this review's base to `parent`
    /// itself, so a base and a local parent that disagree are a chain still
    /// unwinding rather than a fault.
    ///
    /// Asked by every caller that meets the disagreement: those that would
    /// retarget stand down, and those that read it wait rather than report.
    /// `false` while the review is nonetheless in a stack is the dead end -
    /// see [`ReviewProvider::platform_refuses_base_change`].
    ///
    /// Defaults to `false`, and errs that way too, because the two mistakes
    /// are not equally bad. Answering `false` wrongly means attempting a
    /// retarget the platform refuses: a loud, recoverable error, and
    /// `update_review_base` checks again itself, so a blip that clears in
    /// between still lands on the friendly message. Answering `true` wrongly
    /// means skipping a retarget that was needed - in `cleanup` the layer
    /// then still points at a branch about to be deleted, and a platform that
    /// auto-closes a review whose base disappears takes the review with it,
    /// comments and approvals included, silently.
    fn platform_will_base_on(&self, review: &ReviewRequest, parent: &str) -> Result<bool> {
        let _ = (review, parent);
        Ok(false)
    }

    /// Retitle an existing review. Platforms that encode draft state in the
    /// title (Gitea's `WIP:`, GitLab's `Draft:`) re-apply their prefix, so a
    /// retitle never readies a draft.
    fn update_review_title(&self, review: &ReviewRequest, title: &str) -> Result<String>;

    fn review_body(&self, review: &ReviewRequest) -> Result<String>;

    fn update_review_body(&self, review: &ReviewRequest, body: &str) -> Result<String>;

    /// A carried-forward ledger row's current state, re-fetched by id after its
    /// branch has left the local stack. Nothing else re-queries such a row, so
    /// one that merged or closed since it was last recorded keeps rendering as
    /// open in the overview without this. Default None: a provider that cannot
    /// resolve a review by id alone leaves the recorded state untouched, and
    /// the caller treats any error as "leave it as-is" (best-effort refresh).
    fn review_state(&self, review: &ReviewRequest) -> Result<Option<ReviewState>> {
        let _ = review;
        Ok(None)
    }

    /// The platform's own record of the stack `branch` belongs to, when it
    /// keeps one. An authoritative ordering that outlives local metadata, so
    /// `repair` can prefer it to guessing from ancestry.
    ///
    /// Default `None`: no platform but GitHub records stacks, and GitHub only
    /// when `stk.githubStacks` is on. An error here is the caller's to treat
    /// as "no stack" - this is a hint, never a precondition.
    fn native_stack_for(&self, branch: &str) -> Result<Option<NativeStack>> {
        let _ = branch;
        Ok(None)
    }

    /// Record `reviews` (bottom first) as a stack on the platform, extending
    /// `existing` when the stack is already there and the new reviews sit on
    /// top of it. Returns a line describing what happened, or `None` when
    /// there was nothing to do.
    ///
    /// Default `None`: only GitHub keeps stacks, and only when
    /// `stk.githubStacks` is on. Registering is presentation - the stack map
    /// and parallel review - so a failure here is reported, never fatal to a
    /// submit whose reviews already exist.
    fn register_stack(
        &self,
        reviews: &[String],
        existing: Option<&NativeStack>,
    ) -> Result<Option<String>> {
        let _ = (reviews, existing);
        Ok(None)
    }

    /// Whether this provider would register a stack at all - the provider
    /// keeps stacks, and the user has asked for it.
    ///
    /// Exists so a dry run can decline for the same reason the real run would,
    /// rather than promising a stack on a provider that has none or with the
    /// setting off. Default `false`: only GitHub keeps stacks.
    fn registers_stacks(&self) -> bool {
        false
    }

    /// Merge the review with the given strategy: squash, rebase, or merge.
    /// With `auto`, schedule the merge for when required checks pass
    /// instead of merging now.
    fn merge_review(&self, review: &ReviewRequest, strategy: &str, auto: bool) -> Result<String>;

    /// Why the platform won't merge the review right now, read from its
    /// structured status. Consulted after a merge is rejected to explain it
    /// without parsing the CLI's error text.
    fn merge_blocker(&self, review: &ReviewRequest) -> Result<MergeBlocker>;

    /// Block until the review's checks settle, returning how the wait ended:
    /// checks passed (or there are none), one failed, or the review merged
    /// out-of-band while we waited.
    fn wait_for_checks(&self, review: &ReviewRequest) -> Result<WaitOutcome>;

    /// Every open review, in one call - for annotating the stack with review
    /// numbers (and CI status) without a lookup per branch.
    fn open_reviews(&self) -> Result<Vec<ReviewRequest>>;

    /// Review annotations (id, CI dot, queue state, and - with `detail` -
    /// review tallies) for the given branches, in as few calls as the provider
    /// allows. The default is the generic per-branch path; a provider can
    /// override to batch (GitHub folds it into a single GraphQL query). Only
    /// branches with an open review appear in the result.
    fn annotate_branches(
        &self,
        branches: &[String],
        detail: bool,
    ) -> Result<BTreeMap<String, ReviewAnnotation>> {
        generic_annotate(self, branches, detail)
    }

    /// The CI check rollup for the review's head, for the `list`/`status` dot.
    /// Best-effort display data: the default is [`CheckStatus::None`] (no dot),
    /// which is also the right answer for a provider that cannot report it.
    fn check_status(&self, _review: &ReviewRequest) -> Result<CheckStatus> {
        Ok(CheckStatus::None)
    }

    /// The review's latest-review tallies, for `list --reviews`. Fetched per
    /// branch only when the flag is set; the default is an empty summary.
    fn review_summary(&self, _review: &ReviewRequest) -> Result<ReviewSummary> {
        Ok(ReviewSummary::default())
    }

    /// Mark a draft review as ready for review.
    fn mark_ready(&self, review: &ReviewRequest) -> Result<String>;

    /// Request reviews from the given users or teams on the review, additively
    /// (anyone already requested stays). Team reviewers use the provider's own
    /// form (GitHub/Gitea `org/team`). The default errors, so a provider
    /// without reviewer support surfaces that rather than dropping the request.
    fn request_reviewers(&self, _review: &ReviewRequest, _reviewers: &[String]) -> Result<String> {
        bail!("requesting reviewers is not supported by this provider")
    }

    /// Close the review without merging, deleting its source branch when
    /// `delete_branch`. Used to retire a review superseded by a branch rename.
    fn close_review(&self, review: &ReviewRequest, delete_branch: bool) -> Result<String>;

    /// Open the review in the user's browser.
    fn open_review(&self, review: &ReviewRequest) -> Result<String>;

    /// Of `branches`, those whose review is locked by a merge queue (GitHub)
    /// or merge train (GitLab): they must be neither rebased nor force-pushed.
    /// Rebasing would diverge from the frozen remote tip; a push is rejected
    /// outright (GitHub locks the branch) or silently drops the review from the
    /// queue (GitLab does not lock it). The default is empty - for providers
    /// without a queue, and as the safe degradation when the lookup itself
    /// fails (the reactive push-rejection net in `git` is the backstop).
    fn enqueued_branches(&self, _branches: &[String]) -> Result<BTreeSet<String>> {
        Ok(BTreeSet::new())
    }
}

/// Detect the provider and build its review client together - the pair nearly
/// every provider-backed command opens with. The returned [`DetectedProvider`]
/// still carries the kind and detection source for messages.
pub fn detect_review_provider() -> Result<(DetectedProvider, Box<dyn ReviewProvider>)> {
    let provider = detect_provider()?;
    let client = review_provider(provider.kind);
    Ok((provider, client))
}

/// The generic per-branch annotation path behind [`ReviewProvider::
/// annotate_branches`]: list the open reviews, keep the wanted branches, then
/// look up CI status, queue membership, and (with `detail`) review tallies.
/// Every lookup is best-effort - a failure drops that branch's dot/tallies,
/// not the whole map. A provider with a cheaper bulk API overrides the trait
/// method instead of using this.
fn generic_annotate<P: ReviewProvider + ?Sized>(
    provider: &P,
    branches: &[String],
    detail: bool,
) -> Result<BTreeMap<String, ReviewAnnotation>> {
    let wanted: BTreeSet<&str> = branches.iter().map(String::as_str).collect();
    let reviewed: Vec<ReviewRequest> = provider
        .open_reviews()?
        .into_iter()
        .filter(|review| wanted.contains(review.branch.as_str()))
        .collect();
    let names: Vec<String> = reviewed
        .iter()
        .map(|review| review.branch.clone())
        .collect();
    let queued = provider.enqueued_branches(&names).unwrap_or_default();
    let mut annotations = BTreeMap::new();
    for review in reviewed {
        let checks = provider.check_status(&review).unwrap_or(CheckStatus::None);
        let summary = if detail {
            provider.review_summary(&review).ok()
        } else {
            None
        };
        let is_queued = queued.contains(&review.branch);
        annotations.insert(
            review.branch.clone(),
            ReviewAnnotation {
                id: review.id,
                checks,
                queued: is_queued,
                summary,
            },
        );
    }
    Ok(annotations)
}

/// The branch's review only when it actually heads that branch. A provider can
/// return a review for a different head (a stale or look-alike match); a flow
/// acting on "this branch's review" wants None there, not someone else's.
pub fn owned_review_for_branch(
    provider: &dyn ReviewProvider,
    branch: &str,
) -> Result<Option<ReviewRequest>> {
    Ok(provider
        .review_for_branch(branch)?
        .filter(|review| review.branch == branch))
}

/// Whether the review has merged out-of-band since a `wait_for_checks` loop
/// began. Only a definite Merged stops the wait; anything else (still open, or
/// no longer listed) keeps polling, leaving stk.checkTimeout as the backstop.
pub(super) fn review_merged_out_of_band(
    provider: &dyn ReviewProvider,
    review: &ReviewRequest,
) -> Result<bool> {
    Ok(matches!(
        provider.review_for_branch(&review.branch)?,
        Some(current) if current.state == ReviewState::Merged
    ))
}

pub fn detect_provider() -> Result<DetectedProvider> {
    if let Some(value) = git::config_get(settings::PROVIDER_KEY)? {
        let Some(kind) = ProviderKind::parse(&value) else {
            bail!(
                "unsupported stk.provider value {value:?}; expected github, gitlab, gitea, or demo"
            );
        };

        return Ok(DetectedProvider {
            kind,
            source: ProviderSource::Config,
        });
    }

    let remote = settings::remote()?;
    let Some(url) = git::remote_url(&remote)? else {
        bail!("could not detect provider: remote {remote:?} does not exist");
    };

    let gitlab_host = settings::gitlab_host()?;
    let gitea_host = settings::gitea_host()?;
    let Some(kind) = detect_provider_from_url(&url, gitlab_host.as_deref(), gitea_host.as_deref())
    else {
        bail!(
            "could not detect provider from remote {remote} ({})",
            redact_url(&url)
        );
    };

    Ok(DetectedProvider {
        kind,
        source: ProviderSource::Remote { remote, url },
    })
}

/// Detect the provider from a remote URL by its host. A configured
/// `stk.gitlabHost`/`stk.giteaHost` widens GitLab/Gitea detection to a
/// self-hosted instance.
fn detect_provider_from_url(
    url: &str,
    gitlab_host: Option<&str>,
    gitea_host: Option<&str>,
) -> Option<ProviderKind> {
    let normalized = url.to_ascii_lowercase();
    let host = host_of(&normalized);
    // Match the host itself or a subdomain of it, never a look-alike that
    // merely embeds the name (mygithub.com, evil.com/github.com/...).
    let is = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));

    // The configured host goes through host_of too, so a full URL
    // (https://gitlab.example.com) works as well as a bare host.
    let self_hosted = |configured: Option<&str>| {
        configured.is_some_and(|configured| is(host_of(&configured.to_ascii_lowercase())))
    };

    if is("github.com") {
        Some(ProviderKind::GitHub)
    } else if is("gitlab.com") || self_hosted(gitlab_host) {
        Some(ProviderKind::GitLab)
    } else if is("gitea.com") || is("codeberg.org") || self_hosted(gitea_host) {
        Some(ProviderKind::Gitea)
    } else {
        None
    }
}

/// The host of a git remote URL: the part after any `scheme://` and `user@`,
/// up to the path, port, or scp-style `:`. Covers `https://host/owner/repo`,
/// `ssh://git@host:port/owner/repo`, scp-like `git@host:owner/repo`, and
/// `[ipv6]` literals.
fn host_of(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    // Userinfo and the port live in the authority, before the path's first
    // '/'. (The scp form `git@host:owner/repo` keeps the host before that '/'
    // too.) Strip userinfo at the last '@' so an '@' inside it is tolerated.
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, rest)| rest);
    // An IPv6 literal keeps its colons inside `[..]`; any port follows it.
    if let Some(after_bracket) = host_port.strip_prefix('[') {
        return after_bracket
            .split_once(']')
            .map_or(host_port, |(addr, _)| addr);
    }
    // Otherwise the host ends at a ':' - a port, or the scp path separator.
    host_port.split(':').next().unwrap_or(host_port)
}

/// A remote URL with any embedded userinfo (`user:token@`) dropped, for safe
/// display - an HTTPS remote can carry an auth token in the URL. scp-style
/// `git@host:path` (no `scheme://`) carries no password, so it is left as is.
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    // Drop everything up to the last '@' in the authority (covers `token@`,
    // `user:token@`, and an '@' inside the userinfo).
    let Some((_, host)) = authority.rsplit_once('@') else {
        return url.to_owned();
    };
    match path {
        Some(path) => format!("{scheme}://{host}/{path}"),
        None => format!("{scheme}://{host}"),
    }
}

pub(crate) fn review_provider(kind: ProviderKind) -> Box<dyn ReviewProvider> {
    match kind {
        ProviderKind::GitHub => Box::new(GitHubProvider),
        ProviderKind::GitLab => Box::new(GitLabProvider),
        ProviderKind::Gitea => Box::new(GiteaProvider),
        ProviderKind::Demo => Box::new(DemoProvider),
    }
}

/// A provider CLI's (full name, install URL, auth command), or None for a
/// program that isn't one (e.g. `git`).
fn provider_cli(program: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match program {
        "gh" => Some(("GitHub CLI", "https://cli.github.com", "gh auth login")),
        "glab" => Some((
            "GitLab CLI",
            "https://gitlab.com/gitlab-org/cli",
            "glab auth login",
        )),
        "tea" => Some((
            "Gitea CLI (tea)",
            "https://gitea.com/gitea/tea",
            "tea login add",
        )),
        _ => None,
    }
}

/// Whether a provider CLI's stderr reads like a not-signed-in failure, so we
/// can point the user at `... auth login` rather than just echoing it.
fn looks_unauthenticated(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    [
        "auth login",
        "not logged",
        "401",
        "unauthorized",
        "authentication required",
    ]
    .iter()
    .any(|needle| stderr.contains(needle))
}

fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        // The most common newcomer failure: the provider CLI isn't installed.
        // Turn the raw "No such file or directory (os error 2)" into guidance.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some((name, url, auth)) = provider_cli(program) {
                bail!("{program} ({name}) is not installed - get it from {url}, then run `{auth}`");
            }
            return Err(error).with_context(|| format!("failed to run {program}"));
        }
        Err(error) => return Err(error).with_context(|| format!("failed to run {program}")),
    };

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    // Installed but (probably) not signed in: keep the CLI's own message and
    // add the actionable hint.
    if let Some((_, _, auth)) = provider_cli(program)
        && looks_unauthenticated(&stderr)
    {
        bail!("{program} failed: {stderr}\n(if you are not signed in, run `{auth}`)");
    }
    if stderr.is_empty() {
        Err(anyhow!("{program} exited with status {}", output.status))
    } else {
        Err(anyhow!("{program} failed: {stderr}"))
    }
}

/// Attempts and the pause between them for a merge the platform briefly
/// rejects because it has not finished recomputing the moved base. Landing a
/// tall stack moves the trunk on every merge, so this race is common.
const MERGE_ATTEMPTS: u32 = 3;
const MERGE_RETRY_BACKOFF: Duration = Duration::from_millis(1500);

/// Whether a failed merge is the platform transiently rejecting against a base
/// it has not settled - worth retrying - rather than a real failure (conflict,
/// failed check, closed review), which must surface immediately. GitHub says
/// the "base/head branch was modified"; GitLab returns a 405 Method Not Allowed
/// while the MR's merge status is still recomputing after a push (which
/// `merge --all` triggers by force-pushing each branch just before merging it);
/// Gitea rejects with "failed to merge PR, is it still open?" in the same window.
fn is_transient_merge_error(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_lowercase();
    [
        "base branch was modified",
        "head branch was modified",
        "try the merge again",
        "method not allowed",
        "is it still open",
        // Transient API 5xx (the server hiccupped - not a verdict on the
        // merge): 502/503/504/500. Worth retrying rather than failing the run.
        "bad gateway",
        "service unavailable",
        "gateway time",
        "internal server error",
    ]
    .iter()
    .any(|signature| text.contains(signature))
}

/// Run a merge, retrying while it fails transiently so the "base branch was
/// modified" race does not stop a `merge --all` loop. Between transient
/// retries it only waits a fixed backoff - the right default when there is no
/// per-provider signal to poll.
fn merge_with_retry<T>(attempt: impl FnMut() -> Result<T>) -> Result<T> {
    retry_transient_merge(
        MERGE_ATTEMPTS,
        || std::thread::sleep(MERGE_RETRY_BACKOFF),
        attempt,
    )
}

/// What registering a submitted line as a platform stack would do.
///
/// Shared by the real run and the dry run so the two cannot disagree - the
/// decision lives here, and each caller only performs or renders it.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum StackPlan {
    /// No stack recorded yet: create one holding all of these reviews.
    Register(Vec<String>),
    /// The submitted line continues the recorded stack: some tail of the
    /// stack is where the submitted reviews begin, and `fresh` is everything
    /// past that overlap. Growth on top is the one shape `/add`, which
    /// carries no position, can express.
    Extend { number: u64, fresh: Vec<String> },
    /// The submitted line does not continue the recorded stack - nothing in
    /// common at the join, or something past it the stack already holds: a
    /// branch rooted below, a reorder, a layer removed. Appending would
    /// record an order that is not this stack's.
    Mismatch { number: u64 },
}

/// Plan the registration, or `None` when there is nothing to do - the stack is
/// already exactly this, or already reaches further (a `--downstack` from the
/// middle submits less than is recorded, which is not a divergence).
pub fn plan_stack_registration(
    reviews: &[String],
    existing: Option<&NativeStack>,
) -> Option<StackPlan> {
    let Some(stack) = existing else {
        // GitHub answers 422 for a one-layer stack, and a single review is not
        // a stack in any case.
        return (reviews.len() >= 2).then(|| StackPlan::Register(reviews.to_vec()));
    };
    let recorded: Vec<String> = stack.layers.iter().map(|layer| layer.id.clone()).collect();
    // Submitting part of a stack that is already right is not a divergence,
    // and it need not be the bottom part. A `--downstack` from the middle
    // submits a prefix; resubmitting after the bottom layer lands submits a
    // suffix, because GitHub keeps a landed layer listed in an open stack
    // (verified live). Either way there is nothing to add, so say nothing.
    if recorded
        .windows(reviews.len().max(1))
        .any(|run| run == reviews)
    {
        return None;
    }

    // Otherwise this can only be growth on top, because `/add` carries no
    // position. The submitted line has to *continue* the recorded one: some
    // non-empty tail of the stack must be where the submitted reviews begin,
    // and everything past that overlap must be new. That covers the plain
    // case (the whole stack, then more) and the one after a layer lands and
    // another is stacked on - where the submitted line starts mid-stack and
    // still grows from the top.
    let overlap = (1..=recorded.len().min(reviews.len()))
        .rev()
        .find(|size| recorded[recorded.len() - size..] == reviews[..*size]);
    let Some(overlap) = overlap else {
        // Nothing in common at the join: a review rooted below the stack, a
        // reorder, a different stack entirely. Appending would record an
        // order that is not this stack's, and `repair` reads that order back
        // as a parent `restack` rebases against.
        return Some(StackPlan::Mismatch {
            number: stack.number,
        });
    };
    let fresh = &reviews[overlap..];
    // A "new" review the stack already holds means the submitted order
    // disagrees with the recorded one somewhere behind the join.
    if fresh.iter().any(|id| recorded.contains(id)) {
        return Some(StackPlan::Mismatch {
            number: stack.number,
        });
    }
    (!fresh.is_empty()).then_some(StackPlan::Extend {
        number: stack.number,
        fresh: fresh.to_vec(),
    })
}

/// A merge git-stk itself refused, rather than one the platform rejected.
///
/// The distinction matters at the point of reporting: a platform failure is
/// worth re-diagnosing against the review's merge blocker, and a refusal is
/// not - its reason is already exact, and re-diagnosing one can answer it with
/// advice that contradicts it.
#[derive(Debug)]
pub struct MergeRefused(pub String);

impl fmt::Display for MergeRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for MergeRefused {}

/// Like [`merge_with_retry`], but instead of a blind backoff it runs `resettle`
/// between transient retries - re-polling the provider until the review is
/// actually mergeable again. GitLab's 405-while-recomputing race needs this:
/// the recompute can outlast a fixed sleep, but tracking the real status waits
/// exactly as long as it takes.
pub(super) fn merge_with_resettle(
    mut resettle: impl FnMut(),
    attempt: impl FnMut() -> Result<String>,
) -> Result<String> {
    retry_transient_merge(
        MERGE_ATTEMPTS,
        move || {
            // A short floor delay first, so a provider that reports "mergeable"
            // yet still 405s for a beat isn't hammered in a tight loop.
            std::thread::sleep(MERGE_RETRY_BACKOFF);
            resettle();
        },
        attempt,
    )
}

fn retry_transient_merge<T>(
    attempts: u32,
    mut on_transient: impl FnMut(),
    mut attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    for remaining in (0..attempts).rev() {
        match attempt() {
            Ok(output) => return Ok(output),
            Err(error) if remaining > 0 && is_transient_merge_error(&error) => {
                on_transient();
            }
            Err(error) => return Err(error),
        }
    }
    // attempts is always nonzero, so the final iteration returns above.
    Err(anyhow!("merge retried with no attempts left"))
}

impl fmt::Display for ReviewState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => write!(formatter, "open"),
            Self::Merged => write!(formatter, "merged"),
            Self::Closed => write!(formatter, "closed"),
            Self::Unknown(state) => write!(formatter, "{state}"),
        }
    }
}

impl ReviewRequest {
    pub(crate) fn id_value(&self) -> &str {
        self.id
            .strip_prefix('#')
            .or_else(|| self.id.strip_prefix('!'))
            .unwrap_or(&self.id)
    }

    /// "Title (#12)", or just the id when there is no title.
    pub fn label(&self) -> String {
        label(&self.title, &self.id)
    }
}

/// The display label for a review: "Title (#12)", or the bare id.
pub(crate) fn label(title: &str, id: &str) -> String {
    if title.is_empty() {
        id.to_owned()
    } else {
        format!("{title} ({id})")
    }
}

#[cfg(test)]
mod tests {

    fn stack_of(number: u64, ids: &[&str]) -> NativeStack {
        NativeStack {
            number,
            base: "main".to_owned(),
            layers: ids
                .iter()
                .map(|id| NativeStackLayer {
                    id: (*id).to_owned(),
                    branch: id.trim_start_matches('#').to_owned(),
                })
                .collect(),
        }
    }

    /// The two destinations the platform can bring a layer's base to, and
    /// nothing else. A layer above this one is the case that matters: after a
    /// local reorder it is what the stack's *bottom* would name as its
    /// parent, and the bottom is the one layer never retargeted.
    #[test]
    fn can_base_on_accepts_only_the_predecessor_and_the_stack_base() {
        let stack = stack_of(7, &["#12", "#13", "#14"]);

        // The recorded predecessor, and the stack's own base.
        assert!(stack.can_base_on("13", "12"));
        assert!(stack.can_base_on("13", "main"));
        assert!(stack.can_base_on("12", "main"));

        // A layer above, which a reorder makes the bottom's local parent.
        assert!(!stack.can_base_on("12", "13"));
        // A layer below, but not the one recorded directly beneath.
        assert!(!stack.can_base_on("14", "12"));
        // Somewhere the stack has never heard of.
        assert!(!stack.can_base_on("13", "rc-20260817"));
        // And a branch the stack does not hold at all.
        assert!(!stack.can_base_on("other", "main"));
    }

    /// The whole decision table for registering, in one place - this is what
    /// the dry run renders and the real run performs, so a disagreement
    /// between them is impossible by construction.
    #[test]
    fn plan_stack_registration_covers_every_shape() {
        let ids = |s: &[&str]| s.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();

        // Nothing recorded: register, but only for a real stack. GitHub
        // answers 422 for one review, and one review is not a stack.
        assert_eq!(
            plan_stack_registration(&ids(&["#12", "#13"]), None),
            Some(StackPlan::Register(ids(&["#12", "#13"])))
        );
        assert_eq!(plan_stack_registration(&ids(&["#12"]), None), None);

        let recorded = stack_of(7, &["#12", "#13"]);

        // Already exactly this, and growth on top - the one shape `/add` can
        // express, since it carries no position.
        assert_eq!(
            plan_stack_registration(&ids(&["#12", "#13"]), Some(&recorded)),
            None
        );
        assert_eq!(
            plan_stack_registration(&ids(&["#12", "#13", "#14"]), Some(&recorded)),
            Some(StackPlan::Extend {
                number: 7,
                fresh: ids(&["#14"])
            })
        );

        // Part of a stack that is already right, from either end. A prefix is
        // `--downstack` from the middle; a suffix is what remains after the
        // bottom layer lands, which GitHub keeps listed in the open stack.
        let three = stack_of(7, &["#12", "#13", "#14"]);
        assert_eq!(
            plan_stack_registration(&ids(&["#12", "#13"]), Some(&three)),
            None
        );
        assert_eq!(
            plan_stack_registration(&ids(&["#13", "#14"]), Some(&three)),
            None
        );
        assert_eq!(plan_stack_registration(&ids(&["#13"]), Some(&three)), None);

        // A suffix that then grew on top: merge the bottom, stack another
        // branch, resubmit. The overlap is a tail of the stack rather than
        // the whole of it, and #15 is still the only thing to append.
        assert_eq!(
            plan_stack_registration(&ids(&["#13", "#14", "#15"]), Some(&three)),
            Some(StackPlan::Extend {
                number: 7,
                fresh: ids(&["#15"])
            })
        );

        // And the shapes `/add` cannot express: a review that belongs below,
        // and a reorder. Appending either would record an order that is not
        // this stack's, which `repair` reads back as a parent.
        assert_eq!(
            plan_stack_registration(&ids(&["#11", "#12", "#13"]), Some(&recorded)),
            Some(StackPlan::Mismatch { number: 7 })
        );
        assert_eq!(
            plan_stack_registration(&ids(&["#13", "#12"]), Some(&recorded)),
            Some(StackPlan::Mismatch { number: 7 })
        );
        // A tail that lines up but re-adds a layer behind the join: the
        // overlap is #14, and #12 is already in the stack.
        assert_eq!(
            plan_stack_registration(&ids(&["#14", "#12"]), Some(&three)),
            Some(StackPlan::Mismatch { number: 7 })
        );
        // And an unrelated stack entirely.
        assert_eq!(
            plan_stack_registration(&ids(&["#20", "#21"]), Some(&recorded)),
            Some(StackPlan::Mismatch { number: 7 })
        );
    }
    use super::*;

    #[test]
    fn provider_cli_maps_only_the_provider_clis() {
        assert!(provider_cli("gh").is_some());
        assert!(provider_cli("glab").is_some());
        assert!(provider_cli("git").is_none());
    }

    #[test]
    fn looks_unauthenticated_matches_signin_failures_only() {
        assert!(looks_unauthenticated(
            "error: not logged into any GitHub hosts"
        ));
        assert!(looks_unauthenticated(
            "To get started, please run: gh auth login"
        ));
        assert!(looks_unauthenticated("GET ...: 401 Unauthorized"));
        // A normal failure must not be misread as an auth problem.
        assert!(!looks_unauthenticated("pull request not found"));
        assert!(!looks_unauthenticated("merge conflict in src/lib.rs"));
    }

    #[test]
    fn transient_error_is_retried_then_succeeds() {
        let mut calls = 0;
        let result: Result<String> = retry_transient_merge(
            3,
            || {},
            || {
                calls += 1;
                if calls < 2 {
                    Err(anyhow!(
                        "gh failed: GraphQL: Base branch was modified. Review and try the merge again."
                    ))
                } else {
                    Ok("merged".to_owned())
                }
            },
        );
        assert_eq!(result.unwrap(), "merged");
        assert_eq!(calls, 2, "should retry once then succeed");
    }

    #[test]
    fn a_gitlab_405_while_the_merge_status_recomputes_is_retried() {
        let mut calls = 0;
        let result: Result<String> = retry_transient_merge(
            3,
            || {},
            || {
                calls += 1;
                if calls < 2 {
                    Err(anyhow!("glab failed: ... /merge: 405 Method Not Allowed"))
                } else {
                    Ok("merged".to_owned())
                }
            },
        );
        assert_eq!(result.unwrap(), "merged");
        assert_eq!(calls, 2, "GitLab's transient 405 should be retried");
    }

    #[test]
    fn the_between_retry_action_runs_once_per_transient_retry() {
        // `merge_with_resettle` re-polls via this hook instead of a blind
        // sleep; the hook runs once per transient retry, never after success.
        let mut resettles = 0;
        let mut calls = 0;
        let result: Result<String> = retry_transient_merge(
            3,
            || resettles += 1,
            || {
                calls += 1;
                // 405 twice (recompute still in flight), then mergeable.
                if calls < 3 {
                    Err(anyhow!("glab failed: ... /merge: 405 Method Not Allowed"))
                } else {
                    Ok("merged".to_owned())
                }
            },
        );
        assert_eq!(result.unwrap(), "merged");
        assert_eq!(calls, 3, "should retry until the merge lands");
        assert_eq!(
            resettles, 2,
            "re-poll once per transient retry, not after the final success"
        );
    }

    #[test]
    fn the_between_retry_action_does_not_run_on_a_real_failure() {
        let mut resettles = 0;
        let result: Result<String> = retry_transient_merge(
            3,
            || resettles += 1,
            || {
                Err(anyhow!(
                    "glab failed: Merge request is not mergeable: conflict"
                ))
            },
        );
        assert!(result.is_err());
        assert_eq!(resettles, 0, "a non-transient failure must not re-poll");
    }

    #[test]
    fn a_transient_5xx_from_the_api_is_retried() {
        let mut calls = 0;
        let result: Result<String> = retry_transient_merge(
            3,
            || {},
            || {
                calls += 1;
                if calls < 2 {
                    Err(anyhow!(
                        "gh failed: non-200 OK status code: 502 Bad Gateway"
                    ))
                } else {
                    Ok("merged".to_owned())
                }
            },
        );
        assert_eq!(result.unwrap(), "merged");
        assert_eq!(calls, 2, "a 502 is a server hiccup, not a merge verdict");
    }

    #[test]
    fn a_persistent_transient_error_gives_up_after_the_attempt_budget() {
        let mut calls = 0;
        let result: Result<String> = retry_transient_merge(
            3,
            || {},
            || {
                calls += 1;
                Err(anyhow!("gh failed: Base branch was modified"))
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 3, "should try exactly the budgeted number of times");
    }

    #[test]
    fn a_real_failure_is_not_retried() {
        let mut calls = 0;
        let result: Result<String> = retry_transient_merge(
            3,
            || {},
            || {
                calls += 1;
                Err(anyhow!(
                    "gh failed: Pull request is not mergeable: conflicts"
                ))
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 1, "a non-transient error must surface immediately");
    }

    #[test]
    fn host_of_extracts_the_host_across_url_shapes() {
        assert_eq!(host_of("https://github.com/owner/repo.git"), "github.com");
        assert_eq!(host_of("git@github.com:owner/repo.git"), "github.com");
        assert_eq!(
            host_of("ssh://git@gitlab.example.com:22/g/r"),
            "gitlab.example.com"
        );
        assert_eq!(host_of("https://user@github.com/owner/repo"), "github.com");
        assert_eq!(host_of("https://github.com:8443/owner/repo"), "github.com");
        assert_eq!(
            host_of("https://[2001:db8::1]:443/owner/repo"),
            "2001:db8::1"
        );
        assert_eq!(host_of("gitlab.example.com"), "gitlab.example.com");
        // Userinfo with an embedded '@' is stripped at the last one.
        assert_eq!(host_of("https://user@name@github.com/r"), "github.com");
    }

    #[test]
    fn redact_url_strips_embedded_credentials() {
        // An HTTPS remote can carry a token; it must never be displayed.
        assert_eq!(
            redact_url("https://x-access-token:ghp_SECRET@github.com/owner/repo.git"),
            "https://github.com/owner/repo.git"
        );
        assert_eq!(
            redact_url("https://glpat-SECRET@gitlab.com/owner/repo"),
            "https://gitlab.com/owner/repo"
        );
        // ssh userinfo (no secret) is dropped too; port and path stay.
        assert_eq!(redact_url("ssh://git@host:22/g/r"), "ssh://host:22/g/r");
    }

    #[test]
    fn redact_url_leaves_credential_free_urls_unchanged() {
        assert_eq!(
            redact_url("https://github.com/owner/repo.git"),
            "https://github.com/owner/repo.git"
        );
        // scp form has no scheme and carries no password - left as is.
        assert_eq!(
            redact_url("git@github.com:owner/repo.git"),
            "git@github.com:owner/repo.git"
        );
    }

    #[test]
    fn self_hosted_gitlab_accepts_a_bare_host_or_a_full_url() {
        let remote = "git@gitlab.example.com:team/repo.git";
        for configured in ["gitlab.example.com", "https://gitlab.example.com"] {
            assert_eq!(
                detect_provider_from_url(remote, Some(configured), None),
                Some(ProviderKind::GitLab),
                "configured {configured:?} should detect the self-hosted host"
            );
        }
        // A look-alike host is still not matched.
        assert_eq!(
            detect_provider_from_url("git@notgitlab.com:o/r", Some("gitlab.example.com"), None),
            None
        );
    }

    #[test]
    fn gitea_is_detected_for_gitea_com_codeberg_and_a_configured_host() {
        assert_eq!(
            detect_provider_from_url("git@gitea.com:o/r.git", None, None),
            Some(ProviderKind::Gitea)
        );
        assert_eq!(
            detect_provider_from_url("https://codeberg.org/o/r", None, None),
            Some(ProviderKind::Gitea)
        );
        for configured in ["gitea.example.com", "https://gitea.example.com"] {
            assert_eq!(
                detect_provider_from_url("git@gitea.example.com:o/r.git", None, Some(configured)),
                Some(ProviderKind::Gitea),
                "configured {configured:?} should detect the self-hosted Gitea host"
            );
        }
        // A look-alike host is not matched.
        assert_eq!(
            detect_provider_from_url("git@notgitea.com:o/r", None, Some("gitea.example.com")),
            None
        );
    }
}
