//! A review's discussion - verdicts, threads on code, and comments on the
//! review as a whole - in one shape for every forge.

#[derive(Debug, Default)]
pub struct Conversation {
    pub verdicts: Vec<Verdict>,
    /// Comments on the review as a whole rather than on its code.
    pub comments: Vec<Comment>,
    pub threads: Vec<Thread>,
    /// The forge had more than one listing returns, so some are missing.
    pub incomplete: bool,
}

/// A reviewer's submitted review: its outcome and summary.
#[derive(Debug)]
pub struct Verdict {
    pub author: String,
    pub state: VerdictState,
    pub body: String,
    pub submitted_at: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum VerdictState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}

impl VerdictState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ChangesRequested => "changes_requested",
            Self::Commented => "commented",
            Self::Dismissed => "dismissed",
        }
    }
}

#[derive(Debug)]
pub struct Comment {
    pub id: String,
    pub author: String,
    /// Markdown, as written.
    pub body: String,
    pub created_at: String,
    pub url: String,
}

/// Comments on one file, or on lines of it.
#[derive(Debug)]
pub struct Thread {
    pub id: String,
    pub path: String,
    /// Where on the file the thread sits; None for the whole file.
    pub anchor: Option<Anchor>,
    /// Left on an earlier version of the review, so `anchor` is a line of
    /// that version and may not match the current diff.
    pub outdated: bool,
    /// None where the forge does not track resolution.
    pub resolved: Option<bool>,
    /// The diff the thread was started on, for showing an outdated one.
    pub diff_hunk: Option<String>,
    pub comments: Vec<Comment>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Anchor {
    pub side: Side,
    pub line: u32,
    /// The first line of a multi-line comment.
    pub start_line: Option<u32>,
}

/// Which side of a diff a line is on: the old file or the new one.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }
}
