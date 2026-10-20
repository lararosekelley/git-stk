//! What a command's review argument names: a branch, or a review by number or
//! URL - so `view` and `review` can reach a review whose branch is not local.

use anyhow::{Context, Result, bail};

use super::{ProviderKind, command_output, host_of};
use crate::{git, settings};

#[derive(Debug, Eq, PartialEq)]
pub enum ReviewRef {
    Branch(String),
    Number(u64),
}

impl ReviewRef {
    /// Resolve `arg`, defaulting to the current branch. `12` names a review
    /// unless a local branch has exactly that name; `#12`, `!12`, and review
    /// URLs always do.
    pub fn parse(arg: Option<&str>, kind: ProviderKind) -> Result<Self> {
        let Some(arg) = arg else {
            return Ok(Self::Branch(git::current_branch()?));
        };
        if let Some(number) = arg.strip_prefix(['#', '!']).and_then(parse_number) {
            return Ok(Self::Number(number));
        }
        if let Some(number) = parse_number(arg)
            && !git::local_branches()?.iter().any(|branch| branch == arg)
        {
            return Ok(Self::Number(number));
        }
        if arg.starts_with("https://") || arg.starts_with("http://") {
            let remote = settings::remote()?;
            let remote_url = git::remote_url(&remote)?.with_context(|| {
                format!("cannot match {arg} to this repository: remote {remote} does not exist")
            })?;
            return parse_url(arg, kind, &remote_url, &ssh_hostname).map(Self::Number);
        }
        Ok(Self::Branch(arg.to_owned()))
    }
}

/// Who owns this repository, from its remote: `owner` of `owner/repo`, or
/// the group path on GitLab.
pub fn remote_owner() -> Result<String> {
    let remote = settings::remote()?;
    let remote_url =
        git::remote_url(&remote)?.with_context(|| format!("remote {remote} does not exist"))?;
    let path = repo_path(&remote_url);
    path.rsplit_once('/')
        .map(|(owner, _)| owner.to_owned())
        .with_context(|| format!("cannot read an owner from remote {remote} ({remote_url})"))
}

/// A plain review number: ASCII digits only, so `+1` or ` 1` stay branch names.
fn parse_number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The review number in a provider's web URL for one of this repository's
/// reviews. The repository path must match the remote's, so a link to another
/// repository's review never opens this one's review of the same number. The
/// host must match too, after resolving a `~/.ssh/config` alias with
/// `resolve_alias`.
fn parse_url(
    url: &str,
    kind: ProviderKind,
    remote_url: &str,
    resolve_alias: &dyn Fn(&str) -> Option<String>,
) -> Result<u64> {
    let marker = match kind {
        ProviderKind::GitHub => "/pull/",
        ProviderKind::GitLab => "/-/merge_requests/",
        ProviderKind::Gitea => "/pulls/",
        ProviderKind::Demo => bail!("demo reviews have no web URL; pass the review number"),
    };

    let lowered = url.to_ascii_lowercase();
    let after_scheme = lowered
        .split_once("://")
        .map_or(&*lowered, |(_, rest)| rest);
    let path = after_scheme
        .find('/')
        .map_or("", |slash| &after_scheme[slash..]);
    // The last marker, so a repository named like one (`owner/pull`) still parses.
    let Some((repo, rest)) = path.rsplit_once(marker) else {
        bail!("{url} is not a {kind} review URL");
    };
    let digits = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let Some(number) = parse_number(digits) else {
        bail!("{url} is not a {kind} review URL");
    };

    let host = host_of(&lowered);
    let mut remote_host = host_of(&remote_url.to_ascii_lowercase()).to_owned();
    // A dotless SSH host is most likely an alias; compare the host it stands for.
    let ssh_remote = !remote_url.contains("://") || remote_url.starts_with("ssh://");
    if host != remote_host
        && ssh_remote
        && !remote_host.contains(['.', ':'])
        && let Some(resolved) = resolve_alias(&remote_host)
    {
        remote_host = resolved.to_ascii_lowercase();
    }
    let same_host = host == remote_host
        // GitHub's and GitLab's SSH-over-443 hosts: ssh.github.com, altssh.gitlab.com.
        || ["ssh.", "altssh."]
            .iter()
            .any(|prefix| remote_host.strip_prefix(prefix) == Some(host));
    let repo = repo.trim_matches('/');
    let remote_repo = repo_path(remote_url).to_ascii_lowercase();
    if !same_host || repo != remote_repo {
        bail!(
            "{url} is not a review in this repository ({remote_host}/{remote_repo}); \
             pass the review number to look it up here"
        );
    }
    Ok(number)
}

/// The host an SSH alias connects to, from `ssh -G`, or None when ssh cannot
/// say.
fn ssh_hostname(alias: &str) -> Option<String> {
    if alias.starts_with('-') {
        return None;
    }
    command_output("ssh", &["-G", "--", alias])
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("hostname ").map(str::to_owned))
}

/// The repository path of a git remote URL - `owner/repo`, or deeper for a
/// GitLab subgroup - without any `.git` suffix.
fn repo_path(url: &str) -> &str {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map_or("", |(_, path)| path),
        // scp-like `git@host:owner/repo`.
        None => url.split_once(':').map_or("", |(_, path)| path),
    };
    let path = path.trim_matches('/');
    path.strip_suffix(".git").unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_alias(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn parse_number_takes_only_plain_digits() {
        assert_eq!(parse_number("12"), Some(12));
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("+1"), None);
        assert_eq!(parse_number("12a"), None);
        assert_eq!(parse_number("feature/12"), None);
    }

    #[test]
    fn repo_path_handles_remote_shapes() {
        assert_eq!(repo_path("https://github.com/owner/repo.git"), "owner/repo");
        assert_eq!(repo_path("https://github.com/owner/repo/"), "owner/repo");
        assert_eq!(repo_path("git@github.com:owner/repo.git"), "owner/repo");
        assert_eq!(
            repo_path("ssh://git@gitea.example.com:2222/owner/repo"),
            "owner/repo"
        );
        assert_eq!(
            repo_path("https://user:token@gitlab.com/group/sub/repo.git"),
            "group/sub/repo"
        );
    }

    #[test]
    fn parse_url_reads_each_providers_review_urls() {
        let github = "git@github.com:owner/repo.git";
        for url in [
            "https://github.com/owner/repo/pull/12",
            "https://github.com/owner/repo/pull/12/files",
            "https://github.com/Owner/Repo/pull/12?diff=split#top",
            "https://github.com/owner/repo/pull/12/commits/abc123",
        ] {
            assert_eq!(
                parse_url(url, ProviderKind::GitHub, github, &no_alias).unwrap(),
                12,
                "{url}"
            );
        }

        assert_eq!(
            parse_url(
                "https://gitlab.com/group/sub/repo/-/merge_requests/34/diffs",
                ProviderKind::GitLab,
                "https://gitlab.com/group/sub/repo.git",
                &no_alias,
            )
            .unwrap(),
            34
        );

        assert_eq!(
            parse_url(
                "https://codeberg.org/owner/repo/pulls/7",
                ProviderKind::Gitea,
                "ssh://git@codeberg.org/owner/repo.git",
                &no_alias,
            )
            .unwrap(),
            7
        );
    }

    #[test]
    fn parse_url_tolerates_a_repository_named_like_the_marker() {
        assert_eq!(
            parse_url(
                "https://github.com/owner/pull/pull/5",
                ProviderKind::GitHub,
                "git@github.com:owner/pull.git",
                &no_alias,
            )
            .unwrap(),
            5
        );
    }

    #[test]
    fn parse_url_rejects_another_repositorys_review() {
        let error = parse_url(
            "https://github.com/someone/else/pull/12",
            ProviderKind::GitHub,
            "git@github.com:owner/repo.git",
            &no_alias,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not a review in this repository (github.com/owner/repo)"),
            "{error:#}"
        );
    }

    #[test]
    fn parse_url_rejects_another_providers_url_shape() {
        let error = parse_url(
            "https://github.com/owner/repo/pull/12",
            ProviderKind::GitLab,
            "git@github.com:owner/repo.git",
            &no_alias,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("is not a gitlab review URL"),
            "{error:#}"
        );
    }

    #[test]
    fn parse_url_compares_the_host_an_ssh_alias_stands_for() {
        let aliases = |alias: &str| match alias {
            "github-work" => Some("github.com".to_owned()),
            "gl" => Some("gitlab.corp.com".to_owned()),
            _ => None,
        };
        assert_eq!(
            parse_url(
                "https://github.com/owner/repo/pull/12",
                ProviderKind::GitHub,
                "git@github-work:owner/repo.git",
                &aliases,
            )
            .unwrap(),
            12
        );
        // An alias for a self-hosted instance does not stand for gitlab.com.
        assert!(
            parse_url(
                "https://gitlab.com/group/repo/-/merge_requests/3",
                ProviderKind::GitLab,
                "git@gl:group/repo.git",
                &aliases,
            )
            .is_err()
        );
        // Nor does one ssh cannot resolve.
        assert!(
            parse_url(
                "https://github.com/owner/repo/pull/12",
                ProviderKind::GitHub,
                "git@mystery:owner/repo.git",
                &aliases,
            )
            .is_err()
        );
    }

    #[test]
    fn parse_url_resolves_only_a_mismatched_ssh_host() {
        let resolve = |alias: &str| -> Option<String> { panic!("resolved {alias}") };
        // Same host, and an HTTPS remote: neither consults ssh.
        for remote in [
            "git@gitea:owner/repo.git",
            "https://gitea:3000/owner/repo.git",
        ] {
            assert_eq!(
                parse_url(
                    "https://gitea/owner/repo/pulls/4",
                    ProviderKind::Gitea,
                    remote,
                    &resolve,
                )
                .unwrap(),
                4,
                "{remote}"
            );
        }
    }

    #[test]
    fn parse_url_accepts_an_ssh_subdomain_of_the_url_host() {
        assert_eq!(
            parse_url(
                "https://github.com/owner/repo/pull/12",
                ProviderKind::GitHub,
                "ssh://git@ssh.github.com:443/owner/repo.git",
                &no_alias,
            )
            .unwrap(),
            12
        );
    }

    #[test]
    fn parse_url_rejects_a_parent_domain_of_the_remote_host() {
        assert!(
            parse_url(
                "https://corp.com/group/repo/-/merge_requests/3",
                ProviderKind::GitLab,
                "git@gitlab.corp.com:group/repo.git",
                &no_alias,
            )
            .is_err()
        );
    }

    #[test]
    fn parse_url_rejects_another_instance_of_the_same_provider() {
        assert!(
            parse_url(
                "https://gitlab.com/group/repo/-/merge_requests/3",
                ProviderKind::GitLab,
                "git@gitlab.corp.com:group/repo.git",
                &no_alias,
            )
            .is_err()
        );
        assert!(
            parse_url(
                "https://gitea.com/owner/repo/pulls/3",
                ProviderKind::Gitea,
                "https://codeberg.org/owner/repo.git",
                &no_alias,
            )
            .is_err()
        );
    }

    #[test]
    fn parse_url_rejects_a_foreign_host_with_the_same_repo_path() {
        assert!(
            parse_url(
                "https://gitlab.example.com/owner/repo/-/merge_requests/3",
                ProviderKind::GitLab,
                "git@gitlab.com:owner/repo.git",
                &no_alias,
            )
            .is_err()
        );
    }
}
