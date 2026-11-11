//! Recurring task rules. A task with a `repeat` rule queues its next instance when it finishes
//! (see `tasks::enqueue_next`). The next instance is an ordinary Pending task gated by
//! `schedule = after:<time>`, so it passes every scheduler/preflight/billing check like any other.
//! An expiry on a repeating task is carried over as a lifetime: each instance may wait the same time
//! (original `expiration_at - created_at`) from its own scheduled start before it expires.
//!
//! Grammar (local time):
//! - `every:<N><m|h|d|w>`: N minutes/hours/days/weeks after the previous instance finished (`every:6h`).
//! - `daily:HH:MM`: the next local HH:MM after the previous instance finished.
//! - `weekly:<days>@HH:MM`: the next matching weekday at HH:MM; `<days>` is a list/range of `mon..sun`
//!   as in account availability (`weekly:mon,wed,fri@09:00`, `weekly:mon-fri@18:30`).
use crate::availability::parse_days;
use crate::settings::parse_hours;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repeat {
    Every(Duration),
    Daily(u32),
    /// Bit `n` = weekday `n` (0 = Monday), minute of day.
    Weekly(u8, u32),
}

pub const GRAMMAR: &str = "every:<N><m|h|d|w> (every:6h), daily:HH:MM, weekly:mon,wed-fri@HH:MM";

fn time_of_day(s: &str) -> Option<u32> {
    parse_hours(&format!("{s}-{s}")).map(|(m, _)| m)
}

pub fn parse(spec: &str) -> Result<Repeat, String> {
    let bad = || format!("repeat \"{spec}\": expected {GRAMMAR}");
    let spec = spec.trim();
    if let Some(r) = spec.strip_prefix("every:") {
        let unit = r.chars().last().ok_or_else(bad)?;
        let n: i64 = r[..r.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
        if !(1..=10_000).contains(&n) {
            return Err(bad());
        }
        let d = match unit {
            'm' => Duration::minutes(n),
            'h' => Duration::hours(n),
            'd' => Duration::days(n),
            'w' => Duration::weeks(n),
            _ => return Err(bad()),
        };
        // Repeating more often than every 5 minutes would only flood the queue.
        if d < Duration::minutes(5) {
            return Err(format!("repeat \"{spec}\": minimum interval is 5 minutes"));
        }
        Ok(Repeat::Every(d))
    } else if let Some(t) = spec.strip_prefix("daily:") {
        time_of_day(t).map(Repeat::Daily).ok_or_else(bad)
    } else if let Some(r) = spec.strip_prefix("weekly:") {
        let (days, t) = r.split_once('@').ok_or_else(bad)?;
        Ok(Repeat::Weekly(
            parse_days(days)?,
            time_of_day(t).ok_or_else(bad)?,
        ))
    } else {
        Err(bad())
    }
}

/// Rejects rules `next_after` could not interpret. `None`/empty is valid (one-off task).
pub fn validate(spec: Option<&str>) -> crate::Result<()> {
    match spec {
        None | Some("") => Ok(()),
        Some(s) => parse(s).map(|_| ()).map_err(crate::Error::Invalid),
    }
}

/// First local time-of-day occurrence strictly after `from` on a day accepted by `day_ok`.
fn next_clock(
    from: DateTime<Utc>,
    minute: u32,
    day_ok: impl Fn(u32) -> bool,
) -> Option<DateTime<Utc>> {
    let local = from.with_timezone(&Local);
    let t = NaiveTime::from_hms_opt(minute / 60, minute % 60, 0)?;
    for add in 0..=8 {
        let date = local.date_naive() + Duration::days(add);
        if !day_ok(date.weekday().num_days_from_monday()) {
            continue;
        }
        // Skips nonexistent local times (DST gap) by moving on to the next matching day.
        if let Some(c) = Local.from_local_datetime(&date.and_time(t)).earliest() {
            let c = c.with_timezone(&Utc);
            if c > from {
                return Some(c);
            }
        }
    }
    None
}

/// The next time an instance may start after one that finished at `from`. `None` if the rule is
/// unparsable (fails closed: nothing is queued).
pub fn next_after(spec: &str, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match parse(spec).ok()? {
        Repeat::Every(d) => Some(from + d),
        Repeat::Daily(m) => next_clock(from, m, |_| true),
        Repeat::Weekly(mask, m) => next_clock(from, m, |d| mask & (1 << d) != 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rejects() {
        assert_eq!(parse("every:6h"), Ok(Repeat::Every(Duration::hours(6))));
        assert_eq!(parse("every:2w"), Ok(Repeat::Every(Duration::weeks(2))));
        assert_eq!(parse("daily:09:30"), Ok(Repeat::Daily(570)));
        assert_eq!(
            parse("weekly:mon,wed@18:00"),
            Ok(Repeat::Weekly(0b101, 1080))
        );
        for bad in [
            "",
            "every:",
            "every:0h",
            "every:2x",
            "every:1m",
            "daily:25:00",
            "weekly:mon",
            "weekly:xyz@09:00",
            "hourly",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(validate(None).is_ok() && validate(Some("")).is_ok());
        assert!(validate(Some("nope")).is_err());
    }

    #[test]
    fn next_occurrences_are_strictly_later() {
        let from = Utc::now();
        assert_eq!(
            next_after("every:2h", from),
            Some(from + Duration::hours(2))
        );
        for spec in [
            "daily:00:00",
            "daily:12:34",
            "weekly:mon-sun@08:15",
            "weekly:fri@23:59",
        ] {
            let n = next_after(spec, from).unwrap();
            assert!(n > from && n <= from + Duration::days(8), "{spec}");
        }
        let wd = next_after("weekly:wed@10:00", from)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(wd.weekday().num_days_from_monday(), 2);
        assert_eq!(next_after("garbage", from), None);
    }
}
