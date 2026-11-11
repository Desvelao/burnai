//! Decides when and what to run. Conditions are combined with AND; an unset condition is ignored.
//! Unknown idle time never satisfies an idle requirement.
use crate::engine::{effective_policy, preflight, Block, Preflight};
use crate::models::{Account, BillingMode, ExecutionPolicy};
use crate::provider::Detection;
use crate::settings::{in_window, Settings};
use crate::tasks::{self, Task};
use crate::{usage, Result};
use chrono::{DateTime, Timelike, Utc};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

pub trait IdleSource {
    /// Minutes since the last user input, or `None` if it cannot be determined.
    fn idle_minutes(&self) -> Option<u32>;
}

pub struct NoIdle;
impl IdleSource for NoIdle {
    fn idle_minutes(&self) -> Option<u32> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wait {
    Disabled,
    Halted,
    Busy,
    OutsideAllowedHours,
    NotIdle,
    IdleUnknown,
}

impl std::fmt::Display for Wait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Wait::Disabled => "automation disabled",
            Wait::Halted => "automation paused or stopped",
            Wait::Busy => "a task is already running",
            Wait::OutsideAllowedHours => "outside allowed hours",
            Wait::NotIdle => "computer not idle long enough",
            Wait::IdleUnknown => "idle time unknown",
        })
    }
}

pub struct Gate {
    pub now: DateTime<Utc>,
    pub local_minute: u32,
    pub idle_minutes: Option<u32>,
    pub launches_allowed: bool,
    pub executing: bool,
}

pub fn local_minute_of_day(t: chrono::DateTime<chrono::Local>) -> u32 {
    t.hour() * 60 + t.minute()
}

/// Global conditions (hours, idle, state). Per-account capacity is checked separately.
pub fn gate(s: &Settings, g: &Gate) -> std::result::Result<(), Wait> {
    state_gate(s, g)?;
    schedule_gate(s, g.idle_minutes, g.local_minute)
}

/// Run state only: enabled, not paused/stopped, nothing already running.
pub fn state_gate(s: &Settings, g: &Gate) -> std::result::Result<(), Wait> {
    if !s.enabled {
        return Err(Wait::Disabled);
    }
    if !g.launches_allowed {
        return Err(Wait::Halted);
    }
    if g.executing {
        return Err(Wait::Busy);
    }
    Ok(())
}

/// Allowed hours and idle requirement. Evaluated per account with its effective settings, so an account
/// may relax or tighten the general values. Unknown idle time never satisfies an idle requirement.
pub fn schedule_gate(
    s: &Settings,
    idle_minutes: Option<u32>,
    local_minute: u32,
) -> std::result::Result<(), Wait> {
    if !in_window(&s.allowed_hours, local_minute) {
        return Err(Wait::OutsideAllowedHours);
    }
    if s.idle_requirement > 0 {
        match idle_minutes {
            None => return Err(Wait::IdleUnknown),
            Some(m) if m < s.idle_requirement => return Err(Wait::NotIdle),
            _ => {}
        }
    }
    Ok(())
}

/// Task-level schedule: None = anytime; "after:<RFC3339>" = not before; "daily:HH:MM" = not before that
/// local time of day. Despite the name `daily:` is a gate only, NOT a recurrence: a task runs once like any
/// other and is not re-queued (re-add or retry it to run it again).
pub fn task_due(t: &Task, now: DateTime<Utc>, local_minute: u32) -> bool {
    match parse_schedule(t.schedule.as_deref()) {
        Ok(Schedule::Anytime) => true,
        Ok(Schedule::After(x)) => now >= x,
        Ok(Schedule::Daily(m)) => local_minute >= m,
        // Unparsable schedules fail closed: never run a task whose timing we cannot understand.
        Err(_) => false,
    }
}

enum Schedule {
    Anytime,
    After(DateTime<Utc>),
    Daily(u32),
}

fn parse_schedule(s: Option<&str>) -> std::result::Result<Schedule, String> {
    match s {
        None | Some("") => Ok(Schedule::Anytime),
        Some(s) => {
            if let Some(ts) = s.strip_prefix("after:") {
                crate::db::parse_ts(ts)
                    .map(Schedule::After)
                    .ok_or_else(|| format!("schedule \"{s}\": expected after:<RFC3339 time>, e.g. after:2026-01-31T09:30:00Z"))
            } else if let Some(hm) = s.strip_prefix("daily:") {
                crate::settings::parse_hours(&format!("{hm}-{hm}"))
                    .map(|(m, _)| Schedule::Daily(m))
                    .ok_or_else(|| {
                        format!("schedule \"{s}\": expected daily:HH:MM (24 h), e.g. daily:09:30")
                    })
            } else {
                Err(format!(
                    "schedule \"{s}\": must be empty, after:<RFC3339 time> or daily:HH:MM"
                ))
            }
        }
    }
}

/// Rejects schedules `task_due` could not interpret (they would never run). Empty/None is valid.
pub fn validate_schedule(s: Option<&str>) -> crate::Result<()> {
    parse_schedule(s).map(|_| ()).map_err(crate::Error::Invalid)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Proposal {
    pub account_id: i64,
    pub tasks: Vec<Task>,
    pub est_consumption: f64,
    pub est_minutes: i64,
    /// True if no approval is needed (Automatic policy, enabled, no task requires approval).
    pub auto: bool,
    /// "Burn now": run with thresholds and reserve ignored (never staleness, billing or the task's
    /// real capacity need). Set only by an explicit user request.
    #[serde(default)]
    pub burn: bool,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub proposals: Vec<Proposal>,
    pub blocked: Vec<(i64, Vec<Block>)>,
    /// Accounts skipped this pass by their own hours/idle conditions.
    pub held: Vec<(i64, Wait)>,
}

fn eligible(t: &Task, a: &Account, allow_fallback: bool) -> bool {
    match (&t.provider_type, t.account_id) {
        (_, Some(id)) if id == a.id => true,
        (Some(p), Some(_)) => allow_fallback && t.allow_fallback && *p == a.provider_type,
        (Some(p), None) => *p == a.provider_type,
        (None, _) => true,
    }
}

/// Percent consumed on the account since the snapshot was taken (estimates stand in for unmeasured runs).
pub fn consumed_since(conn: &Connection, account_id: i64, since: DateTime<Utc>) -> Result<f64> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(COALESCE(actual_consumption, estimated_consumption, 0)), 0) FROM executions
         WHERE account_id=?1 AND started_at >= ?2 AND status IN ('running','completed','failed','timed_out','cancelled','rate_limited','interrupted')",
        rusqlite::params![account_id, crate::db::ts(since)],
        |r| r.get(0),
    )?)
}

/// Builds one batch per account from ready tasks. Pure planning: nothing is written to the database.
/// Only accounts with `BillingMode::Subscription` detections are considered.
pub fn plan(
    conn: &Connection,
    settings: &Settings,
    accounts: &[Account],
    detections: &HashMap<i64, Detection>,
    now: DateTime<Utc>,
    clock: crate::availability::LocalClock,
    launches_allowed: bool,
) -> Result<Plan> {
    plan_scheduled(
        conn,
        settings,
        accounts,
        detections,
        now,
        clock,
        launches_allowed,
        None,
    )
}

/// Like `plan`, but with `idle = Some(idle minutes)` each account's own allowed hours and idle
/// requirement are enforced too (the automatic scheduler). `None` skips them (user-initiated runs).
#[allow(clippy::too_many_arguments)]
pub fn plan_scheduled(
    conn: &Connection,
    settings: &Settings,
    accounts: &[Account],
    detections: &HashMap<i64, Detection>,
    now: DateTime<Utc>,
    clock: crate::availability::LocalClock,
    launches_allowed: bool,
    idle: Option<Option<u32>>,
) -> Result<Plan> {
    let local_minute = clock.minute;
    let mut out = Plan::default();
    let ready: Vec<Task> = tasks::ready(conn, now)?
        .into_iter()
        .filter(|t| task_due(t, now, local_minute))
        .collect();
    let mut assigned: HashSet<i64> = HashSet::new();
    let mut by_account: HashMap<i64, Vec<Task>> = HashMap::new();
    let mut blocked: HashMap<i64, Vec<Block>> = HashMap::new();

    for allow_fallback in [false, true] {
        for acct in accounts {
            if effective_policy(settings, acct) == ExecutionPolicy::Manual
                || acct.availability(clock).is_err()
            {
                continue;
            }
            // Per-account effective settings: overrides + "ignore limits".
            let eff = settings.effective_for(acct);
            if let Some(idle_minutes) = idle {
                if let Err(w) = schedule_gate(&eff, idle_minutes, local_minute) {
                    if !allow_fallback {
                        out.held.push((acct.id, w));
                    }
                    continue;
                }
            }
            let Some(det) = detections.get(&acct.id) else {
                continue;
            };
            let snap = usage::latest(conn, acct.id)?;
            let base_consumed = match &snap {
                Some(s) => consumed_since(conn, acct.id, s.captured_at)?,
                None => 0.0,
            };
            for t in &ready {
                if assigned.contains(&t.id) || !eligible(t, acct, allow_fallback) {
                    continue;
                }
                let batch = by_account.entry(acct.id).or_default();
                if batch.len() as u32 >= eff.maximum_tasks_per_batch {
                    continue;
                }
                let planned: f64 = batch.iter().map(|b| b.estimated_consumption).sum();
                let minutes: i64 = batch.iter().map(|b| b.estimated_minutes).sum();
                if minutes + t.estimated_minutes > eff.maximum_execution_minutes as i64
                    && !batch.is_empty()
                {
                    continue;
                }
                let blocks: Vec<Block> = preflight(&Preflight {
                    settings: &eff,
                    account: acct,
                    task: t,
                    snapshot: snap.as_ref(),
                    billing: det.billing,
                    billing_reasons: det.reasons.clone(),
                    cli_found: det.cli_found,
                    launches_allowed,
                    automatic: true,
                    approved: true, // approval is requested separately; don't report it as a block here
                    now,
                    clock,
                    consumed_since_snapshot: base_consumed + planned,
                })
                .into_iter()
                .filter(|b| *b != Block::AutomationDisabled)
                .collect();
                if blocks.is_empty() {
                    assigned.insert(t.id);
                    blocked.remove(&t.id);
                    by_account.get_mut(&acct.id).unwrap().push(t.clone());
                } else {
                    blocked.entry(t.id).or_insert(blocks);
                }
            }
        }
    }

    let mut ids: Vec<i64> = by_account.keys().copied().collect();
    ids.sort();
    for id in ids {
        let ts = by_account.remove(&id).unwrap();
        if ts.is_empty() {
            continue;
        }
        let acct = accounts.iter().find(|a| a.id == id).unwrap();
        let auto = settings.enabled
            && effective_policy(settings, acct) == ExecutionPolicy::Automatic
            && ts.iter().all(|t| !t.requires_approval)
            && detections
                .get(&id)
                .is_some_and(|d| d.billing == BillingMode::Subscription);
        out.proposals.push(Proposal {
            account_id: id,
            est_consumption: ts.iter().map(|t| t.estimated_consumption).sum(),
            est_minutes: ts.iter().map(|t| t.estimated_minutes).sum(),
            tasks: ts,
            auto,
            burn: false,
        });
    }
    out.blocked = blocked
        .into_iter()
        .filter(|(id, _)| !assigned.contains(id))
        .collect();
    out.blocked.sort_by_key(|(id, _)| *id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Confidence, TaskStatus};
    use crate::tasks::NewTask;
    use crate::usage::NewSnapshot;
    use crate::{accounts, db};
    use chrono::Duration;

    fn add_usage(c: &Connection, acct: i64, sess: f64, week: f64, now: DateTime<Utc>) {
        usage::record(
            c,
            &NewSnapshot {
                account_id: acct,
                session_remaining: Some(sess),
                weekly_remaining: Some(week),
                session_reset_at: Some(now + Duration::hours(2)),
                weekly_reset_at: Some(now + Duration::days(2)),
                confidence: Confidence::Manual,
                source: "manual".into(),
            },
            now - Duration::minutes(1),
        )
        .unwrap();
    }
    fn det(b: BillingMode) -> Detection {
        Detection {
            cli_found: true,
            billing: b,
            reasons: vec![],
        }
    }
    fn nt(name: &str) -> NewTask {
        NewTask {
            name: name.into(),
            prompt: "p".into(),
            estimated_consumption: 5.0,
            estimated_minutes: 10,
            ..Default::default()
        }
    }
    fn settings() -> Settings {
        Settings {
            enabled: true,
            ..Settings::default()
        }
    }

    #[test]
    fn gate_conditions_combine() {
        let now = Utc::now();
        let mut s = settings();
        s.allowed_hours = "22:00-07:00".into();
        s.idle_requirement = 10;
        let g = |lm, idle| Gate {
            now,
            local_minute: lm,
            idle_minutes: idle,
            launches_allowed: true,
            executing: false,
        };
        assert_eq!(
            gate(&s, &g(12 * 60, Some(30))),
            Err(Wait::OutsideAllowedHours)
        );
        assert_eq!(gate(&s, &g(23 * 60, Some(3))), Err(Wait::NotIdle));
        assert_eq!(gate(&s, &g(23 * 60, None)), Err(Wait::IdleUnknown));
        assert_eq!(gate(&s, &g(23 * 60, Some(30))), Ok(()));
        s.enabled = false;
        assert_eq!(gate(&s, &g(23 * 60, Some(30))), Err(Wait::Disabled));
    }

    #[test]
    fn gate_blocks_when_busy_or_halted() {
        let s = settings();
        let now = Utc::now();
        let mk = |h, e| Gate {
            now,
            local_minute: 0,
            idle_minutes: None,
            launches_allowed: h,
            executing: e,
        };
        assert_eq!(gate(&s, &mk(false, false)), Err(Wait::Halted));
        assert_eq!(gate(&s, &mk(true, true)), Err(Wait::Busy));
    }

    #[test]
    fn task_schedules() {
        let c = db::open_memory().unwrap();
        let now = Utc::now();
        let mut t = tasks::create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        assert!(task_due(&t, now, 0));
        t.schedule = Some(format!("after:{}", db::ts(now + Duration::hours(1))));
        assert!(!task_due(&t, now, 0));
        assert!(task_due(&t, now + Duration::hours(2), 0));
        t.schedule = Some("daily:09:30".into());
        assert!(!task_due(&t, now, 9 * 60));
        assert!(task_due(&t, now, 9 * 60 + 30));
        // A time-of-day gate, not a recurrence: stays due for the rest of the day (and any later day)
        // until the task has run once; nothing here re-queues it.
        assert!(task_due(&t, now, 23 * 60 + 59));
        // Malformed values fail closed (not due) and are rejected by validate_schedule.
        for bad in [
            "after:2026-13-01",
            "daily:9.30",
            "daily:25:00",
            "daily:",
            "after:",
            "weekly:1",
            "DAILY:09:30",
        ] {
            t.schedule = Some(bad.into());
            assert!(!task_due(&t, now, 23 * 60 + 59), "{bad}");
            assert!(validate_schedule(Some(bad)).is_err(), "{bad}");
        }
        for ok in [
            None,
            Some(""),
            Some("daily:09:30"),
            Some("after:2026-01-31T09:30:00Z"),
        ] {
            assert!(validate_schedule(ok).is_ok());
        }
    }

    #[test]
    fn unavailable_account_is_never_planned() {
        let c = db::open_memory().unwrap();
        let a = accounts::create(&c, "claude", "A", "Max", None).unwrap();
        let now = Utc::now();
        add_usage(&c, a.id, 95.0, 95.0, now);
        tasks::create(&c, &nt("t"), TaskStatus::Pending).unwrap();
        let dets = HashMap::from([(a.id, det(BillingMode::Subscription))]);
        let clock = crate::availability::LocalClock::new(2, 600);
        let n = |a: &Account| {
            plan(
                &c,
                &settings(),
                std::slice::from_ref(a),
                &dets,
                now,
                clock,
                true,
            )
            .unwrap()
            .proposals
            .len()
        };
        assert_eq!(n(&a), 1);
        accounts::set_available(&c, a.id, false).unwrap();
        assert_eq!(n(&accounts::get(&c, a.id).unwrap()), 0);
        accounts::set_available(&c, a.id, true).unwrap();
        let mut b = accounts::get(&c, a.id).unwrap();
        b.availability_schedule = "sat,sun 00:00-00:00".into();
        assert_eq!(n(&b), 0, "Wednesday is outside a weekend-only schedule");
        b.availability_schedule = "wed 09:00-11:00".into();
        assert_eq!(n(&b), 1);
    }

    #[test]
    fn per_account_idle_hours_and_override() {
        let c = db::open_memory().unwrap();
        let a = accounts::create(&c, "claude", "A", "Max", None).unwrap();
        let b = accounts::create(&c, "claude", "B", "Max", None).unwrap();
        let now = Utc::now();
        add_usage(&c, a.id, 95.0, 95.0, now);
        add_usage(&c, b.id, 95.0, 95.0, now);
        tasks::create(&c, &nt("t"), TaskStatus::Pending).unwrap();
        let mut st = settings();
        st.idle_requirement = 10;
        let dets = HashMap::from([
            (a.id, det(BillingMode::Subscription)),
            (b.id, det(BillingMode::Subscription)),
        ]);
        let run = |accts: &[Account], idle: Option<u32>| {
            plan_scheduled(
                &c,
                &st,
                accts,
                &dets,
                now,
                Default::default(),
                true,
                Some(idle),
            )
            .unwrap()
        };
        let (a, mut b) = (
            accounts::get(&c, a.id).unwrap(),
            accounts::get(&c, b.id).unwrap(),
        );
        // general idle requirement applies, and unknown idle never satisfies it
        assert!(run(std::slice::from_ref(&a), Some(1)).proposals.is_empty());
        assert!(run(std::slice::from_ref(&a), None).proposals.is_empty());
        assert_eq!(
            run(std::slice::from_ref(&a), Some(1)).held[0].1,
            Wait::NotIdle
        );
        // account B runs on idle 0 even though the general setting requires 10 minutes
        b.limits.idle_requirement = Some(0);
        let p = run(&[a.clone(), b.clone()], None);
        assert_eq!(p.proposals.len(), 1);
        assert_eq!(p.proposals[0].account_id, b.id);
        // ignore_limits drops idle and allowed hours too
        let mut a2 = a.clone();
        a2.limits.allowed_hours = Some("03:00-03:01".into());
        assert!(run(&[a2.clone()], Some(60)).proposals.is_empty());
        a2.limits.ignore_limits = true;
        assert_eq!(run(&[a2], None).proposals.len(), 1);
    }

    #[test]
    fn plans_batch_with_capacity_and_limits() {
        let c = db::open_memory().unwrap();
        let a = accounts::create(&c, "claude", "A", "Max", None).unwrap();
        let now = Utc::now();
        add_usage(&c, a.id, 95.0, 95.0, now);
        for i in 0..5 {
            tasks::create(&c, &nt(&format!("t{i}")), TaskStatus::Pending).unwrap();
        }
        let dets = HashMap::from([(a.id, det(BillingMode::Subscription))]);
        let p = plan(&c, &settings(), &[a], &dets, now, Default::default(), true).unwrap();
        assert_eq!(p.proposals.len(), 1);
        assert_eq!(p.proposals[0].tasks.len(), 3, "max tasks per batch");
        assert!(!p.proposals[0].auto, "approval is the default policy");
    }

    #[test]
    fn capacity_limits_batch_size() {
        let c = db::open_memory().unwrap();
        let a = accounts::create(&c, "claude", "A", "Max", None).unwrap();
        let now = Utc::now();
        add_usage(&c, a.id, 70.0, 95.0, now); // 70 - 20 reserve = 50 usable
        for i in 0..3 {
            tasks::create(
                &c,
                &NewTask {
                    estimated_consumption: 30.0,
                    ..nt(&format!("t{i}"))
                },
                TaskStatus::Pending,
            )
            .unwrap();
        }
        let dets = HashMap::from([(a.id, det(BillingMode::Subscription))]);
        let p = plan(&c, &settings(), &[a], &dets, now, Default::default(), true).unwrap();
        assert_eq!(p.proposals[0].tasks.len(), 1);
        assert_eq!(p.blocked.len(), 2);
    }

    #[test]
    fn no_plan_without_verified_subscription_or_fresh_usage() {
        let c = db::open_memory().unwrap();
        let a = accounts::create(&c, "claude", "A", "Max", None).unwrap();
        let now = Utc::now();
        tasks::create(&c, &nt("t"), TaskStatus::Pending).unwrap();
        let sub = HashMap::from([(a.id, det(BillingMode::Subscription))]);
        assert!(
            plan(
                &c,
                &settings(),
                std::slice::from_ref(&a),
                &sub,
                now,
                Default::default(),
                true
            )
            .unwrap()
            .proposals
            .is_empty(),
            "no usage data"
        );
        add_usage(&c, a.id, 95.0, 95.0, now);
        let api = HashMap::from([(a.id, det(BillingMode::Api))]);
        assert!(plan(
            &c,
            &settings(),
            std::slice::from_ref(&a),
            &api,
            now,
            Default::default(),
            true
        )
        .unwrap()
        .proposals
        .is_empty());
        let unv = HashMap::from([(a.id, det(BillingMode::Unverified))]);
        assert!(plan(
            &c,
            &settings(),
            std::slice::from_ref(&a),
            &unv,
            now,
            Default::default(),
            true
        )
        .unwrap()
        .proposals
        .is_empty());
        assert_eq!(
            plan(&c, &settings(), &[a], &sub, now, Default::default(), true)
                .unwrap()
                .proposals
                .len(),
            1
        );
    }

    #[test]
    fn pinned_and_any_account_targeting() {
        let c = db::open_memory().unwrap();
        let a = accounts::create(&c, "claude", "A", "Max", None).unwrap();
        let b = accounts::create(&c, "claude", "B", "Pro", None).unwrap();
        let now = Utc::now();
        add_usage(&c, a.id, 30.0, 95.0, now); // A is below threshold
        add_usage(&c, b.id, 95.0, 95.0, now);
        let pinned_a = tasks::create(
            &c,
            &NewTask {
                account_id: Some(a.id),
                ..nt("pinned-a")
            },
            TaskStatus::Pending,
        )
        .unwrap();
        let any = tasks::create(&c, &nt("any"), TaskStatus::Pending).unwrap();
        let fb = tasks::create(
            &c,
            &NewTask {
                account_id: Some(a.id),
                allow_fallback: true,
                ..nt("fallback")
            },
            TaskStatus::Pending,
        )
        .unwrap();
        let dets = HashMap::from([
            (a.id, det(BillingMode::Subscription)),
            (b.id, det(BillingMode::Subscription)),
        ]);
        let p = plan(
            &c,
            &settings(),
            &[a, b.clone()],
            &dets,
            now,
            Default::default(),
            true,
        )
        .unwrap();
        assert_eq!(p.proposals.len(), 1);
        assert_eq!(p.proposals[0].account_id, b.id);
        let names: Vec<_> = p.proposals[0].tasks.iter().map(|t| t.id).collect();
        assert!(names.contains(&any.id));
        assert!(names.contains(&fb.id), "fallback allowed");
        assert!(
            !names.contains(&pinned_a.id),
            "pinned without fallback never moves"
        );
    }

    #[test]
    fn automatic_policy_skips_approval_only_when_allowed() {
        let c = db::open_memory().unwrap();
        let a = accounts::create(&c, "claude", "A", "Max", None).unwrap();
        accounts::set_policy(&c, a.id, ExecutionPolicy::Automatic).unwrap();
        let a = accounts::get(&c, a.id).unwrap();
        let now = Utc::now();
        add_usage(&c, a.id, 95.0, 95.0, now);
        tasks::create(
            &c,
            &NewTask {
                requires_approval: false,
                ..nt("t")
            },
            TaskStatus::Pending,
        )
        .unwrap();
        let dets = HashMap::from([(a.id, det(BillingMode::Subscription))]);
        let mut s = settings();
        s.execution_mode = ExecutionPolicy::Automatic;
        assert!(
            plan(
                &c,
                &s,
                std::slice::from_ref(&a),
                &dets,
                now,
                Default::default(),
                true
            )
            .unwrap()
            .proposals[0]
                .auto
        );
        s.enabled = false;
        assert!(
            !plan(&c, &s, &[a], &dets, now, Default::default(), true)
                .unwrap()
                .proposals[0]
                .auto
        );
    }

    #[test]
    fn manual_policy_never_planned() {
        let c = db::open_memory().unwrap();
        let a = accounts::create(&c, "claude", "A", "Max", None).unwrap();
        accounts::set_policy(&c, a.id, ExecutionPolicy::Manual).unwrap();
        let a = accounts::get(&c, a.id).unwrap();
        let now = Utc::now();
        add_usage(&c, a.id, 95.0, 95.0, now);
        tasks::create(&c, &nt("t"), TaskStatus::Pending).unwrap();
        let dets = HashMap::from([(a.id, det(BillingMode::Subscription))]);
        assert!(
            plan(&c, &settings(), &[a], &dets, now, Default::default(), true)
                .unwrap()
                .proposals
                .is_empty()
        );
    }
}
