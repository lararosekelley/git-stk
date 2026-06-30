use std::collections::BTreeSet;
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

    /// Open a review for the branch; with `draft`, as a draft.
    fn create_review(&self, branch: &str, base: &str, draft: bool) -> Result<String>;

    fn update_review_base(&self, review: &ReviewRequest, base: &str) -> Result<String>;

    fn review_body(&self, review: &ReviewRequest) -> Result<String>;

    fn update_review_body(&self, review: &ReviewRequest, body: &str) -> Result<String>;

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
    /// numbers without a lookup per branch.
    fn open_reviews(&self) -> Result<Vec<ReviewRequest>>;

    /// Mark a draft review as ready for review.
    fn mark_ready(&self, review: &ReviewRequest) -> Result<String>;

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
fn merge_with_retry(attempt: impl FnMut() -> Result<String>) -> Result<String> {
    retry_transient_merge(
        MERGE_ATTEMPTS,
        || std::thread::sleep(MERGE_RETRY_BACKOFF),
        attempt,
    )
}

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

fn retry_transient_merge(
    attempts: u32,
    mut on_transient: impl FnMut(),
    mut attempt: impl FnMut() -> Result<String>,
) -> Result<String> {
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
        let result = retry_transient_merge(
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
        let result = retry_transient_merge(
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
        let result = retry_transient_merge(
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
        let result = retry_transient_merge(
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
        let result = retry_transient_merge(
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
        let result = retry_transient_merge(
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
        let result = retry_transient_merge(
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
