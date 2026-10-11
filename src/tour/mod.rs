//! A review's changes as a self-contained HTML page that walks through them
//! step by step, for `git stk tour`.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::git;
use crate::notes;
use crate::providers::{ProviderKind, ReviewProvider, ReviewRequest};
use crate::settings;

mod diff;
mod steps;

use diff::{FileDiff, LineKind};
use steps::Step;

const TEMPLATE: &str = include_str!("template.html");
const DATA_PLACEHOLDER: &str = "/*TOUR_DATA*/null";
const TITLE_PLACEHOLDER: &str = "<!--TOUR_TITLE-->";

/// A review's commits and changes, read from git.
pub struct Tour {
    review: ReviewRequest,
    body: String,
    base_sha: String,
    head_sha: String,
    commits: Vec<(String, String)>,
    steps: Vec<Step>,
}

impl Tour {
    /// Fetch the review's head and base and read the change between them.
    pub fn load(
        kind: ProviderKind,
        provider: &dyn ReviewProvider,
        review: ReviewRequest,
    ) -> Result<Self> {
        let (base_tip, head_sha) = match head_ref(kind, review.id_value()) {
            Some(head_ref) => {
                let remote = settings::remote()?;
                let base = git::fetch_commit(&remote, &format!("refs/heads/{}", review.base))?;
                (base, git::fetch_commit(&remote, &head_ref)?)
            }
            // The demo provider's reviews live only in this repository.
            None => (
                git::rev_parse(&review.base)?,
                git::rev_parse(&review.branch)?,
            ),
        };
        let base_sha = git::merge_base(&base_tip, &head_sha)?;
        let files = diff::parse(&git::tour_diff(&base_sha, &head_sha)?);
        if files.is_empty() {
            bail!(
                "{} has no changes against {}; a merged review's changes are already in its base",
                review.id,
                review.base
            );
        }

        let paths: Vec<&str> = files.iter().map(FileDiff::path).collect();
        let generated = git::generated_paths(&paths, &head_sha)?;
        let steps = steps::group(files, |path| generated.iter().any(|g| g == path));

        let mut commits = git::log_oneline(&format!("{base_sha}..{head_sha}"))?;
        commits.reverse();
        let body = provider
            .review_body(&review)
            .with_context(|| format!("failed to read the description of {}", review.id))?;
        let body = notes::readable_body(&body);

        Ok(Self {
            review,
            body,
            base_sha,
            head_sha,
            commits,
            steps,
        })
    }

    /// The file name a tour of this review at its current head is saved as.
    pub fn file_name(&self) -> String {
        let short = self.head_sha.get(..12).unwrap_or(&self.head_sha);
        format!("{}-{short}.html", self.review.id_value())
    }

    pub fn file_count(&self) -> usize {
        self.steps.iter().map(|step| step.files.len()).sum()
    }

    pub fn render(&self) -> String {
        let title = format!("Tour of {}", self.review.label());
        // Data first: its JSON escapes `<`, so it cannot carry the title marker.
        TEMPLATE
            .replacen(DATA_PLACEHOLDER, &script_json(&self.data()), 1)
            .replacen(TITLE_PLACEHOLDER, &escape_html(&title), 1)
    }

    fn data(&self) -> Value {
        json!({
            "review": {
                "id": self.review.id,
                "title": self.review.title,
                "url": self.review.url,
                "state": self.review.state.to_string(),
                "branch": self.review.branch,
                "base": self.review.base,
                "body": self.body,
            },
            "baseSha": self.base_sha,
            "headSha": self.head_sha,
            "commits": self.commits.iter().map(|(sha, subject)| {
                json!({ "sha": sha, "subject": subject })
            }).collect::<Vec<_>>(),
            "steps": self.steps.iter().map(|step| json!({
                "id": step.kind.id(),
                "title": step.kind.title(),
                "collapsed": step.kind == steps::StepKind::Generated,
                "files": step.files.iter().map(file_data).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
    }
}

/// The ref a review's head is published under on the remote, or None for a
/// provider with no remote.
fn head_ref(kind: ProviderKind, number: &str) -> Option<String> {
    match kind {
        ProviderKind::GitHub | ProviderKind::Gitea => Some(format!("refs/pull/{number}/head")),
        ProviderKind::GitLab => Some(format!("refs/merge-requests/{number}/head")),
        ProviderKind::Demo => None,
    }
}

fn file_data(file: &FileDiff) -> Value {
    json!({
        "path": file.path(),
        "oldPath": file.old_path,
        "status": file.status.as_str(),
        "binary": file.binary,
        "additions": file.additions(),
        "deletions": file.deletions(),
        "hunks": file.hunks.iter().map(|hunk| json!({
            "oldStart": hunk.old_start,
            "oldLines": hunk.old_lines,
            "newStart": hunk.new_start,
            "newLines": hunk.new_lines,
            "section": hunk.section,
            // [kind, text] pairs keep a large diff's payload small.
            "lines": hunk.lines.iter().map(|line| json!([
                match line.kind {
                    LineKind::Context => " ",
                    LineKind::Added => "+",
                    LineKind::Deleted => "-",
                },
                line.text,
            ])).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// JSON that is safe inside a `<script>` element: no `</script>` or `<!--`
/// can end it early, and the line separators old engines reject are escaped.
fn script_json(value: &Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_json_cannot_close_the_script_element() {
        let json = script_json(&json!({ "text": "</script><!-- \u{2028}" }));
        assert!(!json.contains('<'), "{json}");
        assert!(json.contains("\\u003c/script>"));
        assert!(json.contains("\\u2028"));
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["text"], "</script><!-- \u{2028}");
    }

    #[test]
    fn template_carries_both_placeholders() {
        assert!(TEMPLATE.contains(DATA_PLACEHOLDER));
        assert!(TEMPLATE.contains(TITLE_PLACEHOLDER));
    }
}
