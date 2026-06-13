//! Every stk-owned git config key and its resolution logic, in one place.

use std::time::Duration;

use anyhow::Result;

use crate::cli::PushMode;
use crate::git;

pub const PROVIDER_KEY: &str = "stk.provider";
pub const REMOTE_KEY: &str = "stk.remote";
pub const UPDATE_REFS_KEY: &str = "stk.updateRefs";
pub const PUSH_ON_RESTACK_KEY: &str = "stk.pushOnRestack";
pub const PUSH_ON_SUBMIT_KEY: &str = "stk.pushOnSubmit";
pub const SUBMIT_STACK_KEY: &str = "stk.submitStack";
pub const MERGE_STRATEGY_KEY: &str = "stk.mergeStrategy";
pub const MERGE_WAIT_KEY: &str = "stk.mergeWait";
pub const SUBMIT_DRAFT_KEY: &str = "stk.submitDraft";
pub const NO_UPDATE_CHECK_KEY: &str = "stk.noUpdateCheck";
pub const ABSORB_INCLUDE_UNSTAGED_KEY: &str = "stk.absorbIncludeUnstaged";
pub const GITLAB_HOST_KEY: &str = "stk.gitlabHost";
pub const CHECK_TIMEOUT_KEY: &str = "stk.checkTimeout";
pub const DEFAULT_REMOTE: &str = "origin";

/// How long `merge --wait` polls a review's checks before giving up, unless
/// `stk.checkTimeout` overrides it. Generous so a slow-but-real CI is not cut
/// off; the point is to bound a pipeline that never settles, not a long one.
pub const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 1800;

/// Every `[stk]` setting the tool reads, with its default behavior. Shown by
/// `git stk config`.
pub const SETTINGS: &[(&str, &str)] = &[
    (PROVIDER_KEY, "auto-detect from the remote URL"),
    (REMOTE_KEY, DEFAULT_REMOTE),
    (UPDATE_REFS_KEY, "false"),
    (PUSH_ON_RESTACK_KEY, "false"),
    (PUSH_ON_SUBMIT_KEY, "false"),
    (SUBMIT_STACK_KEY, "false"),
    (MERGE_STRATEGY_KEY, "squash"),
    (MERGE_WAIT_KEY, "false"),
    (SUBMIT_DRAFT_KEY, "false"),
    (NO_UPDATE_CHECK_KEY, "false"),
    (ABSORB_INCLUDE_UNSTAGED_KEY, "false"),
    (GITLAB_HOST_KEY, "none; gitlab.com is always detected"),
    (CHECK_TIMEOUT_KEY, "1800 (30m); 0 waits indefinitely"),
];

/// The remote used for provider detection, trunk discovery, and pushes.
pub fn remote() -> Result<String> {
    Ok(git::config_get(REMOTE_KEY)?.unwrap_or_else(|| DEFAULT_REMOTE.to_owned()))
}

/// A self-hosted GitLab host (e.g. `gitlab.example.com`) to recognize as
/// GitLab alongside gitlab.com (`stk.gitlabHost`). `glab` reads the host from
/// the git remote on its own, so this only widens stk's provider detection.
pub fn gitlab_host() -> Result<Option<String>> {
    git::config_get(GITLAB_HOST_KEY)
}

/// The merge strategy for `git stk merge`: squash, rebase, or merge.
pub fn merge_strategy() -> Result<String> {
    let strategy = git::config_get(MERGE_STRATEGY_KEY)?.unwrap_or_else(|| "squash".to_owned());
    match strategy.as_str() {
        "squash" | "rebase" | "merge" => Ok(strategy),
        other => anyhow::bail!(
            "unsupported stk.mergeStrategy value {other:?}; expected squash, rebase, or merge"
        ),
    }
}

/// How long `merge --wait` keeps polling a review's checks before giving up,
/// from `stk.checkTimeout` (whole seconds). `0` waits indefinitely; unset uses
/// [`DEFAULT_CHECK_TIMEOUT_SECS`].
pub fn check_timeout() -> Result<Option<Duration>> {
    parse_check_timeout(git::config_get(CHECK_TIMEOUT_KEY)?.as_deref())
}

fn parse_check_timeout(value: Option<&str>) -> Result<Option<Duration>> {
    let seconds = match value {
        Some(raw) => raw.trim().parse::<u64>().map_err(|_| {
            anyhow::anyhow!(
                "invalid {CHECK_TIMEOUT_KEY} value {raw:?}; expected a whole number of seconds"
            )
        })?,
        None => DEFAULT_CHECK_TIMEOUT_SECS,
    };
    // Zero is the explicit "wait forever" escape hatch.
    Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

/// A boolean setting's value, defaulting to false when unset.
pub fn bool_setting(key: &str) -> Result<bool> {
    Ok(git::config_get_bool(key)?.unwrap_or(false))
}

/// Resolve a `--push`/`--no-push` flag pair against its config-key default.
pub fn push_enabled(mode: PushMode, key: &str) -> Result<bool> {
    match mode {
        PushMode::Config => Ok(git::config_get_bool(key)?.unwrap_or(false)),
        PushMode::Enabled => Ok(true),
        PushMode::Disabled => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_timeout_defaults_when_unset() {
        assert_eq!(
            parse_check_timeout(None).unwrap(),
            Some(Duration::from_secs(DEFAULT_CHECK_TIMEOUT_SECS))
        );
    }

    #[test]
    fn check_timeout_zero_waits_indefinitely() {
        assert_eq!(parse_check_timeout(Some("0")).unwrap(), None);
    }

    #[test]
    fn check_timeout_reads_whole_seconds() {
        assert_eq!(
            parse_check_timeout(Some("300")).unwrap(),
            Some(Duration::from_secs(300))
        );
        // Surrounding whitespace is tolerated (git config values can carry it).
        assert_eq!(
            parse_check_timeout(Some(" 60 ")).unwrap(),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn check_timeout_rejects_non_numbers() {
        let error = parse_check_timeout(Some("soon")).unwrap_err();
        assert!(
            error.to_string().contains("stk.checkTimeout"),
            "unexpected error: {error:#}"
        );
    }
}
