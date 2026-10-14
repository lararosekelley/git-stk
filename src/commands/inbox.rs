use std::collections::BTreeMap;

use anyhow::Result;

use crate::commands::Run;
use crate::providers::{
    Decision, FETCH_LIMIT, InboxEntry, InboxList, Listing, age, detect_review_provider,
    epoch_seconds,
};
use crate::style;

/// List the reviews waiting on you in this repository: requested of you or
/// your teams, returned to you, and your own.
#[derive(Debug, clap::Args)]
pub struct Inbox {
    /// Lists to show, comma-separated: incoming (requested of you), team
    /// (requested of a team you are on), returned (yours, with changes
    /// requested), mine (yours otherwise). Default: all four.
    #[arg(long, value_delimiter = ',', value_name = "LISTS")]
    filter: Vec<InboxList>,
    /// Reviews shown per list.
    #[arg(long, default_value_t = 10, value_name = "N")]
    limit: usize,
    /// Order within each list, newest or largest first.
    #[arg(long, value_enum, default_value_t = InboxSort::Updated)]
    sort: InboxSort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum InboxSort {
    /// Most recently updated first.
    Updated,
    /// Most recently opened first.
    Created,
    /// Most lines changed first.
    Size,
}

impl Run for Inbox {
    fn run(self) -> Result<()> {
        let lists: Vec<InboxList> = InboxList::ALL
            .into_iter()
            .filter(|list| self.filter.is_empty() || self.filter.contains(list))
            .collect();
        let (_, provider) = detect_review_provider()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as i64);
        let toured = crate::tour::seen::read();
        let mut first = true;
        let mut waiting = false;
        for (list, listing) in provider.inbox(&lists)? {
            if !first {
                println!();
            }
            first = false;
            waiting |= list != InboxList::Mine
                && matches!(&listing, Listing::Found { entries, .. } if !entries.is_empty());
            for line in render(list, listing, self.limit, self.sort, now, &toured) {
                anstream::println!("{line}");
            }
        }
        if waiting {
            anstream::println!(
                "\n{} walk through one with `git stk tour <number>`",
                style::hint_prefix()
            );
        }
        Ok(())
    }
}

/// One list's heading and rows.
fn render(
    list: InboxList,
    listing: Listing,
    limit: usize,
    sort: InboxSort,
    now: i64,
    toured: &BTreeMap<String, i64>,
) -> Vec<String> {
    let (mut entries, more) = match listing {
        Listing::Found { entries, more } => (entries, more),
        Listing::Unsupported(why) => {
            return vec![
                style::paint(style::HEADING, list.title()),
                format!("  {}", style::dim(&why)),
            ];
        }
        Listing::Failed(why) => {
            return vec![
                style::paint(style::HEADING, list.title()),
                format!("  {}", style::warn(&format!("could not list these: {why}"))),
            ];
        }
    };
    let found = entries.len();
    let count = if more {
        format!("{found}+")
    } else {
        found.to_string()
    };
    let mut lines = vec![format!(
        "{} {}",
        style::paint(style::HEADING, list.title()),
        style::dim(&format!("({count})"))
    )];
    if entries.is_empty() {
        lines.push(format!("  {}", style::dim("nothing here")));
        return lines;
    }
    sort_entries(&mut entries, sort);
    let width = entries
        .iter()
        .take(limit)
        .map(|entry| entry.review.id.len())
        .max()
        .unwrap_or(0);
    for entry in entries.iter().take(limit) {
        lines.push(row(entry, list, width, now, toured));
    }
    let left = if found > limit {
        let plus = if more { "+" } else { "" };
        Some(format!(
            "{}{plus} more; raise --limit to see them",
            found - limit
        ))
    } else if more {
        Some(format!(
            "more on the forge; only the {FETCH_LIMIT} most recently updated are listed"
        ))
    } else {
        None
    };
    if let Some(left) = left {
        lines.push(format!("  {}", style::dim(&left)));
    }
    lines
}

fn sort_entries(entries: &mut [InboxEntry], sort: InboxSort) {
    let time = |timestamp: &str| epoch_seconds(timestamp).unwrap_or(0);
    match sort {
        InboxSort::Updated => {
            entries.sort_by_key(|entry| std::cmp::Reverse(time(&entry.updated_at)))
        }
        InboxSort::Created => {
            entries.sort_by_key(|entry| std::cmp::Reverse(time(&entry.created_at)))
        }
        InboxSort::Size => {
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.additions + entry.deletions));
        }
    }
}

/// `🟢 #12  Add two  ann, 2d, +40/-3, approved, 1 open thread`
fn row(
    entry: &InboxEntry,
    list: InboxList,
    width: usize,
    now: i64,
    toured: &BTreeMap<String, i64>,
) -> String {
    let mut details = Vec::new();
    if entry
        .activity
        .new_since(toured.get(&entry.review.id).copied())
    {
        details.push(style::paint(style::CURRENT, "new"));
    }
    // Your own reviews need no author.
    if !matches!(list, InboxList::Mine | InboxList::Returned) {
        details.push(entry.author.clone());
    }
    let updated = age(&entry.updated_at, now);
    if !updated.is_empty() {
        details.push(updated);
    }
    let size = format!(
        "{}/{}",
        style::paint(style::ADDED, &format!("+{}", entry.additions)),
        style::paint(style::REMOVED, &format!("-{}", entry.deletions))
    );
    match entry.decision {
        Decision::Approved => details.push(style::success("approved")),
        Decision::ChangesRequested => details.push(style::warn("changes requested")),
        Decision::ReviewRequired | Decision::None => {}
    }
    match entry.unresolved {
        Some(0) | None => {}
        Some(1) => details.push("1 open thread".to_owned()),
        Some(count) => details.push(format!("{count} open threads")),
    }
    if entry.review.draft {
        details.push("draft".to_owned());
    }
    let dot = match entry.checks.dot() {
        "" => "   ",
        dot => dot,
    };
    format!(
        "  {dot}{} {}  {} {}",
        style::dim(&format!("{:<width$}", entry.review.id)),
        entry.review.title,
        size,
        style::dim(&details.join(", "))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{CheckStatus, ReviewRequest, ReviewState};

    fn entry(id: u32, updated: &str, size: u64) -> InboxEntry {
        InboxEntry {
            review: ReviewRequest {
                id: format!("#{id}"),
                branch: format!("b{id}"),
                base: "main".into(),
                state: ReviewState::Open,
                url: String::new(),
                title: format!("Review {id}"),
                draft: false,
            },
            author: "ann".into(),
            created_at: "2026-10-01T00:00:00Z".into(),
            updated_at: updated.into(),
            additions: size,
            deletions: 0,
            checks: CheckStatus::None,
            decision: Decision::None,
            unresolved: Some(0),
            activity: Default::default(),
        }
    }

    fn render_untoured(
        list: InboxList,
        listing: Listing,
        limit: usize,
        sort: InboxSort,
        now: i64,
    ) -> Vec<String> {
        render(list, listing, limit, sort, now, &BTreeMap::new())
    }

    fn plain(lines: Vec<String>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| console::strip_ansi_codes(&line).into_owned())
            .collect()
    }

    #[test]
    fn a_list_is_sorted_cut_to_its_limit_and_says_what_is_left() {
        let now = epoch_seconds("2026-10-13T00:00:00Z").unwrap();
        let listing = Listing::Found {
            entries: vec![
                entry(1, "2026-10-10T00:00:00Z", 5),
                entry(2, "2026-10-12T00:00:00Z", 50),
                entry(3, "2026-10-11T00:00:00Z", 500),
            ],
            more: false,
        };
        let lines = plain(render_untoured(
            InboxList::Incoming,
            listing,
            2,
            InboxSort::Updated,
            now,
        ));
        assert_eq!(
            lines,
            [
                "Requested of you (3)",
                "     #2 Review 2  +50/-0 ann, 1d",
                "     #3 Review 3  +500/-0 ann, 2d",
                "  1 more; raise --limit to see them",
            ]
        );
        let by_size = plain(render_untoured(
            InboxList::Mine,
            Listing::Found {
                entries: vec![entry(1, "", 5), entry(2, "", 50)],
                more: true,
            },
            5,
            InboxSort::Size,
            now,
        ));
        assert_eq!(by_size[0], "Yours (2+)");
        assert!(by_size[1].contains("#2") && !by_size[1].contains("ann"));
        assert_eq!(
            by_size.last().unwrap(),
            "  more on the forge; only the 50 most recently updated are listed"
        );
    }

    #[test]
    fn an_empty_or_unsupported_list_says_so() {
        let empty = plain(render_untoured(
            InboxList::Returned,
            Listing::Found {
                entries: Vec::new(),
                more: false,
            },
            10,
            InboxSort::Updated,
            0,
        ));
        assert_eq!(empty, ["Returned to you (0)", "  nothing here"]);
        let unsupported = plain(render_untoured(
            InboxList::Team,
            Listing::Unsupported("GitLab has no team review requests".into()),
            10,
            InboxSort::Updated,
            0,
        ));
        assert_eq!(
            unsupported,
            [
                "Requested of your teams",
                "  GitLab has no team review requests"
            ]
        );
    }
}
