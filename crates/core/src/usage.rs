//! Usage snapshots and the capacity decision. Unknown or stale usage is never "available".
use crate::models::Confidence;
use crate::{db, Error, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub id: i64,
    pub account_id: i64,
    /// Percent remaining 0-100. `None` = unknown.
    pub session_remaining: Option<f64>,
    pub weekly_remaining: Option<f64>,
    pub session_reset_at: Option<DateTime<Utc>>,
    pub weekly_reset_at: Option<DateTime<Utc>>,
    pub confidence: Confidence,
    pub source: String,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityBlock {
    NoUsageData,
    UnknownConfidence,
    Stale {
        age_minutes: i64,
        max_minutes: i64,
    },
    ResetPassed,
    /// The session (5 h) window has no reading, so capacity cannot be verified (fails closed).
    MissingSession,
    /// The weekly window has no reading (fails closed).
    MissingWeekly,
    /// Neither window has a reading.
    Incomplete,
    /// Percentages are rounded to whole numbers (keeps the type `Eq`).
    BelowThreshold {
        which: &'static str,
        remaining: i32,
        threshold: i32,
    },
    Insufficient {
        which: &'static str,
        remaining: i32,
        reserve: i32,
        needed: i32,
    },
    ExceedsBudget,
}

impl std::fmt::Display for CapacityBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapacityBlock::NoUsageData => write!(
                f,
                "no usage reading yet: turn on Automatic usage for the account, or enter the usage manually"
            ),
            CapacityBlock::UnknownConfidence => write!(
                f,
                "usage confidence is unknown: refresh the usage or enter it manually"
            ),
            CapacityBlock::Stale {
                age_minutes,
                max_minutes,
            } => {
                write!(
                    f,
                    "usage reading is {age_minutes} min old (limit {max_minutes} min): refresh the usage or enter it manually"
                )
            }
            CapacityBlock::ResetPassed => {
                write!(
                    f,
                    "a usage window has reset since the last reading: refresh the usage"
                )
            }
            CapacityBlock::MissingSession => {
                write!(
                    f,
                    "5 h session usage is unknown (only the weekly window was reported): enter both windows or refresh"
                )
            }
            CapacityBlock::MissingWeekly => {
                write!(
                    f,
                    "weekly usage is unknown (only the 5 h session window was reported): enter both windows or refresh"
                )
            }
            CapacityBlock::Incomplete => write!(
                f,
                "5 h session and weekly usage are both unknown: refresh the usage or enter it manually"
            ),
            CapacityBlock::BelowThreshold {
                which,
                remaining,
                threshold,
            } => {
                write!(
                    f,
                    "{which} window has {remaining}% left, below the {threshold}% start threshold"
                )
            }
            CapacityBlock::Insufficient {
                which,
                remaining,
                reserve,
                needed,
            } => {
                write!(
                    f,
                    "{which} window: {remaining}% left minus the {reserve}% reserve is less than the ~{needed}% this task needs"
                )
            }
            CapacityBlock::ExceedsBudget => write!(f, "estimate exceeds the task's maximum budget"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CapacityRules {
    pub max_age_minutes: i64,
    pub threshold: f64,
    pub reserved: f64,
    /// Rules for the session (5 h) window; weekly uses `threshold`/`reserved`.
    pub session_threshold: f64,
    pub session_reserved: f64,
    /// False = the session window is ignored entirely (decision on the weekly window only).
    pub check_session: bool,
    /// "Burn before reset": when that window's known reset is at most this many hours away,
    /// its threshold and reserve drop to 0 so capacity that would expire is spent. `None` = off.
    /// An unknown reset time never triggers it (fail closed).
    pub burn_session_hours: Option<f64>,
    pub burn_weekly_hours: Option<f64>,
}

impl CapacityRules {
    /// Same rule for both windows.
    pub fn uniform(max_age_minutes: i64, threshold: f64, reserved: f64) -> Self {
        CapacityRules {
            max_age_minutes,
            threshold,
            reserved,
            session_threshold: threshold,
            session_reserved: reserved,
            check_session: true,
            burn_session_hours: None,
            burn_weekly_hours: None,
        }
    }
}

/// True when `reset_at` is known, still in the future and within `hours` of `now`.
fn burning(hours: Option<f64>, reset_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match (hours, reset_at) {
        (Some(h), Some(r)) => r > now && (r - now).num_seconds() as f64 <= h * 3600.0,
        _ => false,
    }
}

/// Decides whether `needed` percent (a task's estimated consumption) fits.
/// Pure function: all safety-relevant capacity logic lives here.
pub fn check_capacity(
    snap: Option<&UsageSnapshot>,
    now: DateTime<Utc>,
    rules: CapacityRules,
    needed: f64,
    max_budget: Option<f64>,
) -> std::result::Result<(), CapacityBlock> {
    let s = snap.ok_or(CapacityBlock::NoUsageData)?;
    if s.confidence == Confidence::Unknown {
        return Err(CapacityBlock::UnknownConfidence);
    }
    let age = now - s.captured_at;
    if age > Duration::minutes(rules.max_age_minutes) || age < Duration::minutes(-5) {
        return Err(CapacityBlock::Stale {
            age_minutes: age.num_minutes(),
            max_minutes: rules.max_age_minutes,
        });
    }
    if (rules.check_session && s.session_reset_at.is_some_and(|r| r <= now))
        || s.weekly_reset_at.is_some_and(|r| r <= now)
    {
        return Err(CapacityBlock::ResetPassed);
    }
    // Unknown is never "available": a missing window blocks, with a message naming which one.
    let week = match (s.session_remaining, s.weekly_remaining, rules.check_session) {
        (_, Some(w), false) | (Some(_), Some(w), true) => w,
        (_, None, false) | (Some(_), None, true) => return Err(CapacityBlock::MissingWeekly),
        (None, Some(_), true) => return Err(CapacityBlock::MissingSession),
        (None, None, true) => return Err(CapacityBlock::Incomplete),
    };
    let sess = s.session_remaining.filter(|_| rules.check_session);
    let (s_thr, s_res) = if burning(rules.burn_session_hours, s.session_reset_at, now) {
        (0.0, 0.0)
    } else {
        (rules.session_threshold, rules.session_reserved)
    };
    let (w_thr, w_res) = if burning(rules.burn_weekly_hours, s.weekly_reset_at, now) {
        (0.0, 0.0)
    } else {
        (rules.threshold, rules.reserved)
    };
    let pct = |v: f64| v.round() as i32;
    if let Some(v) = sess.filter(|v| *v < s_thr) {
        return Err(CapacityBlock::BelowThreshold {
            which: "5 h session",
            remaining: pct(v),
            threshold: pct(s_thr),
        });
    }
    if week < w_thr {
        return Err(CapacityBlock::BelowThreshold {
            which: "weekly",
            remaining: pct(week),
            threshold: pct(w_thr),
        });
    }
    if let Some(v) = sess.filter(|v| v - s_res < needed) {
        return Err(CapacityBlock::Insufficient {
            which: "5 h session",
            remaining: pct(v),
            reserve: pct(s_res),
            needed: pct(needed),
        });
    }
    if week - w_res < needed {
        return Err(CapacityBlock::Insufficient {
            which: "weekly",
            remaining: pct(week),
            reserve: pct(w_res),
            needed: pct(needed),
        });
    }
    if max_budget.is_some_and(|b| needed > b) {
        return Err(CapacityBlock::ExceedsBudget);
    }
    Ok(())
}

pub struct NewSnapshot {
    pub account_id: i64,
    pub session_remaining: Option<f64>,
    pub weekly_remaining: Option<f64>,
    pub session_reset_at: Option<DateTime<Utc>>,
    pub weekly_reset_at: Option<DateTime<Utc>>,
    pub confidence: Confidence,
    pub source: String,
}

pub fn record(conn: &Connection, n: &NewSnapshot, now: DateTime<Utc>) -> Result<i64> {
    for v in [n.session_remaining, n.weekly_remaining]
        .into_iter()
        .flatten()
    {
        if !(0.0..=100.0).contains(&v) {
            return Err(Error::Invalid("usage percentages must be 0-100".into()));
        }
    }
    conn.execute(
        "INSERT INTO usage_snapshots (account_id, session_remaining, weekly_remaining, session_reset_at,
            weekly_reset_at, confidence, source, captured_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            n.account_id,
            n.session_remaining,
            n.weekly_remaining,
            n.session_reset_at.map(db::ts),
            n.weekly_reset_at.map(db::ts),
            n.confidence.as_str(),
            n.source,
            db::ts(now)
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn map(r: &rusqlite::Row) -> rusqlite::Result<UsageSnapshot> {
    let t = |i: usize| -> rusqlite::Result<Option<DateTime<Utc>>> {
        Ok(r.get::<_, Option<String>>(i)?
            .and_then(|s| db::parse_ts(&s)))
    };
    Ok(UsageSnapshot {
        id: r.get(0)?,
        account_id: r.get(1)?,
        session_remaining: r.get(2)?,
        weekly_remaining: r.get(3)?,
        session_reset_at: t(4)?,
        weekly_reset_at: t(5)?,
        confidence: Confidence::parse(&r.get::<_, String>(6)?).unwrap_or(Confidence::Unknown),
        source: r.get(7)?,
        captured_at: t(8)?.unwrap_or(DateTime::UNIX_EPOCH),
    })
}

const COLS: &str = "id, account_id, session_remaining, weekly_remaining, session_reset_at, weekly_reset_at, confidence, source, captured_at";

pub fn latest(conn: &Connection, account_id: i64) -> Result<Option<UsageSnapshot>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {COLS} FROM usage_snapshots WHERE account_id=?1 ORDER BY id DESC LIMIT 1"
            ),
            [account_id],
            map,
        )
        .optional()?)
}

pub fn history(conn: &Connection, account_id: i64, limit: usize) -> Result<Vec<UsageSnapshot>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM usage_snapshots WHERE account_id=?1 ORDER BY id DESC LIMIT ?2"
    ))?;
    let v = st
        .query_map(params![account_id, limit as i64], map)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

/// Rough "after this much time, resets happened" helper for the UI.
/// The snapshot's 5 h session reset, only when the reading is fresh enough to trust and the reset is
/// still ahead of `now` (a past reset on a stale snapshot says nothing about the next one).
pub fn upcoming_session_reset(
    s: &UsageSnapshot,
    max_age_minutes: i64,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let age = now - s.captured_at;
    if age > Duration::minutes(max_age_minutes) {
        return None;
    }
    s.session_reset_at.filter(|t| *t > now)
}

pub fn minutes_until(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<i64> {
    t.map(|t| (t - now).num_minutes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> CapacityRules {
        CapacityRules::uniform(30, 65.0, 20.0)
    }
    fn snap(now: DateTime<Utc>) -> UsageSnapshot {
        UsageSnapshot {
            id: 1,
            account_id: 1,
            session_remaining: Some(80.0),
            weekly_remaining: Some(90.0),
            session_reset_at: Some(now + Duration::hours(2)),
            weekly_reset_at: Some(now + Duration::days(3)),
            confidence: Confidence::Manual,
            source: "manual".into(),
            captured_at: now - Duration::minutes(5),
        }
    }

    #[test]
    fn capacity_available() {
        let now = Utc::now();
        assert!(check_capacity(Some(&snap(now)), now, rules(), 10.0, Some(15.0)).is_ok());
    }
    #[test]
    fn burn_before_reset_spends_a_window_that_is_about_to_expire() {
        let now = Utc::now();
        let mut s = snap(now); // session resets in 2 h, weekly in 3 days
        s.session_remaining = Some(30.0); // below the 65 threshold
        let off = rules();
        assert!(matches!(
            check_capacity(Some(&s), now, off, 25.0, None),
            Err(CapacityBlock::BelowThreshold {
                which: "5 h session",
                ..
            })
        ));
        // reset is 2 h away: a 3 h burn window drops threshold and reserve to 0
        let on = CapacityRules {
            burn_session_hours: Some(3.0),
            ..off
        };
        assert!(check_capacity(Some(&s), now, on, 25.0, None).is_ok());
        // a 1 h burn window is not reached yet
        let early = CapacityRules {
            burn_session_hours: Some(1.0),
            ..off
        };
        assert!(check_capacity(Some(&s), now, early, 25.0, None).is_err());
        // the estimate must still fit
        assert!(matches!(
            check_capacity(Some(&s), now, on, 35.0, None),
            Err(CapacityBlock::Insufficient {
                which: "5 h session",
                ..
            })
        ));
    }
    #[test]
    fn burning_one_window_does_not_relax_the_other() {
        let now = Utc::now();
        let mut s = snap(now);
        s.session_remaining = Some(30.0);
        s.weekly_remaining = Some(30.0); // weekly resets in 3 days
        let r = CapacityRules {
            burn_session_hours: Some(3.0),
            ..rules()
        };
        assert!(matches!(
            check_capacity(Some(&s), now, r, 5.0, None),
            Err(CapacityBlock::BelowThreshold {
                which: "weekly",
                ..
            })
        ));
        let r = CapacityRules {
            burn_session_hours: Some(3.0),
            burn_weekly_hours: Some(100.0),
            ..rules()
        };
        assert!(check_capacity(Some(&s), now, r, 5.0, None).is_ok());
    }
    #[test]
    fn burn_needs_a_known_reset_and_never_bypasses_safety() {
        let now = Utc::now();
        let r = CapacityRules {
            burn_session_hours: Some(5.0),
            burn_weekly_hours: Some(168.0),
            ..rules()
        };
        let mut s = snap(now);
        s.session_remaining = Some(30.0);
        s.session_reset_at = None; // unknown reset: burn does not apply
        assert!(check_capacity(Some(&s), now, r, 5.0, None).is_err());
        s.session_reset_at = Some(now + Duration::hours(1));
        assert!(check_capacity(Some(&s), now, r, 5.0, None).is_ok());
        // stale, passed reset and missing data still block
        s.captured_at = now - Duration::minutes(90);
        assert!(matches!(
            check_capacity(Some(&s), now, r, 5.0, None),
            Err(CapacityBlock::Stale { .. })
        ));
        s.captured_at = now - Duration::minutes(5);
        s.session_reset_at = Some(now - Duration::minutes(1));
        assert_eq!(
            check_capacity(Some(&s), now, r, 5.0, None),
            Err(CapacityBlock::ResetPassed)
        );
        s.session_reset_at = Some(now + Duration::hours(1));
        s.weekly_remaining = None;
        assert_eq!(
            check_capacity(Some(&s), now, r, 5.0, None),
            Err(CapacityBlock::MissingWeekly)
        );
    }
    #[test]
    fn block_messages_carry_numbers_and_a_remedy() {
        let now = Utc::now();
        let mut s = snap(now);
        s.weekly_remaining = Some(58.0);
        let m = check_capacity(Some(&s), now, rules(), 5.0, None)
            .unwrap_err()
            .to_string();
        assert!(
            m.contains("weekly") && m.contains("58%") && m.contains("65%"),
            "{m}"
        );
        s.weekly_remaining = Some(90.0);
        s.session_remaining = Some(70.0);
        let m = check_capacity(Some(&s), now, rules(), 60.0, None)
            .unwrap_err()
            .to_string();
        assert!(
            m.contains("70%") && m.contains("20%") && m.contains("60%"),
            "{m}"
        );
        s.captured_at = now - Duration::minutes(90);
        let m = check_capacity(Some(&s), now, rules(), 5.0, None)
            .unwrap_err()
            .to_string();
        assert!(
            m.contains("90 min") && m.contains("limit 30") && m.contains("refresh"),
            "{m}"
        );
        let m = check_capacity(None, now, rules(), 5.0, None)
            .unwrap_err()
            .to_string();
        assert!(
            m.contains("Automatic usage") && m.contains("manually"),
            "{m}"
        );
    }
    #[test]
    fn no_data_is_blocked() {
        assert_eq!(
            check_capacity(None, Utc::now(), rules(), 1.0, None),
            Err(CapacityBlock::NoUsageData)
        );
    }
    #[test]
    fn no_capacity() {
        let now = Utc::now();
        let mut s = snap(now);
        s.session_remaining = Some(10.0);
        assert!(matches!(
            check_capacity(Some(&s), now, rules(), 1.0, None),
            Err(CapacityBlock::BelowThreshold { .. })
        ));
        s.session_remaining = Some(70.0);
        assert!(matches!(
            check_capacity(Some(&s), now, rules(), 60.0, None),
            Err(CapacityBlock::Insufficient {
                which: "5 h session",
                ..
            })
        ));
    }
    #[test]
    fn weekly_insufficient_while_session_ok() {
        let now = Utc::now();
        let mut s = snap(now);
        s.weekly_remaining = Some(30.0);
        assert!(matches!(
            check_capacity(Some(&s), now, rules(), 5.0, None),
            Err(CapacityBlock::BelowThreshold {
                which: "weekly",
                ..
            })
        ));
        let r = CapacityRules {
            threshold: 10.0,
            session_threshold: 10.0,
            ..rules()
        };
        s.weekly_remaining = Some(30.0);
        assert!(matches!(
            check_capacity(Some(&s), now, r, 15.0, None),
            Err(CapacityBlock::Insufficient {
                which: "weekly",
                ..
            })
        ));
    }
    #[test]
    fn session_window_has_its_own_rules() {
        let now = Utc::now();
        let mut s = snap(now);
        s.session_remaining = Some(40.0);
        // general rule blocks the session...
        assert!(matches!(
            check_capacity(Some(&s), now, rules(), 5.0, None),
            Err(CapacityBlock::BelowThreshold {
                which: "5 h session",
                ..
            })
        ));
        // ...a lower session threshold/reserve lets it through while weekly keeps the general rule
        let r = CapacityRules {
            session_threshold: 30.0,
            session_reserved: 5.0,
            ..rules()
        };
        assert!(check_capacity(Some(&s), now, r, 5.0, None).is_ok());
        s.weekly_remaining = Some(50.0);
        assert!(matches!(
            check_capacity(Some(&s), now, r, 5.0, None),
            Err(CapacityBlock::BelowThreshold {
                which: "weekly",
                ..
            })
        ));
    }
    #[test]
    fn ignoring_the_session_window_uses_weekly_only_but_stays_fail_closed() {
        let now = Utc::now();
        let r = CapacityRules {
            check_session: false,
            ..rules()
        };
        let mut s = snap(now);
        s.session_remaining = Some(1.0);
        s.session_reset_at = Some(now - Duration::minutes(1));
        assert!(check_capacity(Some(&s), now, r, 5.0, None).is_ok());
        s.session_remaining = None;
        assert!(check_capacity(Some(&s), now, r, 5.0, None).is_ok());
        // weekly is still required, fresh and sufficient
        s.weekly_remaining = None;
        assert_eq!(
            check_capacity(Some(&s), now, r, 5.0, None),
            Err(CapacityBlock::MissingWeekly)
        );
        s.weekly_remaining = Some(90.0);
        s.captured_at = now - Duration::minutes(31);
        assert!(matches!(
            check_capacity(Some(&s), now, r, 5.0, None),
            Err(CapacityBlock::Stale { .. })
        ));
        s.captured_at = now;
        assert!(matches!(
            check_capacity(Some(&s), now, r, 95.0, None),
            Err(CapacityBlock::Insufficient {
                which: "weekly",
                ..
            })
        ));
    }
    #[test]
    fn stale_is_blocked() {
        let now = Utc::now();
        let mut s = snap(now);
        s.captured_at = now - Duration::minutes(31);
        assert!(matches!(
            check_capacity(Some(&s), now, rules(), 1.0, None),
            Err(CapacityBlock::Stale { .. })
        ));
    }
    #[test]
    fn unknown_values_are_not_available() {
        let now = Utc::now();
        let mut s = snap(now);
        s.weekly_remaining = None;
        assert_eq!(
            check_capacity(Some(&s), now, rules(), 1.0, None),
            Err(CapacityBlock::MissingWeekly)
        );
        let mut s = snap(now);
        s.session_remaining = None;
        assert_eq!(
            check_capacity(Some(&s), now, rules(), 1.0, None),
            Err(CapacityBlock::MissingSession)
        );
        s.weekly_remaining = None;
        assert_eq!(
            check_capacity(Some(&s), now, rules(), 1.0, None),
            Err(CapacityBlock::Incomplete)
        );
        for b in [
            CapacityBlock::MissingSession,
            CapacityBlock::MissingWeekly,
            CapacityBlock::Incomplete,
        ] {
            assert!(!b.to_string().is_empty());
        }
        let mut s = snap(now);
        s.confidence = Confidence::Unknown;
        assert_eq!(
            check_capacity(Some(&s), now, rules(), 1.0, None),
            Err(CapacityBlock::UnknownConfidence)
        );
    }
    #[test]
    fn reset_passed_invalidates() {
        let now = Utc::now();
        let mut s = snap(now);
        s.session_reset_at = Some(now - Duration::minutes(1));
        assert_eq!(
            check_capacity(Some(&s), now, rules(), 1.0, None),
            Err(CapacityBlock::ResetPassed)
        );
    }
    #[test]
    fn budget_enforced() {
        let now = Utc::now();
        assert_eq!(
            check_capacity(Some(&snap(now)), now, rules(), 12.0, Some(10.0)),
            Err(CapacityBlock::ExceedsBudget)
        );
    }
    #[test]
    fn snapshots_persist() {
        let c = db::open_memory().unwrap();
        let a = crate::accounts::create(&c, "claude", "A", "Pro", None).unwrap();
        let now = Utc::now();
        let n = NewSnapshot {
            account_id: a.id,
            session_remaining: Some(50.0),
            weekly_remaining: None,
            session_reset_at: None,
            weekly_reset_at: None,
            confidence: Confidence::Manual,
            source: "manual".into(),
        };
        record(&c, &n, now).unwrap();
        let l = latest(&c, a.id).unwrap().unwrap();
        assert_eq!(l.session_remaining, Some(50.0));
        assert_eq!(l.weekly_remaining, None);
        let bad = NewSnapshot {
            session_remaining: Some(120.0),
            ..n
        };
        assert!(record(&c, &bad, now).is_err());
    }

    #[test]
    fn upcoming_reset_ignores_stale_and_past() {
        let now = Utc::now();
        let snap = |captured: DateTime<Utc>, reset: Option<DateTime<Utc>>| UsageSnapshot {
            id: 1,
            account_id: 1,
            session_remaining: Some(50.0),
            weekly_remaining: Some(50.0),
            session_reset_at: reset,
            weekly_reset_at: None,
            confidence: Confidence::Provider,
            source: "t".into(),
            captured_at: captured,
        };
        let soon = now + Duration::hours(2);
        assert_eq!(
            upcoming_session_reset(&snap(now, Some(soon)), 30, now),
            Some(soon)
        );
        assert_eq!(
            upcoming_session_reset(&snap(now - Duration::minutes(90), Some(soon)), 30, now),
            None,
            "stale reading"
        );
        assert_eq!(
            upcoming_session_reset(&snap(now, Some(now - Duration::minutes(1))), 30, now),
            None,
            "past reset"
        );
        assert_eq!(upcoming_session_reset(&snap(now, None), 30, now), None);
    }
}
