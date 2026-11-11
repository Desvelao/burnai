//! Per-account availability: an on-demand master switch plus an optional weekly schedule.
//!
//! Schedule grammar (local time): `;`-separated rules, each `DAYS HH:MM-HH:MM`.
//! `DAYS` is `*`/`all`, or a comma list of `mon..sun` and ranges (`mon-fri`, may wrap: `fri-mon`).
//! A rule without `DAYS` applies every day. Windows may wrap midnight (the part after midnight belongs to
//! the next weekday); `start == end` means the whole day. Empty schedule = always available.
//! Example: `mon-fri 09:00-17:00; sat,sun 10:00-14:00`.
use crate::models::Account;
use crate::settings::parse_hours;
use chrono::{Datelike, Timelike};

const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// A point in local time: weekday (0 = Monday .. 6 = Sunday) and minute of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalClock {
    pub weekday: u32,
    pub minute: u32,
}

impl LocalClock {
    pub fn new(weekday: u32, minute: u32) -> Self {
        LocalClock {
            weekday: weekday % 7,
            minute,
        }
    }

    pub fn from_local(t: chrono::DateTime<chrono::Local>) -> Self {
        LocalClock::new(
            t.weekday().num_days_from_monday(),
            t.hour() * 60 + t.minute(),
        )
    }

    pub fn now() -> Self {
        Self::from_local(chrono::Local::now())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    /// Bit `n` = weekday `n` (0 = Monday).
    days: u8,
    start: u32,
    end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WeeklySchedule {
    rules: Vec<Rule>,
}

fn day_index(name: &str) -> Result<u32, String> {
    let n = name.trim().to_ascii_lowercase();
    DAY_NAMES
        .iter()
        .position(|d| *d == n)
        .map(|i| i as u32)
        .ok_or_else(|| format!("unknown day '{}' (use mon..sun)", name.trim()))
}

pub(crate) fn parse_days(spec: &str) -> Result<u8, String> {
    let mut mask = 0u8;
    for part in spec.split(',') {
        let part = part.trim();
        if part == "*" || part.eq_ignore_ascii_case("all") {
            return Ok(0b0111_1111);
        }
        if let Some((a, b)) = part.split_once('-') {
            let (a, b) = (day_index(a)?, day_index(b)?);
            let mut d = a;
            loop {
                mask |= 1 << d;
                if d == b {
                    break;
                }
                d = (d + 1) % 7;
            }
        } else {
            mask |= 1 << day_index(part)?;
        }
    }
    Ok(mask)
}

impl WeeklySchedule {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut rules = Vec::new();
        for raw in spec.split(';') {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let (days, window) = match raw.split_once(char::is_whitespace) {
                Some((d, w)) if !d.contains(':') => (parse_days(d)?, w.trim()),
                _ => (0b0111_1111, raw),
            };
            let (start, end) = parse_hours(window)
                .ok_or_else(|| format!("invalid time window '{window}' (use HH:MM-HH:MM)"))?;
            rules.push(Rule { days, start, end });
        }
        Ok(WeeklySchedule { rules })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// True if `c` is inside any rule; an empty schedule is always true.
    pub fn contains(&self, c: LocalClock) -> bool {
        if self.rules.is_empty() {
            return true;
        }
        let day = c.weekday % 7;
        let prev = (day + 6) % 7;
        self.rules.iter().any(|r| {
            let on = |d: u32| r.days & (1 << d) != 0;
            if r.start == r.end {
                on(day)
            } else if r.start < r.end {
                on(day) && c.minute >= r.start && c.minute < r.end
            } else {
                (on(day) && c.minute >= r.start) || (on(prev) && c.minute < r.end)
            }
        })
    }
}

/// Validates a user-supplied schedule (empty is valid).
pub fn validate(spec: &str) -> Result<(), String> {
    WeeklySchedule::parse(spec).map(|_| ())
}

impl Account {
    /// `Ok` if the account may be used now; `Err(reason)` otherwise. The switch is a hard master gate,
    /// the schedule only applies while it is on. An unparsable stored schedule fails closed.
    pub fn availability(&self, clock: LocalClock) -> Result<(), String> {
        if !self.available {
            return Err("account is marked unavailable".into());
        }
        match WeeklySchedule::parse(&self.availability_schedule) {
            Err(e) => Err(format!("invalid availability schedule: {e}")),
            Ok(s) if s.contains(clock) => Ok(()),
            Ok(_) => Err(format!(
                "outside the account's availability schedule ({})",
                self.availability_schedule.trim()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(d: u32, h: u32, m: u32) -> LocalClock {
        LocalClock::new(d, h * 60 + m)
    }

    #[test]
    fn empty_is_always() {
        let s = WeeklySchedule::parse("").unwrap();
        assert!(s.contains(at(3, 3, 0)));
    }

    #[test]
    fn weekday_range_and_window() {
        let s = WeeklySchedule::parse("mon-fri 09:00-17:00").unwrap();
        assert!(s.contains(at(0, 9, 0)));
        assert!(s.contains(at(4, 16, 59)));
        assert!(!s.contains(at(4, 17, 0)));
        assert!(!s.contains(at(5, 12, 0)));
        assert!(!s.contains(at(2, 8, 59)));
    }

    #[test]
    fn multiple_rules_and_bare_window() {
        let s = WeeklySchedule::parse("mon-fri 09:00-17:00; sat,sun 10:00-14:00").unwrap();
        assert!(s.contains(at(6, 11, 0)));
        assert!(!s.contains(at(6, 15, 0)));
        let all = WeeklySchedule::parse("08:00-09:00").unwrap();
        assert!(all.contains(at(6, 8, 30)));
        assert!(WeeklySchedule::parse("* 08:00-09:00")
            .unwrap()
            .contains(at(2, 8, 0)));
    }

    #[test]
    fn midnight_wrap_belongs_to_next_weekday() {
        let s = WeeklySchedule::parse("fri 22:00-06:00").unwrap();
        assert!(s.contains(at(4, 23, 0))); // fri night
        assert!(s.contains(at(5, 3, 0))); // sat early morning (tail of fri)
        assert!(!s.contains(at(4, 3, 0))); // fri early morning is thu's tail
        assert!(!s.contains(at(5, 23, 0)));
    }

    #[test]
    fn day_range_wraps_across_week_end() {
        let s = WeeklySchedule::parse("fri-mon 00:00-00:00").unwrap();
        for d in [4, 5, 6, 0] {
            assert!(s.contains(at(d, 12, 0)), "day {d}");
        }
        assert!(!s.contains(at(2, 12, 0)));
    }

    #[test]
    fn invalid_specs_rejected() {
        for bad in [
            "funday 09:00-10:00",
            "mon 9-10",
            "mon-fri",
            "mon 25:00-26:00",
            "mon xx",
        ] {
            assert!(validate(bad).is_err(), "{bad}");
        }
    }

    fn acct(available: bool, sched: &str) -> Account {
        use crate::models::*;
        Account {
            id: 1,
            provider_id: 1,
            provider_type: "claude".into(),
            label: "A".into(),
            subscription_plan: "".into(),
            billing_mode: BillingMode::Subscription,
            connection_status: ConnectionStatus::Connected,
            config_dir: None,
            subscription_confirmed: true,
            execution_policy: ExecutionPolicy::Automatic,
            credentials_file: None,
            usage_source: UsageSource::Manual,
            user_agent: None,
            custom: None,
            custom_status: None,
            cli_command: None,
            available,
            availability_schedule: sched.into(),
            run_mode: RunMode::Inherit,
            limits: Default::default(),
        }
    }

    #[test]
    fn switch_is_master_gate() {
        assert!(acct(true, "").availability(at(0, 1, 0)).is_ok());
        assert!(acct(false, "").availability(at(0, 1, 0)).is_err());
        assert!(acct(false, "* 00:00-00:00")
            .availability(at(0, 1, 0))
            .is_err());
    }

    #[test]
    fn schedule_applies_and_invalid_fails_closed() {
        let a = acct(true, "mon-fri 09:00-17:00");
        assert!(a.availability(at(1, 10, 0)).is_ok());
        assert!(a.availability(at(6, 10, 0)).is_err());
        assert!(acct(true, "garbage").availability(at(1, 10, 0)).is_err());
    }
}
