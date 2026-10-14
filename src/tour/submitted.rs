//! A record of each review submission until the forge confirms it, so a
//! submission whose answer was lost is not posted twice. Kept in
//! `.git/stk/tours/submissions/<review>.json`.

use serde_json::{Value, json};

use crate::git;
use crate::providers::{
    Conversation, ReviewEvent, ReviewRequest, Submission, VerdictState, epoch_seconds,
};

/// How far the forge's clock may run behind this machine's.
const CLOCK_SLACK_SECONDS: i64 = 120;

/// What of a submission reached the forge.
#[derive(Debug, PartialEq, Eq)]
pub enum Landed {
    All,
    Part,
    Nothing,
    /// The discussion was cut short before all of it could be checked.
    Unsure,
}

/// A submission's contents as a short id, to tell a retry from a new review.
pub fn fingerprint(submission: &Submission) -> String {
    let mut text = format!(
        "{}\0{}\0",
        submission.event.as_str(),
        submission.body.trim()
    );
    for comment in &submission.comments {
        text.push_str(&format!(
            "{}\0{}\0{}\0{}\0",
            comment.path,
            comment.anchor.side.as_str(),
            comment.anchor.line,
            comment.body.trim()
        ));
    }
    format!("{:016x}", crate::tour::fnv1a(text.as_bytes()))
}

fn path(review: &ReviewRequest) -> Option<std::path::PathBuf> {
    git::git_common_path_absolute(&format!("stk/tours/submissions/{}.json", review.id_value())).ok()
}

/// When the same submission to `review` was started and not confirmed, in
/// seconds since the epoch.
pub fn pending(review: &ReviewRequest, submission: &Submission) -> Option<i64> {
    let record: Value = serde_json::from_str(&std::fs::read_to_string(path(review)?).ok()?).ok()?;
    (record["fingerprint"] == fingerprint(submission))
        .then(|| record["at"].as_i64())
        .flatten()
}

/// Note that `submission` to `review` is being posted.
pub fn start(review: &ReviewRequest, submission: &Submission) -> anyhow::Result<()> {
    let path = git::git_common_path_absolute(&format!(
        "stk/tours/submissions/{}.json",
        review.id_value()
    ))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    std::fs::write(
        path,
        json!({ "fingerprint": fingerprint(submission), "at": at }).to_string(),
    )?;
    Ok(())
}

/// Forget the record once the forge has confirmed, or ruled on, it.
pub fn finish(review: &ReviewRequest) {
    if let Some(path) = path(review) {
        let _ = std::fs::remove_file(path);
    }
}

/// What of `submission` shows in `conversation` since `since`, written by
/// `me`: its outcome, its summary, and each line comment.
pub fn landed(
    submission: &Submission,
    conversation: &Conversation,
    since: i64,
    me: &str,
) -> Landed {
    let recent =
        |when: &str| epoch_seconds(when).is_some_and(|at| at >= since - CLOCK_SLACK_SECONDS);
    let said = |body: &str| {
        let body = body.trim();
        conversation.verdicts.iter().any(|verdict| {
            verdict.author == me && recent(&verdict.submitted_at) && verdict.body.trim() == body
        }) || conversation
            .comments
            .iter()
            .chain(
                conversation
                    .threads
                    .iter()
                    .flat_map(|thread| &thread.comments),
            )
            .any(|comment| {
                comment.author == me && recent(&comment.created_at) && comment.body.trim() == body
            })
    };
    let mut parts: Vec<bool> = submission
        .comments
        .iter()
        .map(|comment| said(&comment.body))
        .collect();
    if !submission.body.trim().is_empty() {
        parts.push(said(&submission.body));
    }
    let outcome = match submission.event {
        ReviewEvent::Approve => Some(VerdictState::Approved),
        ReviewEvent::RequestChanges => Some(VerdictState::ChangesRequested),
        ReviewEvent::Comment => None,
    };
    if let Some(state) = outcome {
        // GitLab keeps an approval as standing state, with no time on it.
        parts.push(conversation.verdicts.iter().any(|verdict| {
            verdict.author == me
                && verdict.state == state
                && (verdict.submitted_at.is_empty() || recent(&verdict.submitted_at))
        }));
    }
    match (
        parts.iter().all(|&found| found),
        parts.iter().any(|&found| found),
    ) {
        (true, _) => Landed::All,
        // What was not seen may lie past where the listing stopped.
        _ if conversation.incomplete => Landed::Unsure,
        (false, true) => Landed::Part,
        (false, false) => Landed::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Anchor, Comment, DraftComment, Side, Thread, Verdict};

    fn submission(event: ReviewEvent, body: &str, comments: &[&str]) -> Submission {
        Submission {
            event,
            body: body.into(),
            comments: comments
                .iter()
                .map(|body| DraftComment {
                    path: "a.rs".into(),
                    old_path: "a.rs".into(),
                    anchor: Anchor {
                        side: Side::New,
                        line: 3,
                        start_line: None,
                    },
                    paired_line: None,
                    body: (*body).into(),
                })
                .collect(),
            commit: "abc".into(),
        }
    }

    fn comment(body: &str, at: &str) -> Comment {
        Comment {
            id: "c".into(),
            author: "me".into(),
            body: body.into(),
            created_at: at.into(),
            url: String::new(),
        }
    }

    fn conversation(verdict: Option<(VerdictState, &str)>, lines: &[&str]) -> Conversation {
        Conversation {
            verdicts: verdict
                .into_iter()
                .map(|(state, body)| Verdict {
                    author: "me".into(),
                    state,
                    body: body.into(),
                    submitted_at: "2026-10-14T12:00:00Z".into(),
                    url: String::new(),
                })
                .collect(),
            comments: Vec::new(),
            threads: lines
                .iter()
                .map(|body| Thread {
                    id: "t".into(),
                    path: "a.rs".into(),
                    anchor: None,
                    outdated: false,
                    resolved: Some(false),
                    diff_hunk: None,
                    comments: vec![comment(body, "2026-10-14T12:00:00Z")],
                })
                .collect(),
            incomplete: false,
        }
    }

    #[test]
    fn a_retry_finds_what_reached_the_forge() {
        let since = epoch_seconds("2026-10-14T11:59:00Z").unwrap();
        let sent = submission(ReviewEvent::Approve, "Looks good", &["Nit", "Typo"]);
        let all = conversation(
            Some((VerdictState::Approved, "Looks good")),
            &["Nit", "Typo"],
        );
        assert_eq!(landed(&sent, &all, since, "me"), Landed::All);
        let part = conversation(None, &["Nit"]);
        assert_eq!(landed(&sent, &part, since, "me"), Landed::Part);
        assert_eq!(
            landed(&sent, &conversation(None, &[]), since, "me"),
            Landed::Nothing
        );
        let later = epoch_seconds("2026-10-14T13:00:00Z").unwrap();
        assert_eq!(
            landed(&sent, &all, since, "ann"),
            Landed::Nothing,
            "someone else's approval and comments are not yours"
        );
        let mut timeless = conversation(
            Some((VerdictState::Approved, "Looks good")),
            &["Nit", "Typo"],
        );
        timeless.verdicts[0].submitted_at = String::new();
        timeless
            .comments
            .push(comment("Looks good", "2026-10-14T12:00:00Z"));
        assert_eq!(
            landed(&sent, &timeless, since, "me"),
            Landed::All,
            "a GitLab approval has no time, and its summary is a note"
        );
        let mut cut = conversation(None, &["Nit"]);
        cut.incomplete = true;
        assert_eq!(landed(&sent, &cut, since, "me"), Landed::Unsure);
        assert_eq!(
            landed(&sent, &all, later, "me"),
            Landed::Nothing,
            "only what came since counts"
        );
    }

    #[test]
    fn a_submission_is_known_by_what_it_says() {
        let one = submission(ReviewEvent::Comment, "Hi", &["Nit"]);
        assert_eq!(
            fingerprint(&one),
            fingerprint(&submission(ReviewEvent::Comment, " Hi ", &["Nit"]))
        );
        assert_ne!(
            fingerprint(&one),
            fingerprint(&submission(ReviewEvent::Approve, "Hi", &["Nit"]))
        );
        assert_ne!(
            fingerprint(&one),
            fingerprint(&submission(ReviewEvent::Comment, "Hi", &["Nits"]))
        );
    }
}
