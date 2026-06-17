use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, sync::mpsc, thread};

use anyhow::{Context, Result, bail};
use axoupdater::{AxoUpdater, UpdateRequest};

use crate::prompt::confirm;

/// Source repository used for `--head` installs and release discovery.
const REPO_URL: &str = "https://github.com/lararosekelley/git-stk";

/// The first release that shipped `downgrade`, and the floor it can reach.
/// Going below would strand the user on a binary with no `downgrade` (unable
/// to step back again) and predates the state-format guarantees this command
/// assumes. MUST equal the version this command ships in.
const MIN_DOWNGRADE_VERSION: &str = "0.9.17";

/// Stamp file next to the install receipt; one release check per day.
const UPDATE_CHECK_FILE: &str = "update-check";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Once a day, after a common command: print one dim line when a newer
/// release exists. Best effort with a hard time cap; anything unusual (no
/// receipt, offline, piped stderr, opt-out) prints nothing.
pub fn maybe_hint_update() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let Some(path) = update_check_path() else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    if !should_check(fs::read_to_string(&path).ok().as_deref(), now) {
        return;
    }
    if crate::settings::bool_setting(crate::settings::NO_UPDATE_CHECK_KEY).unwrap_or(false) {
        return;
    }

    // The query runs on a thread the process is free to abandon: the
    // command's work is already done, so cap the wait.
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut updater = AxoUpdater::new_for("git-stk");
        let behind =
            updater.load_receipt().is_ok() && updater.is_update_needed_sync().unwrap_or(false);
        let _ = sender.send(behind);
    });

    // On a timeout, leave the stamp untouched so the next command retries
    // instead of waiting out the daily window on a check that never answered.
    if let Ok(behind) = receiver.recv_timeout(Duration::from_secs(5)) {
        // Stamp only once the check actually finished, so a slow network
        // retries on the next command rather than going quiet for a day.
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, format!("checked={now}\n"));
        if behind {
            anstream::eprintln!(
                "{}",
                crate::style::paint(
                    crate::style::DIM,
                    "a newer git-stk release is available - run `git stk upgrade`"
                )
            );
        }
    }
}

/// git-stk's config/state directory, where the install receipt and the
/// update-check stamp live: `$XDG_CONFIG_HOME/git-stk`, `%LOCALAPPDATA%\git-stk`
/// on Windows, or `~/.config/git-stk`.
pub(crate) fn config_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        // Windows has no HOME; %LOCALAPPDATA% is the home for app state.
        .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("git-stk"))
}

fn update_check_path() -> Option<PathBuf> {
    Some(config_dir()?.join(UPDATE_CHECK_FILE))
}

/// Whether the daily window has passed (or the stamp is missing/garbled).
fn should_check(cache: Option<&str>, now: u64) -> bool {
    let Some(cache) = cache else {
        return true;
    };
    cache
        .lines()
        .find_map(|line| line.strip_prefix("checked="))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_none_or(|checked| now.saturating_sub(checked) >= CHECK_INTERVAL_SECS)
}

pub fn upgrade(head: bool, force: bool, yes: bool) -> Result<()> {
    if head {
        upgrade_to_head(yes)
    } else {
        upgrade_to_latest_release(force)
    }
}

fn upgrade_to_head(yes: bool) -> Result<()> {
    anstream::println!("--head builds and installs the latest unreleased commit from {REPO_URL}");
    anstream::println!("HEAD is a pre-release snapshot: it may be broken or untested");

    if !yes && !confirm("continue? [y/N] ")? {
        anstream::println!("upgrade cancelled");
        return Ok(());
    }

    let status = Command::new("cargo")
        .args(["install", "--git", REPO_URL, "--locked", "git-stk"])
        .status()
        .context("failed to run cargo; --head requires a Rust toolchain")?;

    if !status.success() {
        bail!("cargo install exited with status {status}");
    }

    anstream::println!("installed git-stk from HEAD");
    anstream::println!("to return to the latest release, run: git stk upgrade --force");
    refresh_assets_with_new_binary();
    Ok(())
}

/// Re-render generated assets (man page) after an upgrade, using the newly
/// installed binary so the assets match its version rather than the running
/// (pre-upgrade) one. Failure is a warning, not an error: the upgrade itself
/// already succeeded.
fn refresh_assets_with_new_binary() {
    let refreshed = Command::new("git-stk")
        .args(["setup", "--refresh"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if !refreshed {
        anstream::eprintln!(
            "{} failed to refresh generated assets; run `git stk setup` manually",
            crate::style::paint(crate::style::WARN, "warning:")
        );
    }
}

fn upgrade_to_latest_release(force: bool) -> Result<()> {
    let mut updater = AxoUpdater::new_for("git-stk");
    updater
        .load_receipt()
        .map_err(anyhow::Error::from)
        .context(
            "no usable install receipt found; if git-stk was installed with cargo, \
             upgrade with `cargo install git-stk --locked` instead",
        )?;
    updater.always_update(force);

    match updater
        .run_sync()
        .context("failed to upgrade to the latest release")?
    {
        Some(result) => {
            let old = result
                .old_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "unknown".to_owned());
            anstream::println!(
                "{}",
                crate::style::success(&format!("upgraded git-stk {old} -> {}", result.new_version))
            );
            refresh_assets_with_new_binary();
        }
        None => anstream::println!(
            "git-stk {} is already the latest release",
            env!("CARGO_PKG_VERSION")
        ),
    }

    Ok(())
}

/// Step back to an earlier release. Riskier than upgrading - an older binary
/// may not understand state a newer one wrote - so it confirms first (unless
/// `yes`) and never goes below [`MIN_DOWNGRADE_VERSION`].
pub fn downgrade(to: Option<String>, yes: bool) -> Result<()> {
    let installed = env!("CARGO_PKG_VERSION");
    let installed_version = parse_version(installed)
        .with_context(|| format!("could not parse the installed version {installed}"))?;
    let floor = parse_version(MIN_DOWNGRADE_VERSION).expect("floor is a valid version");

    if installed_version <= floor {
        anstream::println!(
            "git-stk {installed} is the earliest release `downgrade` can reach; \
             nothing older to downgrade to"
        );
        return Ok(());
    }

    let requested = match &to {
        Some(to) => Some(parse_version(to).with_context(|| format!("not a version: {to}"))?),
        None => None,
    };
    // Only the default (no `--to`) needs the release list.
    let available = match requested {
        Some(_) => Vec::new(),
        None => remote_release_versions()?,
    };
    let target = version_string(resolve_target(
        installed_version,
        floor,
        requested,
        &available,
    )?);

    let mut updater = AxoUpdater::new_for("git-stk");
    updater
        .load_receipt()
        .map_err(anyhow::Error::from)
        .context(
            "no usable install receipt found; if git-stk was installed with cargo, \
         downgrade with `cargo install git-stk@<version> --locked` instead",
        )?;

    anstream::println!("downgrade git-stk {installed} -> {target}");
    anstream::println!(
        "a release older than {installed} may not understand state a newer one wrote \
         (PR ledger, branch metadata, the shared metadata ref)"
    );
    if !yes && !confirm("continue? [y/N] ")? {
        anstream::println!("downgrade cancelled");
        return Ok(());
    }

    updater.configure_version_specifier(UpdateRequest::SpecificVersion(target.clone()));
    // Going backward is never "needed" by the cur < new check; force it. The
    // installer for the target version rewrites the receipt, keeping it honest.
    updater.always_update(true);

    match updater
        .run_sync()
        .context("failed to downgrade to the requested release")?
    {
        Some(result) => {
            anstream::println!(
                "{}",
                crate::style::success(&format!(
                    "downgraded git-stk {installed} -> {}",
                    result.new_version
                ))
            );
            anstream::println!("to move forward again, run: git stk upgrade");
            refresh_assets_with_new_binary();
        }
        None => anstream::println!("git-stk is already at {target}"),
    }
    Ok(())
}

type Version3 = (u64, u64, u64);

/// Choose the version to downgrade to. `available` is consulted only for the
/// default (no `--to`); an explicit target is validated against the installed
/// version and the floor rather than silently clamped.
fn resolve_target(
    installed: Version3,
    floor: Version3,
    requested: Option<Version3>,
    available: &[Version3],
) -> Result<Version3> {
    match requested {
        Some(target) => {
            if target >= installed {
                bail!(
                    "{} is not older than the installed {}; use `git stk upgrade` to move forward",
                    version_string(target),
                    version_string(installed)
                );
            }
            if target < floor {
                bail!(
                    "{} is below {}, the earliest release `downgrade` can reach",
                    version_string(target),
                    version_string(floor)
                );
            }
            Ok(target)
        }
        None => available
            .iter()
            .copied()
            .filter(|version| *version < installed && *version >= floor)
            .max()
            .with_context(|| {
                format!(
                    "no release between {} and {} to downgrade to",
                    version_string(floor),
                    version_string(installed)
                )
            }),
    }
}

/// Parse a plain `X.Y.Z`. Anything else - extra parts, pre-release suffixes -
/// yields None, so non-release tags are skipped.
fn parse_version(text: &str) -> Option<Version3> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn version_string((major, minor, patch): Version3) -> String {
    format!("{major}.{minor}.{patch}")
}

/// Released versions, from the repo's `vX.Y.Z` tags.
fn remote_release_versions() -> Result<Vec<Version3>> {
    let output = Command::new("git")
        .args(["ls-remote", "--tags", REPO_URL])
        .output()
        .context("failed to list releases; check your network connection")?;
    if !output.status.success() {
        bail!("failed to fetch the release list from {REPO_URL}");
    }

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .filter_map(|line| line.split("refs/tags/").nth(1))
        .filter(|tag| !tag.ends_with("^{}"))
        .filter_map(|tag| parse_version(tag.strip_prefix('v').unwrap_or(tag)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_accepts_plain_xyz_and_rejects_the_rest() {
        assert_eq!(parse_version("0.9.16"), Some((0, 9, 16)));
        assert_eq!(parse_version("10.0.3"), Some((10, 0, 3)));
        assert_eq!(parse_version("0.9"), None);
        assert_eq!(parse_version("0.9.16.1"), None);
        assert_eq!(parse_version("0.9.0-rc.1"), None);
        assert_eq!(parse_version("v0.9.16"), None);
    }

    #[test]
    fn resolve_target_requires_explicit_to_be_older() {
        let (installed, floor) = ((0, 9, 18), (0, 9, 17));
        assert!(resolve_target(installed, floor, Some((0, 9, 18)), &[]).is_err());
        assert!(resolve_target(installed, floor, Some((0, 9, 19)), &[]).is_err());
    }

    #[test]
    fn resolve_target_refuses_explicit_below_the_floor() {
        let (installed, floor) = ((0, 9, 18), (0, 9, 17));
        assert!(resolve_target(installed, floor, Some((0, 9, 16)), &[]).is_err());
        assert_eq!(
            resolve_target(installed, floor, Some((0, 9, 17)), &[]).unwrap(),
            (0, 9, 17)
        );
    }

    #[test]
    fn resolve_target_default_picks_the_previous_release() {
        let (installed, floor) = ((0, 9, 20), (0, 9, 17));
        let available = [(0, 9, 17), (0, 9, 18), (0, 9, 19), (0, 9, 20)];
        assert_eq!(
            resolve_target(installed, floor, None, &available).unwrap(),
            (0, 9, 19)
        );
    }

    #[test]
    fn resolve_target_default_never_crosses_the_floor() {
        let (installed, floor) = ((0, 9, 18), (0, 9, 17));
        let available = [(0, 9, 15), (0, 9, 16), (0, 9, 17), (0, 9, 18)];
        // Releases below the floor are out; the previous in-range one is the floor.
        assert_eq!(
            resolve_target(installed, floor, None, &available).unwrap(),
            (0, 9, 17)
        );
    }

    #[test]
    fn resolve_target_default_errors_when_nothing_older_in_range() {
        let (installed, floor) = ((0, 9, 18), (0, 9, 17));
        assert!(resolve_target(installed, floor, None, &[(0, 9, 18), (0, 9, 19)]).is_err());
    }

    #[test]
    fn should_check_when_stamp_is_missing_or_garbled() {
        assert!(should_check(None, 1_000_000));
        assert!(should_check(Some(""), 1_000_000));
        assert!(should_check(Some("checked=not-a-number\n"), 1_000_000));
    }

    #[test]
    fn should_check_once_per_day() {
        let stamp = format!("checked={}\n", 1_000_000);
        assert!(!should_check(Some(&stamp), 1_000_000 + 60));
        assert!(!should_check(
            Some(&stamp),
            1_000_000 + CHECK_INTERVAL_SECS - 1
        ));
        assert!(should_check(Some(&stamp), 1_000_000 + CHECK_INTERVAL_SECS));
    }
}
