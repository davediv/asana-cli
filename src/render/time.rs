//! Relative times phrased exactly like gh's `text.FuzzyAgo`, plus natural
//! due-date phrases.

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("{n} {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// `less than a minute ago`, `about 1 minute ago`, `about 9 hours ago`,
/// `about 2 months ago`, `about 1 year ago`. Mirrors gh's FuzzyAgo.
pub fn fuzzy_ago(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    let minutes = secs / 60;
    let hours = secs / 3600;
    let days = hours / 24;
    if secs < 60 {
        return "less than a minute ago".to_string();
    }
    let body = if secs < 3600 {
        plural(minutes, "minute")
    } else if secs < 24 * 3600 {
        plural(hours, "hour")
    } else if secs < 30 * 24 * 3600 {
        plural(days, "day")
    } else if secs < 365 * 24 * 3600 {
        plural(days / 30, "month")
    } else {
        plural(days / 365, "year")
    };
    format!("about {body} ago")
}

/// `5m`, `3h`, `4d`, or `Sep 2, 2026` when older than a month. Mirrors gh's
/// FuzzyAgoAbbr, used in the newest-comment header.
pub fn fuzzy_ago_abbr(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 24 * 3600 {
        format!("{}h", secs / 3600)
    } else if secs < 30 * 24 * 3600 {
        format!("{}d", secs / 86_400)
    } else {
        long_date(then)
    }
}

/// `May 11, 2026` in local time, like gh's comment headers.
pub fn long_date(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%b %-d, %Y").to_string()
}

/// `Sep 12` or `Sep 12, 2027` for a date in another year.
pub fn short_date(date: NaiveDate, today: NaiveDate) -> String {
    if date.year() == today.year() {
        date.format("%b %-d").to_string()
    } else {
        date.format("%b %-d, %Y").to_string()
    }
}

/// How overdue or far away a due date is, relative to `today`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueState {
    Overdue,
    Today,
    Upcoming,
}

/// `today`, `tomorrow`, `yesterday`, `Sep 12`, `3 days overdue`.
pub fn due_phrase(date: NaiveDate, today: NaiveDate) -> (String, DueState) {
    let diff = (date - today).num_days();
    match diff {
        0 => ("today".to_string(), DueState::Today),
        1 => ("tomorrow".to_string(), DueState::Upcoming),
        -1 => ("yesterday".to_string(), DueState::Overdue),
        d if d < -1 => (format!("{} days overdue", -d), DueState::Overdue),
        _ => (short_date(date, today), DueState::Upcoming),
    }
}

pub fn today() -> NaiveDate {
    Local::now().date_naive()
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// ISO-8601 in UTC with second precision, for piped output.
pub fn iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0)
            .single()
            .expect("valid timestamp")
    }

    #[test]
    fn fuzzy_ago_matches_gh() {
        let now = at(0);
        let ago = |secs: i64| fuzzy_ago(now, at(-secs));
        assert_eq!(ago(0), "less than a minute ago");
        assert_eq!(ago(59), "less than a minute ago");
        assert_eq!(ago(60), "about 1 minute ago");
        assert_eq!(ago(5 * 60), "about 5 minutes ago");
        assert_eq!(ago(3600), "about 1 hour ago");
        assert_eq!(ago(9 * 3600 + 120), "about 9 hours ago");
        assert_eq!(ago(24 * 3600), "about 1 day ago");
        assert_eq!(ago(20 * 3600), "about 20 hours ago");
        assert_eq!(ago(13 * 24 * 3600), "about 13 days ago");
        assert_eq!(ago(65 * 24 * 3600), "about 2 months ago");
        assert_eq!(ago(400 * 24 * 3600), "about 1 year ago");
        assert_eq!(ago(3 * 365 * 24 * 3600), "about 3 years ago");
        // future timestamps clamp to "now"
        assert_eq!(fuzzy_ago(now, at(500)), "less than a minute ago");
    }

    #[test]
    fn abbreviations() {
        let now = at(0);
        assert_eq!(fuzzy_ago_abbr(now, at(-30)), "0m");
        assert_eq!(fuzzy_ago_abbr(now, at(-4 * 86_400 - 5)), "4d");
        assert_eq!(fuzzy_ago_abbr(now, at(-5 * 3600)), "5h");
    }

    #[test]
    fn due_phrases() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 9).expect("date");
        let d = |days: i64| due_phrase(today + Duration::days(days), today);
        assert_eq!(d(0), ("today".into(), DueState::Today));
        assert_eq!(d(1), ("tomorrow".into(), DueState::Upcoming));
        assert_eq!(d(-1), ("yesterday".into(), DueState::Overdue));
        assert_eq!(d(-3), ("3 days overdue".into(), DueState::Overdue));
        assert_eq!(d(3), ("Sep 12".into(), DueState::Upcoming));
        assert_eq!(d(200), ("Mar 28, 2027".into(), DueState::Upcoming));
    }
}
