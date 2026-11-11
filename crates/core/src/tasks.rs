use crate::models::{RunMode, TaskStatus};
use crate::{db, Error, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const PRIORITY_LOW: i64 = 0;
pub const PRIORITY_NORMAL: i64 = 1;
pub const PRIORITY_HIGH: i64 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub prompt: String,
    /// `None` = any provider.
    pub provider_type: Option<String>,
    /// `None` = any account of `provider_type`. `Some` = pinned.
    pub account_id: Option<i64>,
    pub allow_fallback: bool,
    pub model: String,
    pub priority: i64,
    pub position: i64,
    pub estimated_consumption: f64,
    pub estimated_minutes: i64,
    pub maximum_budget: Option<f64>,
    pub timeout_secs: i64,
    pub retry_max: i64,
    pub retry_count: i64,
    pub working_dir: String,
    pub requires_approval: bool,
    pub status: TaskStatus,
    pub schedule: Option<String>,
    pub expiration_at: Option<DateTime<Utc>>,
    pub result_path: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Run-mode override for this task (`Inherit` = account/global setting).
    pub run_mode: RunMode,
    /// Recurrence rule (`core::recurrence`); `None` = one-off. Set with `set_repeat`.
    #[serde(default)]
    pub repeat: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NewTask {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub provider_type: Option<String>,
    pub account_id: Option<i64>,
    pub allow_fallback: bool,
    pub model: String,
    pub priority: i64,
    pub estimated_consumption: f64,
    pub estimated_minutes: i64,
    pub maximum_budget: Option<f64>,
    pub timeout_secs: i64,
    pub retry_max: i64,
    pub working_dir: String,
    pub requires_approval: bool,
    pub schedule: Option<String>,
    pub expiration_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub run_mode: RunMode,
}

impl Default for NewTask {
    fn default() -> Self {
        NewTask {
            name: String::new(),
            description: String::new(),
            prompt: String::new(),
            provider_type: Some("claude".into()),
            account_id: None,
            allow_fallback: false,
            model: String::new(),
            priority: PRIORITY_NORMAL,
            estimated_consumption: 5.0,
            estimated_minutes: 10,
            maximum_budget: None,
            timeout_secs: 1200,
            retry_max: 0,
            working_dir: String::new(),
            requires_approval: true,
            schedule: None,
            expiration_at: None,
            run_mode: RunMode::Inherit,
        }
    }
}

/// A model name is passed to the provider CLI as an option value: it must never be able to look like a
/// flag or carry whitespace/control characters. Empty means "provider default".
pub(crate) fn validate_model(m: &str) -> Result<()> {
    let ok = m.is_empty()
        || (m.len() <= 100
            && !m.starts_with('-')
            && m.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:[]/-".contains(c)));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "model “{m}” is not valid: use letters, digits and . _ : [ ] / - only, not starting with “-” (leave empty for the default)"
        )))
    }
}

pub(crate) fn validate(n: &NewTask) -> Result<()> {
    if n.name.trim().is_empty() || n.name.len() > 200 {
        return Err(Error::Invalid("task name must be 1-200 characters".into()));
    }
    if n.prompt.trim().is_empty() {
        return Err(Error::Invalid("prompt must not be empty".into()));
    }
    validate_model(&n.model)?;
    if n.prompt.contains('\0') || n.working_dir.contains('\0') {
        return Err(Error::Invalid("NUL bytes are not allowed".into()));
    }
    if !(0.0..=100.0).contains(&n.estimated_consumption) {
        return Err(Error::Invalid("estimated consumption must be 0-100".into()));
    }
    if n.maximum_budget
        .is_some_and(|b| !(0.0..=100.0).contains(&b))
    {
        return Err(Error::Invalid("maximum budget must be 0-100".into()));
    }
    if n.timeout_secs <= 0 || n.timeout_secs > 24 * 3600 {
        return Err(Error::Invalid(
            "timeout must be between 1 s and 24 h".into(),
        ));
    }
    if !(PRIORITY_LOW..=PRIORITY_HIGH).contains(&n.priority) {
        return Err(Error::Invalid("priority must be 0-2".into()));
    }
    if n.account_id.is_some() && n.provider_type.is_none() {
        return Err(Error::Invalid("a pinned account needs a provider".into()));
    }
    crate::scheduler::validate_schedule(n.schedule.as_deref())?;
    Ok(())
}

const COLS: &str = "id, name, description, prompt, provider_type, account_id, allow_fallback, model, priority,
    position, estimated_consumption, estimated_minutes, maximum_budget, timeout_secs, retry_max, retry_count,
    working_dir, requires_approval, status, schedule, expiration_at, result_path, created_at, run_mode, repeat";

fn map(r: &rusqlite::Row) -> rusqlite::Result<Task> {
    let t = |i: usize| -> rusqlite::Result<Option<DateTime<Utc>>> {
        Ok(r.get::<_, Option<String>>(i)?
            .and_then(|s| db::parse_ts(&s)))
    };
    Ok(Task {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        prompt: r.get(3)?,
        provider_type: r.get(4)?,
        account_id: r.get(5)?,
        allow_fallback: r.get(6)?,
        model: r.get(7)?,
        priority: r.get(8)?,
        position: r.get(9)?,
        estimated_consumption: r.get(10)?,
        estimated_minutes: r.get(11)?,
        maximum_budget: r.get(12)?,
        timeout_secs: r.get(13)?,
        retry_max: r.get(14)?,
        retry_count: r.get(15)?,
        working_dir: r.get(16)?,
        requires_approval: r.get(17)?,
        status: TaskStatus::parse(&r.get::<_, String>(18)?).unwrap_or(TaskStatus::Failed),
        schedule: r.get(19)?,
        expiration_at: t(20)?,
        result_path: r.get(21)?,
        created_at: t(22)?.unwrap_or(DateTime::UNIX_EPOCH),
        run_mode: RunMode::parse(&r.get::<_, String>(23)?).unwrap_or_default(),
        repeat: r.get(24)?,
    })
}

pub fn create(conn: &Connection, n: &NewTask, status: TaskStatus) -> Result<Task> {
    validate(n)?;
    let pos: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM tasks",
        [],
        |r| r.get(0),
    )?;
    let now = db::ts(Utc::now());
    conn.execute(
        "INSERT INTO tasks (name, description, prompt, provider_type, account_id, allow_fallback, model, priority,
            position, estimated_consumption, estimated_minutes, maximum_budget, timeout_secs, retry_max,
            working_dir, requires_approval, status, schedule, expiration_at, created_at, updated_at, run_mode)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?20,?21)",
        params![
            n.name.trim(),
            n.description,
            n.prompt,
            n.provider_type,
            n.account_id,
            n.allow_fallback,
            n.model,
            n.priority,
            pos,
            n.estimated_consumption,
            n.estimated_minutes,
            n.maximum_budget,
            n.timeout_secs,
            n.retry_max,
            n.working_dir,
            n.requires_approval,
            status.as_str(),
            n.schedule,
            n.expiration_at.map(db::ts),
            now,
            n.run_mode.as_str()
        ],
    )?;
    get(conn, conn.last_insert_rowid())
}

pub fn get(conn: &Connection, id: i64) -> Result<Task> {
    conn.query_row(&format!("SELECT {COLS} FROM tasks WHERE id=?1"), [id], map)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("task {id}")))
}

pub fn list(conn: &Connection) -> Result<Vec<Task>> {
    let mut st = conn.prepare(&format!("SELECT {COLS} FROM tasks ORDER BY position, id"))?;
    let v = st
        .query_map([], map)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

/// Edit the definition of a task. Running tasks cannot be edited.
pub fn update(conn: &Connection, id: i64, n: &NewTask) -> Result<Task> {
    validate(n)?;
    let cur = get(conn, id)?;
    if cur.status == TaskStatus::Running {
        return Err(Error::Blocked("cannot edit a running task".into()));
    }
    conn.execute(
        "UPDATE tasks SET name=?1, description=?2, prompt=?3, provider_type=?4, account_id=?5, allow_fallback=?6,
            model=?7, priority=?8, estimated_consumption=?9, estimated_minutes=?10, maximum_budget=?11,
            timeout_secs=?12, retry_max=?13, working_dir=?14, requires_approval=?15, schedule=?16,
            expiration_at=?17, updated_at=?18, run_mode=?20 WHERE id=?19",
        params![
            n.name.trim(),
            n.description,
            n.prompt,
            n.provider_type,
            n.account_id,
            n.allow_fallback,
            n.model,
            n.priority,
            n.estimated_consumption,
            n.estimated_minutes,
            n.maximum_budget,
            n.timeout_secs,
            n.retry_max,
            n.working_dir,
            n.requires_approval,
            n.schedule,
            n.expiration_at.map(db::ts),
            db::ts(Utc::now()),
            id,
            n.run_mode.as_str()
        ],
    )?;
    get(conn, id)
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    if get(conn, id)?.status == TaskStatus::Running {
        return Err(Error::Blocked("cannot delete a running task".into()));
    }
    conn.execute("DELETE FROM tasks WHERE id=?1", [id])?;
    Ok(())
}

pub fn set_status(conn: &Connection, id: i64, s: TaskStatus) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET status=?1, updated_at=?2 WHERE id=?3",
        params![s.as_str(), db::ts(Utc::now()), id],
    )?;
    // A recurring task queues its next instance when it finishes (never on Cancelled).
    if matches!(
        s,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Expired
    ) {
        enqueue_next_best_effort(conn, id, Utc::now());
    }
    Ok(())
}

/// Queues the next instance of a recurring task without ever failing the caller: the finished task's
/// status is already committed and a batch in progress must carry on. A failure is audited instead.
fn enqueue_next_best_effort(conn: &Connection, id: i64, now: DateTime<Utc>) {
    if let Err(e) = enqueue_next(conn, id, now) {
        let _ = crate::audit::record(
            conn,
            "engine",
            "repeat_enqueue_failed",
            &format!("task={id}: {e}"),
        );
    }
}

/// Sets (or clears, with `None`/empty) the recurrence rule of a task. The rule is validated.
pub fn set_repeat(conn: &Connection, id: i64, repeat: Option<&str>) -> Result<()> {
    let repeat = repeat.map(str::trim).filter(|r| !r.is_empty());
    crate::recurrence::validate(repeat)?;
    get(conn, id)?;
    conn.execute(
        "UPDATE tasks SET repeat=?1, updated_at=?2 WHERE id=?3",
        params![repeat, db::ts(Utc::now()), id],
    )?;
    Ok(())
}

fn as_new(t: &Task) -> NewTask {
    NewTask {
        name: t.name.clone(),
        description: t.description.clone(),
        prompt: t.prompt.clone(),
        provider_type: t.provider_type.clone(),
        account_id: t.account_id,
        allow_fallback: t.allow_fallback,
        model: t.model.clone(),
        priority: t.priority,
        estimated_consumption: t.estimated_consumption,
        estimated_minutes: t.estimated_minutes,
        maximum_budget: t.maximum_budget,
        timeout_secs: t.timeout_secs,
        retry_max: t.retry_max,
        working_dir: t.working_dir.clone(),
        requires_approval: t.requires_approval,
        schedule: t.schedule.clone(),
        expiration_at: t.expiration_at,
        run_mode: t.run_mode,
    }
}

/// Copies a task as a fresh Pending task named "x (copy)", "x (copy 2)", … at the end of the queue,
/// keeping its dependencies and repeat rule. Result, retry state and (past) expiry are not copied.
pub fn duplicate(conn: &Connection, id: i64) -> Result<Task> {
    let t = get(conn, id)?;
    let names: Vec<String> = list(conn)?.into_iter().map(|t| t.name).collect();
    let free = |n: &str| !names.iter().any(|x| x.eq_ignore_ascii_case(n));
    let mut name = format!("{} (copy)", t.name);
    let mut i = 2;
    while !free(&name) {
        name = format!("{} (copy {i})", t.name);
        i += 1;
    }
    let mut n = as_new(&t);
    n.name = name;
    if n.expiration_at.is_some_and(|e| e <= Utc::now()) {
        n.expiration_at = None;
    }
    finish_copy(conn, &n, id, t.repeat.as_deref())
}

/// Creates the copy with its dependencies and rule; removes it again if any step fails.
fn finish_copy(conn: &Connection, n: &NewTask, from: i64, repeat: Option<&str>) -> Result<Task> {
    let copy = create(conn, n, TaskStatus::Pending)?;
    let r = set_dependencies(conn, copy.id, &dependencies(conn, from)?)
        .and_then(|_| set_repeat(conn, copy.id, repeat));
    if let Err(e) = r {
        let _ = conn.execute("DELETE FROM tasks WHERE id=?1", [copy.id]);
        return Err(e);
    }
    get(conn, copy.id)
}

/// Queues the next instance of a finished recurring task: a Pending copy (same dependencies and rule)
/// gated by `schedule = after:<next occurrence>`. Returns `None` when the task does not repeat, its rule
/// is unparsable, or the chain already has a live instance (same name and rule, not finished), so a
/// chain never holds more than one pending instance. It starts nothing: the copy goes through all the
/// normal scheduler, preflight and billing checks.
pub fn enqueue_next(conn: &Connection, id: i64, now: DateTime<Utc>) -> Result<Option<Task>> {
    let t = get(conn, id)?;
    let Some(rule) = t.repeat.as_deref() else {
        return Ok(None);
    };
    let Some(next) = crate::recurrence::next_after(rule, now) else {
        return Ok(None);
    };
    let live: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE id<>?1 AND name=?2 AND repeat=?3
         AND status NOT IN ('completed','failed','cancelled','expired')",
        params![id, t.name, rule],
        |r| r.get(0),
    )?;
    if live > 0 {
        return Ok(None);
    }
    let mut n = as_new(&t);
    n.schedule = Some(format!("after:{}", db::ts(next)));
    // The expiry is a lifetime (created -> expires), so each instance gets the same window counted from
    // its own scheduled start; an absolute date would already be in the past for the next instance.
    n.expiration_at = t
        .expiration_at
        .map(|e| e - t.created_at)
        .filter(|w| *w > Duration::zero())
        .map(|w| next + w);
    Ok(Some(finish_copy(conn, &n, id, Some(rule))?))
}

pub fn set_result_path(conn: &Connection, id: i64, p: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET result_path=?1, updated_at=?2 WHERE id=?3",
        params![p, db::ts(Utc::now()), id],
    )?;
    Ok(())
}

pub fn increment_retry(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET retry_count = retry_count + 1 WHERE id=?1",
        [id],
    )?;
    Ok(())
}

pub fn pause(conn: &Connection, id: i64) -> Result<()> {
    let t = get(conn, id)?;
    if t.status.is_terminal() || t.status == TaskStatus::Running {
        return Err(Error::Blocked(format!(
            "cannot pause a {} task",
            t.status.label()
        )));
    }
    set_status(conn, id, TaskStatus::Paused)
}

pub fn resume(conn: &Connection, id: i64) -> Result<()> {
    if get(conn, id)?.status != TaskStatus::Paused {
        return Err(Error::Blocked("task is not paused".into()));
    }
    set_status(conn, id, TaskStatus::Pending)
}

pub fn cancel(conn: &Connection, id: i64) -> Result<()> {
    let t = get(conn, id)?;
    if t.status.is_terminal() {
        return Err(Error::Blocked("task already finished".into()));
    }
    if t.status == TaskStatus::Running {
        return Err(Error::Blocked("use Stop to cancel a running task".into()));
    }
    set_status(conn, id, TaskStatus::Cancelled)
}

/// Failed/cancelled/expired tasks can be retried manually; this does not bypass any safeguard.
pub fn retry(conn: &Connection, id: i64) -> Result<()> {
    let t = get(conn, id)?;
    if !matches!(
        t.status,
        TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Expired
    ) {
        return Err(Error::Blocked(
            "only failed, cancelled or expired tasks can be retried".into(),
        ));
    }
    conn.execute("UPDATE tasks SET retry_count=0, expiration_at=CASE WHEN expiration_at < ?2 THEN NULL ELSE expiration_at END WHERE id=?1", params![id, db::ts(Utc::now())])?;
    set_status(conn, id, TaskStatus::Pending)
}

/// Reorders so that the given ids occupy the lowest positions in the given order.
pub fn reorder(conn: &Connection, ordered_ids: &[i64]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for (i, id) in ordered_ids.iter().enumerate() {
        tx.execute(
            "UPDATE tasks SET position=?1 WHERE id=?2",
            params![i as i64 + 1, id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn move_by(conn: &Connection, id: i64, delta: i32) -> Result<()> {
    let mut ids: Vec<i64> = list(conn)?.into_iter().map(|t| t.id).collect();
    let Some(i) = ids.iter().position(|x| *x == id) else {
        return Err(Error::NotFound(format!("task {id}")));
    };
    let j = (i as i64 + delta as i64).clamp(0, ids.len() as i64 - 1) as usize;
    let v = ids.remove(i);
    ids.insert(j, v);
    reorder(conn, &ids)
}

/// Moves task `id` directly before (or after, if `after`) task `target` in the full queue order.
pub fn move_next_to(conn: &Connection, id: i64, target: i64, after: bool) -> Result<()> {
    if id == target {
        return Ok(());
    }
    let mut ids: Vec<i64> = list(conn)?.into_iter().map(|t| t.id).collect();
    let Some(i) = ids.iter().position(|x| *x == id) else {
        return Err(Error::NotFound(format!("task {id}")));
    };
    ids.remove(i);
    let Some(j) = ids.iter().position(|x| *x == target) else {
        return Err(Error::NotFound(format!("task {target}")));
    };
    ids.insert(if after { j + 1 } else { j }, id);
    reorder(conn, &ids)
}

pub fn dependencies(conn: &Connection, id: i64) -> Result<Vec<i64>> {
    let mut st = conn
        .prepare("SELECT dependency_task_id FROM task_dependencies WHERE task_id=?1 ORDER BY 1")?;
    let v = st
        .query_map([id], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

/// Replaces a task's dependencies; rejects self-dependencies and cycles.
pub fn set_dependencies(conn: &Connection, id: i64, deps: &[i64]) -> Result<()> {
    for d in deps {
        if *d == id {
            return Err(Error::Invalid("a task cannot depend on itself".into()));
        }
        get(conn, *d)?;
        if reaches(conn, *d, id)? {
            return Err(Error::Invalid("dependency cycle".into()));
        }
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM task_dependencies WHERE task_id=?1", [id])?;
    for d in deps {
        tx.execute(
            "INSERT OR IGNORE INTO task_dependencies (task_id, dependency_task_id) VALUES (?1,?2)",
            params![id, d],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Does `from` (transitively) depend on `target`?
fn reaches(conn: &Connection, from: i64, target: i64) -> Result<bool> {
    let mut stack = vec![from];
    let mut seen = std::collections::HashSet::new();
    while let Some(n) = stack.pop() {
        if n == target {
            return Ok(true);
        }
        if seen.insert(n) {
            stack.extend(dependencies(conn, n)?);
        }
    }
    Ok(false)
}

pub fn deps_done(conn: &Connection, id: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM task_dependencies d JOIN tasks t ON t.id = d.dependency_task_id
         WHERE d.task_id=?1 AND t.status <> 'completed'",
        [id],
        |r| r.get(0),
    )?;
    Ok(n == 0)
}

/// Marks tasks past their expiration as Expired. Returns the count.
pub fn expire_due(conn: &Connection, now: DateTime<Utc>) -> Result<usize> {
    let repeating = ids_where(
        conn,
        "SELECT id FROM tasks WHERE repeat IS NOT NULL AND expiration_at IS NOT NULL AND expiration_at <= ?1
         AND status IN ('draft','pending','waiting_for_capacity','awaiting_approval','paused')",
        &db::ts(now),
    )?;
    let n = conn.execute(
        "UPDATE tasks SET status='expired', updated_at=?1 WHERE expiration_at IS NOT NULL AND expiration_at <= ?1
         AND status IN ('draft','pending','waiting_for_capacity','awaiting_approval','paused')",
        [db::ts(now)],
    )?;
    for id in repeating {
        enqueue_next_best_effort(conn, id, now);
    }
    Ok(n)
}

fn ids_where(conn: &Connection, sql: &str, arg: &str) -> Result<Vec<i64>> {
    let mut st = conn.prepare(sql)?;
    let v = st
        .query_map([arg], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

/// Runnable tasks (dependencies satisfied, not expired), best first: priority desc, then queue position.
pub fn ready(conn: &Connection, now: DateTime<Utc>) -> Result<Vec<Task>> {
    let mut v = Vec::new();
    for t in list(conn)? {
        if t.status.is_runnable()
            && t.expiration_at.is_none_or(|e| e > now)
            && deps_done(conn, t.id)?
        {
            v.push(t);
        }
    }
    v.sort_by_key(|t| (-t.priority, t.position, t.id));
    Ok(v)
}

/// After a crash/restart: anything left Running is marked interrupted and Failed (never silently re-run).
pub fn recover_interrupted(conn: &Connection) -> Result<usize> {
    let now = db::ts(Utc::now());
    let repeating = ids_where(
        conn,
        "SELECT id FROM tasks WHERE repeat IS NOT NULL AND status='running' AND ?1 IS NOT NULL",
        &now,
    )?;
    conn.execute(
        "UPDATE executions SET status='interrupted', completed_at=?1, error_message='application stopped during execution' WHERE status='running'",
        [&now],
    )?;
    let n = conn.execute(
        "UPDATE tasks SET status='failed', updated_at=?1 WHERE status='running'",
        [&now],
    )?;
    for id in repeating {
        enqueue_next(conn, id, Utc::now())?;
    }
    // A restart also invalidates any approval requests that were pending in memory.
    conn.execute(
        "UPDATE tasks SET status='pending', updated_at=?1 WHERE status='awaiting_approval'",
        [&now],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn nt(name: &str) -> NewTask {
        NewTask {
            name: name.into(),
            prompt: "do it".into(),
            ..Default::default()
        }
    }

    #[test]
    fn crud_and_validation() {
        let c = db::open_memory().unwrap();
        let t = create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        assert_eq!(t.status, TaskStatus::Pending);
        let bad = NewTask {
            schedule: Some("daily:9.30".into()),
            ..nt("s")
        };
        assert!(create(&c, &bad, TaskStatus::Pending).is_err());
        assert!(update(&c, t.id, &bad).is_err());
        let good = NewTask {
            schedule: Some("daily:09:30".into()),
            ..nt("s")
        };
        assert!(create(&c, &good, TaskStatus::Pending).is_ok());
        assert!(create(
            &c,
            &NewTask {
                name: "".into(),
                ..nt("x")
            },
            TaskStatus::Pending
        )
        .is_err());
        assert!(create(
            &c,
            &NewTask {
                prompt: "p\0".into(),
                ..nt("x")
            },
            TaskStatus::Pending
        )
        .is_err());
        let u = update(
            &c,
            t.id,
            &NewTask {
                name: "b".into(),
                ..nt("b")
            },
        )
        .unwrap();
        assert_eq!(u.name, "b");
        delete(&c, t.id).unwrap();
        assert!(get(&c, t.id).is_err());
    }

    #[test]
    fn reorder_and_move() {
        let c = db::open_memory().unwrap();
        let a = create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        let b = create(&c, &nt("b"), TaskStatus::Pending).unwrap();
        let d = create(&c, &nt("d"), TaskStatus::Pending).unwrap();
        reorder(&c, &[d.id, a.id, b.id]).unwrap();
        let ids: Vec<_> = list(&c).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![d.id, a.id, b.id]);
        move_by(&c, b.id, -2).unwrap();
        let ids: Vec<_> = list(&c).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![b.id, d.id, a.id]);
        move_next_to(&c, b.id, a.id, true).unwrap();
        let ids: Vec<_> = list(&c).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![d.id, a.id, b.id]);
        move_next_to(&c, b.id, d.id, false).unwrap();
        let ids: Vec<_> = list(&c).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![b.id, d.id, a.id]);
        assert!(move_next_to(&c, b.id, 9999, false).is_err());
    }

    #[test]
    fn dependencies_are_respected() {
        let c = db::open_memory().unwrap();
        let a = create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        let b = create(&c, &nt("b"), TaskStatus::Pending).unwrap();
        set_dependencies(&c, b.id, &[a.id]).unwrap();
        let ids: Vec<_> = ready(&c, Utc::now())
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![a.id]);
        set_status(&c, a.id, TaskStatus::Completed).unwrap();
        let ids: Vec<_> = ready(&c, Utc::now())
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![b.id]);
        assert!(
            set_dependencies(&c, a.id, &[b.id]).is_err(),
            "cycle rejected"
        );
        assert!(set_dependencies(&c, a.id, &[a.id]).is_err());
    }

    #[test]
    fn priority_ordering_and_expiry() {
        let c = db::open_memory().unwrap();
        let lo = create(
            &c,
            &NewTask {
                priority: 0,
                ..nt("lo")
            },
            TaskStatus::Pending,
        )
        .unwrap();
        let hi = create(
            &c,
            &NewTask {
                priority: 2,
                ..nt("hi")
            },
            TaskStatus::Pending,
        )
        .unwrap();
        let ex = create(
            &c,
            &NewTask {
                expiration_at: Some(Utc::now() - Duration::hours(1)),
                ..nt("ex")
            },
            TaskStatus::Pending,
        )
        .unwrap();
        let ids: Vec<_> = ready(&c, Utc::now())
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![hi.id, lo.id]);
        assert_eq!(expire_due(&c, Utc::now()).unwrap(), 1);
        assert_eq!(get(&c, ex.id).unwrap().status, TaskStatus::Expired);
    }

    #[test]
    fn retry_and_cancel_rules() {
        let c = db::open_memory().unwrap();
        let a = create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        assert!(retry(&c, a.id).is_err());
        set_status(&c, a.id, TaskStatus::Failed).unwrap();
        retry(&c, a.id).unwrap();
        assert_eq!(get(&c, a.id).unwrap().status, TaskStatus::Pending);
        cancel(&c, a.id).unwrap();
        assert!(cancel(&c, a.id).is_err());
        pause(&c, create(&c, &nt("p"), TaskStatus::Pending).unwrap().id).unwrap();
    }

    #[test]
    fn restart_recovers_running_tasks_without_rerunning() {
        let c = db::open_memory().unwrap();
        let a = create(&c, &nt("a"), TaskStatus::Running).unwrap();
        let b = create(&c, &nt("b"), TaskStatus::AwaitingApproval).unwrap();
        let p = create(&c, &nt("p"), TaskStatus::Pending).unwrap();
        recover_interrupted(&c).unwrap();
        assert_eq!(get(&c, a.id).unwrap().status, TaskStatus::Failed);
        assert_eq!(get(&c, b.id).unwrap().status, TaskStatus::Pending);
        assert_eq!(get(&c, p.id).unwrap().status, TaskStatus::Pending);
    }

    #[test]
    fn duplicate_copies_definition_and_deps_under_a_free_name() {
        let c = db::open_memory().unwrap();
        let a = create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        let b = create(&c, &nt("b"), TaskStatus::Pending).unwrap();
        set_dependencies(&c, b.id, &[a.id]).unwrap();
        set_repeat(&c, b.id, Some("every:6h")).unwrap();
        set_status(&c, b.id, TaskStatus::Cancelled).unwrap();
        increment_retry(&c, b.id).unwrap();
        let d1 = duplicate(&c, b.id).unwrap();
        let d2 = duplicate(&c, b.id).unwrap();
        assert_eq!(d1.name, "b (copy)");
        assert_eq!(d2.name, "b (copy 2)");
        assert_eq!(d1.status, TaskStatus::Pending);
        assert_eq!(d1.retry_count, 0);
        assert_eq!(d1.prompt, b.prompt);
        assert_eq!(d1.repeat.as_deref(), Some("every:6h"));
        assert_eq!(dependencies(&c, d1.id).unwrap(), vec![a.id]);
        assert!(d1.position > b.position && d2.position > d1.position);
        assert!(duplicate(&c, 999).is_err());
    }

    #[test]
    fn repeat_rule_is_validated() {
        let c = db::open_memory().unwrap();
        let a = create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        assert!(set_repeat(&c, a.id, Some("whenever")).is_err());
        set_repeat(&c, a.id, Some(" daily:09:00 ")).unwrap();
        assert_eq!(
            get(&c, a.id).unwrap().repeat.as_deref(),
            Some("daily:09:00")
        );
        set_repeat(&c, a.id, Some("")).unwrap();
        assert_eq!(get(&c, a.id).unwrap().repeat, None);
    }

    #[test]
    fn finishing_a_recurring_task_queues_one_gated_successor() {
        let c = db::open_memory().unwrap();
        let a = create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        let dep = create(&c, &nt("dep"), TaskStatus::Completed).unwrap();
        set_dependencies(&c, a.id, &[dep.id]).unwrap();
        set_repeat(&c, a.id, Some("every:2h")).unwrap();
        set_status(&c, a.id, TaskStatus::Running).unwrap();
        assert_eq!(list(&c).unwrap().len(), 2);
        set_status(&c, a.id, TaskStatus::Completed).unwrap();
        let all = list(&c).unwrap();
        assert_eq!(all.len(), 3);
        let next = all.last().unwrap();
        assert_eq!(
            (next.name.as_str(), next.status),
            ("a", TaskStatus::Pending)
        );
        assert_eq!(next.repeat.as_deref(), Some("every:2h"));
        assert_eq!(dependencies(&c, next.id).unwrap(), vec![dep.id]);
        let due = db::parse_ts(
            next.schedule
                .as_deref()
                .unwrap()
                .strip_prefix("after:")
                .unwrap(),
        )
        .unwrap();
        assert!(due > Utc::now() + chrono::Duration::minutes(110));
        // Not due yet, so it cannot be started early.
        assert!(!crate::scheduler::task_due(next, Utc::now(), 0));
        // A live successor exists: another terminal transition queues nothing more.
        assert!(enqueue_next(&c, a.id, Utc::now()).unwrap().is_none());
    }

    #[test]
    fn repeating_task_expiry_is_carried_as_a_lifetime() {
        let c = db::open_memory().unwrap();
        let mut n = nt("a");
        n.expiration_at = Some(Utc::now() + Duration::hours(3));
        let a = create(&c, &n, TaskStatus::Pending).unwrap();
        set_repeat(&c, a.id, Some("every:2h")).unwrap();
        set_status(&c, a.id, TaskStatus::Running).unwrap();
        set_status(&c, a.id, TaskStatus::Completed).unwrap();
        let next = list(&c).unwrap().into_iter().last().unwrap();
        let start = db::parse_ts(
            next.schedule
                .as_deref()
                .unwrap()
                .strip_prefix("after:")
                .unwrap(),
        )
        .unwrap();
        let window = next.expiration_at.expect("expiry carried over") - start;
        assert!(
            window > Duration::hours(3) - Duration::minutes(1) && window <= Duration::hours(3),
            "{window}"
        );
        // No expiry stays no expiry.
        let b = create(&c, &nt("b"), TaskStatus::Pending).unwrap();
        set_repeat(&c, b.id, Some("every:2h")).unwrap();
        set_status(&c, b.id, TaskStatus::Running).unwrap();
        set_status(&c, b.id, TaskStatus::Completed).unwrap();
        let nb = list(&c).unwrap().into_iter().last().unwrap();
        assert!(nb.expiration_at.is_none());
    }

    #[test]
    fn failing_successor_does_not_fail_the_status_change() {
        let c = db::open_memory().unwrap();
        let a = create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        set_repeat(&c, a.id, Some("every:2h")).unwrap();
        // Stored data the validator now rejects makes creating the copy fail.
        c.execute("UPDATE tasks SET model='--bad' WHERE id=?1", [a.id])
            .unwrap();
        set_status(&c, a.id, TaskStatus::Running).unwrap();
        set_status(&c, a.id, TaskStatus::Completed).expect("status change must succeed");
        assert_eq!(get(&c, a.id).unwrap().status, TaskStatus::Completed);
        assert_eq!(list(&c).unwrap().len(), 1, "no successor was created");
        let log = crate::audit::recent(&c, 10).unwrap();
        assert!(log.iter().any(|e| e.action == "repeat_enqueue_failed"));
    }

    #[test]
    fn cancelled_or_one_off_tasks_do_not_repeat() {
        let c = db::open_memory().unwrap();
        let a = create(&c, &nt("a"), TaskStatus::Pending).unwrap();
        set_repeat(&c, a.id, Some("every:1h")).unwrap();
        cancel(&c, a.id).unwrap();
        assert_eq!(list(&c).unwrap().len(), 1);
        let b = create(&c, &nt("b"), TaskStatus::Pending).unwrap();
        set_status(&c, b.id, TaskStatus::Completed).unwrap();
        assert_eq!(list(&c).unwrap().len(), 2);
        // Failure and expiry keep the chain alive.
        let f = create(&c, &nt("f"), TaskStatus::Pending).unwrap();
        set_repeat(&c, f.id, Some("every:1h")).unwrap();
        set_status(&c, f.id, TaskStatus::Failed).unwrap();
        assert_eq!(list(&c).unwrap().len(), 4);
    }

    #[test]
    fn model_names_are_validated() {
        for ok in [
            "",
            "sonnet",
            "claude-sonnet-4-5",
            "opus[1m]",
            "us.anthropic/claude:1",
        ] {
            assert!(validate_model(ok).is_ok(), "{ok}");
        }
        for bad in [
            "--settings",
            "-x",
            "a b",
            "a\nb",
            "a\0b",
            "x;y",
            &"m".repeat(101),
        ] {
            assert!(validate_model(bad).is_err(), "{bad:?}");
        }
        let n = NewTask {
            name: "t".into(),
            prompt: "p".into(),
            model: "--dangerously-skip-permissions".into(),
            ..Default::default()
        };
        assert!(validate(&n).is_err());
    }
}
