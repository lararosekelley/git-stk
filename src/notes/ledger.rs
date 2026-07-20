//! The stack-overview ledger kept in every review body: build, parse, and
//! refresh the marker-delimited overview whose merged and closed entries
//! outlive their local branches.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use super::STACK_SECTION;
use super::sections::{body_with_section, extract_section};
use crate::providers::{ReviewProvider, ReviewRequest, ReviewState};

const DATA_PREFIX: &str = "<!-- git-stk:data ";
const COMMENT_END: &str = "-->";
const TOOL_URL: &str = "https://github.com/lararosekelley/git-stk";
const LOGO_URL: &str =
    "https://raw.githubusercontent.com/lararosekelley/git-stk/main/assets/logo.svg";

/// One row of the stack-overview ledger. Live rows come from the provider;
/// merged and closed rows outlive their local branches and are carried
/// forward from the previous note, so the ledger is append-only history
/// rather than a snapshot of the live stack.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NoteEntry {
    id: String,
    url: String,
    title: String,
    state: String,
}

impl NoteEntry {
    fn from_review(review: &ReviewRequest) -> Self {
        Self {
            id: review.id.clone(),
            url: review.url.clone(),
            title: review.title.clone(),
            state: review.state.to_string(),
        }
    }

    /// A review the ledger only knows by its row: enough identity to fetch
    /// and update the body, nothing more.
    fn to_review(&self) -> ReviewRequest {
        let state = match self.state.as_str() {
            "open" => ReviewState::Open,
            "merged" => ReviewState::Merged,
            "closed" => ReviewState::Closed,
            other => ReviewState::Unknown(other.to_owned()),
        };
        ReviewRequest {
            id: self.id.clone(),
            branch: String::new(),
            base: String::new(),
            state,
            url: self.url.clone(),
            title: self.title.clone(),
            draft: false,
        }
    }

    /// Rows recovered from a hand-edited note may be missing the id, so the
    /// URL doubles as identity.
    fn matches(&self, other: &Self) -> bool {
        (!self.id.is_empty() && self.id == other.id)
            || (!self.url.is_empty() && self.url == other.url)
    }
}

/// Maintain a stack overview in every review body, one independent stack at a
/// time: a PR's overview must list only its own stack, never sibling stacks
/// that merely share the trunk. `branch_parents` can span several such stacks
/// (e.g. a `sync` run from the trunk), so group by each branch's child-of-trunk
/// root and refresh each group's notes on its own.
pub fn update_stack_notes(
    review_provider: &dyn ReviewProvider,
    branch_parents: &[(String, String)],
    dry_run: bool,
    rebuild: bool,
) -> Result<()> {
    let mut stacks: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (branch, parent) in branch_parents {
        // The line base (the branch's child-of-trunk ancestor) identifies the
        // independent stack it belongs to; a broken lookup keeps the branch on
        // its own rather than folding it into another stack.
        let key = crate::stack::line_base(branch).unwrap_or_else(|_| branch.clone());
        stacks
            .entry(key)
            .or_default()
            .push((branch.clone(), parent.clone()));
    }
    for stack in stacks.values() {
        update_one_stack(review_provider, stack, dry_run, rebuild)?;
    }
    Ok(())
}

/// Refresh the overview across one independent stack. `branch_parents` here is
/// a single stack, bottom-first.
fn update_one_stack(
    review_provider: &dyn ReviewProvider,
    branch_parents: &[(String, String)],
    dry_run: bool,
    rebuild: bool,
) -> Result<()> {
    // The bottom branch's parent is the base the whole stack sits on.
    let Some(trunk) = branch_parents.first().map(|(_, parent)| parent.clone()) else {
        return Ok(());
    };

    let mut live = Vec::new();
    for (branch, _) in branch_parents {
        // The closed-inclusive lookup is deliberate: a review closed on the
        // platform should show up red in the ledger, even though every flow
        // that acts on a review treats it as gone.
        match review_provider.review_for_branch_including_closed(branch)? {
            Some(review) if review.branch == *branch => live.push(review),
            _ => {
                // Without every review the overview would be wrong for all of
                // them (dry runs never created the missing ones).
                if !dry_run {
                    anstream::println!("skipped stack notes: no review found for {branch}");
                }
                return Ok(());
            }
        }
    }

    // A normal dry run stays cheap (no body fetches); --rebuild-overview reads
    // the bodies so it can report which drifted rows it would drop.
    if dry_run && !rebuild {
        for review in &live {
            anstream::println!("would update stack note in {}", review.id);
        }
        return Ok(());
    }

    // Fetch every live body up front: each carries its own copy of the
    // ledger, and the union keeps history alive even in bodies that have
    // never seen it (e.g. a review created after earlier entries merged).
    let mut bodies = Vec::new();
    for review in &live {
        bodies.push(review_provider.review_body(review)?);
    }

    // Reviews left behind by a branch rename: a fresh replacement is among the
    // live entries, so the stale row is dropped rather than carried forward.
    let mut superseded: Vec<NoteEntry> = Vec::new();
    for (branch, _) in branch_parents {
        if let Some(old) = crate::stack::renamed_from(branch)?
            && let Some(review) = review_provider.review_for_branch_including_closed(&old)?
        {
            superseded.push(NoteEntry::from_review(&review));
        }
    }

    let live_entries: Vec<NoteEntry> = live.iter().map(NoteEntry::from_review).collect();
    let mut historical: Vec<NoteEntry> = Vec::new();
    let mut dropped: Vec<NoteEntry> = Vec::new();
    for body in &bodies {
        let Some(section) = extract_section(body, STACK_SECTION) else {
            continue;
        };
        for entry in parse_ledger(section) {
            if superseded.iter().any(|stale| stale.matches(&entry)) {
                continue;
            }
            let known = live_entries
                .iter()
                .chain(historical.iter())
                .chain(dropped.iter());
            if known.into_iter().any(|seen| seen.matches(&entry)) {
                continue;
            }
            // --rebuild-overview keeps only genuinely landed history; closed or
            // orphaned rows that drifted in are dropped rather than carried on.
            if rebuild && entry.state != "merged" {
                dropped.push(entry);
            } else {
                historical.push(entry);
            }
        }
    }

    if dry_run {
        for review in &live {
            anstream::println!("would update stack note in {}", review.id);
        }
        for entry in &dropped {
            anstream::println!(
                "would drop drifted entry {} ({})",
                if entry.id.is_empty() { "?" } else { &entry.id },
                entry.state
            );
        }
        return Ok(());
    }

    // Refresh carried-forward rows still recorded as open: their branch is gone
    // from the local stack, so nothing else re-queries them, and one may have
    // merged or closed since it was last written. Terminal (merged/closed) rows
    // never change, so leave them be. Best-effort: a lookup that fails keeps the
    // recorded state rather than dropping the row.
    for entry in &mut historical {
        if entry.state != "open" || entry.id.is_empty() {
            continue;
        }
        if let Ok(Some(state)) = review_provider.review_state(&entry.to_review()) {
            entry.state = state.to_string();
        }
    }

    // Bottom-first, like the stack itself: already-landed history below,
    // the live stack on top of it.
    let mut entries = historical.clone();
    entries.extend(live_entries);

    for (offset, review) in live.iter().enumerate() {
        let note = build_stack_note(&entries, historical.len() + offset, &trunk);
        let updated = body_with_section(&bodies[offset], STACK_SECTION, &note);
        if updated == bodies[offset] {
            continue;
        }

        review_provider.update_review_body(review, &updated)?;
        anstream::println!("updated stack note in {}", review.id);
    }

    // Historical reviews get the refreshed ledger too, so a just-merged
    // review stops presenting the stack as it was. Failures are non-fatal:
    // an old review may have become unreachable.
    for (index, entry) in historical.iter().enumerate() {
        if entry.id.is_empty() {
            continue;
        }
        let review = entry.to_review();
        let Ok(body) = review_provider.review_body(&review) else {
            anstream::println!("skipped stack note in {}: could not read body", review.id);
            continue;
        };

        let note = build_stack_note(&entries, index, &trunk);
        let updated = body_with_section(&body, STACK_SECTION, &note);
        if updated == body {
            continue;
        }

        if review_provider
            .update_review_body(&review, &updated)
            .is_err()
        {
            anstream::println!("skipped stack note in {}: could not update body", review.id);
            continue;
        }
        anstream::println!("updated stack note in {}", review.id);
    }

    Ok(())
}

/// Render the overview for one review: a hidden data line carrying the
/// ledger, every entry leaf-first as a status-styled bullet, a pointer on
/// the review being viewed, the trunk in backticks at the bottom, and a
/// footer crediting the tool.
fn build_stack_note(entries: &[NoteEntry], current: usize, trunk: &str) -> String {
    let mut lines = vec![data_line(entries)];
    for (index, entry) in entries.iter().enumerate().rev() {
        lines.push(render_entry(entry, index == current));
    }
    lines.push(format!("- `{trunk}`"));

    format!(
        "{}\n\n---\n\nStack managed by \
         <img src=\"{LOGO_URL}\" width=\"12\" height=\"12\" alt=\"\" /> \
         [git-stk]({TOOL_URL})",
        lines.join("\n")
    )
}

/// A status emoji as the bullet, strikethrough plus a suffix for entries
/// that have left the stack, and the pointer on the current review.
fn render_entry(entry: &NoteEntry, current: bool) -> String {
    let label = crate::providers::label(&entry.title, &entry.id);
    let link = format!("[{label}]({})", entry.url);

    let mut line = match entry.state.as_str() {
        "merged" => format!("- \u{1F7E3} ~~{link}~~ (merged)"),
        "closed" => format!("- \u{1F534} ~~{link}~~ (closed)"),
        _ => format!("- \u{1F7E2} {link}"),
    };
    if current {
        line.push_str(" \u{1F448}");
    }
    line
}

/// One hidden machine-readable line so the ledger survives restyling: the
/// rendered bullets are presentation, this is the data.
fn data_line(entries: &[NoteEntry]) -> String {
    let data = Value::Array(
        entries
            .iter()
            .map(|entry| {
                json!({
                    "id": entry.id,
                    "url": entry.url,
                    "title": entry.title,
                    "state": entry.state,
                })
            })
            .collect(),
    );

    // '>' only ever appears inside JSON strings, so escaping it globally
    // keeps a title containing "-->" from terminating the comment early.
    let encoded = data.to_string().replace('>', "\\u003e");
    format!("{DATA_PREFIX}{encoded} {COMMENT_END}")
}

/// Read the ledger out of a stack section: the embedded data line when it
/// is intact, otherwise whatever the rendered bullets still reveal (the
/// hidden line may have been edited or deleted along with everything else).
fn parse_ledger(section: &str) -> Vec<NoteEntry> {
    for line in section.lines() {
        if let Some(rest) = line.trim().strip_prefix(DATA_PREFIX)
            && let Some(encoded) = rest.trim_end().strip_suffix(COMMENT_END)
            && let Some(entries) = parse_data_json(encoded.trim())
        {
            return entries;
        }
    }

    section.lines().filter_map(parse_entry_line).collect()
}

fn parse_data_json(encoded: &str) -> Option<Vec<NoteEntry>> {
    let value: Value = serde_json::from_str(encoded).ok()?;
    let mut entries = Vec::new();
    for item in value.as_array()? {
        entries.push(NoteEntry {
            id: item.get("id")?.as_str()?.to_owned(),
            url: item.get("url")?.as_str()?.to_owned(),
            title: item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            state: item
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("open")
                .to_owned(),
        });
    }
    Some(entries)
}

/// Best-effort recovery of one rendered bullet: `[label](url)` plus the
/// state suffix. The trunk line (backticks, no link) and the footer fall
/// through to None.
fn parse_entry_line(line: &str) -> Option<NoteEntry> {
    let rest = line.trim().strip_prefix("- ")?;
    if rest.starts_with('`') {
        return None;
    }

    let open = rest.find('[')?;
    let split = rest[open..].find("](")? + open;
    let close = rest[split + 2..].find(')')? + split + 2;
    let label = &rest[open + 1..split];
    let url = &rest[split + 2..close];
    let tail = &rest[close + 1..];

    let state = if tail.contains("(merged)") {
        "merged"
    } else if tail.contains("(closed)") {
        "closed"
    } else {
        "open"
    };

    // "Title (#12)" carries both; a bare "#12" label is just the id.
    let (title, id) = match rest[open + 1..split].rfind(" (") {
        Some(position) if label.ends_with(')') => {
            let id = &label[position + 2..label.len() - 1];
            if id.starts_with('#') || id.starts_with('!') {
                (label[..position].to_owned(), id.to_owned())
            } else {
                (label.to_owned(), String::new())
            }
        }
        _ if label.starts_with('#') || label.starts_with('!') => (String::new(), label.to_owned()),
        _ => (label.to_owned(), String::new()),
    };

    Some(NoteEntry {
        id,
        url: url.to_owned(),
        title,
        state: state.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, title: &str, url: &str, state: &str) -> NoteEntry {
        NoteEntry {
            id: id.to_owned(),
            url: url.to_owned(),
            title: title.to_owned(),
            state: state.to_owned(),
        }
    }

    #[test]
    fn build_stack_note_lists_ledger_leaf_first_with_pointer_and_trunk() {
        let entries = vec![
            entry("#12", "Bottom change", "https://example.com/12", "open"),
            entry("#13", "Top change", "https://example.com/13", "open"),
        ];

        let note = build_stack_note(&entries, 0, "main");
        let lines: Vec<&str> = note.lines().collect();
        assert!(
            lines[0].starts_with(DATA_PREFIX),
            "missing data line: {note}"
        );
        assert_eq!(
            lines[1],
            "- \u{1F7E2} [Top change (#13)](https://example.com/13)"
        );
        assert_eq!(
            lines[2],
            "- \u{1F7E2} [Bottom change (#12)](https://example.com/12) \u{1F448}"
        );
        assert_eq!(lines[3], "- `main`");
        assert!(note.ends_with(
            "Stack managed by \
             <img src=\"https://raw.githubusercontent.com/lararosekelley/git-stk/main/assets/logo.svg\" \
             width=\"12\" height=\"12\" alt=\"\" /> \
             [git-stk](https://github.com/lararosekelley/git-stk)"
        ));
    }

    #[test]
    fn build_stack_note_styles_merged_and_closed_entries() {
        let entries = vec![
            entry("#11", "Landed", "https://example.com/11", "merged"),
            entry("#12", "Abandoned", "https://example.com/12", "closed"),
            entry("#13", "Live", "https://example.com/13", "open"),
        ];

        let note = build_stack_note(&entries, 2, "main");
        assert!(note.contains("- \u{1F7E2} [Live (#13)](https://example.com/13) \u{1F448}"));
        assert!(
            note.contains("- \u{1F534} ~~[Abandoned (#12)](https://example.com/12)~~ (closed)")
        );
        assert!(note.contains("- \u{1F7E3} ~~[Landed (#11)](https://example.com/11)~~ (merged)"));
    }

    #[test]
    fn build_stack_note_falls_back_to_id_without_title() {
        let entries = vec![entry("#12", "", "https://example.com/12", "open")];
        let note = build_stack_note(&entries, 0, "main");
        assert!(note.contains("- \u{1F7E2} [#12](https://example.com/12) \u{1F448}"));
    }

    #[test]
    fn parse_ledger_round_trips_the_data_line() {
        let entries = vec![
            entry("#11", "Landed", "https://example.com/11", "merged"),
            entry("#13", "Top -> change", "https://example.com/13", "open"),
        ];

        let note = build_stack_note(&entries, 1, "main");
        assert_eq!(parse_ledger(&note), entries);
    }

    #[test]
    fn data_line_survives_a_title_containing_a_comment_terminator() {
        let entries = vec![entry(
            "#12",
            "weird --> title",
            "https://example.com/12",
            "open",
        )];
        let line = data_line(&entries);
        assert!(!line[DATA_PREFIX.len()..line.len() - COMMENT_END.len()].contains("-->"));
        assert_eq!(parse_ledger(&line), entries);
    }

    #[test]
    fn parse_ledger_recovers_entries_from_bullets_when_data_line_is_gone() {
        let entries = vec![
            entry("#11", "Landed", "https://example.com/11", "merged"),
            entry("#12", "", "https://example.com/12", "closed"),
            entry("#13", "Live", "https://example.com/13", "open"),
        ];

        let note = build_stack_note(&entries, 2, "main");
        let without_data: String = note
            .lines()
            .filter(|line| !line.trim().starts_with(DATA_PREFIX))
            .collect::<Vec<_>>()
            .join("\n");

        // Bullets render leaf-first, so recovery reverses back to
        // bottom-first ledger order.
        let mut recovered = parse_ledger(&without_data);
        recovered.reverse();
        assert_eq!(recovered, entries);
    }

    #[test]
    fn parse_ledger_falls_back_to_bullets_when_data_line_is_corrupt() {
        let section = "<!-- git-stk:data [{\"id\": -->\n\
                       - \u{1F7E3} ~~[Landed (#11)](https://example.com/11)~~ (merged)\n\
                       - `main`";
        assert_eq!(
            parse_ledger(section),
            vec![entry("#11", "Landed", "https://example.com/11", "merged")]
        );
    }

    #[test]
    fn parse_ledger_reads_the_legacy_unstyled_format() {
        let section = "- [Top change (#13)](https://example.com/13)\n\
                       - [Bottom change (#12)](https://example.com/12) \u{1F448}\n\
                       - `main`\n\n---\n\nfooter";
        assert_eq!(
            parse_ledger(section),
            vec![
                entry("#13", "Top change", "https://example.com/13", "open"),
                entry("#12", "Bottom change", "https://example.com/12", "open"),
            ]
        );
    }

    #[test]
    fn note_entry_round_trips_through_review() {
        let landed = entry("#11", "Landed", "https://example.com/11", "merged");
        let review = landed.to_review();
        assert_eq!(review.state, ReviewState::Merged);
        assert_eq!(NoteEntry::from_review(&review), landed);
    }

    #[test]
    fn note_entry_matches_by_id_or_url() {
        let by_id = entry("#11", "", "", "open");
        let by_url = entry("", "", "https://example.com/11", "open");
        assert!(by_id.matches(&entry("#11", "x", "y", "merged")));
        assert!(by_url.matches(&entry("#12", "", "https://example.com/11", "open")));
        assert!(!by_url.matches(&by_id));
    }
}
