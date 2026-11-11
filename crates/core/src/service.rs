//! Orchestration used by the GUI: periodic tick, approvals, batch threads, emergency STOP.
use crate::approval::{ApprovalManager, ApprovedBatch};
use crate::control::{self, Controller, RunState};
use crate::ipc::{self, Request, Response, StatusView};
use crate::models::{Account, Confidence};
use crate::notify::{self, ExternalSink, Kind, Notice};
use crate::provider::{self, Detection, ProviderAdapter};
use crate::runner::{self, BatchRequest, Event, Runtime};
use crate::scheduler::{self, Gate, IdleSource};
use crate::{accounts, audit, db, settings, tasks, usage, Error, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

pub struct Service {
    pub db_path: PathBuf,
    pub conn: Connection,
    pub controller: Arc<Controller>,
    pub approvals: ApprovalManager,
    pub adapters: Arc<Vec<Box<dyn ProviderAdapter>>>,
    pub env: HashMap<String, String>,
    events_tx: Sender<Event>,
    pub events_rx: Receiver<Event>,
    running: Arc<AtomicBool>,
    sink: notify::BackgroundSink,
    pub last_stop: Mutex<Option<StopReport>>,
    /// Last automatic-usage error per account (cleared on success). Never contains tokens.
    pub usage_errors: Mutex<HashMap<i64, String>>,
    usage_inflight: Arc<Mutex<std::collections::HashSet<i64>>>,
    pub usage_url: String,
    /// True when another front-end process owns the scheduler (see `session::acquire_driver_lock`).
    /// A read-only service never ticks, proposes, starts or stops anything.
    pub read_only: bool,
    driver_lock: Option<std::fs::File>,
    /// Driver side: serves read-only front-ends (see `ipc`). Set by `start_ipc_server`.
    ipc_server: Option<ipc::Server>,
    /// Read-only side: last state reported by the driver (`None` = no driver reachable).
    remote: Mutex<Option<StatusView>>,
    remote_polled: Mutex<Option<std::time::Instant>>,
    last_prune: Mutex<Option<std::time::Instant>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StopReport {
    pub at: DateTime<Utc>,
    pub cancelled_requests: usize,
    pub had_active_process: bool,
    /// None while waiting for the process to exit; Some(true) only after the OS confirmed it.
    pub process_confirmed: Option<bool>,
}

#[derive(Debug, Default)]
pub struct TickReport {
    pub held: Option<String>,
    pub started: Vec<i64>,
    pub approvals_requested: usize,
}

/// What a "Burn now" click did, for the status line.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BurnReport {
    /// Batches queued under Approvals.
    pub proposed: usize,
    /// Batches started right away (no approval).
    pub started: usize,
    pub message: String,
}

/// State of the scheduler daemon as seen by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerStatus {
    Running,
    /// A socket accepts connections but does not answer (hung daemon).
    Unreachable,
    /// No daemon: clients offer to start it, they never drive the scheduler themselves.
    Offline,
}

impl SchedulerStatus {
    pub fn label(self) -> &'static str {
        match self {
            SchedulerStatus::Running => "scheduler running",
            SchedulerStatus::Unreachable => "scheduler not responding",
            SchedulerStatus::Offline => "scheduler not running",
        }
    }
}

/// Why queued tasks are not running right now, for every client to show next to the queue.
#[derive(Debug, Default, Clone)]
pub struct QueueStatus {
    /// A condition that holds back the whole scheduler (automation off, paused, a task running).
    pub global: Option<String>,
    /// Per open task id: why it is not running (absent = nothing to report).
    pub reasons: HashMap<i64, String>,
}

const BATCH_RUNNING: &str =
    "a batch is already running: wait for it to finish, or press STOP to end it";

impl Service {
    pub fn new(
        db_path: PathBuf,
        adapters: Vec<Box<dyn ProviderAdapter>>,
        env: HashMap<String, String>,
        sink: Arc<dyn ExternalSink + Send + Sync>,
    ) -> Result<Service> {
        let conn = db::open(&db_path)?;
        let recovered = tasks::recover_interrupted(&conn)?;
        if recovered > 0 {
            audit::record(
                &conn,
                "system",
                "recovered_interrupted",
                &format!("{recovered} task(s) marked failed after restart"),
            )?;
        }
        let (tx, rx) = channel();
        Ok(Service {
            db_path,
            conn,
            controller: Arc::new(Controller::default()),
            approvals: ApprovalManager::default(),
            adapters: Arc::new(adapters),
            env,
            events_tx: tx,
            events_rx: rx,
            running: Arc::new(AtomicBool::new(false)),
            sink: notify::BackgroundSink::new(sink),
            last_stop: Mutex::new(None),
            usage_errors: Mutex::new(HashMap::new()),
            usage_inflight: Arc::new(Mutex::new(Default::default())),
            usage_url: crate::claude_usage::USAGE_URL.to_string(),
            read_only: false,
            driver_lock: None,
            ipc_server: None,
            remote: Mutex::new(None),
            remote_polled: Mutex::new(None),
            last_prune: Mutex::new(None),
        })
    }

    /// Attach the exclusive driver lock (`Some`), or demote to read-only (`None`).
    /// Driver only: re-applies the persisted STOP/pause state (see `control::restore_state`).
    pub fn restore_control_state(&self) -> Result<()> {
        control::restore_state(&self.conn, &self.controller, Utc::now())
    }

    pub fn set_driver_lock(&mut self, lock: Option<std::fs::File>) {
        self.read_only = lock.is_none();
        self.driver_lock = lock;
    }

    fn read_only_error(&self) -> Error {
        Error::Blocked(format!(
            "the {} scheduler is not running or not responding: start it and retry",
            crate::brand::NAME
        ))
    }

    fn ensure_driver(&self) -> Result<()> {
        if self.read_only {
            return Err(self.read_only_error());
        }
        Ok(())
    }

    pub fn data_dir(&self) -> &std::path::Path {
        self.db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
    }

    /// Read-only side: ask the driver to do `req`. Unreachable driver = the usual read-only error.
    fn forward(&self, req: &Request) -> Result<Response> {
        let resp = ipc::call(self.data_dir(), req).map_err(|_| self.read_only_error())?;
        if let Response::Err { message } = resp {
            return Err(Error::Blocked(message));
        }
        self.refresh_remote();
        Ok(resp)
    }

    /// Read-only side: fetch the driver's in-memory state (run state, approvals, STOP report).
    pub fn refresh_remote(&self) {
        if !self.read_only {
            return;
        }
        *self.remote_polled.lock().unwrap() = Some(std::time::Instant::now());
        let st = match ipc::call(self.data_dir(), &Request::Status) {
            Ok(Response::Status { status }) => Some(status),
            _ => None,
        };
        *self.remote.lock().unwrap() = st;
    }

    /// Whether the scheduler daemon is up, for every client to show next to the queue.
    pub fn scheduler_status(&self) -> SchedulerStatus {
        if !self.read_only || self.remote.lock().unwrap().is_some() {
            return SchedulerStatus::Running;
        }
        if ipc::socket_reachable(self.data_dir()) {
            SchedulerStatus::Unreachable
        } else {
            SchedulerStatus::Offline
        }
    }

    /// True when this process is read-only but a driver answered recently, so actions are forwarded to it.
    pub fn driven_remotely(&self) -> bool {
        self.read_only && self.remote.lock().unwrap().is_some()
    }

    /// Current run state: the driver's when read-only (unknown driver = Waiting), else the local controller's.
    pub fn run_state(&self, now: DateTime<Utc>) -> RunState {
        if self.read_only {
            return self
                .remote
                .lock()
                .unwrap()
                .as_ref()
                .map_or(RunState::Waiting, |s| s.run_state);
        }
        self.controller.state(now)
    }

    /// Batches awaiting approval (the driver's when read-only).
    pub fn pending_batches(&self) -> Vec<crate::approval::PendingBatch> {
        if self.read_only {
            return self
                .remote
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| s.pending.clone())
                .unwrap_or_default();
        }
        self.approvals.pending().into_iter().cloned().collect()
    }

    pub fn stop_report(&self) -> Option<StopReport> {
        if self.read_only {
            return self
                .remote
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|s| s.last_stop.clone());
        }
        self.last_stop.lock().unwrap().clone()
    }

    pub fn is_running(&self) -> bool {
        if self.read_only {
            return self
                .remote
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.running);
        }
        self.running.load(Ordering::SeqCst)
    }

    /// Flip only the global `enabled` flag: reloads the stored settings first so concurrent changes
    /// (other front-ends; unsaved GUI edits are not in the DB) are never overwritten.
    /// A read-only front-end forwards this to the driver, which applies it through this same method.
    pub fn set_enabled(&self, on: bool) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::SetEnabled { on }).map(|_| ());
        }
        let mut s = settings::load(&self.conn)?;
        s.enabled = on;
        settings::save(&self.conn, &s)
    }

    /// A read-only front-end forwards the settings to the driver (they hold no credentials).
    pub fn save_settings(&self, s: &settings::Settings) -> Result<()> {
        s.validate_working_root()?;
        if self.read_only {
            return self
                .forward(&Request::SaveSettings {
                    settings: s.clone(),
                })
                .map(|_| ());
        }
        settings::save(&self.conn, s)
    }

    /// Account writes: a read-only front-end forwards them to the driver, which runs the same method
    /// (validation and the billing guard apply there). No credentials cross the socket, only paths.
    fn forward_account(&self, req: Request) -> Result<Account> {
        match self.forward(&req)? {
            Response::Account { account } => Ok(*account),
            _ => Err(Error::Blocked("unexpected reply from the driver".into())),
        }
    }

    pub fn create_account(
        &self,
        provider_type: &str,
        label: &str,
        plan: &str,
        config_dir: Option<&str>,
    ) -> Result<Account> {
        if self.read_only {
            return self.forward_account(Request::CreateAccount {
                provider_type: provider_type.into(),
                label: label.into(),
                plan: plan.into(),
                config_dir: config_dir.map(Into::into),
            });
        }
        accounts::create(&self.conn, provider_type, label, plan, config_dir)
    }

    pub fn update_account(&self, id: i64, u: &accounts::AccountUpdate) -> Result<Account> {
        if self.read_only {
            return self.forward_account(Request::UpdateAccount {
                id,
                update: u.clone(),
            });
        }
        accounts::update(&self.conn, id, u)
    }

    pub fn delete_account(&self, id: i64) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::DeleteAccount { id }).map(|_| ());
        }
        accounts::delete(&self.conn, id)
    }

    pub fn set_account_policy(&self, id: i64, p: crate::models::ExecutionPolicy) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SetAccountPolicy { id, policy: p })
                .map(|_| ());
        }
        accounts::set_policy(&self.conn, id, p)
    }

    /// On-demand availability switch for an account (the schedule is edited via `update_account`).
    pub fn set_account_available(&self, id: i64, available: bool) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SetAccountAvailable { id, available })
                .map(|_| ());
        }
        accounts::set_available(&self.conn, id, available)
    }

    /// Per-account run-mode override (`Inherit` = use the global default).
    pub fn set_account_run_mode(&self, id: i64, mode: crate::models::RunMode) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SetAccountRunMode { id, mode })
                .map(|_| ());
        }
        accounts::set_run_mode(&self.conn, id, mode)
    }

    /// Creates or updates an account from a client form: the identity/connection settings, the
    /// per-account limits and the ignore-limits switch. Fetches usage afterwards if the account polls.
    pub fn save_account(&self, f: &crate::forms::AccountForm) -> Result<Account> {
        let a = match f.id {
            Some(id) => self.update_account(id, &f.to_update(&f.label, &f.plan))?,
            None => {
                let a = self.create_account(
                    &f.provider,
                    &f.label,
                    &f.plan,
                    crate::forms::opt_str(&f.config_dir).as_deref(),
                )?;
                // Same form values, but identity fields as stored by `create`.
                let mut u = f.to_update(&a.label, &a.subscription_plan);
                u.config_dir = a.config_dir.clone();
                self.update_account(a.id, &u)?
            }
        };
        self.set_account_limits(a.id, &f.limits)?;
        self.set_account_ignore_limits(a.id, f.limits.ignore_limits)?;
        if a.polls() {
            let _ = self.refresh_usage(a.id);
        }
        Ok(a)
    }

    /// Creates or updates a task (and its dependencies) from a client form.
    pub fn save_task(&self, form: &crate::forms::TaskForm) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SaveTask { form: form.clone() })
                .map(|_| ());
        }
        form.save(&self.conn).map(|_| ())
    }

    /// All tasks in queue order.
    pub fn list_tasks(&self) -> Result<Vec<tasks::Task>> {
        tasks::list(&self.conn)
    }

    /// Puts these task ids first, in this order.
    pub fn reorder_tasks(&self, ids: &[i64]) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::ReorderTasks { ids: ids.to_vec() })
                .map(|_| ());
        }
        for id in ids {
            tasks::get(&self.conn, *id)?;
        }
        tasks::reorder(&self.conn, ids)
    }

    /// Creates tasks from JSON / Markdown text. Every record goes through the same validation
    /// as the task editor (and is queued as Pending with its approval setting - an import never
    /// starts anything); bad records are reported and skipped. `dry_run` validates only.
    /// `depends_on` entries are task ids or names (existing tasks, or others in the same import).
    pub fn import_tasks(
        &self,
        text: &str,
        format: crate::import::ImportFormat,
        dry_run: bool,
    ) -> Result<crate::import::ImportReport> {
        use crate::import::{self, ImportReport};
        if self.read_only {
            return match self.forward(&Request::ImportTasks {
                text: text.to_string(),
                format,
                dry_run,
            })? {
                Response::Import { report } => Ok(report),
                _ => Err(Error::Blocked(
                    "unexpected reply from the scheduler driver".into(),
                )),
            };
        }
        let items = import::parse(text, format)?;
        let mut report = ImportReport {
            dry_run,
            ..Default::default()
        };
        let mut by_name: HashMap<String, i64> = HashMap::new();
        for t in tasks::list(&self.conn)? {
            by_name.entry(t.name.to_lowercase()).or_insert(t.id);
        }
        // (record number, id, dependency references) of the tasks created in this run.
        let mut made: Vec<(usize, i64, Vec<String>)> = Vec::new();
        for (i, item) in items.into_iter().enumerate() {
            let n = i + 1;
            let stored = item.and_then(|it| {
                if dry_run {
                    let (nt, _) = it.form.to_new_task()?;
                    tasks::validate(&nt)?;
                    Ok((0, it))
                } else {
                    let t = it.form.save(&self.conn)?;
                    by_name.entry(t.name.to_lowercase()).or_insert(t.id);
                    Ok((t.id, it))
                }
            });
            match stored {
                Ok((id, it)) => {
                    report.created.push((n, id));
                    made.push((n, id, it.depends_on));
                }
                Err(e) => report.errors.push((n, e.to_string())),
            }
        }
        if dry_run {
            return Ok(report);
        }
        for (n, id, refs) in made.into_iter().filter(|(_, _, r)| !r.is_empty()) {
            let resolved: Result<Vec<i64>> = refs
                .iter()
                .map(|r| match r.parse::<i64>() {
                    Ok(i) => Ok(i),
                    Err(_) => by_name.get(&r.to_lowercase()).copied().ok_or_else(|| {
                        Error::Invalid(format!("dependency \"{r}\" matches no task"))
                    }),
                })
                .collect();
            if let Err(e) = resolved.and_then(|d| tasks::set_dependencies(&self.conn, id, &d)) {
                report
                    .errors
                    .push((n, format!("created as task {id} without dependencies: {e}")));
            }
        }
        Ok(report)
    }

    /// [`Service::import_tasks`] for a file on this machine (the clients' import screens).
    pub fn import_file(
        &self,
        path: &str,
        format: crate::import::ImportFormat,
        dry_run: bool,
    ) -> Result<crate::import::ImportReport> {
        let (text, format) = crate::import::read_file(path, format)?;
        self.import_tasks(&text, format, dry_run)
    }

    /// Pause / resume / cancel / retry / delete / reorder one task.
    pub fn task_action(&self, id: i64, action: crate::ipc::TaskAction) -> Result<()> {
        use crate::ipc::TaskAction as A;
        if self.read_only {
            return self
                .forward(&Request::TaskAction { id, action })
                .map(|_| ());
        }
        match action {
            A::Pause => tasks::pause(&self.conn, id),
            A::Resume => tasks::resume(&self.conn, id),
            A::Cancel => tasks::cancel(&self.conn, id),
            A::Retry => tasks::retry(&self.conn, id),
            A::Duplicate => tasks::duplicate(&self.conn, id).map(|_| ()),
            A::Delete => tasks::delete(&self.conn, id),
            A::MoveBy { delta } => tasks::move_by(&self.conn, id, delta),
            A::MoveNextTo { target, after } => tasks::move_next_to(&self.conn, id, target, after),
        }
    }

    /// Records a manual usage entry (never a provider measurement).
    pub fn record_usage(&self, form: &crate::forms::UsageForm) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::RecordUsage { form: form.clone() })
                .map(|_| ());
        }
        let now = Utc::now();
        usage::record(&self.conn, &form.to_snapshot(now), now).map(|_| ())
    }

    /// Records measured consumption of an execution; true if it exceeded the task's budget.
    pub fn record_actual(&self, execution_id: i64, actual: f64) -> Result<bool> {
        if self.read_only {
            return match self.forward(&Request::RecordActual {
                execution_id,
                actual,
            })? {
                Response::OverBudget { over } => Ok(over),
                _ => Err(Error::Blocked("unexpected reply from the driver".into())),
            };
        }
        crate::analytics::record_actual(&self.conn, execution_id, actual)
    }

    /// Saves task defaults under a name, overwriting a template of the same name ("save as template").
    pub fn save_template(&self, name: &str, t: &crate::tasks::NewTask) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SaveTemplate {
                    name: name.into(),
                    task: t.clone(),
                })
                .map(|_| ());
        }
        crate::templates::save(&self.conn, name, t).map(|_| ())
    }

    /// Creates (new name required) or edits (renaming allowed) a template from a client form.
    pub fn save_template_form(&self, f: &crate::forms::TemplateForm) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SaveTemplateForm { form: f.clone() })
                .map(|_| ());
        }
        let (name, t) = f.to_template()?;
        match f.id {
            Some(id) => crate::templates::update(&self.conn, id, &name, &t),
            None => {
                if crate::templates::list(&self.conn)?
                    .iter()
                    .any(|x| x.name.eq_ignore_ascii_case(name.trim()))
                {
                    return Err(Error::Invalid(format!(
                        "a template named “{name}” already exists"
                    )));
                }
                crate::templates::save(&self.conn, &name, &t).map(|_| ())
            }
        }
    }

    pub fn duplicate_template(&self, id: i64) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::DuplicateTemplate { id }).map(|_| ());
        }
        crate::templates::duplicate(&self.conn, id).map(|_| ())
    }

    /// Queues a Pending task from a template's defaults without opening the editor.
    pub fn queue_from_template(&self, id: i64) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::QueueTemplate { id }).map(|_| ());
        }
        let tpl = crate::templates::get(&self.conn, id)?;
        let mut t = tpl.task;
        t.name = tpl.name;
        tasks::create(&self.conn, &t, crate::models::TaskStatus::Pending).map(|_| ())
    }

    pub fn delete_template(&self, id: i64) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::DeleteTemplate { id }).map(|_| ());
        }
        crate::templates::delete(&self.conn, id)
    }

    pub fn mark_notifications_read(&self) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::MarkNotificationsRead).map(|_| ());
        }
        crate::notify::mark_all_read(&self.conn, Utc::now())
    }

    /// Per-account eligibility overrides (`None` fields inherit the general settings).
    pub fn set_account_limits(&self, id: i64, l: &crate::models::AccountLimits) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SetAccountLimits {
                    id,
                    limits: l.clone(),
                })
                .map(|_| ());
        }
        accounts::set_limits(&self.conn, id, l)
    }

    /// Persistent "eligible ignoring thresholds, reserve, idle and hours" switch. Staleness, billing,
    /// availability and STOP still apply.
    pub fn set_account_ignore_limits(&self, id: i64, on: bool) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SetAccountIgnoreLimits { id, on })
                .map(|_| ());
        }
        accounts::set_ignore_limits(&self.conn, id, on)
    }

    /// General "burn before reset" button: ON spends a window's leftovers in its last hour (5 h) /
    /// last day (weekly); OFF clears both. Account overrides are untouched.
    pub fn set_burn_before_reset(&self, on: bool) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::SetBurn { on }).map(|_| ());
        }
        let mut s = settings::load(&self.conn)?;
        (s.burn_session_hours, s.burn_weekly_hours) = if on {
            (
                Some(settings::BURN_DEFAULT_SESSION_H),
                Some(settings::BURN_DEFAULT_WEEKLY_H),
            )
        } else {
            (None, None)
        };
        settings::save(&self.conn, &s)
    }

    /// Per-account button. OFF is an explicit 0 h override, so it also wins over a general ON.
    pub fn set_account_burn(&self, id: i64, on: bool) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SetAccountBurn { id, on })
                .map(|_| ());
        }
        let mut l = accounts::get(&self.conn, id)?.limits;
        (l.burn_session_hours, l.burn_weekly_hours) = if on {
            (
                Some(settings::BURN_DEFAULT_SESSION_H),
                Some(settings::BURN_DEFAULT_WEEKLY_H),
            )
        } else {
            (Some(0.0), Some(0.0))
        };
        accounts::set_limits(&self.conn, id, &l)
    }

    pub fn set_subscription_confirmed(&self, id: i64, v: bool) -> Result<()> {
        if self.read_only {
            return self
                .forward(&Request::SetSubscriptionConfirmed { id, confirmed: v })
                .map(|_| ());
        }
        accounts::set_subscription_confirmed(&self.conn, id, v)
    }

    /// Driver only: delete data past the retention window at most once a day (first call after start runs
    /// it). Returns what was deleted, or `None` if it was not due or this instance is read-only.
    pub fn prune_if_due(&self, now: DateTime<Utc>) -> Result<Option<db::PruneReport>> {
        if self.read_only {
            return Ok(None);
        }
        let mut last = self.last_prune.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(24 * 3600)) {
            return Ok(None);
        }
        let days = settings::load(&self.conn)?.retention_days;
        let rep = db::prune(&self.conn, now, days)?;
        *last = Some(std::time::Instant::now());
        if rep != db::PruneReport::default() {
            audit::record(
                &self.conn,
                "system",
                "retention_prune",
                &format!(
                    "deleted audit={} notifications={} usage_snapshots={}",
                    rep.audit_log, rep.notifications, rep.usage_snapshots
                ),
            )?;
        }
        Ok(Some(rep))
    }

    pub fn pause_now(&self) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::Pause).map(|_| ());
        }
        self.controller.pause_now();
        control::save_state(&self.conn, &self.controller)
    }

    pub fn pause_until(&self, t: DateTime<Utc>) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::PauseUntil { until: t }).map(|_| ());
        }
        self.controller.pause_until(t);
        control::save_state(&self.conn, &self.controller)
    }

    /// Pause until the earliest upcoming 5 h session reset among available accounts with a fresh usage
    /// reading. Returns the resume time. Errors (and pauses nothing) when no such reset is known.
    pub fn pause_until_next_reset(&self) -> Result<DateTime<Utc>> {
        let now = Utc::now();
        let base = settings::load(&self.conn)?;
        let mut next: Option<DateTime<Utc>> = None;
        for a in accounts::list(&self.conn)?.iter().filter(|a| a.available) {
            let max_age = base.effective_for(a).max_usage_age_minutes as i64;
            if let Some(t) = usage::latest(&self.conn, a.id)?
                .and_then(|u| usage::upcoming_session_reset(&u, max_age, now))
            {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        }
        let t = next.ok_or_else(|| {
            Error::Blocked(
                "no upcoming usage reset is known (needs an available account with a fresh usage reading): use Pause instead"
                    .into(),
            )
        })?;
        self.pause_until(t)?;
        Ok(t)
    }

    pub fn resume(&self) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::Resume).map(|_| ());
        }
        self.controller.resume();
        control::save_state(&self.conn, &self.controller)
    }

    /// Cancel only the active run (not an emergency STOP).
    pub fn cancel_active(&self) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::CancelActive).map(|_| ());
        }
        self.controller.cancel_active();
        Ok(())
    }

    pub fn reject_batch(&mut self, batch_id: u64) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::Reject { batch_id }).map(|_| ());
        }
        self.approvals.reject(&self.conn, batch_id)
    }

    /// Driver side: bind the control socket so read-only front-ends can control this process.
    pub fn start_ipc_server(&mut self) {
        if self.read_only {
            return;
        }
        match ipc::Server::start(self.data_dir()) {
            Ok(s) => self.ipc_server = Some(s),
            Err(e) => tracing::warn!("control socket unavailable: {e}"),
        }
    }

    /// Driver side: answer queued control requests. Cheap; call from the main loop / every frame.
    pub fn serve_ipc(&mut self) {
        let Some(server) = self.ipc_server.take() else {
            return;
        };
        server.drain(|req| self.handle_request(req));
        self.ipc_server = Some(server);
    }

    fn status_view(&self) -> StatusView {
        StatusView {
            run_state: self.controller.state(Utc::now()),
            running: self.is_running(),
            pending: self.approvals.pending().into_iter().cloned().collect(),
            last_stop: self.last_stop.lock().unwrap().clone(),
        }
    }

    /// Execute a control request through the same methods the local UI uses (so every gate still applies).
    fn handle_request(&mut self, req: Request) -> Response {
        let run = |this: &mut Service, req: Request| -> Result<Response> {
            Ok(match req {
                Request::Status => Response::Status {
                    status: this.status_view(),
                },
                Request::Pause => {
                    this.pause_now()?;
                    Response::Ok
                }
                Request::PauseUntil { until } => {
                    this.pause_until(until)?;
                    Response::Ok
                }
                Request::Resume => {
                    this.resume()?;
                    Response::Ok
                }
                Request::Stop => {
                    this.emergency_stop()?;
                    Response::Ok
                }
                Request::ClearStop => {
                    this.clear_stop()?;
                    Response::Ok
                }
                Request::CancelActive => {
                    this.cancel_active()?;
                    Response::Ok
                }
                Request::Approve { batch_id, tasks } => {
                    this.approve_and_start(batch_id, &tasks)?;
                    Response::Ok
                }
                Request::Reject { batch_id } => {
                    this.reject_batch(batch_id)?;
                    Response::Ok
                }
                Request::Propose { account_id } => Response::Proposed {
                    batch_id: this.propose_for(account_id)?,
                },
                Request::RunNow {
                    task_id,
                    account_id,
                } => {
                    this.run_now(task_id, account_id)?;
                    Response::Ok
                }
                Request::SaveSettings { settings } => {
                    this.save_settings(&settings)?;
                    Response::Ok
                }
                Request::SetEnabled { on } => {
                    this.set_enabled(on)?;
                    Response::Ok
                }
                Request::CreateAccount {
                    provider_type,
                    label,
                    plan,
                    config_dir,
                } => Response::Account {
                    account: Box::new(this.create_account(
                        &provider_type,
                        &label,
                        &plan,
                        config_dir.as_deref(),
                    )?),
                },
                Request::UpdateAccount { id, update } => Response::Account {
                    account: Box::new(this.update_account(id, &update)?),
                },
                Request::DeleteAccount { id } => {
                    this.delete_account(id)?;
                    Response::Ok
                }
                Request::SetAccountPolicy { id, policy } => {
                    this.set_account_policy(id, policy)?;
                    Response::Ok
                }
                Request::SetAccountAvailable { id, available } => {
                    this.set_account_available(id, available)?;
                    Response::Ok
                }
                Request::SetAccountRunMode { id, mode } => {
                    this.set_account_run_mode(id, mode)?;
                    Response::Ok
                }
                Request::SaveTask { form } => {
                    this.save_task(&form)?;
                    Response::Ok
                }
                Request::TaskAction { id, action } => {
                    this.task_action(id, action)?;
                    Response::Ok
                }
                Request::RecordUsage { form } => {
                    this.record_usage(&form)?;
                    Response::Ok
                }
                Request::RecordActual {
                    execution_id,
                    actual,
                } => Response::OverBudget {
                    over: this.record_actual(execution_id, actual)?,
                },
                Request::SaveTemplate { name, task } => {
                    this.save_template(&name, &task)?;
                    Response::Ok
                }
                Request::SaveTemplateForm { form } => {
                    this.save_template_form(&form)?;
                    Response::Ok
                }
                Request::DuplicateTemplate { id } => {
                    this.duplicate_template(id)?;
                    Response::Ok
                }
                Request::QueueTemplate { id } => {
                    this.queue_from_template(id)?;
                    Response::Ok
                }
                Request::DeleteTemplate { id } => {
                    this.delete_template(id)?;
                    Response::Ok
                }
                Request::MarkNotificationsRead => {
                    this.mark_notifications_read()?;
                    Response::Ok
                }
                Request::SetAccountLimits { id, limits } => {
                    this.set_account_limits(id, &limits)?;
                    Response::Ok
                }
                Request::SetAccountIgnoreLimits { id, on } => {
                    this.set_account_ignore_limits(id, on)?;
                    Response::Ok
                }
                Request::SetBurn { on } => {
                    this.set_burn_before_reset(on)?;
                    Response::Ok
                }
                Request::SetAccountBurn { id, on } => {
                    this.set_account_burn(id, on)?;
                    Response::Ok
                }
                Request::BurnNow {
                    account_id,
                    with_approval,
                } => Response::Burn {
                    report: this.burn_now(account_id, with_approval)?,
                },
                Request::SetSubscriptionConfirmed { id, confirmed } => {
                    this.set_subscription_confirmed(id, confirmed)?;
                    Response::Ok
                }
                Request::ListTasks => Response::Tasks {
                    tasks: this.list_tasks()?,
                },
                Request::ImportTasks {
                    text,
                    format,
                    dry_run,
                } => Response::Import {
                    report: this.import_tasks(&text, format, dry_run)?,
                },
                Request::ReorderTasks { ids } => {
                    this.reorder_tasks(&ids)?;
                    Response::Ok
                }
            })
        };
        run(self, req).unwrap_or_else(|e| Response::Err {
            message: e.to_string(),
        })
    }

    pub fn detections(&self) -> Result<HashMap<i64, Detection>> {
        let mut m = HashMap::new();
        for a in accounts::list(&self.conn)? {
            m.insert(a.id, self.detect(&a));
        }
        Ok(m)
    }

    pub fn detect(&self, a: &Account) -> Detection {
        match provider::find(&self.adapters, &a.provider_type) {
            Some(ad) => ad.detect(a, &self.env),
            None => Detection {
                cli_found: false,
                billing: crate::models::BillingMode::Unverified,
                reasons: vec!["no adapter installed for this provider".into()],
            },
        }
    }

    /// Polls usage for one account on a worker thread (no-op for manual accounts or if one is in flight).
    pub fn refresh_usage(&self, account_id: i64) -> Result<()> {
        let a = accounts::get(&self.conn, account_id)?;
        if !a.polls() {
            return Ok(());
        }
        if !self.usage_inflight.lock().unwrap().insert(account_id) {
            return Ok(());
        }
        let (path, env, url, tx, inflight) = (
            self.db_path.clone(),
            self.env.clone(),
            self.usage_url.clone(),
            self.events_tx.clone(),
            self.usage_inflight.clone(),
        );
        std::thread::spawn(move || {
            let result = db::open(&path).map_err(|e| e.to_string()).and_then(|conn| {
                if a.provider_type == "custom" {
                    crate::custom::poll(&conn, &a, &env, Utc::now())
                } else {
                    crate::claude_usage::poll_account(&conn, &a, &env, &url, Utc::now())
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            });
            inflight.lock().unwrap().remove(&account_id);
            let _ = tx.send(Event::UsageFetched { account_id, result });
        });
        Ok(())
    }

    /// Polls every account that uses automatic usage.
    pub fn refresh_all_usage(&self) -> Result<()> {
        for a in accounts::list(&self.conn)? {
            self.refresh_usage(a.id)?;
        }
        Ok(())
    }

    /// Refreshes cached billing/connection state of every account (visible on the dashboard).
    pub fn refresh_accounts(&self) -> Result<()> {
        for a in accounts::list(&self.conn)? {
            let d = self.detect(&a);
            let c = if d.cli_found {
                crate::models::ConnectionStatus::Connected
            } else {
                crate::models::ConnectionStatus::Error
            };
            accounts::update_status(&self.conn, a.id, d.billing, c)?;
        }
        Ok(())
    }

    fn token(&self) -> Option<String> {
        std::env::var("PLAN_BURNER_TELEGRAM_TOKEN").ok()
    }

    pub fn notify(&self, n: &Notice) {
        let Ok(s) = settings::load(&self.conn) else {
            return;
        };
        let lm = scheduler::local_minute_of_day(chrono::Local::now());
        if let Err(e) = notify::send(
            &self.conn,
            &s,
            n,
            Utc::now(),
            lm,
            self.token().as_deref(),
            &self.sink,
        ) {
            tracing::warn!("notification failed: {e}");
        }
    }

    /// Why `task_id` could not be run by hand on `account_id` right now (empty = it can). Runs the
    /// same pre-flight the launch will (billing, usage freshness, capacity, availability, CLI), so
    /// clients can mark ineligible accounts before the user clicks. Read-only.
    pub fn run_now_blockers(&self, task_id: i64, account_id: i64) -> Result<Vec<String>> {
        let now = Utc::now();
        let t = tasks::get(&self.conn, task_id)?;
        let a = accounts::get(&self.conn, account_id)?;
        let s = settings::load(&self.conn)?;
        let det = self.detect(&a);
        let snap = usage::latest(&self.conn, a.id)?;
        let consumed = match &snap {
            Some(u) => scheduler::consumed_since(&self.conn, a.id, u.captured_at)?,
            None => 0.0,
        };
        let blocks = crate::engine::preflight(&crate::engine::Preflight {
            settings: &s,
            account: &a,
            task: &t,
            snapshot: snap.as_ref(),
            billing: det.billing,
            billing_reasons: det.reasons,
            cli_found: det.cli_found,
            launches_allowed: self.controller.can_launch(now),
            automatic: false,
            approved: true,
            now,
            clock: crate::availability::LocalClock::now(),
            consumed_since_snapshot: consumed,
        });
        Ok(blocks.iter().map(|b| b.to_string()).collect())
    }

    /// Explains, per open task, why it is not running: the same planning and pre-flight checks the
    /// scheduler uses (usage, billing, capacity, schedule, dependencies), read-only. Never starts anything.
    pub fn queue_status(&self) -> Result<QueueStatus> {
        use crate::models::TaskStatus as S;
        let now = Utc::now();
        let s = settings::load(&self.conn)?;
        let clock = crate::availability::LocalClock::now();
        let g = Gate {
            now,
            local_minute: clock.minute,
            idle_minutes: None,
            launches_allowed: self.controller.can_launch(now),
            executing: self.is_running(),
        };
        let global = scheduler::state_gate(&s, &g).err().map(|w| w.to_string());
        let accts = accounts::list(&self.conn)?;
        let dets = self.detections()?;
        let plan =
            scheduler::plan_scheduled(&self.conn, &s, &accts, &dets, now, clock, true, None)?;
        let planned: std::collections::HashSet<i64> = plan
            .proposals
            .iter()
            .flat_map(|p| p.tasks.iter().map(|t| t.id))
            .collect();
        let mut reasons = HashMap::new();
        for t in tasks::list(&self.conn)? {
            let why = match t.status {
                S::Paused => Some("paused: resume it to let it run".to_string()),
                S::AwaitingApproval => {
                    Some("waiting for your approval (Approvals page)".to_string())
                }
                S::Pending | S::WaitingForCapacity => {
                    if t.expiration_at.is_some_and(|e| e <= now) {
                        Some("expired".to_string())
                    } else if !tasks::deps_done(&self.conn, t.id)? {
                        let open: Vec<String> = tasks::dependencies(&self.conn, t.id)?
                            .into_iter()
                            .map(|d| format!("#{d}"))
                            .collect();
                        Some(format!(
                            "waiting for task(s) {} to complete",
                            open.join(", ")
                        ))
                    } else if !scheduler::task_due(&t, now, clock.minute) {
                        Some(format!(
                            "not due yet (schedule {})",
                            t.schedule.as_deref().unwrap_or("")
                        ))
                    } else if planned.contains(&t.id) {
                        Some(match &global {
                            Some(g) => format!("ready, but {g}"),
                            None => "ready: will be proposed at the next check".to_string(),
                        })
                    } else if let Some((_, blocks)) =
                        plan.blocked.iter().find(|(id, _)| *id == t.id)
                    {
                        let mut parts: Vec<String> = Vec::new();
                        for b in blocks {
                            let p = b.to_string();
                            if !parts.contains(&p) {
                                parts.push(p);
                            }
                        }
                        Some(format!("blocked: {}", parts.join("; ")))
                    } else if !accts
                        .iter()
                        .any(|a| match (&t.provider_type, t.account_id) {
                            (_, Some(id)) => a.id == id,
                            (Some(p), None) => *p == a.provider_type,
                            (None, None) => true,
                        })
                    {
                        Some("no matching account (add or re-pin one)".to_string())
                    } else {
                        Some(
                            "no eligible account right now (check Dashboard for hours, availability or manual-only policy)"
                                .to_string(),
                        )
                    }
                }
                _ => None,
            };
            if let Some(w) = why {
                reasons.insert(t.id, w);
            }
        }
        Ok(QueueStatus { global, reasons })
    }

    /// One scheduler pass. Never starts anything unless every gate and pre-flight check passes.
    pub fn tick(&mut self, idle: &dyn IdleSource) -> Result<TickReport> {
        if self.read_only {
            return Ok(TickReport::default());
        }
        let now = Utc::now();
        let mut rep = TickReport::default();
        tasks::expire_due(&self.conn, now)?;
        let s = settings::load(&self.conn)?;
        let clock = crate::availability::LocalClock::now();
        let lm = clock.minute;
        let g = Gate {
            now,
            local_minute: lm,
            idle_minutes: idle.idle_minutes(),
            launches_allowed: self.controller.can_launch(now),
            executing: self.is_running(),
        };
        // Run state is global; allowed hours and idle are evaluated per account inside the plan.
        if let Err(w) = scheduler::state_gate(&s, &g) {
            rep.held = Some(w.to_string());
            return Ok(rep);
        }
        let accts = accounts::list(&self.conn)?;
        let dets = self.detections()?;
        let plan = scheduler::plan_scheduled(
            &self.conn,
            &s,
            &accts,
            &dets,
            now,
            clock,
            true,
            Some(g.idle_minutes),
        )?;
        if plan.proposals.is_empty() && !plan.held.is_empty() && plan.blocked.is_empty() {
            rep.held = plan.held.first().map(|(_, w)| w.to_string());
        }
        // Tasks that could not run for capacity reasons wait visibly.
        for (id, blocks) in &plan.blocked {
            if blocks
                .iter()
                .any(|b| matches!(b, crate::engine::Block::Capacity(_)))
                && tasks::get(&self.conn, *id)?.status == crate::models::TaskStatus::Pending
            {
                tasks::set_status(
                    &self.conn,
                    *id,
                    crate::models::TaskStatus::WaitingForCapacity,
                )?;
            }
        }
        for p in plan.proposals {
            let acct = accts.iter().find(|a| a.id == p.account_id).unwrap();
            let names: Vec<String> = p.tasks.iter().map(|t| t.name.clone()).collect();
            if p.auto {
                if self.is_running() {
                    continue;
                }
                let ids: Vec<i64> = p.tasks.iter().map(|t| t.id).collect();
                self.start_batch(
                    ApprovedBatch {
                        account_id: p.account_id,
                        task_ids: ids.clone(),
                        burn: false,
                    },
                    true,
                )?;
                rep.started.extend(ids);
                self.notify(&Notice {
                    kind: Kind::ExecutionStarted,
                    account: acct.display(),
                    count: names.len(),
                    names,
                    detail: String::new(),
                    hint: String::new(),
                });
                break; // one batch at a time
            } else if !self.approvals.has_pending_for_account(p.account_id) {
                let conf = usage::latest(&self.conn, p.account_id)?
                    .map(|u| u.confidence)
                    .unwrap_or(Confidence::Unknown);
                let n = p.tasks.len();
                self.approvals.request(&self.conn, p, conf, now)?;
                rep.approvals_requested += 1;
                self.notify(&Notice {
                    kind: Kind::ApprovalRequested,
                    account: acct.display(),
                    count: n,
                    names,
                    detail: String::new(),
                    hint: "review it under Approvals".into(),
                });
            }
        }
        Ok(rep)
    }

    /// User-initiated "Run tasks here…": propose a batch for one account (ignores the global enable switch and
    /// schedule gates, but not capacity/billing checks) and queue it for approval.
    pub fn propose_for(&mut self, account_id: i64) -> Result<Option<u64>> {
        if self.read_only {
            return match self.forward(&Request::Propose { account_id })? {
                Response::Proposed { batch_id } => Ok(batch_id),
                _ => Err(Error::Blocked("unexpected reply from the driver".into())),
            };
        }
        match self.plan_for(account_id, false)? {
            Some(p) => Ok(Some(self.request_approval(p)?)),
            None => Ok(None),
        }
    }

    fn request_approval(&mut self, p: scheduler::Proposal) -> Result<u64> {
        let conf = usage::latest(&self.conn, p.account_id)?
            .map(|u| u.confidence)
            .unwrap_or(Confidence::Unknown);
        self.approvals.request(&self.conn, p, conf, Utc::now())
    }

    /// Plans one manual batch for `account_id`. With `burn` the account's thresholds, reserve, idle
    /// and hours are ignored for this plan (the same switch as "ignore limits"); staleness, billing,
    /// availability, STOP and the capacity the tasks really need are enforced as always.
    fn plan_for(&self, account_id: i64, burn: bool) -> Result<Option<scheduler::Proposal>> {
        let now = Utc::now();
        let s = settings::load(&self.conn)?;
        let mut a = accounts::get(&self.conn, account_id)?;
        // A manual-only account can still be run by explicit request.
        if a.execution_policy == crate::models::ExecutionPolicy::Manual {
            a.execution_policy = crate::models::ExecutionPolicy::Approval;
        }
        if burn {
            a.limits.ignore_limits = true;
        }
        let mut s2 = s.clone();
        if s2.execution_mode == crate::models::ExecutionPolicy::Manual {
            s2.execution_mode = crate::models::ExecutionPolicy::Approval;
        }
        let dets = HashMap::from([(a.id, self.detect(&a))]);
        let clock = crate::availability::LocalClock::now();
        if let Err(why) = a.availability(clock) {
            return Err(Error::Blocked(format!(
                "account '{}' is unavailable: {why}",
                a.label
            )));
        }
        let plan = scheduler::plan(
            &self.conn,
            &s2,
            std::slice::from_ref(&a),
            &dets,
            now,
            clock,
            self.controller.can_launch(now),
        )?;
        Ok(plan.proposals.into_iter().next().map(|mut p| {
            p.auto = false;
            p.burn = burn;
            p
        }))
    }

    /// "Burn now": spend what is left immediately. One batch per account, thresholds and reserve
    /// ignored for it. `with_approval` queues the batch under Approvals; without, it starts at once
    /// (only one batch runs at a time, so with several accounts the first eligible one starts).
    /// `account_id: None` = every account.
    pub fn burn_now(&mut self, account_id: Option<i64>, with_approval: bool) -> Result<BurnReport> {
        if self.read_only {
            return match self.forward(&Request::BurnNow {
                account_id,
                with_approval,
            })? {
                Response::Burn { report } => Ok(report),
                _ => Err(Error::Blocked("unexpected reply from the driver".into())),
            };
        }
        let ids: Vec<i64> = match account_id {
            Some(id) => vec![id],
            None => accounts::list(&self.conn)?
                .into_iter()
                .map(|a| a.id)
                .collect(),
        };
        let mut rep = BurnReport::default();
        let mut notes: Vec<String> = Vec::new();
        for id in ids {
            let label = accounts::get(&self.conn, id)?.label;
            match self.plan_for(id, true) {
                Ok(Some(p)) if with_approval => {
                    let n = p.tasks.len();
                    self.request_approval(p)?;
                    rep.proposed += 1;
                    notes.push(format!("{label}: {n} task(s) proposed, approve them under Approvals"));
                }
                Ok(Some(p)) if rep.started == 0 && !self.is_running() => {
                    let task_ids: Vec<i64> = p.tasks.iter().map(|t| t.id).collect();
                    let n = task_ids.len();
                    self.start_batch(
                        ApprovedBatch {
                            account_id: id,
                            task_ids,
                            burn: true,
                        },
                        false,
                    )?;
                    rep.started += 1;
                    notes.push(format!("{label}: {n} task(s) started"));
                }
                Ok(Some(_)) => notes.push(format!(
                    "{label}: ready, but a batch is already running; burn again when it finishes"
                )),
                Ok(None) => notes.push(format!(
                    "{label}: nothing to burn (no eligible tasks, or the usage/billing checks failed)"
                )),
                Err(e) if account_id.is_some() => return Err(e),
                Err(e) => notes.push(format!("{label}: {e}")),
            }
        }
        let _ = audit::record(
            &self.conn,
            "user",
            "burn_now",
            &format!(
                "account={} with_approval={with_approval} proposed={} started={}",
                account_id.map_or("all".to_string(), |i| i.to_string()),
                rep.proposed,
                rep.started
            ),
        );
        rep.message = format!("Burn now: {}", notes.join("; "));
        Ok(rep)
    }

    /// Starts an approved batch on a background thread. Fails if one is already running or STOP is active.
    pub fn start_batch(&self, b: ApprovedBatch, automatic: bool) -> Result<()> {
        self.ensure_driver()?;
        if !self.controller.can_launch(Utc::now()) {
            return Err(Error::Blocked("automation is paused or stopped".into()));
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::Blocked(BATCH_RUNNING.into()));
        }
        let rt = Runtime {
            adapters: self.adapters.clone(),
            controller: self.controller.clone(),
            env: self.env.clone(),
            events: self.events_tx.clone(),
        };
        let path = self.db_path.clone();
        let running = self.running.clone();
        let req = BatchRequest {
            account_id: b.account_id,
            task_ids: b.task_ids,
            automatic,
            mode_override: None,
            burn: b.burn,
        };
        std::thread::spawn(move || {
            if let Err(e) = runner::run_batch(&path, &req, &rt) {
                tracing::error!("batch failed: {e}");
            }
            running.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    /// Approve (selected tasks of) a pending batch and start it.
    pub fn approve_and_start(&mut self, batch_id: u64, selected: &[i64]) -> Result<()> {
        if self.read_only {
            let req = Request::Approve {
                batch_id,
                tasks: selected.to_vec(),
            };
            return self.forward(&req).map(|_| ());
        }
        if self.is_running() {
            return Err(Error::Blocked(BATCH_RUNNING.into()));
        }
        let ok = self.approvals.approve(&self.conn, batch_id, selected)?;
        audit::record(
            &self.conn,
            "user",
            "approved",
            &format!("account={} tasks={:?}", ok.account_id, ok.task_ids),
        )?;
        if let Err(e) = self.start_batch(ok.clone(), false) {
            for id in ok.task_ids {
                tasks::set_status(&self.conn, id, crate::models::TaskStatus::Pending)?;
            }
            return Err(e);
        }
        Ok(())
    }

    /// Manual "Run now on…": the user's click is the approval, but every pre-flight check still applies.
    pub fn run_now(&self, task_id: i64, account_id: i64) -> Result<()> {
        if self.read_only {
            let req = Request::RunNow {
                task_id,
                account_id,
            };
            return self.forward(&req).map(|_| ());
        }
        let t = tasks::get(&self.conn, task_id)?;
        let a = accounts::get(&self.conn, account_id)?;
        if t.provider_type
            .as_deref()
            .is_some_and(|p| p != a.provider_type)
        {
            return Err(Error::Invalid(format!(
                "task #{} '{}' is tagged for provider '{}' but account '{}' is a '{}' account; pick an account of that provider",
                t.id,
                t.name,
                t.provider_type.as_deref().unwrap_or_default(),
                a.label,
                a.provider_type
            )));
        }
        if !t.status.is_runnable() && t.status != crate::models::TaskStatus::Paused {
            return Err(Error::Blocked(format!(
                "task #{} '{}' is {}; only pending, waiting, awaiting-approval or paused tasks can be run now",
                t.id,
                t.name,
                t.status.label()
            )));
        }
        self.start_batch(
            ApprovedBatch {
                account_id,
                task_ids: vec![task_id],
                burn: false,
            },
            false,
        )
    }

    /// Emergency STOP: block launches, drop approval requests, ask the active process to terminate.
    /// Success is only reported once `poll_stop` sees the process confirmed gone.
    pub fn emergency_stop(&mut self) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::Stop).map(|_| ());
        }
        let had = self.is_running();
        self.controller.emergency_stop();
        control::save_state(&self.conn, &self.controller)?;
        let n = self.approvals.cancel_all(&self.conn)?;
        audit::record(
            &self.conn,
            "user",
            "emergency_stop",
            &format!("cancelled_requests={n} active_process={had}"),
        )?;
        *self.last_stop.lock().unwrap() = Some(StopReport {
            at: Utc::now(),
            cancelled_requests: n,
            had_active_process: had,
            process_confirmed: if had { None } else { Some(true) },
        });
        Ok(())
    }

    /// Driver shutdown: ask the active run (if any) to terminate and wait up to `wait` for it to exit,
    /// processing runner events meanwhile, so a signalled daemon never leaves a CLI child consuming
    /// capacity unattended. Not an EMERGENCY STOP: nothing is persisted. Returns true once idle.
    pub fn shutdown(&mut self, wait: std::time::Duration) -> bool {
        if self.read_only {
            return true;
        }
        let deadline = std::time::Instant::now() + wait;
        loop {
            self.poll();
            if !self.is_running() {
                return true;
            }
            // Re-asserted every pass: the runner clears the flag as it launches a child, so a cancel
            // sent just before that point would otherwise be lost.
            self.controller.cancel_active();
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Drain runner events, send notifications and update the stop report.
    pub fn poll(&mut self) -> Vec<Event> {
        if self.read_only {
            let due = self
                .remote_polled
                .lock()
                .unwrap()
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1));
            if due {
                self.refresh_remote();
            }
        } else {
            self.serve_ipc();
        }
        let evs: Vec<Event> = self.events_rx.try_iter().collect();
        for e in &evs {
            match e {
                Event::UsageFetched { account_id, result } => {
                    let mut errs = self.usage_errors.lock().unwrap();
                    match result {
                        Ok(()) => {
                            errs.remove(account_id);
                        }
                        Err(m) => {
                            errs.insert(*account_id, m.clone());
                            drop(errs);
                            if let Ok(a) = accounts::get(&self.conn, *account_id) {
                                self.notify(&Notice {
                                    kind: Kind::MonitoringError,
                                    account: a.display(),
                                    count: 0,
                                    names: vec![],
                                    detail: m.chars().take(200).collect(),
                                    hint: "check the account's login and usage settings".into(),
                                });
                            }
                        }
                    }
                }
                Event::Stopped { confirmed } => {
                    if let Some(r) = self.last_stop.lock().unwrap().as_mut() {
                        r.process_confirmed = Some(*confirmed);
                    }
                    let _ = audit::record(
                        &self.conn,
                        "engine",
                        "stop_confirmation",
                        &format!("confirmed={confirmed}"),
                    );
                }
                Event::RateLimited { account_id } => {
                    if let Ok(a) = accounts::get(&self.conn, *account_id) {
                        self.notify(&Notice {
                            kind: Kind::CapacityInsufficient,
                            account: a.display(),
                            count: 0,
                            names: vec![],
                            detail: "provider reported a rate limit".into(),
                            hint: "the batch stopped; tasks stay queued".into(),
                        });
                    }
                }
                Event::BatchDone {
                    account_id,
                    completed,
                    total,
                } if *total > 0 => {
                    if let Ok(a) = accounts::get(&self.conn, *account_id) {
                        self.notify(&Notice {
                            kind: Kind::ExecutionCompleted,
                            account: a.display(),
                            count: *completed,
                            names: vec![],
                            detail: String::new(),
                            hint: if total > completed {
                                format!(
                                    "{} of {total} did not complete, see History",
                                    total - completed
                                )
                            } else {
                                String::new()
                            },
                        });
                    }
                }
                Event::TaskBlocked { reason, .. } => {
                    self.notify(&Notice {
                        kind: Kind::CapacityInsufficient,
                        account: "an account".into(),
                        count: 0,
                        names: vec![],
                        detail: reason.chars().take(200).collect(),
                        hint: "see the queue for the reason".into(),
                    });
                }
                _ => {}
            }
        }
        evs
    }

    pub fn clear_stop(&self) -> Result<()> {
        if self.read_only {
            return self.forward(&Request::ClearStop).map(|_| ());
        }
        self.controller.clear_stop();
        control::save_state(&self.conn, &self.controller)?;
        let _ = audit::record(&self.conn, "user", "stop_cleared", "");
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::claude::ClaudeAdapter;
    use crate::models::{ExecutionPolicy, TaskStatus};
    use crate::scheduler::NoIdle;
    use crate::tasks::NewTask;
    use crate::usage::NewSnapshot;
    use std::os::unix::fs::PermissionsExt;

    struct NullSink;
    impl ExternalSink for NullSink {
        fn desktop(&self, _: &str, _: &str) {}
        fn telegram(&self, _: &str, _: &str, _: &str) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    fn svc() -> (tempfile::TempDir, Service, i64) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(
            &bin,
            "#!/bin/sh\ncat >/dev/null\nif [ \"$SLOW\" = 1 ]; then sleep 30; fi\necho ok\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let profile = dir.path().join("p");
        std::fs::create_dir(&profile).unwrap();
        std::fs::write(profile.join(".credentials.json"), "{}").unwrap();
        let work = dir.path().join("w");
        std::fs::create_dir(&work).unwrap();
        let env = HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let s = Service::new(
            dir.path().join("t.db"),
            vec![Box::new(ClaudeAdapter { binary: bin })],
            env,
            Arc::new(NullSink),
        )
        .unwrap();
        let mut st = settings::load(&s.conn).unwrap();
        st.enabled = true;
        st.notify_desktop = false;
        st.working_root = work.to_string_lossy().into_owned();
        st.results_dir = dir.path().join("r").to_string_lossy().into_owned();
        settings::save(&s.conn, &st).unwrap();
        let a = accounts::create(
            &s.conn,
            "claude",
            "A",
            "Max",
            Some(profile.to_str().unwrap()),
        )
        .unwrap();
        accounts::set_subscription_confirmed(&s.conn, a.id, true).unwrap();
        let now = Utc::now();
        usage::record(
            &s.conn,
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
        (dir, s, a.id)
    }
    fn mk(s: &Service, name: &str) -> i64 {
        tasks::create(
            &s.conn,
            &NewTask {
                name: name.into(),
                prompt: "x".into(),
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
        .id
    }
    fn wait_idle(s: &Service) {
        for _ in 0..200 {
            if !s.is_running() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("batch did not finish");
    }

    fn wait_usage(s: &mut Service) -> Vec<Event> {
        for _ in 0..100 {
            let evs = s.poll();
            if evs.iter().any(|e| matches!(e, Event::UsageFetched { .. })) {
                return evs;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("no usage event");
    }

    fn oauth_account(dir: &std::path::Path, s: &Service, a: i64, exp: i64) {
        std::fs::write(
            dir.join("p/.credentials.json"),
            format!(r#"{{"claudeAiOauth":{{"accessToken":"tok","expiresAt":{exp},"subscriptionType":"max"}}}}"#),
        )
        .unwrap();
        let cur = accounts::get(&s.conn, a).unwrap();
        accounts::update(
            &s.conn,
            a,
            &accounts::AccountUpdate {
                custom: None,
                label: cur.label,
                plan: cur.subscription_plan,
                config_dir: cur.config_dir,
                credentials_file: None,
                usage_source: crate::models::UsageSource::ClaudeOauth,
                user_agent: None,
                cli_command: None,
                availability_schedule: cur.availability_schedule,
            },
        )
        .unwrap();
        accounts::set_subscription_confirmed(&s.conn, a, true).unwrap();
    }

    #[test]
    fn failed_usage_poll_keeps_old_data_and_blocks_when_stale() {
        let (d, mut s, a) = svc();
        oauth_account(d.path(), &s, a, 1_000_000_000_000); // expired token
        s.conn
            .execute(
                "UPDATE usage_snapshots SET captured_at = ?1",
                [db::ts(Utc::now() - chrono::Duration::hours(2))],
            )
            .unwrap();
        s.refresh_usage(a).unwrap();
        wait_usage(&mut s);
        assert!(s.usage_errors.lock().unwrap()[&a].contains("expired"));
        assert!(!s.usage_errors.lock().unwrap()[&a].contains("tok\""));
        mk(&s, "one");
        assert!(
            s.tick(&NoIdle).unwrap().approvals_requested == 0,
            "stale usage => nothing proposed"
        );
        assert!(notify::recent(&s.conn, 5)
            .unwrap()
            .iter()
            .any(|n| n.kind == "monitoring_error"));
    }

    #[test]
    fn successful_usage_poll_clears_error_and_enables_planning() {
        use std::io::{Read, Write};
        let (d, mut s, a) = svc();
        let ms = (Utc::now().timestamp() + 3600) * 1000;
        oauth_account(d.path(), &s, a, ms);
        s.conn.execute("DELETE FROM usage_snapshots", []).unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        s.usage_url = format!("http://{}/u", l.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = l.accept() {
                let mut b = [0u8; 4096];
                let _ = c.read(&mut b);
                let body = r#"{"five_hour":{"utilization":10,"resets_at":"2099-01-01T00:00:00Z"},"seven_day":{"utilization":20,"resets_at":"2099-01-05T00:00:00Z"}}"#;
                let _ = write!(
                    c,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        s.usage_errors.lock().unwrap().insert(a, "old".into());
        s.refresh_usage(a).unwrap();
        wait_usage(&mut s);
        assert!(s.usage_errors.lock().unwrap().is_empty());
        let u = usage::latest(&s.conn, a).unwrap().unwrap();
        assert_eq!(
            (u.session_remaining, u.weekly_remaining),
            (Some(90.0), Some(80.0))
        );
        mk(&s, "one");
        assert_eq!(s.tick(&NoIdle).unwrap().approvals_requested, 1);
    }

    #[test]
    fn approval_flow_runs_once_and_does_not_authorize_next_batch() {
        let (_d, mut s, _a) = svc();
        let t1 = mk(&s, "one");
        let rep = s.tick(&NoIdle).unwrap();
        assert_eq!(rep.approvals_requested, 1);
        assert!(rep.started.is_empty(), "approval is required by default");
        assert_eq!(
            tasks::get(&s.conn, t1).unwrap().status,
            TaskStatus::AwaitingApproval
        );
        let bid = s.approvals.pending()[0].id;
        s.approve_and_start(bid, &[t1]).unwrap();
        wait_idle(&s);
        assert_eq!(
            tasks::get(&s.conn, t1).unwrap().status,
            TaskStatus::Completed
        );
        // A later task needs its own approval.
        let t2 = mk(&s, "two");
        let rep = s.tick(&NoIdle).unwrap();
        assert!(rep.started.is_empty());
        assert_eq!(
            tasks::get(&s.conn, t2).unwrap().status,
            TaskStatus::AwaitingApproval
        );
        assert!(
            s.approve_and_start(bid, &[t2]).is_err(),
            "old approval is not reusable"
        );
    }

    fn set_usage(s: &Service, a: i64, session: f64, weekly: f64, age_min: i64) {
        let now = Utc::now();
        usage::record(
            &s.conn,
            &NewSnapshot {
                account_id: a,
                session_remaining: Some(session),
                weekly_remaining: Some(weekly),
                session_reset_at: Some(now + chrono::Duration::hours(2)),
                weekly_reset_at: Some(now + chrono::Duration::days(2)),
                confidence: Confidence::Manual,
                source: "manual".into(),
            },
            now - chrono::Duration::minutes(age_min),
        )
        .unwrap();
    }
    fn mk_est(s: &Service, name: &str, est: f64) -> i64 {
        tasks::create(
            &s.conn,
            &NewTask {
                name: name.into(),
                prompt: "x".into(),
                estimated_consumption: est,
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
        .id
    }

    #[test]
    fn burn_buttons_toggle_the_general_and_per_account_settings() {
        let (_d, s, a) = svc();
        let burning = |s: &Service| {
            let st = settings::load(&s.conn).unwrap();
            crate::present::burn_active(&st, &accounts::get(&s.conn, a).unwrap())
        };
        assert!(!burning(&s));
        s.set_burn_before_reset(true).unwrap();
        let st = settings::load(&s.conn).unwrap();
        assert_eq!(
            (st.burn_session_hours, st.burn_weekly_hours),
            (Some(1.0), Some(24.0))
        );
        assert!(burning(&s));
        // an account can switch it off although the general one is on...
        s.set_account_burn(a, false).unwrap();
        assert!(!burning(&s));
        // ...and on again, even when the general one is off
        s.set_burn_before_reset(false).unwrap();
        assert_eq!(settings::load(&s.conn).unwrap().burn_session_hours, None);
        assert!(!burning(&s));
        s.set_account_burn(a, true).unwrap();
        assert!(burning(&s));
    }

    #[test]
    fn burn_now_with_approval_proposes_a_batch_the_normal_rules_would_refuse() {
        let (_d, mut s, a) = svc();
        set_usage(&s, a, 30.0, 30.0, 1); // below the 65 % start threshold
        let t = mk_est(&s, "leftovers", 10.0);
        assert_eq!(s.propose_for(a).unwrap(), None, "the normal rules refuse");
        let rep = s.burn_now(Some(a), true).unwrap();
        assert_eq!((rep.proposed, rep.started), (1, 0), "{}", rep.message);
        let b = s.pending_batches().remove(0);
        assert!(b.proposal.burn);
        s.approve_and_start(b.id, &[t]).unwrap();
        wait_idle(&s);
        assert_eq!(
            tasks::get(&s.conn, t).unwrap().status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn shutdown_terminates_the_active_run_and_waits_for_it() {
        let (_d, mut s, a) = svc();
        s.env.insert("SLOW".into(), "1".into());
        set_usage(&s, a, 30.0, 30.0, 1);
        let t = mk_est(&s, "slow", 10.0);
        let rep = s.burn_now(Some(a), false).unwrap();
        assert_eq!(rep.started, 1, "{}", rep.message);
        assert!(s.is_running());
        let began = std::time::Instant::now();
        assert!(s.shutdown(std::time::Duration::from_secs(10)));
        assert!(
            began.elapsed() < std::time::Duration::from_secs(10),
            "must not wait out the 30 s child"
        );
        assert!(!s.is_running());
        assert_ne!(tasks::get(&s.conn, t).unwrap().status, TaskStatus::Running);
    }

    #[test]
    fn shutdown_when_idle_returns_at_once() {
        let (_d, mut s, _a) = svc();
        assert!(s.shutdown(std::time::Duration::from_millis(1)));
    }

    #[test]
    fn burn_now_without_approval_starts_at_once() {
        let (_d, mut s, a) = svc();
        set_usage(&s, a, 30.0, 30.0, 1);
        let t = mk_est(&s, "leftovers", 10.0);
        let rep = s.burn_now(Some(a), false).unwrap();
        assert_eq!((rep.proposed, rep.started), (0, 1), "{}", rep.message);
        wait_idle(&s);
        assert_eq!(
            tasks::get(&s.conn, t).unwrap().status,
            TaskStatus::Completed
        );
        assert!(s.pending_batches().is_empty());
        let log = audit::recent(&s.conn, 5).unwrap();
        assert!(log.iter().any(|e| e.action == "burn_now"));
    }

    #[test]
    fn burn_now_never_bypasses_safety() {
        // stale usage
        let (_d, mut s, a) = svc();
        set_usage(&s, a, 30.0, 30.0, 300);
        let t = mk_est(&s, "t", 10.0);
        let rep = s.burn_now(Some(a), false).unwrap();
        assert_eq!(rep.started + rep.proposed, 0, "{}", rep.message);
        assert_eq!(tasks::get(&s.conn, t).unwrap().status, TaskStatus::Pending);
        // the task needs more than what is left
        set_usage(&s, a, 30.0, 30.0, 1);
        tasks::set_status(&s.conn, t, TaskStatus::Cancelled).unwrap();
        let big = mk_est(&s, "big", 40.0);
        let rep = s.burn_now(Some(a), true).unwrap();
        assert_eq!(rep.started + rep.proposed, 0, "{}", rep.message);
        assert_eq!(
            tasks::get(&s.conn, big).unwrap().status,
            TaskStatus::Pending
        );
        // unverified subscription
        tasks::set_status(&s.conn, big, TaskStatus::Cancelled).unwrap();
        let ok = mk_est(&s, "ok", 10.0);
        accounts::set_subscription_confirmed(&s.conn, a, false).unwrap();
        let rep = s.burn_now(Some(a), false).unwrap();
        assert_eq!(rep.started + rep.proposed, 0, "{}", rep.message);
        accounts::set_subscription_confirmed(&s.conn, a, true).unwrap();
        // STOP
        s.emergency_stop().unwrap();
        let rep = s.burn_now(Some(a), false).unwrap();
        assert_eq!(rep.started + rep.proposed, 0, "{}", rep.message);
        assert_eq!(tasks::get(&s.conn, ok).unwrap().status, TaskStatus::Pending);
    }

    #[test]
    fn run_now_blockers_match_what_the_launch_would_refuse() {
        let (_d, s, a) = svc();
        let ok = mk(&s, "fits");
        assert!(s.run_now_blockers(ok, a).unwrap().is_empty());
        let big = tasks::create(
            &s.conn,
            &NewTask {
                name: "huge".into(),
                prompt: "x".into(),
                estimated_consumption: 99.0,
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
        .id;
        let why = s.run_now_blockers(big, a).unwrap();
        assert!(why.iter().any(|w| w.contains("left")), "{why:?}");
        accounts::set_available(&s.conn, a, false).unwrap();
        let why = s.run_now_blockers(ok, a).unwrap();
        assert!(why.iter().any(|w| w.contains("unavailable")), "{why:?}");
    }

    #[test]
    fn queue_status_explains_why_tasks_are_not_running() {
        let (_d, s, _a) = svc();
        let ready = mk(&s, "ready");
        let dep = mk(&s, "dependent");
        tasks::set_dependencies(&s.conn, dep, &[ready]).unwrap();
        let paused = mk(&s, "paused one");
        tasks::set_status(&s.conn, paused, TaskStatus::Paused).unwrap();
        let q = s.queue_status().unwrap();
        assert!(
            q.reasons[&ready].starts_with("ready: will be proposed"),
            "{:?}",
            q.reasons
        );
        // a task whose estimate cannot fit is reported as blocked, with the capacity reason
        let big = tasks::create(
            &s.conn,
            &NewTask {
                name: "big".into(),
                prompt: "x".into(),
                estimated_consumption: 99.0,
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
        .id;
        let why = s.queue_status().unwrap().reasons[&big].clone();
        assert!(why.starts_with("blocked:"), "{why}");
        assert!(
            q.reasons[&dep].contains(&format!("#{ready}")),
            "{:?}",
            q.reasons
        );
        assert!(q.reasons[&paused].contains("paused"));
        // a finished task has nothing to explain
        tasks::set_status(&s.conn, ready, TaskStatus::Completed).unwrap();
        assert!(!s.queue_status().unwrap().reasons.contains_key(&ready));
    }

    #[test]
    fn run_now_errors_name_the_task_account_and_next_step() {
        let (_d, s, a) = svc();
        let t = tasks::create(
            &s.conn,
            &NewTask {
                name: "wrong".into(),
                prompt: "x".into(),
                provider_type: Some("custom".into()),
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap();
        let e = s.run_now(t.id, a).unwrap_err().to_string();
        assert!(
            e.contains("wrong") && e.contains("'custom'") && e.contains("'claude'"),
            "{e}"
        );
        let id = mk(&s, "done one");
        tasks::set_status(&s.conn, id, TaskStatus::Completed).unwrap();
        let e = s.run_now(id, a).unwrap_err().to_string();
        assert!(
            e.contains("done one") && e.contains("can be run now"),
            "{e}"
        );
    }

    #[test]
    fn automatic_mode_runs_without_prompt_only_when_configured() {
        let (_d, mut s, a) = svc();
        let t = tasks::create(
            &s.conn,
            &NewTask {
                name: "auto".into(),
                prompt: "x".into(),
                requires_approval: false,
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap();
        accounts::set_policy(&s.conn, a, ExecutionPolicy::Automatic).unwrap();
        // global mode still Approval => no auto start
        assert!(s.tick(&NoIdle).unwrap().started.is_empty());
        s.approvals.cancel_all(&s.conn).unwrap();
        let mut st = settings::load(&s.conn).unwrap();
        st.execution_mode = ExecutionPolicy::Automatic;
        settings::save(&s.conn, &st).unwrap();
        assert_eq!(s.tick(&NoIdle).unwrap().started, vec![t.id]);
        wait_idle(&s);
        assert_eq!(
            tasks::get(&s.conn, t.id).unwrap().status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn prune_runs_once_a_day_and_only_on_the_driver() {
        let (_d, mut s, _a) = svc();
        let old = db::ts(Utc::now() - chrono::Duration::days(400));
        s.conn
            .execute(
                "INSERT INTO audit_log (at, actor, action) VALUES (?1,'t','old')",
                [&old],
            )
            .unwrap();
        s.read_only = true;
        assert_eq!(s.prune_if_due(Utc::now()).unwrap(), None);
        s.read_only = false;
        let rep = s
            .prune_if_due(Utc::now())
            .unwrap()
            .expect("first call runs");
        assert_eq!(rep.audit_log, 1);
        assert_eq!(s.prune_if_due(Utc::now()).unwrap(), None, "not due again");
        let gone: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE action='old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gone, 0);
    }

    #[test]
    fn set_enabled_changes_only_enabled() {
        let (_d, s, _a) = svc();
        let mut st = settings::load(&s.conn).unwrap();
        st.execution_mode = ExecutionPolicy::Automatic;
        settings::save(&s.conn, &st).unwrap();
        s.set_enabled(false).unwrap();
        let after = settings::load(&s.conn).unwrap();
        assert!(!after.enabled);
        assert_eq!(after.execution_mode, ExecutionPolicy::Automatic);
        s.set_enabled(true).unwrap();
        assert!(settings::load(&s.conn).unwrap().enabled);
    }

    #[test]
    fn read_only_instance_without_a_driver_cannot_change_settings_or_accounts() {
        let (_d, mut s, a) = svc();
        let before = settings::load(&s.conn).unwrap();
        let acct = accounts::get(&s.conn, a).unwrap();
        s.read_only = true;
        assert!(matches!(
            s.set_enabled(!before.enabled),
            Err(Error::Blocked(_))
        ));
        let mut st = before.clone();
        st.execution_mode = ExecutionPolicy::Automatic;
        assert!(s.save_settings(&st).is_err());
        assert!(s.create_account("claude", "x", "pro", None).is_err());
        assert!(s.set_account_policy(a, ExecutionPolicy::Automatic).is_err());
        assert!(s.set_account_available(a, false).is_err());
        assert!(s
            .set_account_run_mode(a, crate::models::RunMode::AutoEdit)
            .is_err());
        assert!(s
            .set_subscription_confirmed(a, !acct.subscription_confirmed)
            .is_err());
        assert!(s.delete_account(a).is_err());
        assert_eq!(settings::load(&s.conn).unwrap(), before);
        let after = accounts::get(&s.conn, a).unwrap();
        assert_eq!(after.execution_policy, acct.execution_policy);
        assert!(
            after.available,
            "read-only instance must not change availability"
        );
        assert_eq!(after.run_mode, acct.run_mode);
        assert_eq!(after.subscription_confirmed, acct.subscription_confirmed);
        // The driver may still do all of it.
        s.read_only = false;
        s.set_enabled(!before.enabled).unwrap();
        s.set_subscription_confirmed(a, !acct.subscription_confirmed)
            .unwrap();
    }

    #[test]
    fn disabled_or_stopped_never_launches() {
        let (_d, mut s, _a) = svc();
        mk(&s, "one");
        let mut st = settings::load(&s.conn).unwrap();
        st.enabled = false;
        settings::save(&s.conn, &st).unwrap();
        assert!(s.tick(&NoIdle).unwrap().held.is_some());
        st.enabled = true;
        settings::save(&s.conn, &st).unwrap();
        s.emergency_stop().unwrap();
        assert_eq!(
            s.tick(&NoIdle).unwrap().held.as_deref(),
            Some("automation paused or stopped")
        );
        assert!(s
            .start_batch(
                ApprovedBatch {
                    account_id: 1,
                    task_ids: vec![1],
                    burn: false
                },
                false
            )
            .is_err());
        assert_eq!(
            s.last_stop
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .process_confirmed,
            Some(true)
        );
        s.clear_stop().unwrap();
        assert!(s.tick(&NoIdle).unwrap().held.is_none());
    }

    #[test]
    fn stop_cancels_pending_approvals() {
        let (_d, mut s, _a) = svc();
        let t = mk(&s, "one");
        s.tick(&NoIdle).unwrap();
        assert_eq!(
            tasks::get(&s.conn, t).unwrap().status,
            TaskStatus::AwaitingApproval
        );
        s.emergency_stop().unwrap();
        assert_eq!(tasks::get(&s.conn, t).unwrap().status, TaskStatus::Pending);
        assert!(s.approvals.pending().is_empty());
        let a = audit::recent(&s.conn, 5).unwrap();
        assert!(a.iter().any(|e| e.action == "emergency_stop"));
    }

    #[test]
    fn restart_with_pending_tasks_keeps_them() {
        let (d, s, _a) = svc();
        let t = mk(&s, "keep");
        let running = mk(&s, "was running");
        tasks::set_status(&s.conn, running, TaskStatus::Running).unwrap();
        let path = s.db_path.clone();
        drop(s);
        let s2 = Service::new(path, vec![], HashMap::new(), Arc::new(NullSink)).unwrap();
        assert_eq!(tasks::get(&s2.conn, t).unwrap().status, TaskStatus::Pending);
        assert_eq!(
            tasks::get(&s2.conn, running).unwrap().status,
            TaskStatus::Failed
        );
        drop(d);
    }

    #[test]
    fn simultaneous_ticks_do_not_double_start() {
        let (_d, mut s, a) = svc();
        accounts::set_policy(&s.conn, a, ExecutionPolicy::Automatic).unwrap();
        let mut st = settings::load(&s.conn).unwrap();
        st.execution_mode = ExecutionPolicy::Automatic;
        settings::save(&s.conn, &st).unwrap();
        for i in 0..2 {
            tasks::create(
                &s.conn,
                &NewTask {
                    name: format!("t{i}"),
                    prompt: "x".into(),
                    requires_approval: false,
                    ..Default::default()
                },
                TaskStatus::Pending,
            )
            .unwrap();
        }
        let r1 = s.tick(&NoIdle).unwrap();
        let r2 = s.tick(&NoIdle).unwrap();
        assert!(!r1.started.is_empty());
        assert!(r2.started.is_empty() || !s.is_running() || r2.held.is_some());
        wait_idle(&s);
    }

    #[test]
    fn queue_from_template_and_duplicate_task_work_through_the_service() {
        let (_d, s, _a) = svc();
        let mut t = NewTask {
            prompt: "do it".into(),
            ..Default::default()
        };
        t.estimated_minutes = 7;
        let id = crate::templates::save(&s.conn, "nightly", &t).unwrap();
        s.queue_from_template(id).unwrap();
        let q = tasks::list(&s.conn).unwrap();
        let made = q.iter().find(|t| t.name == "nightly").unwrap();
        assert_eq!(
            (made.status, made.estimated_minutes),
            (TaskStatus::Pending, 7)
        );
        assert!(s.queue_from_template(999).is_err());
        s.task_action(made.id, crate::ipc::TaskAction::Duplicate)
            .unwrap();
        assert!(tasks::list(&s.conn)
            .unwrap()
            .iter()
            .any(|t| t.name == "nightly (copy)"));
    }

    #[test]
    fn import_creates_pending_tasks_resolves_dependencies_and_reports_bad_records() {
        use crate::import::ImportFormat;
        let (_d, s, _a) = svc();
        let json = r#"[
            {"name":"build","prompt":"build it"},
            {"name":"test","prompt":"test it","depends_on":["Build"],"priority":"high"},
            {"name":"","prompt":"no name"},
            {"name":"later","prompt":"p","depends_on":["ghost"]}
        ]"#;
        let dry = s.import_tasks(json, ImportFormat::Json, true).unwrap();
        assert_eq!((dry.created.len(), dry.errors.len()), (3, 1));
        assert!(
            tasks::list(&s.conn).unwrap().is_empty(),
            "dry run stores nothing"
        );

        let r = s.import_tasks(json, ImportFormat::Auto, false).unwrap();
        assert_eq!(r.created.len(), 3);
        assert!(r.errors.iter().any(|(n, _)| *n == 3));
        assert!(r.errors.iter().any(|(n, m)| *n == 4 && m.contains("ghost")));
        let q = tasks::list(&s.conn).unwrap();
        assert!(q
            .iter()
            .all(|t| t.status == TaskStatus::Pending && t.requires_approval));
        let build = q.iter().find(|t| t.name == "build").unwrap();
        let test = q.iter().find(|t| t.name == "test").unwrap();
        assert_eq!(test.priority, 2);
        assert_eq!(
            tasks::dependencies(&s.conn, test.id).unwrap(),
            vec![build.id]
        );
    }

    #[test]
    fn import_rejects_garbage_and_read_only_instances_without_a_driver() {
        use crate::import::ImportFormat;
        let (_d, mut s, _a) = svc();
        assert!(s.import_tasks("{nope", ImportFormat::Json, false).is_err());
        s.read_only = true;
        let before = tasks::list(&s.conn).unwrap().len();
        assert!(matches!(
            s.import_tasks("- a: b", ImportFormat::Markdown, false),
            Err(Error::Blocked(_))
        ));
        assert!(s.reorder_tasks(&[1]).is_err());
        assert_eq!(tasks::list(&s.conn).unwrap().len(), before);
    }

    #[test]
    fn reorder_tasks_rejects_unknown_ids_and_reorders() {
        use crate::import::ImportFormat;
        let (_d, s, _a) = svc();
        let r = s
            .import_tasks("- a: one\n- b: two\n", ImportFormat::Markdown, false)
            .unwrap();
        let (ia, ib) = (r.created[0].1, r.created[1].1);
        assert!(s.reorder_tasks(&[ib, 9999]).is_err());
        s.reorder_tasks(&[ib, ia]).unwrap();
        let names: Vec<_> = s
            .list_tasks()
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["b", "a"]);
    }
}
