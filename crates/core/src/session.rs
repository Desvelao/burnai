//! Front-end-agnostic session glue shared by the clients and the daemon: data directory, bootstrap,
//! state snapshot, event -> status message mapping and tick pacing. Contains no UI types.
use crate::models::Account;
use crate::provider::{self, Detection};
use crate::runner::Event;
use crate::scheduler::IdleSource;
use crate::service::Service;
use crate::settings::{self, Settings};
use crate::tasks::{self, Task};
use crate::{accounts, notify};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// `PLAN_BURNER_DATA`, else `$XDG_DATA_HOME/<slug>`, else `~/.local/share/<slug>` (slug from `brand.toml`).
pub fn data_dir() -> PathBuf {
    data_dir_from(|k| std::env::var(k).ok())
}

fn data_dir_from(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(d) = get("PLAN_BURNER_DATA") {
        return PathBuf::from(d);
    }
    let base = get("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(get("HOME").unwrap_or_else(|| ".".into())).join(".local/share")
    });
    base.join(crate::brand::SLUG)
}

/// Idle time via `xprintidle` if installed; otherwise unknown (which never satisfies an idle requirement).
pub struct SystemIdle;
impl IdleSource for SystemIdle {
    fn idle_minutes(&self) -> Option<u32> {
        let out = std::process::Command::new("xprintidle").output().ok()?;
        let ms: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some((ms / 60_000) as u32)
    }
}

/// Take the exclusive "driver" lock in `dir` so only one front-end process runs the scheduler and batches
/// against the shared DB. `None` = another process holds it. The lock is released when the file is dropped
/// (or the process exits, even on a crash).
pub fn acquire_driver_lock(dir: &std::path::Path) -> Option<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("driver.lock"))
        .ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: plain flock(2) on a valid fd we own.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return None;
        }
    }
    Some(f)
}

/// Open the DB under [`data_dir`] and default the results dir. Takes no lock.
fn open_base() -> std::result::Result<(Service, Settings, PathBuf), Box<dyn std::error::Error>> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let env: HashMap<String, String> = std::env::vars().collect();
    let svc = Service::new(
        dir.join(crate::brand::DB_FILE),
        provider::registry(),
        env,
        Arc::new(notify::SystemSinks),
    )?;
    let mut s = settings::load(&svc.conn)?;
    if s.results_dir.is_empty() {
        s.results_dir = dir.join("results").to_string_lossy().into_owned();
        settings::save(&svc.conn, &s)?;
    }
    tracing::info!("data dir: {}", dir.display());
    Ok((svc, s, dir))
}

/// Daemon only: take the exclusive driver lock, serve the control socket and poll usage once.
/// Fails when another daemon already owns the scheduler. No other process may call this.
pub fn open_driver_service() -> std::result::Result<(Service, Settings), Box<dyn std::error::Error>>
{
    let (mut svc, s, dir) = open_base()?;
    let Some(lock) = acquire_driver_lock(&dir) else {
        return Err(format!(
            "another {} scheduler daemon is already running",
            crate::brand::NAME
        )
        .into());
    };
    svc.set_driver_lock(Some(lock));
    svc.restore_control_state()?;
    svc.start_ipc_server();
    let _ = svc.refresh_all_usage();
    Ok((svc, s))
}

/// Every front-end (GUI, TUI, tray, CLI): never takes the lock and never runs the scheduler. Reads the DB
/// directly and forwards writes and actions to the daemon over the control socket.
pub fn open_client_service() -> std::result::Result<(Service, Settings), Box<dyn std::error::Error>>
{
    let (mut svc, s, _) = open_base()?;
    svc.set_driver_lock(None);
    svc.refresh_remote();
    Ok((svc, s))
}

/// Read-only view of the state a front-end renders.
#[derive(Default)]
pub struct Snapshot {
    pub accounts: Vec<Account>,
    pub tasks: Vec<Task>,
    pub dets: HashMap<i64, Detection>,
}

/// Reload what can be loaded; a failed read keeps the previous value in `prev`.
pub fn snapshot(svc: &Service, prev: Snapshot) -> Snapshot {
    Snapshot {
        accounts: accounts::list(&svc.conn).unwrap_or(prev.accounts),
        tasks: tasks::list(&svc.conn).unwrap_or(prev.tasks),
        dets: svc.detections().unwrap_or(prev.dets),
    }
}

/// Human status line for an event: `(message, is_ok)`. `None` = nothing to show.
pub fn event_status(e: &Event) -> Option<(String, bool)> {
    match e {
        Event::TaskFinished {
            status, message, ..
        } => Some((
            format!("Task finished: {status} {message}"),
            status.as_str() == "completed",
        )),
        Event::TaskBlocked { reason, .. } => Some((format!("Blocked: {reason}"), false)),
        Event::RateLimited { .. } => Some((
            "Provider reported a rate limit; batch stopped.".into(),
            false,
        )),
        Event::Stopped { confirmed } => Some((
            if *confirmed {
                "Emergency stop: process termination confirmed.".into()
            } else {
                "Emergency stop: could NOT confirm process termination!".into()
            },
            *confirmed,
        )),
        _ => None,
    }
}

/// How often to poll usage and tick the scheduler: faster while something is runnable.
pub fn tick_interval(tasks: &[Task], s: &Settings) -> Duration {
    let waiting = tasks.iter().any(|t| t.status.is_runnable());
    let mins = if waiting {
        s.interval_waiting_minutes
    } else {
        s.interval_normal_minutes
    };
    Duration::from_secs(60 * mins as u64)
}

/// Daemon loop body: poll usage then tick the scheduler; returns error status lines (empty on success).
/// A client service (never the driver) does nothing here.
pub fn poll_and_tick(svc: &mut Service) -> Vec<String> {
    let mut errs = vec![];
    if svc.read_only {
        return errs;
    }
    if let Err(e) = svc.prune_if_due(chrono::Utc::now()) {
        errs.push(format!("Retention error: {e}"));
    }
    if let Err(e) = svc.refresh_all_usage() {
        errs.push(format!("Usage poll error: {e}"));
    }
    if let Err(e) = svc.tick(&SystemIdle) {
        errs.push(format!("Scheduler error: {e}"));
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn data_dir_precedence() {
        assert_eq!(
            data_dir_from(env(&[("PLAN_BURNER_DATA", "/x"), ("XDG_DATA_HOME", "/y")])),
            PathBuf::from("/x")
        );
        assert_eq!(
            data_dir_from(env(&[("XDG_DATA_HOME", "/y")])),
            PathBuf::from("/y").join(crate::brand::SLUG)
        );
        assert_eq!(
            data_dir_from(env(&[("HOME", "/h")])),
            PathBuf::from("/h/.local/share").join(crate::brand::SLUG)
        );
    }

    #[cfg(unix)]
    #[test]
    fn driver_lock_is_exclusive_and_read_only_blocks_actions() {
        let d = tempfile::tempdir().unwrap();
        let first = acquire_driver_lock(d.path()).expect("first lock");
        assert!(acquire_driver_lock(d.path()).is_none());
        drop(first);
        assert!(acquire_driver_lock(d.path()).is_some());

        let mut svc = Service::new(
            d.path().join("t.db"),
            provider::registry(),
            HashMap::new(),
            Arc::new(notify::SystemSinks),
        )
        .unwrap();
        svc.set_driver_lock(None);
        assert!(svc.read_only);
        assert!(svc.propose_for(1).is_err());
        assert!(svc.emergency_stop().is_err());
        assert!(svc.run_now(1, 1).is_err());
        let rep = svc.tick(&SystemIdle).unwrap();
        assert!(rep.started.is_empty() && rep.approvals_requested == 0);
    }

    #[cfg(unix)]
    #[test]
    fn only_one_driver_and_clients_never_lock() {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("PLAN_BURNER_DATA", d.path());
        let (client, _) = open_client_service().unwrap();
        assert!(client.read_only, "clients are never the driver");
        assert!(
            acquire_driver_lock(d.path()).is_some(),
            "a client holds no lock"
        );
        let (driver, _) = open_driver_service().unwrap();
        assert!(!driver.read_only);
        assert!(open_driver_service().is_err(), "a second daemon is refused");
        let (late_client, _) = open_client_service().unwrap();
        assert!(late_client.read_only);
        std::env::remove_var("PLAN_BURNER_DATA");
    }

    #[cfg(unix)]
    #[test]
    fn read_only_client_controls_the_driver_over_ipc() {
        use crate::control::RunState;
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("t.db");
        let mk = |lock: Option<std::fs::File>| {
            let mut s = Service::new(
                db.clone(),
                provider::registry(),
                HashMap::new(),
                Arc::new(notify::SystemSinks),
            )
            .unwrap();
            s.set_driver_lock(lock);
            s
        };
        let mut driver = mk(acquire_driver_lock(d.path()));
        driver.start_ipc_server();
        let client_db = db.clone();
        let t = std::thread::spawn(move || {
            let mut c = Service::new(
                client_db,
                provider::registry(),
                HashMap::new(),
                Arc::new(notify::SystemSinks),
            )
            .unwrap();
            c.set_driver_lock(None);
            c.refresh_remote();
            assert!(c.driven_remotely());
            c.pause_now().unwrap();
            assert_eq!(c.run_state(chrono::Utc::now()), RunState::Paused);
            c.resume().unwrap();
            c.emergency_stop().unwrap();
            assert_eq!(c.run_state(chrono::Utc::now()), RunState::Stopped);
            assert!(c.stop_report().is_some());
            c.clear_stop().unwrap();
            assert_eq!(c.run_state(chrono::Utc::now()), RunState::Waiting);
            // Driver-side rules still apply: unknown approvals / accounts are refused, not forwarded blindly.
            assert!(c.approve_and_start(999, &[1]).is_err());
            assert!(c.propose_for(12345).is_err());
            assert!(c.run_now(1, 1).is_err());
            // Settings are applied by the driver on the client's behalf.
            let mut st = crate::settings::load(&c.conn).unwrap();
            st.maximum_tasks_per_batch = 7;
            c.save_settings(&st).unwrap();
            assert_eq!(
                crate::settings::load(&c.conn)
                    .unwrap()
                    .maximum_tasks_per_batch,
                7
            );
            c.set_enabled(true).unwrap();
            assert!(crate::settings::load(&c.conn).unwrap().enabled);
            // Task / usage / template writes go through the driver too (no direct DB writes from clients).
            let mut tf = crate::forms::TaskForm::new_task();
            tf.t.name = "via ipc".into();
            tf.t.prompt = "p".into();
            c.save_task(&tf).unwrap();
            let tid = crate::tasks::list(&c.conn).unwrap()[0].id;
            c.task_action(tid, crate::ipc::TaskAction::Pause).unwrap();
            assert_eq!(
                crate::tasks::get(&c.conn, tid).unwrap().status,
                crate::models::TaskStatus::Paused
            );
            c.task_action(tid, crate::ipc::TaskAction::Resume).unwrap();
            assert!(c
                .task_action(tid, crate::ipc::TaskAction::MoveBy { delta: 1 })
                .is_ok());
            let tn = crate::tasks::NewTask {
                prompt: "p".into(),
                ..Default::default()
            };
            c.save_template("t", &tn).unwrap();
            let mut tf = crate::forms::TemplateForm::new();
            tf.name = "t".into();
            tf.task.t.prompt = "p".into();
            assert!(c.save_template_form(&tf).is_err(), "new name required");
            tf.name = "u".into();
            c.save_template_form(&tf).unwrap();
            let tpl = crate::templates::list(&c.conn).unwrap();
            assert_eq!(tpl.len(), 2);
            c.duplicate_template(tpl[0].id).unwrap();
            assert_eq!(crate::templates::list(&c.conn).unwrap().len(), 3);
            tf.id = Some(tpl[1].id);
            tf.name = "renamed".into();
            c.save_template_form(&tf).unwrap();
            for t in crate::templates::list(&c.conn).unwrap() {
                c.delete_template(t.id).unwrap();
            }
            assert!(c.delete_template(424242).is_err());
            c.mark_notifications_read().unwrap();
            assert!(
                c.record_actual(424242, 3.0).is_err(),
                "driver validation applies"
            );
            c.task_action(tid, crate::ipc::TaskAction::Delete).unwrap();
            assert!(crate::tasks::list(&c.conn).unwrap().is_empty());
            // A whole account form is saved through the driver (create + limits + ignore switch).
            let mut af = crate::forms::AccountForm::new();
            af.label = "Form".into();
            af.limits.start_threshold = Some(33.0);
            af.limits.ignore_limits = true;
            let saved = c.save_account(&af).unwrap();
            let got = crate::accounts::get(&c.conn, saved.id).unwrap();
            assert_eq!(got.limits.start_threshold, Some(33.0));
            assert!(got.limits.ignore_limits);
            af.id = Some(saved.id);
            af.limits.start_threshold = Some(150.0);
            assert!(
                c.save_account(&af).is_err(),
                "driver-side validation applies"
            );
            c.delete_account(saved.id).unwrap();
            // Accounts are written by the driver on the client's behalf; its validation still applies.
            let a = c.create_account("claude", "Via IPC", "Pro", None).unwrap();
            c.set_account_policy(a.id, crate::models::ExecutionPolicy::Manual)
                .unwrap();
            c.set_subscription_confirmed(a.id, true).unwrap();
            let got = crate::accounts::get(&c.conn, a.id).unwrap();
            assert_eq!(got.label, "Via IPC");
            assert!(got.subscription_confirmed);
            let mut u = crate::accounts::AccountUpdate {
                label: "Renamed".into(),
                plan: "Max".into(),
                config_dir: None,
                credentials_file: None,
                usage_source: got.usage_source,
                user_agent: None,
                custom: None,
                cli_command: None,
                availability_schedule: String::new(),
            };
            assert_eq!(c.update_account(a.id, &u).unwrap().label, "Renamed");
            // Driver-side validation errors reach the client and change nothing.
            u.label = "Bad".into();
            u.cli_command = Some("claude --flag".into());
            assert!(c.update_account(a.id, &u).is_err());
            assert_eq!(
                crate::accounts::get(&c.conn, a.id).unwrap().label,
                "Renamed"
            );
            c.delete_account(a.id).unwrap();
            assert!(crate::accounts::get(&c.conn, a.id).is_err());
        });
        while !t.is_finished() {
            driver.serve_ipc();
            std::thread::sleep(Duration::from_millis(5));
        }
        t.join().unwrap();
        assert_eq!(
            driver.controller.state(chrono::Utc::now()),
            RunState::Waiting
        );
    }

    #[test]
    fn stop_unconfirmed_is_reported_as_failure() {
        let (m, ok) = event_status(&Event::Stopped { confirmed: false }).unwrap();
        assert!(!ok && m.contains("NOT"));
        assert!(event_status(&Event::Stopped { confirmed: true }).unwrap().1);
    }
}
