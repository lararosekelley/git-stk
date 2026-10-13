//! The reviews waiting on you in this repository, for `git stk inbox`.

use super::{CheckStatus, ReviewRequest};

/// Reviews fetched per list. Past this the list says there are more, and the
/// sort and limit apply to what was fetched.
pub const FETCH_LIMIT: usize = 50;

/// One of the inbox's lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum InboxList {
    /// Reviews requested of you by name.
    Incoming,
    /// Reviews requested of a team you are on, and not of you by name.
    Team,
    /// Your open reviews, except those returned to you.
    Mine,
    /// Your open reviews with changes requested.
    Returned,
}

impl InboxList {
    pub const ALL: [Self; 4] = [Self::Incoming, Self::Team, Self::Mine, Self::Returned];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Team => "team",
            Self::Mine => "mine",
            Self::Returned => "returned",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Incoming => "Requested of you",
            Self::Team => "Requested of your teams",
            Self::Mine => "Yours",
            Self::Returned => "Returned to you",
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
    fn ages_use_the_largest_sensible_unit() {
        let now = epoch_seconds("2026-10-13T12:00:00Z").unwrap();
        assert_eq!(age("2026-10-13T11:55:00Z", now), "5m");
        assert_eq!(age("2026-10-13T09:00:00Z", now), "3h");
        assert_eq!(age("2026-10-11T12:00:00Z", now), "2d");
        assert_eq!(age("2026-08-01T12:00:00Z", now), "10w");
        assert_eq!(age("never", now), "");
    }
}
