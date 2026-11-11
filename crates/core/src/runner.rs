//! Sequential batch execution. Runs on its own thread with its own DB connection.
//! Every task re-runs the full pre-flight, so a stale snapshot or changed billing mode stops the batch.
use crate::control::Controller;
use crate::engine::{self, preflight, run_process, EndReason, Preflight};
use crate::models::{BillingMode, ConnectionStatus, ExecutionStatus, TaskStatus};
use crate::provider::ProviderAdapter;
use crate::{accounts, audit, db, scheduler, settings, tasks, usage, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Event {
    /// Result of an automatic usage poll for one account (`Err` carries a token-free message).
    UsageFetched {
        account_id: i64,
        result: std::result::Result<(), String>,
    },
    TaskStarted {
        task_id: i64,
    },
    TaskFinished {
        task_id: i64,
        status: ExecutionStatus,
        message: String,
    },
    TaskBlocked {
        task_id: i64,
        reason: String,
    },
    RateLimited {
        account_id: i64,
    },
    Stopped {
        confirmed: bool,
    },
    BatchDone {
        account_id: i64,
        completed: usize,
        /// Tasks the batch was asked to run (so clients can say how many did not complete).
        total: usize,
    },
}

pub struct BatchRequest {
    pub account_id: i64,
    pub task_ids: Vec<i64>,
    /// Launched by the scheduler (true) or by a user click (false). Approval is already granted either way.
    pub automatic: bool,
    /// One-off run-mode override for this batch (highest precedence); `None` = resolve from task/account/global.
    pub mode_override: Option<crate::models::RunMode>,
    /// "Burn now": thresholds, reserve, idle and hours are ignored for this batch only.
    pub burn: bool,
}

pub struct Runtime {
    pub adapters: Arc<Vec<Box<dyn ProviderAdapter>>>,
    pub controller: Arc<Controller>,
    pub env: HashMap<String, String>,
    pub events: Sender<Event>,
}

pub fn run_batch(db_path: &Path, req: &BatchRequest, rt: &Runtime) -> Result<usize> {
    let conn = db::open(db_path)?;
    run_batch_conn(&conn, req, rt)
}

pub fn run_batch_conn(conn: &Connection, req: &BatchRequest, rt: &Runtime) -> Result<usize> {
    rt.controller.set_executing(true);
    let r = run_inner(conn, req, rt);
    rt.controller.set_executing(false);
    let done = *r.as_ref().unwrap_or(&0);
    let _ = rt.events.send(Event::BatchDone {
        account_id: req.account_id,
        completed: done,
        total: req.task_ids.len(),
    });
    r
}

fn run_inner(conn: &Connection, req: &BatchRequest, rt: &Runtime) -> Result<usize> {
    let started = Utc::now();
    let mut completed = 0usize;
    let mut stop_batch = false;
    audit::record(
        conn,
        if req.automatic { "scheduler" } else { "user" },
        "batch_start",
        &format!("account={} tasks={:?}", req.account_id, req.task_ids),
    )?;

    for task_id in &req.task_ids {
        if stop_batch {
            // Unstarted tasks go back to the queue.
            if tasks::get(conn, *task_id)?.status.is_runnable() {
                tasks::set_status(conn, *task_id, TaskStatus::Pending)?;
            }
            continue;
        }
        let settings = settings::load(conn)?;
        let now = Utc::now();
        let task = tasks::get(conn, *task_id)?;
        if !task.status.is_runnable() {
            continue;
        }
        let mut account = accounts::get(conn, req.account_id)?;
        if req.burn {
            // Same switch as the per-account "ignore limits", for this batch only.
            account.limits.ignore_limits = true;
        }
        let Some(adapter) = crate::provider::find(&rt.adapters, &account.provider_type) else {
            let _ = rt.events.send(Event::TaskBlocked {
                task_id: task.id,
                reason: "no adapter installed for this provider".into(),
            });
            stop_batch = true;
            continue;
        };
        let eff = settings.effective_for(&account);
        if started + chrono::Duration::minutes(eff.maximum_execution_minutes as i64) <= now {
            let _ = rt.events.send(Event::TaskBlocked {
                task_id: task.id,
                reason: "batch time limit reached".into(),
            });
            tasks::set_status(conn, task.id, TaskStatus::Pending)?;
            stop_batch = true;
            continue;
        }

        // Fresh billing detection before EVERY execution; the stored mode is only a cache for the UI.
        let det = adapter
            .probe(&account, &rt.env)
            .unwrap_or_else(|| adapter.detect(&account, &rt.env));
        let conn_status = if det.cli_found {
            ConnectionStatus::Connected
        } else {
            ConnectionStatus::Error
        };
        accounts::update_status(conn, account.id, det.billing, conn_status)?;
        let snap = usage::latest(conn, account.id)?;
        let consumed = match &snap {
            Some(s) => scheduler::consumed_since(conn, account.id, s.captured_at)?,
            None => 0.0,
        };
        let blocks = preflight(&Preflight {
            settings: &settings,
            account: &account,
            task: &task,
            snapshot: snap.as_ref(),
            billing: det.billing,
            billing_reasons: det.reasons.clone(),
            cli_found: det.cli_found,
            launches_allowed: rt.controller.can_launch(now),
            automatic: req.automatic,
            approved: true,
            now,
            clock: crate::availability::LocalClock::now(),
            consumed_since_snapshot: consumed,
        });
        let workdir = engine::resolve_workdir(&settings.working_root, &task.working_dir);
        let mut blocks = blocks;
        if let Ok(w) = &workdir {
            let r = adapter.workdir_billing_reasons(w);
            if !r.is_empty() {
                blocks.push(engine::Block::Billing(BillingMode::Api, r));
            }
        }
        let mut reasons: Vec<String> = blocks.iter().map(|b| b.to_string()).collect();
        if let Err(e) = &workdir {
            reasons.push(format!("working directory rejected: {e}"));
        }
        if !reasons.is_empty() {
            let reason = reasons.join("; ");
            let capacity_related = blocks
                .iter()
                .any(|b| matches!(b, engine::Block::Capacity(_)));
            tasks::set_status(
                conn,
                task.id,
                if capacity_related {
                    TaskStatus::WaitingForCapacity
                } else {
                    TaskStatus::Pending
                },
            )?;
            audit::record(
                conn,
                "engine",
                "blocked",
                &format!("task={} {reason}", task.id),
            )?;
            let _ = rt.events.send(Event::TaskBlocked {
                task_id: task.id,
                reason,
            });
            stop_batch = true;
            continue;
        }
        let workdir = workdir.unwrap();

        // Go.
        rt.controller.reset_cancel();
        tasks::set_status(conn, task.id, TaskStatus::Running)?;
        let start = Utc::now();
        conn.execute(
            "INSERT INTO executions (task_id, account_id, started_at, estimated_consumption, status) VALUES (?1,?2,?3,?4,'running')",
            params![task.id, account.id, db::ts(start), task.estimated_consumption],
        )?;
        let exec_id = conn.last_insert_rowid();
        audit::record(
            conn,
            if req.automatic { "scheduler" } else { "user" },
            "task_start",
            &format!("task={} exec={exec_id} account={}", task.id, account.id),
        )?;
        let _ = rt.events.send(Event::TaskStarted { task_id: task.id });

        let spec = adapter.command(
            &account,
            &crate::provider::TaskLaunch {
                model: &task.model,
                prompt: &task.prompt,
                task_id: task.id,
                mode: engine::effective_run_mode(&settings, &account, &task, req.mode_override),
            },
            workdir,
            &rt.env,
        );
        let outcome = run_process(
            &spec,
            Duration::from_secs(task.timeout_secs as u64),
            &rt.controller.cancel_flag(),
        );

        let combined = format!("{}\n{}", outcome.stdout, outcome.stderr);
        let (status, message): (ExecutionStatus, String) = match &outcome.reason {
            EndReason::Exited(0) => (ExecutionStatus::Completed, String::new()),
            EndReason::Exited(code) => {
                if adapter.is_rate_limited(&combined, *code) {
                    (
                        ExecutionStatus::RateLimited,
                        "provider reported a usage/rate limit".into(),
                    )
                } else {
                    (
                        ExecutionStatus::Failed,
                        format!("exit code {code}: {}", last_line(&outcome.stderr)),
                    )
                }
            }
            EndReason::TimedOut => (
                ExecutionStatus::TimedOut,
                format!("timed out after {} s", task.timeout_secs),
            ),
            EndReason::Cancelled => (ExecutionStatus::Cancelled, "cancelled".into()),
            EndReason::SpawnFailed(e) => (ExecutionStatus::Failed, format!("could not start: {e}")),
            EndReason::WaitFailed(e) => (
                ExecutionStatus::Failed,
                format!("lost track of the process ({e}); it was killed"),
            ),
        };

        // Save results as plain text (treated as untrusted data; never executed or rendered as markup).
        // A failure to save (unwritable directory, full disk) must not leave the task stuck in Running:
        // it is recorded in the message and the task/execution are finalized regardless.
        let mut message = message;
        let saved: std::io::Result<Option<String>> = if outcome.stdout.is_empty()
            && outcome.stderr.is_empty()
            || settings.results_dir.is_empty()
        {
            Ok(None)
        } else {
            (|| {
                let d = Path::new(&settings.results_dir);
                std::fs::create_dir_all(d)?;
                let p = d.join(format!(
                    "{}-{}-{}.txt",
                    task.id,
                    exec_id,
                    engine::sanitize_filename(&task.name)
                ));
                let mut body = outcome.stdout.clone();
                if status != ExecutionStatus::Completed && !outcome.stderr.is_empty() {
                    body.push_str("\n--- stderr ---\n");
                    body.push_str(&outcome.stderr);
                }
                std::fs::write(&p, body)?;
                Ok(Some(p.to_string_lossy().into_owned()))
            })()
        };
        let result_path = match saved {
            Ok(p) => p,
            Err(e) => {
                let note = format!("result not saved: {e}");
                message = if message.is_empty() {
                    note
                } else {
                    format!("{message}; {note}")
                };
                None
            }
        };

        conn.execute(
            "UPDATE executions SET completed_at=?1, status=?2, error_message=?3, result_path=?4 WHERE id=?5",
            params![db::ts(Utc::now()), status.as_str(), if message.is_empty() { None } else { Some(&message) }, result_path, exec_id],
        )?;
        if let Some(p) = &result_path {
            tasks::set_result_path(conn, task.id, p)?;
        }

        let new_task_status = match status {
            ExecutionStatus::Completed => TaskStatus::Completed,
            ExecutionStatus::RateLimited => TaskStatus::WaitingForCapacity,
            ExecutionStatus::Cancelled => TaskStatus::Cancelled,
            _ => {
                if task.retry_count < task.retry_max {
                    tasks::increment_retry(conn, task.id)?;
                    TaskStatus::Pending
                } else {
                    TaskStatus::Failed
                }
            }
        };
        tasks::set_status(conn, task.id, new_task_status)?;
        audit::record(
            conn,
            "engine",
            "task_end",
            &format!(
                "task={} exec={exec_id} status={status} terminated_confirmed={}",
                task.id, outcome.terminated_confirmed
            ),
        )?;
        let _ = rt.events.send(Event::TaskFinished {
            task_id: task.id,
            status,
            message: message.clone(),
        });

        match status {
            ExecutionStatus::Completed => completed += 1,
            ExecutionStatus::RateLimited => {
                let _ = rt.events.send(Event::RateLimited {
                    account_id: account.id,
                });
                stop_batch = true;
            }
            ExecutionStatus::Cancelled => {
                let _ = rt.events.send(Event::Stopped {
                    confirmed: outcome.terminated_confirmed,
                });
                stop_batch = true;
            }
            _ => {
                // A failure does not stop independent tasks, but a halted controller does.
                if !rt.controller.can_launch(Utc::now()) {
                    stop_batch = true;
                }
            }
        }
        if !outcome.terminated_confirmed {
            stop_batch = true;
        }
        // Any usage the run consumed is accounted for via `consumed_since` on the next pre-flight.
        if accounts::get(conn, account.id)?.billing_mode != BillingMode::Subscription {
            stop_batch = true;
        }
    }
    audit::record(
        conn,
        "engine",
        "batch_end",
        &format!("account={} completed={completed}", req.account_id),
    )?;
    Ok(completed)
}

fn last_line(s: &str) -> String {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::claude::ClaudeAdapter;
    use crate::models::Confidence;
    use crate::tasks::NewTask;
    use crate::usage::NewSnapshot;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    struct Fx {
        _dir: tempfile::TempDir,
        conn: Connection,
        acct: i64,
        rt: Runtime,
        rx: mpsc::Receiver<Event>,
    }

    /// Builds a fake `claude` binary whose behaviour is chosen by the prompt text on stdin.
    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(
            &bin,
            "#!/bin/sh\nP=$(cat)\ncase \"$P\" in\n *FAIL*) echo boom >&2; exit 3;;\n *LIMIT*) echo 'Claude AI usage limit reached' >&2; exit 1;;\n *SLEEP*) sleep 30;;\n *ARGS*) echo \"args: $@\";;\n *) echo \"done: $P\";;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let profile = dir.path().join("profile");
        std::fs::create_dir(&profile).unwrap();
        std::fs::write(profile.join(".credentials.json"), "{}").unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();

        let conn = db::open_memory().unwrap();
        let mut s = settings::load(&conn).unwrap();
        s.working_root = work.to_string_lossy().into_owned();
        s.results_dir = dir.path().join("results").to_string_lossy().into_owned();
        settings::save(&conn, &s).unwrap();
        let a =
            accounts::create(&conn, "claude", "A", "Max", Some(profile.to_str().unwrap())).unwrap();
        accounts::set_subscription_confirmed(&conn, a.id, true).unwrap();
        let now = Utc::now();
        usage::record(
            &conn,
            &NewSnapshot {
                account_id: a.id,
                session_remaining: Some(95.0),
                weekly_remaining: Some(95.0),
                session_reset_at: Some(now + chrono::Duration::hours(2)),
                weekly_reset_at: Some(now + chrono::Duration::days(2)),
                confidence: Confidence::Manual,
                source: "manual".into(),
            },
            now,
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        let env: HashMap<String, String> = HashMap::from([("PATH".into(), "/usr/bin:/bin".into())]);
        let adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(ClaudeAdapter { binary: bin })];
        Fx {
            _dir: dir,
            conn,
            acct: a.id,
            rt: Runtime {
                adapters: Arc::new(adapters),
                controller: Arc::new(Controller::default()),
                env,
                events: tx,
            },
            rx,
        }
    }

    fn mk(f: &Fx, name: &str, prompt: &str) -> i64 {
        tasks::create(
            &f.conn,
            &NewTask {
                name: name.into(),
                prompt: prompt.into(),
                estimated_consumption: 5.0,
                timeout_secs: 20,
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
        .id
    }
    fn req(f: &Fx, ids: Vec<i64>) -> BatchRequest {
        BatchRequest {
            account_id: f.acct,
            task_ids: ids,
            automatic: false,
            mode_override: None,
            burn: false,
        }
    }

    #[test]
    fn runs_sequentially_and_saves_results() {
        let f = fx();
        let (a, b) = (mk(&f, "a", "hello"), mk(&f, "b", "world"));
        let n = run_batch_conn(&f.conn, &req(&f, vec![a, b]), &f.rt).unwrap();
        assert_eq!(n, 2);
        let t = tasks::get(&f.conn, a).unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert_eq!(
            std::fs::read_to_string(t.result_path.unwrap())
                .unwrap()
                .trim(),
            "done: hello"
        );
        let execs: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM executions WHERE status='completed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(execs, 2);
    }

    fn result_of(f: &Fx, id: i64) -> String {
        std::fs::read_to_string(tasks::get(&f.conn, id).unwrap().result_path.unwrap())
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn run_mode_reaches_the_cli_with_precedence() {
        use crate::models::RunMode;
        let f = fx();
        let a = mk(&f, "a", "ARGS");
        run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap();
        assert!(
            !result_of(&f, a).contains("--permission-mode"),
            "default passes nothing"
        );

        let mut st = settings::load(&f.conn).unwrap();
        st.default_run_mode = RunMode::Plan;
        settings::save(&f.conn, &st).unwrap();
        let b = mk(&f, "b", "ARGS");
        run_batch_conn(&f.conn, &req(&f, vec![b]), &f.rt).unwrap();
        assert!(result_of(&f, b).contains("--permission-mode plan"));

        accounts::set_run_mode(&f.conn, f.acct, RunMode::AutoEdit).unwrap();
        let c = mk(&f, "c", "ARGS");
        run_batch_conn(&f.conn, &req(&f, vec![c]), &f.rt).unwrap();
        assert!(result_of(&f, c).contains("--permission-mode acceptEdits"));

        let d = mk(&f, "d", "ARGS");
        let mut r = req(&f, vec![d]);
        r.mode_override = Some(RunMode::Review);
        run_batch_conn(&f.conn, &r, &f.rt).unwrap();
        assert!(result_of(&f, d).contains("--permission-mode default"));
    }

    #[test]
    fn unavailable_account_launches_nothing_even_when_user_initiated() {
        let f = fx();
        accounts::set_available(&f.conn, f.acct, false).unwrap();
        let a = mk(&f, "a", "hello");
        run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap();
        assert_eq!(tasks::get(&f.conn, a).unwrap().status, TaskStatus::Pending);
        let execs: i64 = f
            .conn
            .query_row("SELECT COUNT(*) FROM executions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(execs, 0);
        let blocked: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE action='blocked' AND detail LIKE '%unavailable%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(blocked, 1);
    }

    #[test]
    fn failure_is_recorded_and_retry_policy_applies() {
        let f = fx();
        let a = mk(&f, "a", "FAIL");
        run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap();
        assert_eq!(tasks::get(&f.conn, a).unwrap().status, TaskStatus::Failed);
        let msg: String = f
            .conn
            .query_row("SELECT error_message FROM executions", [], |r| r.get(0))
            .unwrap();
        assert!(msg.contains("boom"), "{msg}");
        tasks::retry(&f.conn, a).unwrap();
        assert_eq!(tasks::get(&f.conn, a).unwrap().status, TaskStatus::Pending);
    }

    #[test]
    fn rate_limit_stops_the_batch() {
        let f = fx();
        let (a, b) = (mk(&f, "a", "LIMIT"), mk(&f, "b", "ok"));
        run_batch_conn(&f.conn, &req(&f, vec![a, b]), &f.rt).unwrap();
        assert_eq!(
            tasks::get(&f.conn, a).unwrap().status,
            TaskStatus::WaitingForCapacity
        );
        assert_eq!(
            tasks::get(&f.conn, b).unwrap().status,
            TaskStatus::Pending,
            "second task not started"
        );
        assert!(f
            .rx
            .try_iter()
            .any(|e| matches!(e, Event::RateLimited { .. })));
    }

    #[test]
    fn unwritable_results_dir_does_not_leave_task_running() {
        let f = fx();
        // results_dir lives below a regular file, so create_dir_all fails (even when running as root).
        let blocker = f._dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let mut st = settings::load(&f.conn).unwrap();
        st.results_dir = blocker.join("results").to_string_lossy().into_owned();
        settings::save(&f.conn, &st).unwrap();
        let a = mk(&f, "a", "ok");
        run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap();
        let t = tasks::get(&f.conn, a).unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert!(t.result_path.is_none());
        let (status, msg, path): (String, Option<String>, Option<String>) = f
            .conn
            .query_row(
                "SELECT status, error_message, result_path FROM executions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "completed");
        assert!(msg.unwrap().contains("result not saved"));
        assert!(path.is_none());
        let ended: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE action='task_end'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ended, 1);
    }

    #[test]
    fn project_settings_with_api_billing_block_the_task() {
        let f = fx();
        let st = settings::load(&f.conn).unwrap();
        let claude = Path::new(&st.working_root).join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"apiKeyHelper":"/bin/key"}"#,
        )
        .unwrap();
        let a = mk(&f, "a", "ok");
        run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap();
        assert_eq!(tasks::get(&f.conn, a).unwrap().status, TaskStatus::Pending);
        let n: i64 = f
            .conn
            .query_row("SELECT COUNT(*) FROM executions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "nothing was launched");
    }

    #[test]
    fn stale_usage_blocks_the_run_and_launches_nothing() {
        let f = fx();
        // A reading older than max_usage_age (30 min by default), e.g. after failed polls.
        f.conn
            .execute(
                "UPDATE usage_snapshots SET captured_at = ?1",
                [db::ts(Utc::now() - chrono::Duration::hours(3))],
            )
            .unwrap();
        let a = mk(&f, "a", "ok");
        let n = run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            tasks::get(&f.conn, a).unwrap().status,
            TaskStatus::WaitingForCapacity
        );
        let execs: i64 = f
            .conn
            .query_row("SELECT COUNT(*) FROM executions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(execs, 0, "no process was started");
        let blocked: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE action='blocked'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(blocked >= 1, "block is audited");
    }

    #[test]
    fn timeout_is_recorded() {
        let f = fx();
        let a = tasks::create(
            &f.conn,
            &NewTask {
                name: "a".into(),
                prompt: "SLEEP".into(),
                timeout_secs: 1,
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
        .id;
        run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap();
        let s: String = f
            .conn
            .query_row("SELECT status FROM executions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(s, "timed_out");
        assert_eq!(tasks::get(&f.conn, a).unwrap().status, TaskStatus::Failed);
    }

    #[test]
    fn emergency_stop_terminates_active_process_and_blocks_next() {
        let f = fx();
        let (a, b) = (mk(&f, "a", "SLEEP"), mk(&f, "b", "ok"));
        let ctl = f.rt.controller.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            ctl.emergency_stop();
        });
        run_batch_conn(&f.conn, &req(&f, vec![a, b]), &f.rt).unwrap();
        h.join().unwrap();
        assert_eq!(
            tasks::get(&f.conn, a).unwrap().status,
            TaskStatus::Cancelled
        );
        assert_eq!(tasks::get(&f.conn, b).unwrap().status, TaskStatus::Pending);
        let evs: Vec<_> = f.rx.try_iter().collect();
        assert!(evs
            .iter()
            .any(|e| matches!(e, Event::Stopped { confirmed: true })));
        // after STOP nothing can launch
        let c = mk(&f, "c", "ok");
        run_batch_conn(&f.conn, &req(&f, vec![c]), &f.rt).unwrap();
        assert_eq!(tasks::get(&f.conn, c).unwrap().status, TaskStatus::Pending);
    }

    #[test]
    fn api_key_in_environment_blocks_execution() {
        let mut f = fx();
        f.rt.env
            .insert("ANTHROPIC_API_KEY".into(), "sk-test".into());
        let a = mk(&f, "a", "hello");
        let n = run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap();
        assert_eq!(n, 0);
        assert_ne!(
            tasks::get(&f.conn, a).unwrap().status,
            TaskStatus::Completed
        );
        assert_eq!(
            accounts::get(&f.conn, f.acct).unwrap().billing_mode,
            BillingMode::Api
        );
        let execs: i64 = f
            .conn
            .query_row("SELECT COUNT(*) FROM executions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(execs, 0, "process never launched");
    }

    #[test]
    fn unverifiable_billing_blocks_execution() {
        let f = fx();
        accounts::set_subscription_confirmed(&f.conn, f.acct, false).unwrap();
        let a = mk(&f, "a", "hello");
        assert_eq!(
            run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap(),
            0
        );
        assert_eq!(
            accounts::get(&f.conn, f.acct).unwrap().billing_mode,
            BillingMode::Unverified
        );
    }

    #[test]
    fn stale_usage_blocks_execution() {
        let f = fx();
        f.conn
            .execute(
                "UPDATE usage_snapshots SET captured_at = ?1",
                [db::ts(Utc::now() - chrono::Duration::hours(5))],
            )
            .unwrap();
        let a = mk(&f, "a", "hello");
        assert_eq!(
            run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap(),
            0
        );
        assert_eq!(
            tasks::get(&f.conn, a).unwrap().status,
            TaskStatus::WaitingForCapacity
        );
    }

    #[test]
    fn missing_usage_blocks_execution() {
        let f = fx();
        f.conn.execute("DELETE FROM usage_snapshots", []).unwrap();
        let a = mk(&f, "a", "hello");
        assert_eq!(
            run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap(),
            0
        );
    }

    #[test]
    fn consumption_accumulates_across_batches_until_usage_refreshed() {
        let f = fx();
        // 95% left, threshold 65, reserve 20 => 75% usable. Each task estimates 40%.
        let ids: Vec<i64> = (0..3)
            .map(|i| {
                tasks::create(
                    &f.conn,
                    &NewTask {
                        name: format!("t{i}"),
                        prompt: "ok".into(),
                        estimated_consumption: 40.0,
                        ..Default::default()
                    },
                    TaskStatus::Pending,
                )
                .unwrap()
                .id
            })
            .collect();
        let n = run_batch_conn(&f.conn, &req(&f, ids.clone()), &f.rt).unwrap();
        assert_eq!(
            n, 1,
            "second task would exceed remaining capacity based on estimates"
        );
        assert_eq!(
            tasks::get(&f.conn, ids[1]).unwrap().status,
            TaskStatus::WaitingForCapacity
        );
    }

    #[test]
    fn working_dir_outside_root_is_rejected() {
        let f = fx();
        let a = tasks::create(
            &f.conn,
            &NewTask {
                name: "a".into(),
                prompt: "ok".into(),
                working_dir: "/etc".into(),
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
        .id;
        assert_eq!(
            run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap(),
            0
        );
    }

    #[test]
    fn dependencies_and_simultaneous_batches_are_sequential() {
        let f = fx();
        let a = mk(&f, "a", "one");
        let b = mk(&f, "b", "two");
        tasks::set_dependencies(&f.conn, b, &[a]).unwrap();
        assert_eq!(tasks::ready(&f.conn, Utc::now()).unwrap().len(), 1);
        run_batch_conn(&f.conn, &req(&f, vec![a]), &f.rt).unwrap();
        assert_eq!(tasks::ready(&f.conn, Utc::now()).unwrap()[0].id, b);
    }
}
