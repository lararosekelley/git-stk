//! A review's discussion as tour page data, with every body rendered from
//! Markdown through the same sanitizer as the description.

use serde_json::{Value, json};

use super::markdown;
use crate::providers::{Comment, Conversation, VerdictState};

pub fn data(conversation: &Conversation) -> Value {
    json!({
        // A comment-only review with no summary just carries its line
        // comments, which the threads already show.
        "verdicts": conversation.verdicts.iter().filter(|verdict| {
            verdict.state != VerdictState::Commented || !verdict.body.trim().is_empty()
        }).map(|verdict| json!({
            "author": verdict.author,
            "state": verdict.state.as_str(),
            "bodyHtml": markdown::to_html(&verdict.body),
            "at": verdict.submitted_at,
            "url": verdict.url,
        })).collect::<Vec<_>>(),
        "comments": conversation.comments.iter().map(comment).collect::<Vec<_>>(),
        "threads": conversation.threads.iter().map(|thread| json!({
            "id": thread.id,
            "path": thread.path,
            "side": thread.anchor.map(|anchor| anchor.side.as_str()),
            "line": thread.anchor.map(|anchor| anchor.line),
            "startLine": thread.anchor.and_then(|anchor| anchor.start_line),
            "outdated": thread.outdated,
            "resolved": thread.resolved,
            "diffHunk": thread.diff_hunk.as_deref().map(hunk_tail),
            "comments": thread.comments.iter().map(comment).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "incomplete": conversation.incomplete,
    })
}

/// A discussion that could not be read, for the page to say so.
pub fn error(message: &str) -> Value {
    json!({ "error": message })
}

fn comment(comment: &Comment) -> Value {
    json!({
        "id": comment.id,
        "author": comment.author,
        "bodyHtml": markdown::to_html(&comment.body),
        "at": comment.created_at,
        "url": comment.url,
    })
}

/// The last lines of a thread's diff hunk, where the commented line is:
/// forges send the hunk from its start, which can run to hundreds of lines.
fn hunk_tail(hunk: &str) -> String {
    const LINES: usize = 6;
    let lines: Vec<&str> = hunk
        .lines()
        .filter(|line| !line.starts_with("@@"))
        .collect();
    lines[lines.len().saturating_sub(LINES)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Anchor, Side, Thread, Verdict};

    #[test]
    fn bodies_are_sanitized_and_hunks_trimmed() {
        let conversation = Conversation {
            verdicts: vec![Verdict {
                author: "rev".into(),
                state: VerdictState::ChangesRequested,
                body: "Please <script>x</script>fix".into(),
                submitted_at: "2026-10-11T00:00:00Z".into(),
                url: String::new(),
            }],
            comments: Vec::new(),
            threads: vec![Thread {
                id: "T1".into(),
                path: "src/a.rs".into(),
                anchor: Some(Anchor {
                    side: Side::Old,
                    line: 9,
                    start_line: Some(7),
                }),
                outdated: true,
                resolved: Some(false),
                diff_hunk: Some(format!(
                    "@@ -1,9 +1,9 @@\n{}",
                    (1..=9)
                        .map(|n| format!(" line {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )),
                comments: Vec::new(),
            }],
            incomplete: false,
        };
        let data = data(&conversation);
        assert_eq!(data["verdicts"][0]["state"], "changes_requested");
        assert!(
            !data["verdicts"][0]["bodyHtml"]
                .as_str()
                .unwrap()
                .contains("<script")
        );
        let thread = &data["threads"][0];
        assert_eq!(
            (thread["side"].as_str(), thread["line"].as_u64()),
            (Some("old"), Some(9))
        );
        assert_eq!(thread["startLine"], 7);
        assert_eq!(
            thread["diffHunk"],
            " line 4\n line 5\n line 6\n line 7\n line 8\n line 9"
        );
    }
}
