use crate::{Error, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::path::Path;

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("../migrations/0001_init.sql")),
    (
        "0002_account_usage",
        include_str!("../migrations/0002_account_usage.sql"),
    ),
    (
        "0003_custom_provider",
        include_str!("../migrations/0003_custom_provider.sql"),
    ),
    (
        "0004_cli_command",
        include_str!("../migrations/0004_cli_command.sql"),
    ),
    (
        "0005_retention",
        include_str!("../migrations/0005_retention.sql"),
    ),
    (
        "0006_availability",
        include_str!("../migrations/0006_availability.sql"),
    ),
    (
        "0007_run_mode",
        include_str!("../migrations/0007_run_mode.sql"),
    ),
    (
        "0008_account_limits",
        include_str!("../migrations/0008_account_limits.sql"),
    ),
    (
        "0009_template_defaults",
        include_str!("../migrations/0009_template_defaults.sql"),
    ),
    (
        "0010_burn_before_reset",
        include_str!("../migrations/0010_burn_before_reset.sql"),
    ),
    (
        "0011_task_repeat",
        include_str!("../migrations/0011_task_repeat.sql"),
    ),
    (
        "0012_control_state",
        include_str!("../migrations/0012_control_state.sql"),
    ),
];

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    enable_wal(&conn)?;
    init(conn)
}

/// Switches the database to WAL. SQLite reports BUSY for this switch immediately, without invoking the
/// busy handler, when another connection is opening or migrating at the same time, so retry for a
/// bounded time. Once any connection has switched, the mode is persistent and this is a cheap check.
fn enable_wal(conn: &Connection) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if mode.eq_ignore_ascii_case("wal") {
            return Ok(());
        }
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn open_memory() -> Result<Connection> {
    init(Connection::open_in_memory()?)
}

fn init(conn: Connection) -> Result<Connection> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

/// Applies pending migrations using `PRAGMA user_version` as the applied count.
/// Safe against concurrent processes: each step takes the write lock (`BEGIN IMMEDIATE`) and re-reads
/// `user_version` inside it, so a migration another process already applied is skipped. A database
/// from a newer build (`user_version` beyond what we know) is refused rather than used blindly.
pub fn migrate(conn: &Connection) -> Result<()> {
    check_not_newer(schema_version(conn)?)?;
    for (i, (name, sql)) in MIGRATIONS.iter().enumerate() {
        if schema_version(conn)? > i {
            continue;
        }
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let applied = schema_version(&tx)?;
        check_not_newer(applied)?;
        if applied > i {
            continue; // another process migrated while we waited for the lock
        }
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
        tracing::info!("applied migration {name}");
    }
    Ok(())
}

fn check_not_newer(version: usize) -> Result<()> {
    if version > MIGRATIONS.len() {
        return Err(Error::Blocked(format!(
            "the database schema (v{version}) is newer than this build supports (v{}); upgrade the app",
            MIGRATIONS.len()
        )));
    }
    Ok(())
}

/// Rows deleted by [`prune`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    pub audit_log: usize,
    pub notifications: usize,
    pub usage_snapshots: usize,
}

/// Deletes audit entries, notifications and usage snapshots older than `retention_days` (0 = keep forever).
/// The newest snapshot of every account is always kept, so a quiet account still has its last reading
/// (which is then stale and blocks runs, as designed).
pub fn prune(conn: &Connection, now: DateTime<Utc>, retention_days: u32) -> Result<PruneReport> {
    if retention_days == 0 {
        return Ok(PruneReport::default());
    }
    let cutoff = ts(now - chrono::Duration::days(retention_days as i64));
    Ok(PruneReport {
        audit_log: conn.execute("DELETE FROM audit_log WHERE at < ?1", [&cutoff])?,
        notifications: conn
            .execute("DELETE FROM notifications WHERE created_at < ?1", [&cutoff])?,
        usage_snapshots: conn.execute(
            "DELETE FROM usage_snapshots WHERE captured_at < ?1
               AND id NOT IN (SELECT MAX(id) FROM usage_snapshots GROUP BY account_id)",
            [&cutoff],
        )?,
    })
}

pub fn schema_version(conn: &Connection) -> Result<usize> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

pub fn latest_version() -> usize {
    MIGRATIONS.len()
}

pub fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339()
}

pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_and_are_idempotent() {
        let conn = open_memory().unwrap();
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        migrate(&conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM automation_settings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        {
            let c = open(&p).unwrap();
            c.pragma_update(None, "user_version", (latest_version() + 1) as i64)
                .unwrap();
        }
        match open(&p) {
            Err(Error::Blocked(m)) => assert!(m.contains("newer")),
            other => panic!("expected Blocked, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn concurrent_open_applies_each_migration_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let p = p.clone();
                std::thread::spawn(move || open(&p).map(|_| ()))
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
        let c = open(&p).unwrap();
        assert_eq!(schema_version(&c).unwrap(), latest_version());
    }

    fn count(c: &Connection, table: &str) -> i64 {
        c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn prune_removes_old_rows_but_keeps_recent_and_latest_snapshot() {
        let c = open_memory().unwrap();
        let now = Utc::now();
        let old = ts(now - chrono::Duration::days(200));
        let recent = ts(now - chrono::Duration::days(1));
        for at in [&old, &recent] {
            c.execute(
                "INSERT INTO audit_log (at, actor, action) VALUES (?1,'t','a')",
                [at],
            )
            .unwrap();
            c.execute(
                "INSERT INTO notifications (notification_type, message, created_at) VALUES ('x','m',?1)",
                [at],
            )
            .unwrap();
        }
        crate::accounts::create(&c, "claude", "A", "", None).unwrap();
        crate::accounts::create(&c, "claude", "B", "", None).unwrap();
        // Account 1: two old snapshots (latest is old too); account 2: one old, one recent.
        for (acct, at) in [(1, &old), (1, &old), (2, &old), (2, &recent)] {
            c.execute(
                "INSERT INTO usage_snapshots (account_id, confidence, source, captured_at) VALUES (?1,'manual','m',?2)",
                rusqlite::params![acct, at],
            )
            .unwrap();
        }
        assert_eq!(
            prune(&c, now, 0).unwrap(),
            PruneReport::default(),
            "0 = keep forever"
        );
        assert_eq!(count(&c, "audit_log"), 2);

        let r = prune(&c, now, 90).unwrap();
        assert_eq!(
            r,
            PruneReport {
                audit_log: 1,
                notifications: 1,
                usage_snapshots: 2
            }
        );
        assert_eq!(count(&c, "audit_log"), 1);
        assert_eq!(count(&c, "notifications"), 1);
        // acct 1 keeps its newest (old) reading; acct 2 keeps the recent one.
        assert_eq!(count(&c, "usage_snapshots"), 2);
        let per: Vec<i64> = c
            .prepare("SELECT account_id FROM usage_snapshots ORDER BY account_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        assert_eq!(per, vec![1, 2]);
        // Idempotent.
        assert_eq!(prune(&c, now, 90).unwrap(), PruneReport::default());
    }

    #[test]
    fn dedup_lookup_uses_the_index() {
        let c = open_memory().unwrap();
        let plan: String = c
            .query_row(
                "EXPLAIN QUERY PLAN SELECT 1 FROM notifications WHERE dedup_key = 'k' AND created_at > 't'",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("idx_notifications_dedup"), "{plan}");
    }

    #[test]
    fn data_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        {
            let c = open(&p).unwrap();
            c.execute(
                "INSERT INTO notifications (notification_type, message, created_at) VALUES ('x','m','t')",
                [],
            )
            .unwrap();
        }
        let c = open(&p).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM notifications", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
