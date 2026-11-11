//! History, estimates vs measurements, forecasts and exports. Estimates are always labelled as such.
use crate::models::Confidence;
use crate::{db, usage, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HistoryRow {
    pub execution_id: i64,
    pub task_id: i64,
    pub task_name: String,
    pub provider: String,
    pub account: String,
    pub model: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub duration_secs: Option<i64>,
    pub estimated_consumption: Option<f64>,
    /// Measured (user-entered) consumption; `None` means unknown, not zero.
    pub actual_consumption: Option<f64>,
    pub status: String,
    pub error: Option<String>,
    pub result_path: Option<String>,
}

pub fn history(conn: &Connection, limit: usize) -> Result<Vec<HistoryRow>> {
    let mut st = conn.prepare(
        "SELECT e.id, e.task_id, t.name, COALESCE(p.provider_type, ''), COALESCE(a.label, ''), t.model,
                e.started_at, e.completed_at, e.estimated_consumption, e.actual_consumption, e.status,
                e.error_message, e.result_path
         FROM executions e JOIN tasks t ON t.id = e.task_id
         LEFT JOIN accounts a ON a.id = e.account_id LEFT JOIN providers p ON p.id = a.provider_id
         ORDER BY e.id DESC LIMIT ?1",
    )?;
    let rows = st
        .query_map([limit as i64], |r| {
            let started: String = r.get(6)?;
            let completed: Option<String> = r.get(7)?;
            let dur = match (
                db::parse_ts(&started),
                completed.as_deref().and_then(db::parse_ts),
            ) {
                (Some(s), Some(c)) => Some((c - s).num_seconds()),
                _ => None,
            };
            Ok(HistoryRow {
                execution_id: r.get(0)?,
                task_id: r.get(1)?,
                task_name: r.get(2)?,
                provider: r.get(3)?,
                account: r.get(4)?,
                model: r.get(5)?,
                started_at: started,
                completed_at: completed,
                duration_secs: dur,
                estimated_consumption: r.get(8)?,
                actual_consumption: r.get(9)?,
                status: r.get(10)?,
                error: r.get(11)?,
                result_path: r.get(12)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// Records measured consumption for an execution. Returns true if it exceeded the task's maximum budget.
pub fn record_actual(conn: &Connection, execution_id: i64, actual: f64) -> Result<bool> {
    if !(0.0..=100.0).contains(&actual) {
        return Err(crate::Error::Invalid("consumption must be 0-100".into()));
    }
    let changed = conn.execute(
        "UPDATE executions SET actual_consumption=?1 WHERE id=?2",
        params![actual, execution_id],
    )?;
    if changed == 0 {
        return Err(crate::Error::NotFound(format!(
            "execution {execution_id} (see History for valid runs)"
        )));
    }
    let over: Option<f64> = conn.query_row(
        "SELECT t.maximum_budget FROM executions e JOIN tasks t ON t.id=e.task_id WHERE e.id=?1",
        [execution_id],
        |r| r.get(0),
    )?;
    let over = over.is_some_and(|b| actual > b);
    if over {
        crate::audit::record(
            conn,
            "engine",
            "over_budget",
            &format!("execution={execution_id} actual={actual}"),
        )?;
    }
    Ok(over)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConsumptionEstimate {
    pub value: f64,
    /// Number of measured runs backing the estimate (0 = pure default).
    pub samples: usize,
    pub confidence: &'static str,
}

/// Suggests a consumption estimate for a model from past *measured* runs; otherwise falls back to the default.
pub fn suggest_estimate(
    conn: &Connection,
    model: &str,
    default: f64,
) -> Result<ConsumptionEstimate> {
    let mut st = conn.prepare(
        "SELECT e.actual_consumption FROM executions e JOIN tasks t ON t.id=e.task_id
         WHERE e.actual_consumption IS NOT NULL AND t.model=?1 ORDER BY e.id DESC LIMIT 20",
    )?;
    let v: Vec<f64> = st
        .query_map([model], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if v.is_empty() {
        return Ok(ConsumptionEstimate {
            value: default,
            samples: 0,
            confidence: "none (default)",
        });
    }
    let avg = v.iter().sum::<f64>() / v.len() as f64;
    Ok(ConsumptionEstimate {
        value: avg,
        samples: v.len(),
        confidence: if v.len() >= 5 { "medium" } else { "low" },
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Forecast {
    pub account_id: i64,
    /// Weekly capacity that will likely expire unused (percentage points). An estimate.
    pub unused_weekly_estimate: Option<f64>,
    pub hours_to_weekly_reset: Option<i64>,
    pub confidence: String,
    pub note: String,
}

/// Naive forecast: remaining weekly capacity minus the reserve, only when the data is fresh.
/// Always flagged as an estimate and never usable as an authoritative reading.
pub fn forecast(
    conn: &Connection,
    account_id: i64,
    reserved: f64,
    max_age_min: i64,
    now: DateTime<Utc>,
) -> Result<Forecast> {
    let Some(s) = usage::latest(conn, account_id)? else {
        return Ok(Forecast {
            account_id,
            unused_weekly_estimate: None,
            hours_to_weekly_reset: None,
            confidence: "none".into(),
            note: "no usage data".into(),
        });
    };
    let age = (now - s.captured_at).num_minutes();
    let hours = s.weekly_reset_at.map(|r| (r - now).num_hours());
    if s.confidence == Confidence::Unknown || age > max_age_min || s.weekly_remaining.is_none() {
        return Ok(Forecast {
            account_id,
            unused_weekly_estimate: None,
            hours_to_weekly_reset: hours,
            confidence: "none".into(),
            note: "usage data stale or incomplete".into(),
        });
    }
    let unused = (s.weekly_remaining.unwrap() - reserved).max(0.0);
    Ok(Forecast {
        account_id,
        unused_weekly_estimate: Some(unused),
        hours_to_weekly_reset: hours,
        confidence: if s.confidence == Confidence::Provider {
            "high".into()
        } else {
            "low (manual entry)".into()
        },
        note: "Estimate: assumes no other use before the weekly reset.".into(),
    })
}

pub fn export_history_json(rows: &[HistoryRow]) -> Result<String> {
    Ok(serde_json::to_string_pretty(rows)?)
}

fn csv_escape(s: &str) -> String {
    // Also neutralise spreadsheet formula injection from untrusted text.
    let s = if s.starts_with(['=', '+', '-', '@']) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

pub fn export_history_csv(rows: &[HistoryRow]) -> String {
    let mut out = String::from("execution_id,task,provider,account,model,started_at,duration_secs,estimated_consumption,actual_consumption,status,error\n");
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            r.execution_id,
            csv_escape(&r.task_name),
            csv_escape(&r.provider),
            csv_escape(&r.account),
            csv_escape(&r.model),
            r.started_at,
            r.duration_secs.map(|d| d.to_string()).unwrap_or_default(),
            r.estimated_consumption
                .map(|d| d.to_string())
                .unwrap_or_default(),
            r.actual_consumption
                .map(|d| d.to_string())
                .unwrap_or_default(),
            r.status,
            csv_escape(r.error.as_deref().unwrap_or("")),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TaskStatus;
    use crate::tasks::{self, NewTask};
    use crate::usage::NewSnapshot;
    use chrono::Duration;

    fn exec(c: &Connection, task: i64, status: &str) -> i64 {
        c.execute(
            "INSERT INTO executions (task_id, started_at, completed_at, estimated_consumption, status) VALUES (?1,?2,?3,5,?4)",
            params![task, db::ts(Utc::now() - Duration::seconds(90)), db::ts(Utc::now()), status],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    #[test]
    fn history_distinguishes_estimate_from_actual() {
        let c = db::open_memory().unwrap();
        let t = tasks::create(
            &c,
            &NewTask {
                name: "a".into(),
                prompt: "p".into(),
                maximum_budget: Some(6.0),
                ..Default::default()
            },
            TaskStatus::Completed,
        )
        .unwrap();
        let e = exec(&c, t.id, "completed");
        let h = history(&c, 10).unwrap();
        assert_eq!(h[0].estimated_consumption, Some(5.0));
        assert_eq!(h[0].actual_consumption, None);
        assert!(h[0].duration_secs.unwrap() >= 89);
        assert!(record_actual(&c, e, 9.0).unwrap(), "over budget is flagged");
        assert_eq!(history(&c, 10).unwrap()[0].actual_consumption, Some(9.0));
        assert!(record_actual(&c, e, 150.0).is_err());
    }

    #[test]
    fn suggestions_use_only_measured_runs() {
        let c = db::open_memory().unwrap();
        assert_eq!(suggest_estimate(&c, "sonnet", 5.0).unwrap().samples, 0);
        let t = tasks::create(
            &c,
            &NewTask {
                name: "a".into(),
                prompt: "p".into(),
                model: "sonnet".into(),
                ..Default::default()
            },
            TaskStatus::Completed,
        )
        .unwrap();
        let e = exec(&c, t.id, "completed");
        record_actual(&c, e, 8.0).unwrap();
        let s = suggest_estimate(&c, "sonnet", 5.0).unwrap();
        assert_eq!((s.value, s.samples, s.confidence), (8.0, 1, "low"));
    }

    #[test]
    fn forecast_requires_fresh_data_and_is_labelled() {
        let c = db::open_memory().unwrap();
        let a = crate::accounts::create(&c, "claude", "A", "Pro", None).unwrap();
        let now = Utc::now();
        assert_eq!(
            forecast(&c, a.id, 20.0, 30, now)
                .unwrap()
                .unused_weekly_estimate,
            None
        );
        usage::record(
            &c,
            &NewSnapshot {
                account_id: a.id,
                session_remaining: Some(50.0),
                weekly_remaining: Some(60.0),
                session_reset_at: None,
                weekly_reset_at: Some(now + Duration::hours(30)),
                confidence: Confidence::Manual,
                source: "manual".into(),
            },
            now - Duration::minutes(3),
        )
        .unwrap();
        let f = forecast(&c, a.id, 20.0, 30, now).unwrap();
        assert_eq!(f.unused_weekly_estimate, Some(40.0));
        assert!(f.confidence.contains("manual"));
        assert_eq!(
            forecast(&c, a.id, 20.0, 30, now + Duration::hours(2))
                .unwrap()
                .unused_weekly_estimate,
            None
        );
    }

    #[test]
    fn exports_and_csv_injection_guard() {
        let c = db::open_memory().unwrap();
        let t = tasks::create(
            &c,
            &NewTask {
                name: "=HYPERLINK(\"x\")".into(),
                prompt: "p".into(),
                ..Default::default()
            },
            TaskStatus::Completed,
        )
        .unwrap();
        exec(&c, t.id, "completed");
        let rows = history(&c, 10).unwrap();
        let csv = export_history_csv(&rows);
        assert!(csv.contains("'=HYPERLINK"));
        assert!(export_history_json(&rows).unwrap().contains("execution_id"));
    }

    #[test]
    fn record_actual_for_an_unknown_execution_is_not_found() {
        let c = crate::db::open_memory().unwrap();
        match record_actual(&c, 424242, 3.0) {
            Err(crate::Error::NotFound(m)) => assert!(m.contains("424242"), "{m}"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
