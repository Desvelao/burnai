use crate::models::{Account, ExecutionPolicy, RunMode};
use crate::{db, Error, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// What the one-click "Burn before reset" buttons switch on: spend a window's leftovers in its last
/// hour (5 h window) / last day (weekly window). Editable afterwards in Settings or the account editor.
pub const BURN_DEFAULT_SESSION_H: f64 = 1.0;
pub const BURN_DEFAULT_WEEKLY_H: f64 = 24.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Global master switch. When false nothing is ever launched automatically.
    pub enabled: bool,
    pub execution_mode: ExecutionPolicy,
    /// "always" | "once_per_batch"
    pub approval_policy: String,
    pub minimum_capacity_threshold: f64,
    pub reserved_capacity: f64,
    pub maximum_tasks_per_batch: u32,
    pub maximum_execution_minutes: u32,
    pub idle_requirement: u32,
    /// "HH:MM-HH:MM" local time, empty = always.
    pub allowed_hours: String,
    pub max_usage_age_minutes: u32,
    pub interval_normal_minutes: u32,
    pub interval_waiting_minutes: u32,
    pub quiet_hours: String,
    pub notify_desktop: bool,
    pub notify_telegram: bool,
    pub telegram_chat_id: String,
    pub notify_include_names: bool,
    /// Tasks may only run inside this directory tree. Empty = no task may run.
    pub working_root: String,
    pub results_dir: String,
    /// Audit entries, notifications and usage snapshots older than this are deleted daily. 0 = keep forever.
    pub retention_days: u32,
    /// Global default run mode (account and task settings override it; `Inherit` = provider default).
    pub default_run_mode: RunMode,
    /// "Burn before reset": when the 5 h window resets within this many hours, its threshold and
    /// reserve drop to 0 so capacity that would expire is spent. `None`/0 = off.
    pub burn_session_hours: Option<f64>,
    /// Same for the weekly window (hours until the weekly reset).
    pub burn_weekly_hours: Option<f64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            execution_mode: ExecutionPolicy::Approval,
            approval_policy: "always".into(),
            minimum_capacity_threshold: 65.0,
            reserved_capacity: 20.0,
            maximum_tasks_per_batch: 3,
            maximum_execution_minutes: 60,
            idle_requirement: 0,
            allowed_hours: String::new(),
            max_usage_age_minutes: 30,
            interval_normal_minutes: 5,
            interval_waiting_minutes: 2,
            quiet_hours: String::new(),
            notify_desktop: true,
            notify_telegram: false,
            telegram_chat_id: String::new(),
            notify_include_names: false,
            working_root: String::new(),
            results_dir: String::new(),
            retention_days: 90,
            default_run_mode: RunMode::Inherit,
            burn_session_hours: None,
            burn_weekly_hours: None,
        }
    }
}

impl Settings {
    /// The settings that apply to one account: its overrides replace the general values, and
    /// `ignore_limits` drops threshold, reserve, idle and allowed-hours. It never touches staleness
    /// (`max_usage_age_minutes`), billing, the global enable switch or the batch size limits.
    pub fn effective_for(&self, a: &Account) -> Settings {
        let l = &a.limits;
        let mut s = self.clone();
        if let Some(v) = l.start_threshold {
            s.minimum_capacity_threshold = v;
        }
        if let Some(v) = l.reserved_capacity {
            s.reserved_capacity = v;
        }
        if let Some(v) = l.idle_requirement {
            s.idle_requirement = v;
        }
        if let Some(v) = &l.allowed_hours {
            s.allowed_hours = v.clone();
        }
        if let Some(v) = l.max_usage_age_minutes {
            s.max_usage_age_minutes = v;
        }
        if let Some(v) = l.max_tasks_per_batch {
            s.maximum_tasks_per_batch = v;
        }
        if let Some(v) = l.max_execution_minutes {
            s.maximum_execution_minutes = v;
        }
        if l.burn_session_hours.is_some() {
            s.burn_session_hours = l.burn_session_hours;
        }
        if l.burn_weekly_hours.is_some() {
            s.burn_weekly_hours = l.burn_weekly_hours;
        }
        if l.ignore_limits {
            s.minimum_capacity_threshold = 0.0;
            s.reserved_capacity = 0.0;
            s.idle_requirement = 0;
            s.allowed_hours.clear();
        }
        s
    }

    /// (start threshold, reserved, check) for the session (5 h) window of `a`, given its effective settings.
    pub fn session_rules(&self, a: &Account) -> (f64, f64, bool) {
        if a.limits.ignore_limits {
            return (0.0, 0.0, !a.limits.ignore_session);
        }
        (
            a.limits
                .session_threshold
                .unwrap_or(self.minimum_capacity_threshold),
            a.limits.session_reserved.unwrap_or(self.reserved_capacity),
            !a.limits.ignore_session,
        )
    }

    /// A configured working root must be an existing directory. Kept out of `validate` (and so out of
    /// `save`) on purpose: switching automation off must keep working even if the folder disappeared.
    /// Called by every user-facing settings save. An empty root is allowed here; the dashboard warns
    /// and tasks cannot run until one is set.
    pub fn validate_working_root(&self) -> Result<()> {
        let r = self.working_root.as_str();
        if r.is_empty() {
            return Ok(());
        }
        match std::fs::metadata(r) {
            Ok(m) if m.is_dir() => Ok(()),
            Ok(_) => Err(Error::Invalid(format!(
                "Working root \"{r}\" is not a directory"
            ))),
            Err(e) => Err(Error::Invalid(format!(
                "Working root \"{r}\" cannot be used: {e}"
            ))),
        }
    }

    pub fn validate(&self) -> Result<()> {
        // Messages use the labels shown in the GUI/TUI and echo the rejected value.
        for (n, v) in [
            (
                "Start when capacity above (%)",
                self.minimum_capacity_threshold,
            ),
            ("Reserved capacity (%)", self.reserved_capacity),
        ] {
            if !(0.0..=100.0).contains(&v) {
                return Err(Error::Invalid(format!(
                    "{n} must be between 0 and 100 (got {v})"
                )));
            }
        }
        for (n, v, max) in [
            (
                "Burn 5 h window when it resets within (h)",
                self.burn_session_hours,
                5.0,
            ),
            (
                "Burn weekly window when it resets within (h)",
                self.burn_weekly_hours,
                168.0,
            ),
        ] {
            if let Some(v) = v {
                if !(0.0..=max).contains(&v) {
                    return Err(Error::Invalid(format!(
                        "{n} must be between 0 and {max} (got {v})"
                    )));
                }
            }
        }
        for (n, v) in [
            ("Max tasks per batch", self.maximum_tasks_per_batch),
            (
                "Max execution minutes per batch",
                self.maximum_execution_minutes,
            ),
            ("Check interval, normal (min)", self.interval_normal_minutes),
            (
                "Check interval, waiting (min)",
                self.interval_waiting_minutes,
            ),
        ] {
            if v == 0 {
                return Err(Error::Invalid(format!("{n} must be at least 1 (got 0)")));
            }
        }
        if self.retention_days != 0 && !(7..=3650).contains(&self.retention_days) {
            return Err(Error::Invalid(format!(
                "Keep history (days) must be 0 (keep forever) or between 7 and 3650 (got {})",
                self.retention_days
            )));
        }
        if !self.allowed_hours.is_empty() && parse_hours(&self.allowed_hours).is_none() {
            return Err(Error::Invalid(format!(
                "Allowed hours must look like 22:00-07:00 (got \"{}\")",
                self.allowed_hours
            )));
        }
        if !self.quiet_hours.is_empty() && parse_hours(&self.quiet_hours).is_none() {
            return Err(Error::Invalid(format!(
                "Quiet hours must look like 23:00-07:00 (got \"{}\")",
                self.quiet_hours
            )));
        }
        Ok(())
    }
}

/// Parses "HH:MM-HH:MM" into (start, end) minutes of day.
pub fn parse_hours(spec: &str) -> Option<(u32, u32)> {
    let (a, b) = spec.split_once('-')?;
    let p = |s: &str| -> Option<u32> {
        let (h, m) = s.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    Some((p(a)?, p(b)?))
}

/// True if `minute_of_day` is inside the window; empty spec = always true. Windows may wrap midnight.
pub fn in_window(spec: &str, minute_of_day: u32) -> bool {
    if spec.is_empty() {
        return true;
    }
    match parse_hours(spec) {
        None => false,
        Some((s, e)) if s == e => true,
        Some((s, e)) if s < e => minute_of_day >= s && minute_of_day < e,
        Some((s, e)) => minute_of_day >= s || minute_of_day < e,
    }
}

pub fn load(conn: &Connection) -> Result<Settings> {
    Ok(conn.query_row(
        "SELECT enabled, execution_mode, approval_policy, minimum_capacity_threshold, reserved_capacity,
                maximum_tasks_per_batch, maximum_execution_minutes, idle_requirement, allowed_hours,
                max_usage_age_minutes, interval_normal_minutes, interval_waiting_minutes, quiet_hours,
                notify_desktop, notify_telegram, telegram_chat_id, notify_include_names, working_root, results_dir, retention_days, default_run_mode,
                burn_session_hours, burn_weekly_hours
         FROM automation_settings WHERE id = 1",
        [],
        |r| {
            Ok(Settings {
                enabled: r.get(0)?,
                execution_mode: ExecutionPolicy::parse(&r.get::<_, String>(1)?)
                    .unwrap_or(ExecutionPolicy::Approval),
                approval_policy: r.get(2)?,
                minimum_capacity_threshold: r.get(3)?,
                reserved_capacity: r.get(4)?,
                maximum_tasks_per_batch: r.get(5)?,
                maximum_execution_minutes: r.get(6)?,
                idle_requirement: r.get(7)?,
                allowed_hours: r.get(8)?,
                max_usage_age_minutes: r.get(9)?,
                interval_normal_minutes: r.get(10)?,
                interval_waiting_minutes: r.get(11)?,
                quiet_hours: r.get(12)?,
                notify_desktop: r.get(13)?,
                notify_telegram: r.get(14)?,
                telegram_chat_id: r.get(15)?,
                notify_include_names: r.get(16)?,
                working_root: r.get(17)?,
                results_dir: r.get(18)?,
                retention_days: r.get(19)?,
                default_run_mode: RunMode::parse(&r.get::<_, String>(20)?).unwrap_or_default(),
                burn_session_hours: r.get(21)?,
                burn_weekly_hours: r.get(22)?,
            })
        },
    )?)
}

pub fn save(conn: &Connection, s: &Settings) -> Result<()> {
    s.validate()?;
    conn.execute(
        "UPDATE automation_settings SET enabled=?1, execution_mode=?2, approval_policy=?3,
            minimum_capacity_threshold=?4, reserved_capacity=?5, maximum_tasks_per_batch=?6,
            maximum_execution_minutes=?7, idle_requirement=?8, allowed_hours=?9, max_usage_age_minutes=?10,
            interval_normal_minutes=?11, interval_waiting_minutes=?12, quiet_hours=?13, notify_desktop=?14,
            notify_telegram=?15, telegram_chat_id=?16, notify_include_names=?17, working_root=?18,
            results_dir=?19, retention_days=?20, default_run_mode=?22, burn_session_hours=?23,
            burn_weekly_hours=?24, updated_at=?21 WHERE id = 1",
        params![
            s.enabled,
            s.execution_mode.as_str(),
            s.approval_policy,
            s.minimum_capacity_threshold,
            s.reserved_capacity,
            s.maximum_tasks_per_batch,
            s.maximum_execution_minutes,
            s.idle_requirement,
            s.allowed_hours,
            s.max_usage_age_minutes,
            s.interval_normal_minutes,
            s.interval_waiting_minutes,
            s.quiet_hours,
            s.notify_desktop,
            s.notify_telegram,
            s.telegram_chat_id,
            s.notify_include_names,
            s.working_root,
            s.results_dir,
            s.retention_days,
            db::ts(Utc::now()),
            s.default_run_mode.as_str(),
            s.burn_session_hours,
            s.burn_weekly_hours,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_validated() {
        let c = db::open_memory().unwrap();
        let mut s = load(&c).unwrap();
        assert_eq!(s.retention_days, 90);
        for bad in [1, 6, 3651] {
            s.retention_days = bad;
            assert!(save(&c, &s).is_err(), "{bad}");
        }
        for ok in [0, 7, 365] {
            s.retention_days = ok;
            save(&c, &s).unwrap();
            assert_eq!(load(&c).unwrap().retention_days, ok);
        }
    }

    #[test]
    fn defaults_are_safe() {
        let c = db::open_memory().unwrap();
        let s = load(&c).unwrap();
        assert!(!s.enabled);
        assert_eq!(s.execution_mode, ExecutionPolicy::Approval);
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn settings_round_trip_and_validate() {
        let c = db::open_memory().unwrap();
        let mut s = load(&c).unwrap();
        s.enabled = true;
        s.reserved_capacity = 30.0;
        s.allowed_hours = "22:00-07:00".into();
        save(&c, &s).unwrap();
        assert_eq!(load(&c).unwrap(), s);
        s.reserved_capacity = 130.0;
        assert!(save(&c, &s).is_err());
    }

    #[test]
    fn burn_hours_round_trip_and_validate() {
        let c = db::open_memory().unwrap();
        let mut s = load(&c).unwrap();
        assert_eq!((s.burn_session_hours, s.burn_weekly_hours), (None, None));
        s.burn_session_hours = Some(2.0);
        s.burn_weekly_hours = Some(24.0);
        save(&c, &s).unwrap();
        assert_eq!(load(&c).unwrap(), s);
        s.burn_session_hours = Some(6.0); // longer than the 5 h window
        assert!(save(&c, &s).is_err());
        s.burn_session_hours = Some(-1.0);
        assert!(save(&c, &s).is_err());
        s.burn_session_hours = None;
        s.burn_weekly_hours = Some(200.0);
        assert!(save(&c, &s).is_err());
    }

    #[test]
    fn working_root_must_exist_when_set() {
        let mut s = Settings::default();
        assert!(s.validate_working_root().is_ok(), "empty is allowed");
        s.working_root = std::env::temp_dir().to_string_lossy().into_owned();
        assert!(s.validate_working_root().is_ok());
        s.working_root = "/definitely/not/here".into();
        let e = s.validate_working_root().unwrap_err().to_string();
        assert!(
            e.contains("Working root") && e.contains("/definitely/not/here"),
            "{e}"
        );
        s.working_root = "/etc/hostname".into();
        if std::path::Path::new("/etc/hostname").is_file() {
            assert!(s.validate_working_root().is_err());
        }
        // saving (e.g. switching automation off) never depends on the folder existing
        let c = db::open_memory().unwrap();
        s.working_root = "/definitely/not/here".into();
        assert!(save(&c, &s).is_ok());
    }

    #[test]
    fn validation_messages_use_ui_labels_and_echo_the_value() {
        let msg = |f: &dyn Fn(&mut Settings)| {
            let mut s = Settings::default();
            f(&mut s);
            s.validate().unwrap_err().to_string()
        };
        let m = msg(&|s| s.reserved_capacity = 130.0);
        assert!(
            m.contains("Reserved capacity (%)") && m.contains("130"),
            "{m}"
        );
        assert!(!m.contains('_'), "{m}");
        let m = msg(&|s| s.interval_waiting_minutes = 0);
        assert!(m.contains("Check interval, waiting"), "{m}");
        let m = msg(&|s| s.quiet_hours = "late".into());
        assert!(m.contains("Quiet hours") && m.contains("late"), "{m}");
        let m = msg(&|s| s.retention_days = 3);
        assert!(m.contains("Keep history") && m.contains('3'), "{m}");
    }

    #[test]
    fn windows_wrap_midnight() {
        assert!(in_window("22:00-07:00", 23 * 60));
        assert!(in_window("22:00-07:00", 3 * 60));
        assert!(!in_window("22:00-07:00", 12 * 60));
        assert!(in_window("", 5));
        assert!(!in_window("garbage", 5));
    }
}
