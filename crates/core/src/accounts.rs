use crate::models::*;
use crate::{db, Error, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

const SELECT: &str =
    "SELECT a.id, a.provider_id, p.provider_type, a.label, a.subscription_plan, a.billing_mode,
        a.connection_status, a.config_dir, a.subscription_confirmed, a.execution_policy,
        a.credentials_file, a.usage_source, a.user_agent,
        a.custom_config, a.custom_status, a.custom_status_at, a.cli_command,
        a.available, a.availability_schedule, a.run_mode,
        a.lim_start_threshold, a.lim_reserved_capacity, a.lim_idle_requirement, a.lim_allowed_hours,
        a.lim_max_usage_age_minutes, a.lim_max_tasks_per_batch, a.lim_max_execution_minutes,
        a.lim_session_threshold, a.lim_session_reserved, a.lim_ignore_session, a.lim_ignore_limits,
        a.lim_burn_session_hours, a.lim_burn_weekly_hours
    FROM accounts a JOIN providers p ON p.id = a.provider_id";

fn map(r: &rusqlite::Row) -> rusqlite::Result<Account> {
    Ok(Account {
        id: r.get(0)?,
        provider_id: r.get(1)?,
        provider_type: r.get(2)?,
        label: r.get(3)?,
        subscription_plan: r.get(4)?,
        billing_mode: BillingMode::parse(&r.get::<_, String>(5)?)
            .unwrap_or(BillingMode::Unverified),
        connection_status: ConnectionStatus::parse(&r.get::<_, String>(6)?)
            .unwrap_or(ConnectionStatus::Disconnected),
        config_dir: r.get(7)?,
        subscription_confirmed: r.get(8)?,
        execution_policy: ExecutionPolicy::parse(&r.get::<_, String>(9)?)
            .unwrap_or(ExecutionPolicy::Approval),
        credentials_file: r.get(10)?,
        usage_source: UsageSource::parse(&r.get::<_, String>(11)?).unwrap_or(UsageSource::Manual),
        user_agent: r.get(12)?,
        custom: r
            .get::<_, Option<String>>(13)?
            .and_then(|j| serde_json::from_str(&j).ok()),
        custom_status: match (
            r.get::<_, Option<String>>(14)?
                .and_then(|j| serde_json::from_str(&j).ok()),
            r.get::<_, Option<String>>(15)?
                .and_then(|t| db::parse_ts(&t)),
        ) {
            (Some(s), Some(t)) => Some((s, t)),
            _ => None,
        },
        cli_command: r.get(16)?,
        available: r.get(17)?,
        availability_schedule: r.get(18)?,
        run_mode: RunMode::parse(&r.get::<_, String>(19)?).unwrap_or_default(),
        limits: AccountLimits {
            start_threshold: r.get(20)?,
            reserved_capacity: r.get(21)?,
            idle_requirement: r.get(22)?,
            allowed_hours: r.get(23)?,
            max_usage_age_minutes: r.get(24)?,
            max_tasks_per_batch: r.get(25)?,
            max_execution_minutes: r.get(26)?,
            session_threshold: r.get(27)?,
            session_reserved: r.get(28)?,
            ignore_session: r.get(29)?,
            ignore_limits: r.get(30)?,
            burn_session_hours: r.get(31)?,
            burn_weekly_hours: r.get(32)?,
        },
    })
}

pub fn ensure_provider(conn: &Connection, provider_type: &str, display_name: &str) -> Result<i64> {
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM providers WHERE provider_type=?1",
            [provider_type],
            |r| r.get(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    let now = db::ts(Utc::now());
    conn.execute(
        "INSERT INTO providers (provider_type, display_name, created_at, updated_at) VALUES (?1,?2,?3,?3)",
        params![provider_type, display_name, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_providers(conn: &Connection) -> Result<Vec<Provider>> {
    let mut st =
        conn.prepare("SELECT id, provider_type, display_name FROM providers ORDER BY id")?;
    let v = st
        .query_map([], |r| {
            Ok(Provider {
                id: r.get(0)?,
                provider_type: r.get(1)?,
                display_name: r.get(2)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

pub fn create(
    conn: &Connection,
    provider_type: &str,
    label: &str,
    plan: &str,
    config_dir: Option<&str>,
) -> Result<Account> {
    let label = validate_label(label)?;
    let pid = ensure_provider(conn, provider_type, provider_type)?;
    let now = db::ts(Utc::now());
    conn.execute(
        "INSERT INTO accounts (provider_id, label, subscription_plan, config_dir, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?5)",
        params![pid, label, plan, config_dir, now],
    )?;
    get(conn, conn.last_insert_rowid())
}

/// Editable account settings (everything except provider and safety state).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AccountUpdate {
    pub label: String,
    pub plan: String,
    pub config_dir: Option<String>,
    pub credentials_file: Option<String>,
    pub usage_source: UsageSource,
    pub user_agent: Option<String>,
    pub custom: Option<crate::custom::CustomConfig>,
    /// Command to launch the CLI: a bare name (PATH lookup) or an absolute path. `None` = default.
    pub cli_command: Option<String>,
    /// Weekly availability schedule (see `availability`); empty = always.
    #[serde(default)]
    pub availability_schedule: String,
}

fn clean(o: &Option<String>) -> Option<String> {
    o.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A CLI command is run without a shell, so it must be a single program: a bare name
/// (resolved via PATH) or an absolute path. No arguments, whitespace tricks or NULs.
fn validate_cli_command(c: &str) -> Result<()> {
    if c.chars().any(|ch| ch == '\0' || ch.is_control()) {
        return Err(Error::Invalid(
            "CLI command must not contain control characters".into(),
        ));
    }
    let p = std::path::Path::new(c);
    let bare = p.components().count() == 1 && !c.contains(['/', '\\']);
    if bare && c.contains(char::is_whitespace) {
        return Err(Error::Invalid(
            "CLI command takes no arguments; use a wrapper script instead".into(),
        ));
    }
    if !(bare || p.is_absolute()) {
        return Err(Error::Invalid(
            "CLI command must be a bare program name or an absolute path".into(),
        ));
    }
    if c.ends_with(['/', '\\']) {
        return Err(Error::Invalid("CLI command must name a program".into()));
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<String> {
    let label = label.trim();
    if label.is_empty() || label.len() > 64 {
        return Err(Error::Invalid(
            "account label must be 1-64 characters".into(),
        ));
    }
    Ok(label.to_string())
}

/// Updates an account. Changing where credentials/profile come from invalidates the
/// subscription confirmation and billing verdict, so the user must re-confirm.
pub fn update(conn: &Connection, id: i64, u: &AccountUpdate) -> Result<Account> {
    let label = validate_label(&u.label)?;
    if u.user_agent
        .as_ref()
        .is_some_and(|s| s.contains(['\r', '\n']))
    {
        return Err(Error::Invalid("user agent must be a single line".into()));
    }
    let cur = get(conn, id)?;
    let (cfg, cred, ua) = (
        clean(&u.config_dir),
        clean(&u.credentials_file),
        clean(&u.user_agent),
    );
    if let Some(c) = &u.custom {
        c.validate_syntax()?;
    }
    let cli = clean(&u.cli_command);
    if let Some(c) = &cli {
        validate_cli_command(c)?;
    }
    let schedule = u.availability_schedule.trim().to_string();
    crate::availability::validate(&schedule).map_err(Error::Invalid)?;
    let identity_changed = cfg != cur.config_dir
        || cred != cur.credentials_file
        || u.custom != cur.custom
        || cli != cur.cli_command;
    let custom_json = u.custom.as_ref().map(serde_json::to_string).transpose()?;
    conn.execute(
        "UPDATE accounts SET label=?1, subscription_plan=?2, config_dir=?3, credentials_file=?4,
            usage_source=?5, user_agent=?6, updated_at=?7, custom_config=?9, cli_command=?10,
            availability_schedule=?11 WHERE id=?8",
        params![
            label,
            u.plan.trim(),
            cfg,
            cred,
            u.usage_source.as_str(),
            ua,
            db::ts(Utc::now()),
            id,
            custom_json,
            cli,
            schedule
        ],
    )?;
    if identity_changed {
        conn.execute(
            "UPDATE accounts SET subscription_confirmed=0, billing_mode='unverified',
                custom_status=NULL, custom_status_at=NULL WHERE id=?1",
            [id],
        )?;
    }
    get(conn, id)
}

pub fn get(conn: &Connection, id: i64) -> Result<Account> {
    conn.query_row(&format!("{SELECT} WHERE a.id=?1"), [id], map)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("account {id}")))
}

pub fn list(conn: &Connection) -> Result<Vec<Account>> {
    let mut st = conn.prepare(&format!("{SELECT} ORDER BY a.id"))?;
    let v = st
        .query_map([], map)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    let used: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE account_id=?1 AND status NOT IN ('completed','failed','cancelled','expired')",
        [id],
        |r| r.get(0),
    )?;
    if used > 0 {
        return Err(Error::Blocked(format!(
            "{used} open task(s) are pinned to this account"
        )));
    }
    conn.execute("UPDATE tasks SET account_id=NULL WHERE account_id=?1", [id])?;
    conn.execute("DELETE FROM usage_snapshots WHERE account_id=?1", [id])?;
    conn.execute(
        "UPDATE executions SET account_id=NULL WHERE account_id=?1",
        [id],
    )?;
    conn.execute("DELETE FROM accounts WHERE id=?1", [id])?;
    Ok(())
}

pub fn update_status(
    conn: &Connection,
    id: i64,
    billing: BillingMode,
    connection: ConnectionStatus,
) -> Result<()> {
    conn.execute(
        "UPDATE accounts SET billing_mode=?1, connection_status=?2, updated_at=?3 WHERE id=?4",
        params![
            billing.as_str(),
            connection.as_str(),
            db::ts(Utc::now()),
            id
        ],
    )?;
    Ok(())
}

/// Caches the last `status` result of a custom script.
pub fn set_custom_status(
    conn: &Connection,
    id: i64,
    s: &crate::custom::CustomStatus,
    now: chrono::DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "UPDATE accounts SET custom_status=?1, custom_status_at=?2 WHERE id=?3",
        params![serde_json::to_string(s)?, db::ts(now), id],
    )?;
    Ok(())
}

pub fn set_policy(conn: &Connection, id: i64, p: ExecutionPolicy) -> Result<()> {
    conn.execute(
        "UPDATE accounts SET execution_policy=?1, updated_at=?2 WHERE id=?3",
        params![p.as_str(), db::ts(Utc::now()), id],
    )?;
    Ok(())
}

/// Run-mode override for an account (`Inherit` = use the global default).
pub fn set_run_mode(conn: &Connection, id: i64, mode: RunMode) -> Result<()> {
    let n = conn.execute(
        "UPDATE accounts SET run_mode=?1, updated_at=?2 WHERE id=?3",
        params![mode.as_str(), db::ts(Utc::now()), id],
    )?;
    if n == 0 {
        return Err(Error::NotFound(format!("account {id}")));
    }
    Ok(())
}

/// On-demand availability switch (master gate; see `availability`).
pub fn set_available(conn: &Connection, id: i64, available: bool) -> Result<()> {
    let n = conn.execute(
        "UPDATE accounts SET available=?1, updated_at=?2 WHERE id=?3",
        params![available, db::ts(Utc::now()), id],
    )?;
    if n == 0 {
        return Err(Error::NotFound(format!("account {id}")));
    }
    Ok(())
}

/// Validates per-account overrides (same ranges as the general settings).
pub fn validate_limits(l: &AccountLimits) -> Result<()> {
    for (n, v) in [
        ("start threshold", l.start_threshold),
        ("reserved capacity", l.reserved_capacity),
        ("session start threshold", l.session_threshold),
        ("session reserved capacity", l.session_reserved),
    ] {
        if v.is_some_and(|v| !(0.0..=100.0).contains(&v)) {
            return Err(Error::Invalid(format!("{n} must be 0-100")));
        }
    }
    for (n, v) in [
        ("usage max age", l.max_usage_age_minutes),
        ("tasks per batch", l.max_tasks_per_batch),
        ("batch minutes", l.max_execution_minutes),
    ] {
        if v == Some(0) {
            return Err(Error::Invalid(format!("{n} must be > 0")));
        }
    }
    for (n, v, max) in [
        ("burn 5 h window", l.burn_session_hours, 5.0),
        ("burn weekly window", l.burn_weekly_hours, 168.0),
    ] {
        if v.is_some_and(|v| !(0.0..=max).contains(&v)) {
            return Err(Error::Invalid(format!("{n} hours must be 0-{max}")));
        }
    }
    if let Some(h) = l.allowed_hours.as_deref() {
        if !h.trim().is_empty() && crate::settings::parse_hours(h.trim()).is_none() {
            return Err(Error::Invalid(
                "allowed hours must look like 22:00-07:00".into(),
            ));
        }
    }
    Ok(())
}

/// Stores the per-account overrides (everything except `ignore_limits`, which has its own setter).
pub fn set_limits(conn: &Connection, id: i64, l: &AccountLimits) -> Result<()> {
    validate_limits(l)?;
    let hours = l.allowed_hours.as_ref().map(|h| h.trim().to_string());
    let n = conn.execute(
        "UPDATE accounts SET lim_start_threshold=?1, lim_reserved_capacity=?2, lim_idle_requirement=?3,
            lim_allowed_hours=?4, lim_max_usage_age_minutes=?5, lim_max_tasks_per_batch=?6,
            lim_max_execution_minutes=?7, lim_session_threshold=?8, lim_session_reserved=?9,
            lim_ignore_session=?10, lim_burn_session_hours=?13,
            lim_burn_weekly_hours=?14, updated_at=?11 WHERE id=?12",
        params![
            l.start_threshold,
            l.reserved_capacity,
            l.idle_requirement,
            hours,
            l.max_usage_age_minutes,
            l.max_tasks_per_batch,
            l.max_execution_minutes,
            l.session_threshold,
            l.session_reserved,
            l.ignore_session,
            db::ts(Utc::now()),
            id,
            l.burn_session_hours,
            l.burn_weekly_hours
        ],
    )?;
    if n == 0 {
        return Err(Error::NotFound(format!("account {id}")));
    }
    Ok(())
}

/// Persistent "eligible ignoring thresholds, reserve, idle and hours" switch.
pub fn set_ignore_limits(conn: &Connection, id: i64, on: bool) -> Result<()> {
    let n = conn.execute(
        "UPDATE accounts SET lim_ignore_limits=?1, updated_at=?2 WHERE id=?3",
        params![on, db::ts(Utc::now()), id],
    )?;
    if n == 0 {
        return Err(Error::NotFound(format!("account {id}")));
    }
    Ok(())
}

/// The user's explicit statement that this account is logged in with a subscription (not an API key).
pub fn set_subscription_confirmed(conn: &Connection, id: i64, v: bool) -> Result<()> {
    conn.execute(
        "UPDATE accounts SET subscription_confirmed=?1, updated_at=?2 WHERE id=?3",
        params![v, db::ts(Utc::now()), id],
    )?;
    Ok(())
}

pub fn set_plan(conn: &Connection, id: i64, plan: &str) -> Result<()> {
    conn.execute(
        "UPDATE accounts SET subscription_plan=?1, updated_at=?2 WHERE id=?3",
        params![plan, db::ts(Utc::now()), id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_accounts_per_provider() {
        let c = db::open_memory().unwrap();
        let a = create(&c, "claude", "Personal", "Max 5x", None).unwrap();
        let b = create(&c, "claude", "Work", "Pro", None).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(a.provider_id, b.provider_id);
        assert_eq!(a.billing_mode, BillingMode::Unverified);
        assert!(create(&c, "claude", "Work", "Pro", None).is_err());
        assert_eq!(list(&c).unwrap().len(), 2);
        assert_eq!(list_providers(&c).unwrap().len(), 1);
    }

    fn upd(a: &Account) -> AccountUpdate {
        AccountUpdate {
            label: a.label.clone(),
            plan: a.subscription_plan.clone(),
            config_dir: a.config_dir.clone(),
            credentials_file: a.credentials_file.clone(),
            usage_source: a.usage_source,
            user_agent: a.user_agent.clone(),
            custom: a.custom.clone(),
            cli_command: a.cli_command.clone(),
            availability_schedule: a.availability_schedule.clone(),
        }
    }

    #[test]
    fn availability_switch_and_schedule_are_stored_and_validated() {
        let c = db::open_memory().unwrap();
        let a = create(&c, "claude", "A", "", None).unwrap();
        assert!(a.available && a.availability_schedule.is_empty());
        set_available(&c, a.id, false).unwrap();
        assert!(!get(&c, a.id).unwrap().available);
        assert!(set_available(&c, 999, true).is_err());
        let mut u = upd(&a);
        u.availability_schedule = " mon-fri 09:00-17:00 ".into();
        assert_eq!(
            update(&c, a.id, &u).unwrap().availability_schedule,
            "mon-fri 09:00-17:00"
        );
        u.availability_schedule = "someday 1-2".into();
        assert!(update(&c, a.id, &u).is_err());
        assert_eq!(
            get(&c, a.id).unwrap().availability_schedule,
            "mon-fri 09:00-17:00"
        );
    }

    #[test]
    fn cli_command_validated_and_changing_it_resets_confirmation() {
        let c = db::open_memory().unwrap();
        let a = create(&c, "claude", "A", "Pro", None).unwrap();
        assert_eq!(a.cli_command, None);
        set_subscription_confirmed(&c, a.id, true).unwrap();
        let mut u = upd(&a);
        for bad in ["claude -p", "rel/claude", "cla\nude", "/usr/bin/"] {
            u.cli_command = Some(bad.into());
            assert!(update(&c, a.id, &u).is_err(), "{bad:?} must be rejected");
        }
        u.cli_command = Some("  ".into());
        let r = update(&c, a.id, &u).unwrap();
        assert_eq!(r.cli_command, None);
        assert!(r.subscription_confirmed);
        u.cli_command = Some("/opt/bin/claude-wrap".into());
        let r = update(&c, a.id, &u).unwrap();
        assert_eq!(r.cli_command.as_deref(), Some("/opt/bin/claude-wrap"));
        assert!(!r.subscription_confirmed);
        u.cli_command = Some("claude2".into());
        assert!(update(&c, a.id, &u).is_ok());
    }

    #[test]
    fn update_account_and_reset_confirmation_when_identity_changes() {
        let c = db::open_memory().unwrap();
        let a = create(&c, "claude", "A", "Pro", None).unwrap();
        let b = create(&c, "claude", "B", "Pro", None).unwrap();
        set_subscription_confirmed(&c, a.id, true).unwrap();
        let mut u = upd(&a);
        u.label = "Renamed".into();
        u.usage_source = UsageSource::ClaudeOauth;
        u.user_agent = Some("  ".into());
        let r = update(&c, a.id, &u).unwrap();
        assert_eq!(r.label, "Renamed");
        assert_eq!(r.usage_source, UsageSource::ClaudeOauth);
        assert_eq!(r.user_agent, None);
        assert!(
            r.subscription_confirmed,
            "label/source change keeps confirmation"
        );
        u.credentials_file = Some("/other/.credentials.json".into());
        let r = update(&c, a.id, &u).unwrap();
        assert!(
            !r.subscription_confirmed,
            "new credentials need re-confirmation"
        );
        assert_eq!(r.billing_mode, BillingMode::Unverified);
        u.label = b.label.clone();
        assert!(update(&c, a.id, &u).is_err(), "duplicate label rejected");
        u.label = "".into();
        assert!(update(&c, a.id, &u).is_err());
        u.label = "ok".into();
        u.user_agent = Some("a\nb".into());
        assert!(update(&c, a.id, &u).is_err());
    }

    #[test]
    fn delete_account_removes_it_but_blocks_with_open_tasks() {
        use crate::tasks::{self, NewTask};
        let c = db::open_memory().unwrap();
        let a = create(&c, "claude", "A", "Pro", None).unwrap();
        let t = tasks::create(
            &c,
            &NewTask {
                name: "t".into(),
                prompt: "p".into(),
                account_id: Some(a.id),
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap();
        assert!(delete(&c, a.id).is_err());
        tasks::set_status(&c, t.id, TaskStatus::Completed).unwrap();
        delete(&c, a.id).unwrap();
        assert!(get(&c, a.id).is_err());
        assert_eq!(tasks::get(&c, t.id).unwrap().account_id, None);
    }

    #[test]
    fn limits_round_trip_and_validate() {
        let c = db::open_memory().unwrap();
        let a = create(&c, "claude", "A", "Pro", None).unwrap();
        assert_eq!(a.limits, AccountLimits::default());
        let l = AccountLimits {
            start_threshold: Some(40.0),
            reserved_capacity: Some(5.0),
            idle_requirement: Some(0),
            allowed_hours: Some("22:00-07:00".into()),
            max_usage_age_minutes: Some(10),
            max_tasks_per_batch: Some(2),
            max_execution_minutes: Some(30),
            session_threshold: Some(30.0),
            session_reserved: Some(0.0),
            ignore_session: true,
            ignore_limits: false,
            burn_session_hours: Some(2.0),
            burn_weekly_hours: Some(0.0),
        };
        set_limits(&c, a.id, &l).unwrap();
        set_ignore_limits(&c, a.id, true).unwrap();
        let got = get(&c, a.id).unwrap().limits;
        assert_eq!(
            got,
            AccountLimits {
                ignore_limits: true,
                ..l.clone()
            }
        );
        // set_limits does not touch the override switch
        set_limits(&c, a.id, &AccountLimits::default()).unwrap();
        assert!(get(&c, a.id).unwrap().limits.ignore_limits);
        for bad in [
            AccountLimits {
                start_threshold: Some(101.0),
                ..Default::default()
            },
            AccountLimits {
                session_reserved: Some(-1.0),
                ..Default::default()
            },
            AccountLimits {
                burn_session_hours: Some(5.5),
                ..Default::default()
            },
            AccountLimits {
                burn_weekly_hours: Some(-1.0),
                ..Default::default()
            },
            AccountLimits {
                max_tasks_per_batch: Some(0),
                ..Default::default()
            },
            AccountLimits {
                allowed_hours: Some("late".into()),
                ..Default::default()
            },
        ] {
            assert!(set_limits(&c, a.id, &bad).is_err());
        }
        assert!(set_ignore_limits(&c, 999, true).is_err());
    }
}
