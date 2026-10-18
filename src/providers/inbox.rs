//! The reviews waiting on you in this repository, for `git stk inbox`.

use super::{CheckStatus, ReviewRequest};

/// Reviews fetched per list. Past this the list says there are more, and the
/// sort and limit apply to what was fetched.
pub const FETCH_LIMIT: usize = 50;

/// One of the inbox's lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InboxList {
    /// Reviews requested of you by name.
    Incoming,
    /// Reviews requested of a team you are on, and not of you by name.
    Team,
    /// Your open reviews, except those returned to you.
    Mine,
    /// Your open reviews with changes requested.
    Returned,
    /// Your open reviews in repositories owned by neither you nor an
    /// organization you belong to, wherever they are on the forge.
    Outside,
}

impl InboxList {
    /// Every list, in the order shown: what waits on you first.
    pub const ALL: [Self; 5] = [
        Self::Incoming,
        Self::Returned,
        Self::Team,
        Self::Mine,
        Self::Outside,
    ];

    /// Whether the list is shown without `--filter`. Outside reaches beyond
    /// this repository, so it is only shown when asked for.
    pub fn shown_by_default(self) -> bool {
        self != Self::Outside
    }

    /// Whether the reviews in the list are your own.
    pub fn is_yours(self) -> bool {
        matches!(self, Self::Mine | Self::Returned | Self::Outside)
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Incoming => "Requested of you",
            Self::Team => "Requested of your teams",
            Self::Mine => "Yours",
            Self::Returned => "Returned to you",
            Self::Outside => "Yours outside your organizations",
        }
    }
}

/// Where a review's reviewing stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    None,
}

/// A review in an inbox list.
#[derive(Debug)]
pub struct InboxEntry {
    pub review: ReviewRequest,
    pub author: String,
    /// RFC 3339 timestamps, as the forge gives them.
    pub created_at: String,
    pub updated_at: String,
    pub additions: u64,
    pub deletions: u64,
    pub checks: CheckStatus,
    pub decision: Decision,
    /// Open review threads, when the forge counts them.
    pub unresolved: Option<u32>,
    /// The latest comment, review, or push by someone else, and by you.
    pub activity: Activity,
}

/// Whether `login` is a bot's: a GitHub app (`name[bot]`), an account named
/// as one (`gitlab-bot`, `ci_bot`), or one listed in `stk.inboxBots`, for a
/// machine user the forge reports as a person. A bot's comments are not news.
pub fn is_bot(login: &str) -> bool {
    static LISTED: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let lower = login.to_ascii_lowercase();
    ["[bot]", "-bot", "_bot"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
        || LISTED
            .get_or_init(crate::settings::inbox_bots)
            .contains(&lower)
}

/// When a review was last touched by someone else and by you, as RFC 3339
/// timestamps, for marking what is new since you looked.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Activity {
    pub others: Option<String>,
    pub yours: Option<String>,
}

impl Activity {
    /// The latest of `events`, each a who and a when, split between `me` and
    /// everyone else.
    pub fn of<'a>(me: &str, events: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut activity = Self::default();
        for (who, when) in events {
            let Some(at) = epoch_seconds(when) else {
                continue;
            };
            let slot = if who == me {
                &mut activity.yours
            } else {
                &mut activity.others
            };
            if slot
                .as_deref()
                .and_then(epoch_seconds)
                .is_none_or(|kept| at > kept)
            {
                *slot = Some(when.to_owned());
            }
        }
        activity
    }

    /// Whether someone else has been at the review since you last were, on
    /// the forge or in a tour (`toured`, seconds since the epoch). A review
    /// you have never looked at is not marked.
    pub fn new_since(&self, toured: Option<i64>) -> bool {
        let yours = self.yours.as_deref().and_then(epoch_seconds);
        let looked = yours.max(toured);
        match (looked, self.others.as_deref().and_then(epoch_seconds)) {
            (Some(looked), Some(others)) => others > looked,
            _ => false,
        }
    }
}

/// One list as the forge answered it.
#[derive(Debug)]
pub enum Listing {
    /// The reviews found, and whether more were left past [`FETCH_LIMIT`].
    Found {
        entries: Vec<InboxEntry>,
        more: bool,
    },
    /// The forge has no such list, and why.
    Unsupported(String),
    /// The forge could not answer for this list, and why; the others may
    /// still have answered.
    Failed(String),
}

/// Seconds since the Unix epoch for an RFC 3339 timestamp such as
/// `2026-10-13T22:10:27Z` or `2026-10-13T19:32:09+02:00`, or None when it
/// does not parse.
pub fn epoch_seconds(timestamp: &str) -> Option<i64> {
    let (date, rest) = timestamp.split_once('T')?;
    let mut date = date.split('-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let time_end = rest.find(['Z', '+', '-']).unwrap_or(rest.len());
    let (time, zone) = rest.split_at(time_end);
    let mut time = time.split('.').next()?.split(':').map(str::parse::<i64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    let offset = match zone {
        "" | "Z" => 0,
        zone => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (hours, minutes) = zone[1..].split_once(':')?;
            sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60)
        }
    };
    // Days from the civil date, after Howard Hinnant's algorithm.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// How long ago `timestamp` was, as `5m`, `3h`, `2d`, or `6w`; empty when it
/// does not parse.
pub fn age(timestamp: &str, now: i64) -> String {
    let Some(then) = epoch_seconds(timestamp) else {
        return String::new();
    };
    let seconds = (now - then).max(0);
    match seconds {
        0..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        86_400..1_209_600 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_with_any_offset() {
        assert_eq!(epoch_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_seconds("2026-10-13T22:10:27Z"), Some(1_791_929_427));
        assert_eq!(
            epoch_seconds("2026-10-13T19:32:09+02:00"),
            epoch_seconds("2026-10-13T17:32:09Z")
        );
        assert_eq!(
            epoch_seconds("2026-10-13T17:32:09.123-04:00"),
            epoch_seconds("2026-10-13T21:32:09Z")
        );
        assert_eq!(epoch_seconds("yesterday"), None);
    }

    #[test]
    fn a_review_is_new_when_someone_else_was_at_it_since_you_looked() {
        let activity = Activity::of(
            "me",
            [
                ("ann", "2026-10-10T00:00:00Z"),
                ("me", "2026-10-11T00:00:00Z"),
                ("bo", "2026-10-12T00:00:00Z"),
                ("ann", "garbage"),
            ],
        );
        assert_eq!(activity.others.as_deref(), Some("2026-10-12T00:00:00Z"));
        assert_eq!(activity.yours.as_deref(), Some("2026-10-11T00:00:00Z"));
        assert!(activity.new_since(None), "bo came back after you");
        let toured_later = epoch_seconds("2026-10-13T00:00:00Z");
        assert!(
            !activity.new_since(toured_later),
            "a tour since counts as looking"
        );

        assert!(is_bot("gitlab-bot") && is_bot("ci_bot") && is_bot("renovate[bot]"));
        assert!(!is_bot("talbot") && !is_bot("botanist") && !is_bot("ann"));
        let never = Activity::of("me", [("ann", "2026-10-10T00:00:00Z")]);
        assert!(!never.new_since(None), "never looked at is not new");
        assert!(never.new_since(epoch_seconds("2026-10-09T00:00:00Z")));
    }

    #[test]
    fn ages_use_the_largest_sensible_unit() {
        let now = epoch_seconds("2026-10-13T12:00:00Z").unwrap();
        assert_eq!(age("2026-10-13T11:55:00Z", now), "5m");
        assert_eq!(age("2026-10-13T09:00:00Z", now), "3h");
        assert_eq!(age("2026-10-11T12:00:00Z", now), "2d");
        assert_eq!(age("2026-08-01T12:00:00Z", now), "10w");
        assert_eq!(age("never", now), "");
    }
}
