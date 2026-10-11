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

use diff::{FileDiff, FileStatus, LineKind};
use steps::Step;

const TEMPLATE: &str = include_str!("template.html");
const HIGHLIGHT_JS: &str = include_str!("vendor/highlight.min.js");
const HIGHLIGHT_LICENSE: &str = include_str!("vendor/highlight.js-LICENSE");

/// The template's placeholders, in the order they appear in it.
const TITLE_PLACEHOLDER: &str = "<!--TOUR_TITLE-->";
const LICENSE_PLACEHOLDER: &str = "@@HIGHLIGHT_LICENSE@@";
const HIGHLIGHT_PLACEHOLDER: &str = "/*HIGHLIGHT_JS*/";
const DATA_PLACEHOLDER: &str = "/*TOUR_DATA*/null";

/// Files larger than this are shown without their full text, so without
/// whole-file highlighting or context expansion.
const MAX_EMBEDDED_BYTES: u64 = 1024 * 1024;

/// Once a tour has embedded this much text, later files go without, keeping
/// the page quick to open on a large review.
const MAX_TOTAL_EMBEDDED_BYTES: u64 = 16 * 1024 * 1024;

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
        let mut steps = steps::group(files, |path| generated.iter().any(|g| g == path));
        embed_texts(&mut steps, &base_sha, &head_sha)?;

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
        fill(
            TEMPLATE,
            &[
                (TITLE_PLACEHOLDER, &escape_html(&title)),
                (LICENSE_PLACEHOLDER, HIGHLIGHT_LICENSE),
                (HIGHLIGHT_PLACEHOLDER, HIGHLIGHT_JS),
                (DATA_PLACEHOLDER, &script_json(&self.data())),
            ],
        )
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

/// Read the whole text of both sides of every modified, renamed, or copied
/// file with hunks, for whole-file highlighting and context expansion. An added or
/// deleted file's hunk already holds every line, and generated and binary
/// files are not worth the space.
fn embed_texts(steps: &mut [Step], base_sha: &str, head_sha: &str) -> Result<()> {
    fn embeddable(step: &Step) -> impl Iterator<Item = &FileDiff> {
        step.files
            .iter()
            .filter(move |file| wants_text(step.kind, file))
    }
    let specs: Vec<String> = steps
        .iter()
        .flat_map(embeddable)
        .flat_map(|file| {
            [
                format!("{base_sha}:{}", file.old_path),
                format!("{head_sha}:{}", file.new_path),
            ]
        })
        .collect();
    let mut texts =
        git::blob_texts(&specs, MAX_EMBEDDED_BYTES, MAX_TOTAL_EMBEDDED_BYTES)?.into_iter();
    for step in steps.iter_mut() {
        let kind = step.kind;
        for file in step.files.iter_mut() {
            if wants_text(kind, file) {
                file.old_text = texts.next().flatten();
                file.new_text = texts.next().flatten();
            }
        }
    }
    Ok(())
}

fn wants_text(kind: steps::StepKind, file: &FileDiff) -> bool {
    kind != steps::StepKind::Generated
        && !file.binary
        // A pure rename or mode change has no hunk to expand around.
        && !file.hunks.is_empty()
        && !matches!(file.status, FileStatus::Added | FileStatus::Deleted)
}

/// Substitute each placeholder in turn, searching only the template text
/// after the previous one, so a value that happens to contain a later
/// placeholder - a diff of this template, say - is never substituted into.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    for (placeholder, value) in values {
        match rest.split_once(placeholder) {
            Some((before, after)) => {
                output.push_str(before);
                output.push_str(value);
                rest = after;
            }
            None => debug_assert!(false, "template has no {placeholder}"),
        }
    }
    output.push_str(rest);
    output
}

/// The highlight.js language for a path, or "" for plain text. Only names
/// in the bundled common build are useful; the page checks before using one.
fn language(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "Makefile" | "makefile" | "GNUmakefile" | "justfile" | "Justfile" => return "makefile",
        "Dockerfile" | "Containerfile" => return "bash",
        _ => {}
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" | "tsx" => "typescript",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "m" | "mm" => "objectivec",
        "cs" => "csharp",
        "vb" => "vbnet",
        "rb" => "ruby",
        "php" => "php",
        "pl" | "pm" => "perl",
        "lua" => "lua",
        "r" => "r",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "graphql" | "gql" => "graphql",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "html" | "htm" | "xml" | "svg" | "vue" | "svelte" => "xml",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" | "ini" | "cfg" | "conf" => "ini",
        "md" | "markdown" | "mdx" => "markdown",
        "diff" | "patch" => "diff",
        "wat" | "wast" => "wasm",
        _ => "",
    }
}

fn file_data(file: &FileDiff) -> Value {
    json!({
        "path": file.path(),
        "oldPath": file.old_path,
        "status": file.status.as_str(),
        "binary": file.binary,
        "language": language(file.path()),
        "oldText": file.old_text,
        "newText": file.new_text,
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
    fn template_carries_the_placeholders_in_fill_order() {
        let positions: Vec<usize> = [
            TITLE_PLACEHOLDER,
            LICENSE_PLACEHOLDER,
            HIGHLIGHT_PLACEHOLDER,
            DATA_PLACEHOLDER,
        ]
        .iter()
        .map(|placeholder| TEMPLATE.find(placeholder).expect(placeholder))
        .collect();
        assert!(positions.is_sorted(), "{positions:?}");
    }

    #[test]
    fn fill_never_substitutes_into_an_earlier_value() {
        let filled = fill(
            "<a>[1]</a><b>[2]</b>",
            &[("[1]", "has [2]"), ("[2]", "two")],
        );
        assert_eq!(filled, "<a>has [2]</a><b>two</b>");
    }

    #[test]
    fn vendored_highlighter_is_safe_to_inline() {
        // Either sequence would end the surrounding element early.
        assert!(!HIGHLIGHT_JS.to_ascii_lowercase().contains("</script"));
        assert!(!HIGHLIGHT_LICENSE.contains("--"));
    }

    #[test]
    fn language_maps_extensions_and_names() {
        assert_eq!(language("src/main.rs"), "rust");
        assert_eq!(language("web/App.TSX"), "typescript");
        assert_eq!(language("Makefile"), "makefile");
        assert_eq!(language("config/app.yml"), "yaml");
        assert_eq!(language("LICENSE"), "");
        assert_eq!(language("archive.tar.gz"), "");
    }
}
