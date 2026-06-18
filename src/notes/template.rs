//! Discovery of the repository's pull/merge request template, so a freshly
//! created review's body starts from it rather than `--fill` replacing it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::git;
use crate::providers::ProviderKind;
use crate::style;

/// The repo's PR/MR template text for `kind`, or None when there isn't a
/// single unambiguous one. Read from the working tree (these are committed
/// files), so it needs no network round-trip.
pub(super) fn discover(kind: ProviderKind) -> Result<Option<String>> {
    let root = git::repo_root()?;
    Ok(match kind {
        ProviderKind::GitHub => github_template(&root),
        ProviderKind::GitLab => gitlab_template(&root),
        ProviderKind::Gitea => gitea_template(&root),
        ProviderKind::Demo => None,
    })
}

/// GitHub honors a `pull_request_template` (`.md`, `.txt`, or extensionless,
/// basename case-insensitive) in the repo root, `.github/`, or `docs/`. A
/// `PULL_REQUEST_TEMPLATE/` directory holds several named templates with no
/// single default, so it is reported and skipped rather than guessed at.
fn github_template(root: &Path) -> Option<String> {
    pr_template(root, &[".github", ".", "docs"])
}

/// Gitea (and Forgejo) read the same `PULL_REQUEST_TEMPLATE` files, also
/// honoring a `.gitea/` directory alongside the GitHub locations.
fn gitea_template(root: &Path) -> Option<String> {
    pr_template(root, &[".gitea", ".github", ".", "docs"])
}

/// Scan `dirs` (in order) for a single `PULL_REQUEST_TEMPLATE` file.
fn pr_template(root: &Path, dirs: &[&str]) -> Option<String> {
    const NAMES: [&str; 2] = ["PULL_REQUEST_TEMPLATE", "pull_request_template"];
    const EXTS: [&str; 3] = [".md", ".txt", ""];

    for dir in dirs {
        let base = root.join(dir);
        for name in NAMES {
            for ext in EXTS {
                if let Some(body) = read_nonempty(&base.join(format!("{name}{ext}"))) {
                    return Some(body);
                }
            }
        }
        let multi = base.join("PULL_REQUEST_TEMPLATE");
        if multi.is_dir() {
            note_multiple(&multi);
            return None;
        }
    }
    None
}

/// GitLab keeps merge request templates in `.gitlab/merge_request_templates/`.
/// A lone template is used; with several, the one named `Default` is applied
/// automatically by GitLab, so prefer it - otherwise there is no default to
/// pick, and the set is reported and skipped.
fn gitlab_template(root: &Path) -> Option<String> {
    let dir = root.join(".gitlab").join("merge_request_templates");
    let mut templates: Vec<PathBuf> = fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        })
        .collect();
    templates.sort();

    match templates.as_slice() {
        [] => None,
        [only] => read_nonempty(only),
        many => match many.iter().find(|path| stem_eq(path, "Default")) {
            Some(default) => read_nonempty(default),
            None => {
                note_multiple(&dir);
                None
            }
        },
    }
}

fn note_multiple(location: &Path) {
    anstream::println!(
        "{}",
        style::dim(&format!(
            "skipped PR template: {} holds multiple templates with no default; none applied",
            location.display()
        ))
    );
}

/// A template file's contents with trailing whitespace trimmed, or None when
/// the file is absent or effectively empty.
fn read_nonempty(path: &Path) -> Option<String> {
    let body = fs::read_to_string(path).ok()?;
    let trimmed = body.trim_end();
    (!trimmed.trim().is_empty()).then(|| trimmed.to_owned())
}

/// Whether `path`'s file stem equals `name`, case-insensitively.
fn stem_eq(path: &Path, name: &str) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn github_reads_a_dot_github_template() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            ".github/pull_request_template.md",
            "## Summary\n",
        );
        assert_eq!(
            github_template(dir.path()),
            Some("## Summary".to_owned()),
            "trailing whitespace is trimmed"
        );
    }

    #[test]
    fn github_prefers_dot_github_over_root_and_docs() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            ".github/PULL_REQUEST_TEMPLATE.md",
            "from github",
        );
        write(dir.path(), "pull_request_template.md", "from root");
        write(dir.path(), "docs/pull_request_template.md", "from docs");
        assert_eq!(github_template(dir.path()), Some("from github".to_owned()));
    }

    #[test]
    fn github_finds_a_root_template_when_dot_github_is_absent() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "PULL_REQUEST_TEMPLATE", "extensionless");
        assert_eq!(
            github_template(dir.path()),
            Some("extensionless".to_owned())
        );
    }

    #[test]
    fn github_skips_a_multi_template_directory() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            ".github/PULL_REQUEST_TEMPLATE/feature.md",
            "feature",
        );
        write(dir.path(), ".github/PULL_REQUEST_TEMPLATE/bug.md", "bug");
        assert_eq!(github_template(dir.path()), None);
    }

    #[test]
    fn github_ignores_an_empty_template() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), ".github/pull_request_template.md", "   \n\n");
        assert_eq!(github_template(dir.path()), None);
    }

    #[test]
    fn gitlab_uses_a_lone_template() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            ".gitlab/merge_request_templates/Anything.md",
            "the body",
        );
        assert_eq!(gitlab_template(dir.path()), Some("the body".to_owned()));
    }

    #[test]
    fn gitlab_prefers_the_default_among_several() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            ".gitlab/merge_request_templates/Feature.md",
            "feature",
        );
        write(
            dir.path(),
            ".gitlab/merge_request_templates/default.md",
            "the default",
        );
        assert_eq!(gitlab_template(dir.path()), Some("the default".to_owned()));
    }

    #[test]
    fn gitlab_skips_several_without_a_default() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            ".gitlab/merge_request_templates/Feature.md",
            "feature",
        );
        write(dir.path(), ".gitlab/merge_request_templates/Bug.md", "bug");
        assert_eq!(gitlab_template(dir.path()), None);
    }

    #[test]
    fn gitlab_is_none_without_the_directory() {
        let dir = TempDir::new().unwrap();
        assert_eq!(gitlab_template(dir.path()), None);
    }
}
