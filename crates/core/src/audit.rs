use crate::{db, Result};
use chrono::Utc;
use rusqlite::{params, Connection};

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub at: String,
    pub actor: String,
    pub action: String,
    pub detail: String,
}

/// Appends to the automation audit log. Never put prompts or credentials in `detail`.
pub fn record(conn: &Connection, actor: &str, action: &str, detail: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO audit_log (at, actor, action, detail) VALUES (?1,?2,?3,?4)",
        params![db::ts(Utc::now()), actor, action, detail],
    )?;
    Ok(())
}

pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<AuditEntry>> {
    let mut st =
        conn.prepare("SELECT at, actor, action, detail FROM audit_log ORDER BY id DESC LIMIT ?1")?;
    let rows = st.query_map([limit as i64], |r| {
        Ok(AuditEntry {
            at: r.get(0)?,
            actor: r.get(1)?,
            action: r.get(2)?,
            detail: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
