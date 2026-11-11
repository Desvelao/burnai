//! Task templates: named sets of task defaults. They never carry a target account, schedule,
//! expiry or dependencies (those belong to one concrete task).
use crate::models::RunMode;
use crate::tasks::NewTask;
use crate::{db, Error, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub id: i64,
    pub name: String,
    /// Task defaults; `account_id`, `schedule` and `expiration_at` are always unset.
    pub task: NewTask,
}

const COLS: &str = "id, name, prompt, model, estimated_consumption, estimated_minutes, description,
    priority, run_mode, maximum_budget, timeout_secs, retry_max, working_dir, requires_approval,
    provider_type, allow_fallback";

fn map(r: &rusqlite::Row) -> rusqlite::Result<Template> {
    Ok(Template {
        id: r.get(0)?,
        name: r.get(1)?,
        task: NewTask {
            name: String::new(),
            prompt: r.get(2)?,
            model: r.get(3)?,
            estimated_consumption: r.get(4)?,
            estimated_minutes: r.get(5)?,
            description: r.get(6)?,
            priority: r.get(7)?,
            run_mode: RunMode::parse(&r.get::<_, String>(8)?).unwrap_or_default(),
            maximum_budget: r.get(9)?,
            timeout_secs: r.get(10)?,
            retry_max: r.get(11)?,
            working_dir: r.get(12)?,
            requires_approval: r.get(13)?,
            provider_type: r.get(14)?,
            allow_fallback: r.get(15)?,
            account_id: None,
            schedule: None,
            expiration_at: None,
        },
    })
}

fn clean_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 {
        return Err(Error::Invalid(
            "template name must be 1-64 characters".into(),
        ));
    }
    Ok(n.to_string())
}

/// The same checks a task gets, applied to the template's defaults.
fn validate(name: &str, t: &NewTask) -> Result<()> {
    let mut probe = t.clone();
    probe.name = name.to_string();
    probe.account_id = None;
    probe.schedule = None;
    probe.expiration_at = None;
    crate::tasks::validate(&probe)
}

pub fn list(conn: &Connection) -> Result<Vec<Template>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM task_templates ORDER BY name COLLATE NOCASE"
    ))?;
    let v = st
        .query_map([], map)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

pub fn get(conn: &Connection, id: i64) -> Result<Template> {
    conn.query_row(
        &format!("SELECT {COLS} FROM task_templates WHERE id=?1"),
        [id],
        map,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("template {id}")))
}

/// Creates a template, or overwrites the one with the same name. Returns its id.
pub fn save(conn: &Connection, name: &str, t: &NewTask) -> Result<i64> {
    let name = clean_name(name)?;
    validate(&name, t)?;
    conn.execute(
        "INSERT INTO task_templates (name, prompt, model, estimated_consumption, estimated_minutes,
            description, priority, run_mode, maximum_budget, timeout_secs, retry_max, working_dir,
            requires_approval, provider_type, allow_fallback, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
         ON CONFLICT(name) DO UPDATE SET prompt=excluded.prompt, model=excluded.model,
            estimated_consumption=excluded.estimated_consumption,
            estimated_minutes=excluded.estimated_minutes, description=excluded.description,
            priority=excluded.priority, run_mode=excluded.run_mode,
            maximum_budget=excluded.maximum_budget, timeout_secs=excluded.timeout_secs,
            retry_max=excluded.retry_max, working_dir=excluded.working_dir,
            requires_approval=excluded.requires_approval, provider_type=excluded.provider_type,
            allow_fallback=excluded.allow_fallback",
        params![
            name,
            t.prompt,
            t.model,
            t.estimated_consumption,
            t.estimated_minutes,
            t.description,
            t.priority,
            t.run_mode.as_str(),
            t.maximum_budget,
            t.timeout_secs,
            t.retry_max,
            t.working_dir,
            t.requires_approval,
            t.provider_type,
            t.allow_fallback,
            db::ts(Utc::now())
        ],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM task_templates WHERE name=?1",
        [&name],
        |r| r.get(0),
    )?)
}

/// Edits template `id`, including renaming it. A name used by another template is rejected.
pub fn update(conn: &Connection, id: i64, name: &str, t: &NewTask) -> Result<()> {
    let name = clean_name(name)?;
    validate(&name, t)?;
    let taken: Option<i64> = conn
        .query_row(
            "SELECT id FROM task_templates WHERE name=?1 AND id<>?2",
            params![name, id],
            |r| r.get(0),
        )
        .optional()?;
    if taken.is_some() {
        return Err(Error::Invalid(format!(
            "a template named “{name}” already exists"
        )));
    }
    let n = conn.execute(
        "UPDATE task_templates SET name=?1, prompt=?2, model=?3, estimated_consumption=?4,
            estimated_minutes=?5, description=?6, priority=?7, run_mode=?8, maximum_budget=?9,
            timeout_secs=?10, retry_max=?11, working_dir=?12, requires_approval=?13,
            provider_type=?14, allow_fallback=?15 WHERE id=?16",
        params![
            name,
            t.prompt,
            t.model,
            t.estimated_consumption,
            t.estimated_minutes,
            t.description,
            t.priority,
            t.run_mode.as_str(),
            t.maximum_budget,
            t.timeout_secs,
            t.retry_max,
            t.working_dir,
            t.requires_approval,
            t.provider_type,
            t.allow_fallback,
            id
        ],
    )?;
    if n == 0 {
        return Err(Error::NotFound(format!("template {id}")));
    }
    Ok(())
}

/// Copies a template under a free name ("x (copy)", "x (copy 2)", …). Returns the new id.
pub fn duplicate(conn: &Connection, id: i64) -> Result<i64> {
    let t = get(conn, id)?;
    let names: Vec<String> = list(conn)?.into_iter().map(|t| t.name).collect();
    let free = |n: &str| !names.iter().any(|x| x.eq_ignore_ascii_case(n));
    let mut name = format!("{} (copy)", t.name);
    let mut i = 2;
    while !free(&name) {
        name = format!("{} (copy {i})", t.name);
        i += 1;
    }
    save(conn, &name, &t.task)
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    let n = conn.execute("DELETE FROM task_templates WHERE id=?1", [id])?;
    if n == 0 {
        return Err(Error::NotFound(format!("template {id}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(prompt: &str) -> NewTask {
        NewTask {
            prompt: prompt.into(),
            model: "sonnet".into(),
            ..Default::default()
        }
    }

    #[test]
    fn save_is_an_upsert_by_name_and_stores_full_defaults() {
        let c = db::open_memory().unwrap();
        let mut t = task("write tests");
        t.priority = 2;
        t.run_mode = RunMode::Plan;
        t.maximum_budget = Some(15.0);
        t.retry_max = 2;
        t.timeout_secs = 600;
        t.working_dir = "proj".into();
        t.requires_approval = false;
        t.provider_type = None;
        t.description = "d".into();
        let id = save(&c, "tests", &t).unwrap();
        let id2 = save(&c, "tests", &task("write more tests")).unwrap();
        assert_eq!(id, id2);
        let all = list(&c).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].task.prompt, "write more tests");
        let id3 = save(&c, "full", &t).unwrap();
        let got = get(&c, id3).unwrap();
        assert_eq!(got.task.priority, 2);
        assert_eq!(got.task.run_mode, RunMode::Plan);
        assert_eq!(got.task.maximum_budget, Some(15.0));
        assert_eq!(got.task.provider_type, None, "NULL = any provider");
        assert!(!got.task.requires_approval);
        assert_eq!((got.task.retry_max, got.task.timeout_secs), (2, 600));
        assert_eq!(got.task.account_id, None);
        assert_eq!(got.task.schedule, None);
    }

    #[test]
    fn validation_rejects_bad_templates() {
        let c = db::open_memory().unwrap();
        assert!(save(&c, "", &task("x")).is_err());
        assert!(save(&c, &"n".repeat(65), &task("x")).is_err());
        assert!(save(&c, "a", &task("  ")).is_err(), "prompt required");
        let mut bad = task("x");
        bad.estimated_consumption = 150.0;
        assert!(save(&c, "a", &bad).is_err());
        assert!(list(&c).unwrap().is_empty());
    }

    #[test]
    fn rename_edit_and_conflicts() {
        let c = db::open_memory().unwrap();
        let a = save(&c, "a", &task("pa")).unwrap();
        let b = save(&c, "b", &task("pb")).unwrap();
        update(&c, a, "alpha", &task("new prompt")).unwrap();
        assert_eq!(get(&c, a).unwrap().name, "alpha");
        assert_eq!(get(&c, a).unwrap().task.prompt, "new prompt");
        let e = update(&c, b, "alpha", &task("pb")).unwrap_err();
        assert!(e.to_string().contains("already exists"));
        update(&c, b, "b", &task("pb2")).unwrap();
        assert!(matches!(
            update(&c, 999, "z", &task("x")),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn duplicate_picks_a_free_name_and_delete_removes() {
        let c = db::open_memory().unwrap();
        let a = save(&c, "base", &task("p")).unwrap();
        let d1 = duplicate(&c, a).unwrap();
        let d2 = duplicate(&c, a).unwrap();
        assert_eq!(get(&c, d1).unwrap().name, "base (copy)");
        assert_eq!(get(&c, d2).unwrap().name, "base (copy 2)");
        assert_eq!(list(&c).unwrap().len(), 3);
        delete(&c, d1).unwrap();
        assert!(matches!(get(&c, d1), Err(Error::NotFound(_))));
        assert!(delete(&c, d1).is_err());
    }

    #[test]
    fn rows_from_before_the_migration_still_load_with_defaults() {
        let c = db::open_memory().unwrap();
        c.execute(
            "INSERT INTO task_templates (name, prompt, model, estimated_consumption, estimated_minutes, created_at)
             VALUES ('old','p','m',4,10,'2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let t = &list(&c).unwrap()[0];
        assert_eq!(t.task.priority, 1);
        assert_eq!(t.task.run_mode, RunMode::Inherit);
        assert_eq!(t.task.provider_type.as_deref(), Some("claude"));
        assert!(t.task.requires_approval);
    }
}
