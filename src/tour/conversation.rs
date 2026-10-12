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

/// What reviewers have said, for the tour agent: Markdown as written,
/// without resolved threads. Long bodies are cut short, and past a total the
/// rest is left out: open threads first, then reviews, then the latest
/// comments.
pub fn for_agent(conversation: &Conversation) -> Value {
    const MAX_BODY_CHARS: usize = 2000;
    const MAX_TOTAL_CHARS: usize = 30_000;
    const MAX_THREADS: usize = 40;
    const MAX_COMMENTS: usize = 30;
    let left = std::cell::Cell::new(MAX_TOTAL_CHARS);
    let clip = |body: &str| -> Option<String> {
        let room = MAX_BODY_CHARS.min(left.get());
        if room == 0 {
            return None;
        }
        let text = match body.char_indices().nth(room) {
            Some((at, _)) => format!("{} [cut short]", &body[..at]),
            None => body.to_owned(),
        };
        left.set(left.get().saturating_sub(text.chars().count()));
        Some(text)
    };
    let said = |comment: &Comment| {
        clip(&comment.body).map(|body| json!({ "author": comment.author, "body": body }))
    };
    let threads: Vec<Value> = conversation
        .threads
        .iter()
        .filter(|thread| thread.resolved != Some(true))
        .take(MAX_THREADS)
        .filter_map(|thread| {
            let comments: Vec<Value> = thread.comments.iter().filter_map(said).collect();
            (!comments.is_empty()).then(|| {
                json!({
                    "path": thread.path,
                    "side": thread.anchor.map(|anchor| anchor.side.as_str()),
                    "line": thread.anchor.map(|anchor| anchor.line),
                    "outdated": thread.outdated,
                    "comments": comments,
                })
            })
        })
        .collect();
    let reviews: Vec<Value> = conversation
        .verdicts
        .iter()
        .filter(|verdict| {
            verdict.state != VerdictState::Commented || !verdict.body.trim().is_empty()
        })
        .filter_map(|verdict| {
            clip(&verdict.body).map(|body| {
                json!({ "author": verdict.author, "state": verdict.state.as_str(), "body": body })
            })
        })
        .collect();
    let mut comments: Vec<Value> = conversation
        .comments
        .iter()
        .rev()
        .take(MAX_COMMENTS)
        .filter_map(said)
        .collect();
    comments.reverse();
    json!({ "reviews": reviews, "comments": comments, "openThreads": threads })
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
    fn the_agent_hears_open_threads_in_plain_markdown() {
        let comment = |body: &str| Comment {
            id: "C".into(),
            author: "ann".into(),
            body: body.into(),
            created_at: String::new(),
            url: String::new(),
        };
        let thread = |id: &str, resolved, body: &str| Thread {
            id: id.into(),
            path: "src/a.rs".into(),
            anchor: Some(Anchor {
                side: Side::New,
                line: 3,
                start_line: None,
            }),
            outdated: false,
            resolved,
            diff_hunk: None,
            comments: vec![comment(body)],
        };
        let conversation = Conversation {
            verdicts: Vec::new(),
            comments: vec![comment(&"x".repeat(2500))],
            threads: vec![
                thread("T1", Some(true), "settled"),
                thread("T2", Some(false), "Is `a` *needed*?"),
                thread("T3", None, "Gitea cannot say"),
            ],
            incomplete: false,
        };
        let said = for_agent(&conversation);
        let open: Vec<&str> = said["openThreads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|thread| thread["comments"][0]["body"].as_str().unwrap())
            .collect();
        assert_eq!(open, ["Is `a` *needed*?", "Gitea cannot say"]);
        assert_eq!(said["openThreads"][0]["line"], 3);
        let long = said["comments"][0]["body"].as_str().unwrap();
        assert!(
            long.ends_with(" [cut short]") && long.len() < 2100,
            "{long}"
        );

        let crowded = Conversation {
            verdicts: Vec::new(),
            comments: Vec::new(),
            threads: (0..40)
                .map(|n| thread(&format!("T{n}"), Some(false), &"y".repeat(2500)))
                .collect(),
            incomplete: false,
        };
        let said = for_agent(&crowded);
        let size = said.to_string().len();
        assert!(
            said["openThreads"].as_array().unwrap().len() < 20 && size < 32_000,
            "{size} bytes"
        );
    }

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
