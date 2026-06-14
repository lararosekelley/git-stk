//! Semantic terminal styles. Styled lines must be printed through
//! `anstream::println!`/`eprintln!`, which strip color for pipes, NO_COLOR,
//! and consoles that cannot render it.

use anstyle::{AnsiColor, Style};

use crate::providers::ReviewState;

/// Branch names.
pub const BRANCH: Style = AnsiColor::Cyan.on_default();
/// The branch you are standing on.
pub const CURRENT: Style = AnsiColor::Green.on_default().bold();
/// Secondary detail: URLs, the trunk tag, counts.
pub const DIM: Style = Style::new().dimmed();
/// The `hint:` prefix.
pub const HINT: Style = AnsiColor::Cyan.on_default();
/// The `warning:` prefix.
pub const WARN: Style = AnsiColor::Yellow.on_default();
/// The `error:` prefix.
pub const ERROR: Style = AnsiColor::Red.on_default().bold();

/// Review states, matching the ledger emoji: green open, purple merged,
/// red closed.
pub const OPEN: Style = AnsiColor::Green.on_default();
pub const MERGED: Style = AnsiColor::Magenta.on_default();
pub const CLOSED: Style = AnsiColor::Red.on_default();

/// Faded green/red for a branch's added/removed line counts, like a diff.
pub const ADDED: Style = AnsiColor::Green.on_default().dimmed();
pub const REMOVED: Style = AnsiColor::Red.on_default().dimmed();

pub fn paint(style: Style, text: &str) -> String {
    format!("{style}{text}{style:#}")
}

/// A branch name in the branch color.
pub fn branch(name: &str) -> String {
    paint(BRANCH, name)
}

/// Secondary detail: ids, urls, skip lines, previews.
pub fn dim(text: &str) -> String {
    paint(DIM, text)
}

/// Completion lines ("... complete", "merged ...").
pub fn success(text: &str) -> String {
    paint(OPEN, text)
}

/// Notable-but-not-fatal lines.
pub fn warn(text: &str) -> String {
    paint(WARN, text)
}

/// The shared `hint:` prefix.
pub fn hint_prefix() -> String {
    paint(HINT, "hint:")
}

/// The shared `error:` prefix.
pub fn error_prefix() -> String {
    paint(ERROR, "error:")
}

/// A review state in its color.
pub fn state(state: &ReviewState) -> String {
    let style = match state {
        ReviewState::Open => OPEN,
        ReviewState::Merged => MERGED,
        ReviewState::Closed => CLOSED,
        ReviewState::Unknown(_) => DIM,
    };
    paint(style, &state.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_wraps_text_in_escape_codes() {
        let painted = paint(BRANCH, "feature/a");
        assert!(painted.contains("feature/a"));
        assert!(painted.starts_with('\u{1b}'));
        assert!(painted.ends_with('m'));
    }

    #[test]
    fn state_uses_the_ledger_palette() {
        assert!(state(&ReviewState::Open).contains("open"));
        assert!(state(&ReviewState::Merged).contains("merged"));
        assert!(state(&ReviewState::Closed).contains("closed"));
        assert_ne!(
            state(&ReviewState::Open),
            state(&ReviewState::Merged),
            "states should not share a style"
        );
    }
}
