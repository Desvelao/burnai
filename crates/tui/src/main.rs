//! Terminal front-end. Like the GUI it contains no business rules: every action goes through
//! `plan_burner_core::service::Service`, forms map through `plan_burner_core::forms`, and session
//! glue comes from `plan_burner_core::session`. Feature parity with the GUI is a project rule
//! (see AGENTS.md and docs/FEATURE_PARITY.md).
mod editors;
mod form;
mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use form::{Form, FormEvent};
use plan_burner_core::{
    analytics,
    control::RunState,
    forms::{AccountForm, TaskForm, TemplateForm, UsageForm},
    ipc::TaskAction,
    models::{ExecutionPolicy, RunMode, TaskStatus},
    notify, present,
    service::{SchedulerStatus, Service},
    session,
    settings::Settings,
    tasks, usage,
};
use session::Snapshot;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Dashboard,
    Queue,
    Accounts,
    Approvals,
    History,
    Templates,
    Settings,
}
impl View {
    pub const ALL: [View; 7] = [
        View::Dashboard,
        View::Queue,
        View::Accounts,
        View::Approvals,
        View::History,
        View::Templates,
        View::Settings,
    ];
    pub fn title(self) -> &'static str {
        match self {
            View::Dashboard => "Dashboard",
            View::Queue => "Queue",
            View::Accounts => "Accounts",
            View::Approvals => "Approvals",
            View::History => "History",
            View::Templates => "Templates",
            View::Settings => "Settings",
        }
    }
}

/// Queue filter (same choices as the GUI chips).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filter {
    All,
    Account(i64),
    AnyAccount,
}

pub enum EditKind {
    Settings,
    Account(AccountForm),
    Task(TaskForm),
    Usage(UsageForm),
    Template(TemplateForm),
    /// Record measured consumption of an execution.
    Actual(i64),
    /// Import tasks from a JSON / Markdown file.
    Import,
}

/// A full-page form (editors replace the body; there are no floating windows).
pub struct Editor {
    pub kind: EditKind,
    pub form: Form,
    pub confirm_discard: bool,
    /// Ctrl-D: next discovered login (accounts). Task forms: last template applied (0 = none).
    pub cycle: usize,
}

/// How long a success message stays on screen.
const STATUS_TTL: Duration = Duration::from_secs(8);

pub struct TextView {
    pub title: String,
    pub body: String,
    pub scroll: u16,
}

pub enum Confirm {
    Stop,
    DeleteTask(i64),
    DeleteAccount(i64),
    DeleteTemplate(i64),
    /// Stop the task that is executing (kills its process).
    StopRunning(i64),
    /// "Burn now without approvals" for one account (`Some`) or all of them (`None`).
    BurnNow(Option<i64>),
    /// Reject a proposed batch of this many tasks.
    RejectBatch {
        id: u64,
        tasks: usize,
    },
}

/// Shown (in a neutral colour) when a confirmation was declined.
pub const MSG_CANCELLED: &str = "Cancelled.";

pub enum Mode {
    Browse,
    Edit(Box<Editor>),
    Text(TextView),
    Help,
    /// Choose the account to run a task on now.
    Pick {
        task: i64,
        accounts: Vec<i64>,
        idx: usize,
    },
    Confirm(Confirm),
}

pub struct App {
    pub svc: Service,
    pub settings: Settings,
    pub snap: Snapshot,
    pub view: View,
    pub mode: Mode,
    pub sel: usize,
    pub status: Option<(String, bool)>,
    /// When `status` last changed (successes fade after a few seconds).
    pub status_seen: Option<((String, bool), Instant)>,
    pub history: Vec<analytics::HistoryRow>,
    pub audit: Vec<plan_burner_core::audit::AuditEntry>,
    pub notifs: Vec<notify::Stored>,
    pub templates: Vec<plan_burner_core::templates::Template>,
    pub usage: HashMap<i64, usage::UsageSnapshot>,
    pub forecasts: HashMap<i64, String>,
    pub filter: Filter,
    pub detail: bool,
    pub hist_audit: bool,
    /// Tasks switched off per pending batch (default: all selected) and the task cursor.
    pub approval_off: HashMap<u64, HashSet<i64>>,
    pub tsel: usize,
    pub quit: bool,
    /// Why queued tasks are not running (refreshed with the snapshot).
    pub qstatus: plan_burner_core::service::QueueStatus,
    /// First visible line of the help page, and its last rendered height (set by the renderer).
    pub help_scroll: u16,
    pub help_height: std::cell::Cell<u16>,
    last_refresh: Instant,
}

impl App {
    fn new() -> Result<App, Box<dyn std::error::Error>> {
        let (svc, settings) = session::open_client_service()?;
        Ok(App::with_service(svc, settings))
    }

    pub fn with_service(svc: Service, settings: Settings) -> App {
        let mut app = App {
            svc,
            settings,
            snap: Snapshot::default(),
            view: View::Dashboard,
            mode: Mode::Browse,
            sel: 0,
            status: None,
            status_seen: None,
            history: vec![],
            audit: vec![],
            notifs: vec![],
            templates: vec![],
            usage: Default::default(),
            forecasts: Default::default(),
            filter: Filter::All,
            detail: false,
            hist_audit: false,
            approval_off: Default::default(),
            tsel: 0,
            quit: false,
            qstatus: Default::default(),
            help_scroll: 0,
            help_height: std::cell::Cell::new(0),
            last_refresh: Instant::now(),
        };
        let sched = app.svc.scheduler_status();
        if sched != SchedulerStatus::Running {
            app.status = Some((format!("{}: press R to start it.", sched.label()), false));
        }
        app.refresh();
        app
    }

    pub fn refresh(&mut self) {
        self.snap = session::snapshot(&self.svc, std::mem::take(&mut self.snap));
        // Keep unsaved settings edits: the editor owns the values while it is open.
        let editing_settings = matches!(
            &self.mode,
            Mode::Edit(e) if matches!(e.kind, EditKind::Settings)
        );
        if !editing_settings {
            if let Ok(s) = plan_burner_core::settings::load(&self.svc.conn) {
                self.settings = s;
            }
        }
        if let Ok(q) = self.svc.queue_status() {
            self.qstatus = q;
        }
        let now = chrono::Utc::now();
        for a in &self.snap.accounts {
            if let Ok(Some(u)) = usage::latest(&self.svc.conn, a.id) {
                self.usage.insert(a.id, u);
            }
            let eff = self.settings.effective_for(a);
            self.forecasts.insert(
                a.id,
                match analytics::forecast(
                    &self.svc.conn,
                    a.id,
                    eff.reserved_capacity,
                    eff.max_usage_age_minutes as i64,
                    now,
                ) {
                    Ok(f) => match f.unused_weekly_estimate {
                        Some(v) => format!("~{v:.0}% weekly ({})", f.confidence),
                        None => format!("unknown ({})", f.note),
                    },
                    Err(_) => "error".into(),
                },
            );
        }
        if self.view == View::History {
            if let Ok(h) = analytics::history(&self.svc.conn, 200) {
                self.history = h;
            }
            if self.hist_audit {
                if let Ok(a) = plan_burner_core::audit::recent(&self.svc.conn, 50) {
                    self.audit = a;
                }
            }
        }
        if let Ok(t) = plan_burner_core::templates::list(&self.svc.conn) {
            self.templates = t;
        }
        if self.view == View::Dashboard {
            if let Ok(n) = notify::recent(&self.svc.conn, 6) {
                self.notifs = n;
            }
        }
        self.last_refresh = Instant::now();
        let n = self.list_len();
        if self.sel >= n {
            self.sel = n.saturating_sub(1);
        }
    }

    pub fn pending(&self) -> Vec<(u64, i64, Vec<i64>)> {
        let mut v: Vec<_> = self
            .svc
            .pending_batches()
            .iter()
            .map(|b| {
                (
                    b.id,
                    b.proposal.account_id,
                    b.proposal.tasks.iter().map(|t| t.id).collect(),
                )
            })
            .collect();
        v.sort_by_key(|p| p.0);
        v
    }

    /// Tasks matching the queue filter, in queue order.
    pub fn visible_tasks(&self) -> Vec<&tasks::Task> {
        self.snap
            .tasks
            .iter()
            .filter(|t| match self.filter {
                Filter::All => true,
                Filter::Account(id) => t.account_id == Some(id),
                Filter::AnyAccount => t.account_id.is_none(),
            })
            .collect()
    }

    fn list_len(&self) -> usize {
        match self.view {
            View::Dashboard | View::Accounts => self.snap.accounts.len(),
            View::Queue => self.visible_tasks().len(),
            View::Approvals => self.pending().len(),
            View::History if self.hist_audit => self.audit.len(),
            View::History => self.history.len(),
            View::Templates => self.templates.len(),
            View::Settings => 0,
        }
    }

    fn report<T>(&mut self, r: plan_burner_core::Result<T>, ok: &str) -> Option<T> {
        let out = match r {
            Ok(v) => {
                self.status = Some((ok.to_string(), true));
                Some(v)
            }
            Err(e) => {
                self.status = Some((e.to_string(), false));
                None
            }
        };
        self.refresh();
        out
    }

    fn set_view(&mut self, v: View) {
        if v == View::Settings {
            self.open_settings();
            return;
        }
        self.view = v;
        self.sel = 0;
        self.tsel = 0;
        self.refresh();
    }

    /// Successes fade after `STATUS_TTL`; errors stay until the next key press (see `key`).
    fn age_status(&mut self) {
        match (&self.status, &self.status_seen) {
            (Some(cur), Some((seen, at))) if cur == seen => {
                if cur.1 && at.elapsed() > STATUS_TTL {
                    self.status = None;
                    self.status_seen = None;
                }
            }
            (Some(cur), _) => self.status_seen = Some((cur.clone(), Instant::now())),
            (None, _) => self.status_seen = None,
        }
    }

    fn pump(&mut self) {
        self.age_status();
        let mut any = false;
        for e in self.svc.poll() {
            if let Some(st) = session::event_status(&e) {
                self.status = Some(st);
            }
            any = true;
        }
        if any || self.last_refresh.elapsed() > Duration::from_secs(1) {
            self.refresh();
        }
    }

    fn selected_task(&self) -> Option<tasks::Task> {
        self.visible_tasks().get(self.sel).map(|t| (*t).clone())
    }

    fn selected_account(&self) -> Option<plan_burner_core::models::Account> {
        self.snap.accounts.get(self.sel).cloned()
    }

    // ------------------------------------------------------------ input

    pub fn key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        // An error message has been seen once the user presses anything.
        if matches!(self.status, Some((_, false))) {
            self.status = None;
        }
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.request_quit();
            return;
        }
        match &self.mode {
            Mode::Edit(_) => self.edit_key(k),
            Mode::Text(_) => self.text_key(k),
            Mode::Help => self.help_key(k),
            Mode::Pick { .. } => self.pick_key(k),
            Mode::Confirm(_) => self.confirm_key(k),
            Mode::Browse => self.browse_key(k),
        }
    }

    /// Ctrl-C and `q`. Never loses work silently: a dirty form asks before it is discarded.
    /// Pressing Ctrl-C again at that prompt quits. Quitting never affects a running batch (the daemon owns it).
    fn request_quit(&mut self) {
        match &mut self.mode {
            Mode::Edit(e) if e.form.dirty && !e.confirm_discard => {
                e.confirm_discard = true;
                self.status = Some((
                    "Unsaved changes: y discards the form, Ctrl-C again quits anyway.".into(),
                    false,
                ));
            }
            _ => self.quit = true,
        }
    }

    pub fn paste(&mut self, s: &str) {
        if let Mode::Edit(e) = &mut self.mode {
            e.form.paste(s);
        }
    }

    fn browse_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => {
                self.help_scroll = 0;
                self.mode = Mode::Help;
            }
            KeyCode::Tab | KeyCode::Right => self.cycle(1),
            KeyCode::BackTab | KeyCode::Left => self.cycle(-1),
            KeyCode::Char(c @ '1'..='7') => self.set_view(View::ALL[c as usize - '1' as usize]),
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(self.list_len().saturating_sub(1));
                self.tsel = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                self.tsel = 0;
            }
            KeyCode::Char('S') => self.mode = Mode::Confirm(Confirm::Stop),
            // General burn controls (account ones: b / w / W on the Dashboard and Accounts views).
            KeyCode::Char('B') => {
                let on = self.settings.burn_session_hours.is_some_and(|h| h > 0.0)
                    || self.settings.burn_weekly_hours.is_some_and(|h| h > 0.0);
                let r = self.svc.set_burn_before_reset(!on);
                self.report(
                    r,
                    if on {
                        "Burn before reset turned off."
                    } else {
                        "Burn before reset turned on (last 1 h of the 5 h window, last 24 h of the week)."
                    },
                );
            }
            KeyCode::Char('O') => self.run_burn(None, true),
            KeyCode::Char('X') => self.mode = Mode::Confirm(Confirm::BurnNow(None)),
            KeyCode::Char('C') => {
                let r = self.svc.clear_stop();
                self.report(r, "Stop cleared. Automation may launch again.");
            }
            KeyCode::Char('p') => {
                if self.svc.run_state(chrono::Utc::now()) == RunState::Paused {
                    let r = self.svc.resume();
                    self.report(r, "Automation resumed.");
                } else {
                    let r = self.svc.pause_now();
                    self.report(r, "Automation paused.");
                }
            }
            // Pause until the earliest known session reset (same as the GUI button).
            KeyCode::Char('T') => {
                let r = self.svc.pause_until_next_reset();
                self.report(r, "Paused until the next session reset.");
            }
            // Start the scheduler daemon (clients never run the scheduler themselves).
            KeyCode::Char('R') => {
                self.status = Some(match self.svc.scheduler_status() {
                    SchedulerStatus::Running => ("The scheduler is already running.".into(), true),
                    _ => match plan_burner_core::launch::spawn_daemon() {
                        Ok(()) => ("Starting the scheduler…".into(), true),
                        Err(e) => (e, false),
                    },
                });
            }
            KeyCode::Char('E') => {
                let on = !self.settings.enabled;
                let r = self.svc.set_enabled(on);
                self.report(
                    r,
                    if on {
                        "Automation enabled."
                    } else {
                        "Automation disabled."
                    },
                );
            }
            _ => self.view_key(k),
        }
    }

    fn cycle(&mut self, d: i32) {
        let i = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0) as i32;
        let n = View::ALL.len() as i32;
        self.set_view(View::ALL[((i + d).rem_euclid(n)) as usize]);
    }

    fn view_key(&mut self, k: KeyEvent) {
        match self.view {
            View::Dashboard | View::Accounts => self.account_key(k),
            View::Queue => self.queue_key(k),
            View::Approvals => self.approvals_key(k),
            View::History => self.history_key(k),
            View::Templates => self.template_key(k),
            View::Settings => {}
        }
    }

    fn account_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('a') {
            return self.open_account(None);
        }
        let Some(a) = self.selected_account() else {
            return;
        };
        match k.code {
            KeyCode::Char('e') | KeyCode::Enter if self.view == View::Accounts => {
                self.open_account(Some(a))
            }
            KeyCode::Char('e') => self.open_account(Some(a)),
            KeyCode::Char('D') => self.mode = Mode::Confirm(Confirm::DeleteAccount(a.id)),
            KeyCode::Char('g') => {
                self.status = Some(match self.svc.propose_for(a.id) {
                    Ok(Some(_)) => {
                        self.set_view(View::Approvals);
                        ("Batch proposed. Review it here.".into(), true)
                    }
                    Ok(None) => (
                        "Nothing can run on this account right now (no eligible tasks, or capacity/billing checks failed)."
                            .into(),
                        false,
                    ),
                    Err(e) => (e.to_string(), false),
                });
                self.refresh();
            }
            KeyCode::Char('u') => {
                let r = self.svc.refresh_usage(a.id);
                self.report(r, "Fetching usage…");
            }
            KeyCode::Char('U') => {
                let u = self.usage.get(&a.id);
                let form = UsageForm::for_account(
                    a.id,
                    u.and_then(|u| u.session_remaining),
                    u.and_then(|u| u.weekly_remaining),
                );
                self.open(
                    EditKind::Usage(form.clone()),
                    editors::usage_form(&form, &a.display()),
                );
            }
            KeyCode::Char('i') => {
                let r = self
                    .svc
                    .set_account_ignore_limits(a.id, !a.limits.ignore_limits);
                self.report(
                    r,
                    if a.limits.ignore_limits {
                        "Limits apply again."
                    } else {
                        "Account is now eligible ignoring limits."
                    },
                );
            }
            KeyCode::Char('b') => {
                let on = plan_burner_core::present::burn_active(&self.settings, &a);
                let r = self.svc.set_account_burn(a.id, !on);
                self.report(
                    r,
                    if on {
                        "Burn before reset turned off for this account."
                    } else {
                        "Burn before reset turned on for this account (last 1 h / 24 h)."
                    },
                );
            }
            KeyCode::Char('w') => self.run_burn(Some(a.id), true),
            KeyCode::Char('W') => self.mode = Mode::Confirm(Confirm::BurnNow(Some(a.id))),
            KeyCode::Char('v') => {
                let r = self.svc.set_account_available(a.id, !a.available);
                self.report(r, "");
            }
            KeyCode::Char('c') => {
                let r = self
                    .svc
                    .set_subscription_confirmed(a.id, !a.subscription_confirmed);
                self.report(r, "");
            }
            KeyCode::Char('m') => {
                let all = RunMode::ALL;
                let i = all.iter().position(|m| *m == a.run_mode).unwrap_or(0);
                let r = self
                    .svc
                    .set_account_run_mode(a.id, all[(i + 1) % all.len()]);
                self.report(r, "");
            }
            KeyCode::Char('P') => {
                let all = ExecutionPolicy::ALL;
                let i = all
                    .iter()
                    .position(|p| *p == a.execution_policy)
                    .unwrap_or(0);
                let r = self.svc.set_account_policy(a.id, all[(i + 1) % all.len()]);
                self.report(r, "");
            }
            KeyCode::Char('f') => {
                self.filter = Filter::Account(a.id);
                self.set_view(View::Queue);
            }
            _ => {}
        }
    }

    fn queue_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('n') {
            return self.open_task(None);
        }
        if k.code == KeyCode::Char('i') {
            return self.open(EditKind::Import, editors::import_form());
        }
        if k.code == KeyCode::Char('f') {
            let mut order = vec![Filter::All];
            order.extend(self.snap.accounts.iter().map(|a| Filter::Account(a.id)));
            order.push(Filter::AnyAccount);
            let i = order.iter().position(|f| *f == self.filter).unwrap_or(0);
            self.filter = order[(i + 1) % order.len()];
            self.sel = 0;
            return;
        }
        if k.code == KeyCode::Enter {
            self.detail = !self.detail;
            return;
        }
        let Some(t) = self.selected_task() else {
            return;
        };
        let running = t.status == TaskStatus::Running;
        match k.code {
            KeyCode::Char('e') if !running => self.open_task(Some(t)),
            KeyCode::Char('x') => {
                let (a, ok) = if t.status == TaskStatus::Paused {
                    (TaskAction::Resume, "Task resumed.")
                } else {
                    (TaskAction::Pause, "Task paused.")
                };
                let r = self.svc.task_action(t.id, a);
                self.report(r, ok);
            }
            KeyCode::Char('c') if running => self.mode = Mode::Confirm(Confirm::StopRunning(t.id)),
            KeyCode::Char('c') => {
                let r = self.svc.task_action(t.id, TaskAction::Cancel);
                self.report(r, "Task cancelled.");
            }
            KeyCode::Char('t') => {
                let r = self.svc.task_action(t.id, TaskAction::Retry);
                self.report(r, "Task re-queued.");
            }
            KeyCode::Char('y') => {
                let r = self.svc.task_action(t.id, TaskAction::Duplicate);
                self.report(r, "Task duplicated.");
            }
            KeyCode::Char('K') | KeyCode::Char('J') => {
                let d = if k.code == KeyCode::Char('K') { -1 } else { 1 };
                let r = self.svc.task_action(t.id, TaskAction::MoveBy { delta: d });
                if self.report(r, "").is_some() && self.filter == Filter::All {
                    self.sel = (self.sel as i32 + d).clamp(0, self.list_len() as i32 - 1) as usize;
                }
            }
            KeyCode::Char('d') if !running => self.mode = Mode::Confirm(Confirm::DeleteTask(t.id)),
            KeyCode::Char('v') => {
                if let Some(p) = &t.result_path {
                    self.view_result(&t.name, p);
                } else {
                    self.status = Some(("This task has no result yet.".into(), false));
                }
            }
            // Run the task now on its pinned account, or choose among the accounts of its provider.
            KeyCode::Char('r') => {
                let cands = run_now_candidates(
                    self.snap
                        .accounts
                        .iter()
                        .map(|a| (a.id, a.provider_type.as_str())),
                    t.account_id,
                    t.provider_type.as_deref(),
                );
                match cands.as_slice() {
                    [id] => {
                        let r = self.svc.run_now(t.id, *id);
                        self.report(r, "Started (all pre-flight checks still apply).");
                    }
                    [] => self.status = Some(("No matching account.".into(), false)),
                    _ => {
                        self.mode = Mode::Pick {
                            task: t.id,
                            accounts: cands,
                            idx: 0,
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn template_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('n') {
            let f = TemplateForm::new();
            let form = editors::template_form(&f);
            return self.open(EditKind::Template(f), form);
        }
        let Some(t) = self.templates.get(self.sel).cloned() else {
            return;
        };
        match k.code {
            KeyCode::Char('e') | KeyCode::Enter => {
                let f = TemplateForm::from_template(&t);
                let form = editors::template_form(&f);
                self.open(EditKind::Template(f), form);
            }
            KeyCode::Char('y') => {
                let r = self.svc.duplicate_template(t.id);
                self.report(r, "Template duplicated.");
            }
            KeyCode::Char('Q') => {
                let r = self.svc.queue_from_template(t.id);
                self.report(r, "Task queued from template.");
            }
            KeyCode::Char('D') => self.mode = Mode::Confirm(Confirm::DeleteTemplate(t.id)),
            KeyCode::Char('t') => {
                self.view = View::Queue;
                self.sel = 0;
                let f = TaskForm::from_template(&t);
                let form = editors::task_form(&f, &self.snap.accounts, &self.templates);
                self.open(EditKind::Task(f), form);
            }
            _ => {}
        }
    }

    fn approvals_key(&mut self, k: KeyEvent) {
        let Some((id, _, ids)) = self.pending().get(self.sel).cloned() else {
            return;
        };
        let off = self.approval_off.entry(id).or_default().clone();
        match k.code {
            KeyCode::Char(']') => self.tsel = (self.tsel + 1).min(ids.len().saturating_sub(1)),
            KeyCode::Char('[') => self.tsel = self.tsel.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(t) = ids.get(self.tsel) {
                    let set = self.approval_off.entry(id).or_default();
                    if !set.remove(t) {
                        set.insert(*t);
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char('A') => {
                let chosen = plan_burner_core::forms::selected_ids(&ids, &off);
                if chosen.is_empty() {
                    self.status = Some(("Select at least one task (Space).".into(), false));
                } else {
                    let r = self.svc.approve_and_start(id, &chosen);
                    self.report(r, "Approved; batch started.");
                }
            }
            KeyCode::Char('a') => {
                if self.settings.approval_policy == "always" {
                    self.status = Some((
                        "Approval policy is 'always': pick tasks with Space and press Enter."
                            .into(),
                        false,
                    ));
                } else {
                    let r = self.svc.approve_and_start(id, &ids);
                    self.report(r, "Approved; batch started.");
                }
            }
            KeyCode::Char('x') => {
                self.mode = Mode::Confirm(Confirm::RejectBatch {
                    id,
                    tasks: ids.len(),
                })
            }
            _ => {}
        }
    }

    fn history_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('A') => {
                self.hist_audit = !self.hist_audit;
                self.sel = 0;
                self.refresh();
            }
            KeyCode::Char('e') => {
                let dir = self.settings.results_dir.clone();
                let r = (|| -> plan_burner_core::Result<String> {
                    std::fs::create_dir_all(&dir)?;
                    let rows = analytics::history(&self.svc.conn, 200)?;
                    let d = std::path::Path::new(&dir);
                    std::fs::write(d.join("history.csv"), analytics::export_history_csv(&rows))?;
                    std::fs::write(
                        d.join("history.json"),
                        analytics::export_history_json(&rows)?,
                    )?;
                    Ok(dir.clone())
                })();
                match r {
                    Ok(d) => {
                        self.status = Some((
                            format!("Exported history.csv and history.json to {d}"),
                            true,
                        ))
                    }
                    Err(e) => self.status = Some((e.to_string(), false)),
                }
            }
            _ if self.hist_audit => {}
            KeyCode::Char('a') => {
                if let Some(h) = self.history.get(self.sel) {
                    let form = editors::actual_form(&h.task_name, h.actual_consumption);
                    self.open(EditKind::Actual(h.execution_id), form);
                }
            }
            KeyCode::Char('v') | KeyCode::Enter => {
                if let Some(h) = self.history.get(self.sel).cloned() {
                    match &h.result_path {
                        Some(p) => self.view_result(&h.task_name, p),
                        None => self.status = Some(("This run has no result file.".into(), false)),
                    }
                }
            }
            _ => {}
        }
    }

    // ----------------------------------------------------------- modes

    fn open(&mut self, kind: EditKind, form: Form) {
        self.mode = Mode::Edit(Box::new(Editor {
            kind,
            form,
            confirm_discard: false,
            cycle: 0,
        }));
    }

    fn open_settings(&mut self) {
        if let Ok(s) = plan_burner_core::settings::load(&self.svc.conn) {
            self.settings = s;
        }
        self.view = View::Settings;
        let form = editors::settings_form(&self.settings);
        self.open(EditKind::Settings, form);
    }

    fn open_account(&mut self, a: Option<plan_burner_core::models::Account>) {
        let f = match &a {
            Some(a) => AccountForm::from_account(a),
            None => AccountForm::new(),
        };
        let form = editors::account_form(&f, &self.settings);
        self.open(EditKind::Account(f), form);
    }

    fn open_task(&mut self, t: Option<tasks::Task>) {
        let f = match &t {
            Some(t) => {
                let deps = tasks::dependencies(&self.svc.conn, t.id).unwrap_or_default();
                TaskForm::from_task(t, &deps)
            }
            None => TaskForm::new_task(),
        };
        let form = editors::task_form(&f, &self.snap.accounts, &self.templates);
        self.open(EditKind::Task(f), form);
    }

    fn close_editor(&mut self) {
        if let Mode::Edit(e) = &self.mode {
            if matches!(e.kind, EditKind::Settings) {
                self.view = View::Dashboard;
            }
        }
        self.mode = Mode::Browse;
        self.refresh();
    }

    fn edit_key(&mut self, k: KeyEvent) {
        let Mode::Edit(ed) = &mut self.mode else {
            return;
        };
        if ed.confirm_discard {
            ed.confirm_discard = false;
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.close_editor();
            }
            return;
        }
        let ev = ed.form.handle(k);
        if let EditKind::Account(base) = &ed.kind {
            editors::account_visibility(&mut ed.form);
            editors::account_marks(&mut ed.form, base);
        }
        // Choosing a template in the (always visible) "Load template" field fills the task form.
        if let EditKind::Task(base) = &ed.kind {
            let idx = ed.form.choice_idx("template");
            if idx != ed.cycle && idx > 0 {
                if let Some(tpl) = self.templates.get(idx - 1) {
                    let mut tf = editors::read_task(&ed.form, base, &self.snap.accounts)
                        .unwrap_or_else(|_| base.clone());
                    tf.apply_template(tpl);
                    let focus = ed.form.focus;
                    ed.form = editors::task_form(&tf, &self.snap.accounts, &self.templates);
                    ed.form.focus = focus;
                    ed.form.dirty = true;
                    ed.form.set_choice("template", idx);
                    self.status = Some((format!("Template “{}” loaded.", tpl.name), true));
                }
            }
            ed.cycle = idx;
        }
        match ev {
            FormEvent::None => {}
            FormEvent::Cancel => {
                if ed.form.dirty {
                    ed.confirm_discard = true;
                } else {
                    self.close_editor();
                }
            }
            FormEvent::Save => self.save_editor(),
            FormEvent::Shortcut(c) => self.editor_shortcut(c),
        }
    }

    fn save_editor(&mut self) {
        let Mode::Edit(ed) = &mut self.mode else {
            return;
        };
        let accounts = self.snap.accounts.clone();
        let mut report_text: Option<String> = None;
        let outcome: Result<String, String> = (|| match &ed.kind {
            EditKind::Settings => {
                let s = editors::read_settings(&ed.form, &self.settings)?;
                self.svc.save_settings(&s).map_err(|e| e.to_string())?;
                self.settings = s;
                Ok("Settings saved.".into())
            }
            EditKind::Account(base) => {
                let f = editors::read_account(&ed.form, base)?;
                self.svc.save_account(&f).map_err(|e| e.to_string())?;
                Ok("Account saved.".into())
            }
            EditKind::Task(base) => {
                let f = editors::read_task(&ed.form, base, &accounts)?;
                self.svc.save_task(&f).map_err(|e| e.to_string())?;
                Ok("Task saved.".into())
            }
            EditKind::Template(base) => {
                let f = editors::read_template(&ed.form, base)?;
                self.svc.save_template_form(&f).map_err(|e| e.to_string())?;
                Ok("Template saved.".into())
            }
            EditKind::Usage(base) => {
                let f = editors::read_usage(&ed.form, base)?;
                self.svc.record_usage(&f).map_err(|e| e.to_string())?;
                Ok("Usage recorded.".into())
            }
            EditKind::Import => {
                let (path, format, dry) = editors::read_import(&ed.form);
                let r = self
                    .svc
                    .import_file(&path, format, dry)
                    .map_err(|e| e.to_string())?;
                report_text = Some(r.render());
                Ok(r.summary())
            }
            EditKind::Actual(id) => {
                let v = editors::read_actual(&ed.form)?;
                let over = self.svc.record_actual(*id, v).map_err(|e| e.to_string())?;
                Ok(if over {
                    "Recorded: exceeded the task's maximum budget!".into()
                } else {
                    "Recorded.".into()
                })
            }
        })();
        match outcome {
            Ok(msg) => {
                self.status = Some((msg, true));
                self.close_editor();
                if let Some(body) = report_text {
                    self.mode = Mode::Text(TextView {
                        title: "Import result".into(),
                        body,
                        scroll: 0,
                    });
                }
            }
            Err(e) => {
                ed.form.set_error(e.clone());
                self.status = Some((e, false));
            }
        }
    }

    /// Ctrl-<letter> helpers inside editors.
    fn editor_shortcut(&mut self, c: char) {
        let Mode::Edit(ed) = &mut self.mode else {
            return;
        };
        match (&ed.kind, c) {
            // Discover Claude logins and fill the form with the next one.
            (EditKind::Account(base), 'd') if base.id.is_none() => {
                let found = plan_burner_core::claude_usage::discover_profiles(&self.svc.env);
                if found.is_empty() {
                    self.status = Some(("No Claude logins found under ~/.claude*.".into(), false));
                    return;
                }
                let d = &found[ed.cycle % found.len()];
                ed.cycle += 1;
                let mut tmp = AccountForm::new();
                tmp.label = ed.form.text("label");
                tmp.plan = ed.form.text("plan");
                tmp.apply_discovered(d);
                ed.form.set_text("config_dir", &tmp.config_dir);
                ed.form.set_text("label", &tmp.label);
                ed.form.set_text("plan", &tmp.plan);
                ed.form.dirty = true;
                self.status = Some((
                    format!(
                        "Login {}/{}: {} (Ctrl-D for the next one)",
                        (ed.cycle - 1) % found.len() + 1,
                        found.len(),
                        d.profile_dir
                    ),
                    true,
                ));
            }
            (EditKind::Task(_), 'l') => {
                let model = ed.form.text("model").trim().to_string();
                let cur: f64 = ed.form.text("est").trim().parse().unwrap_or(10.0);
                match analytics::suggest_estimate(&self.svc.conn, &model, cur) {
                    Ok(e) => {
                        ed.form.set_text("est", &format!("{:.0}", e.value));
                        ed.form.dirty = true;
                        self.status = Some((
                            format!(
                                "Estimate {:.0}% from {} measured run(s); confidence {}",
                                e.value, e.samples, e.confidence
                            ),
                            e.samples > 0,
                        ));
                    }
                    Err(e) => self.status = Some((e.to_string(), false)),
                }
            }
            (EditKind::Task(base), 'o') => {
                let r = editors::read_task(&ed.form, base, &self.snap.accounts)
                    .map_err(plan_burner_core::Error::Invalid)
                    .and_then(|tf| TemplateForm::from_task(&tf).to_template())
                    .and_then(|(name, n)| self.svc.save_template(&name, &n));
                self.report(
                    r,
                    "Template saved (a template with the same name was replaced).",
                );
            }
            _ => {}
        }
    }

    fn view_result(&mut self, name: &str, path: &str) {
        let body =
            std::fs::read_to_string(path).unwrap_or_else(|e| format!("cannot read result: {e}"));
        self.mode = Mode::Text(TextView {
            title: format!("Result: {name}"),
            body,
            scroll: 0,
        });
    }

    /// The help page scrolls like the result viewer; Esc, q or ? close it.
    fn help_key(&mut self, k: KeyEvent) {
        let max = (ui::HELP_TEXT.lines().count() as u16).saturating_sub(self.help_height.get());
        let page = self.help_height.get().max(2) - 1;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => self.mode = Mode::Browse,
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = (self.help_scroll + 1).min(max)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1)
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.help_scroll = (self.help_scroll + page).min(max)
            }
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => self.help_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.help_scroll = max,
            _ => {}
        }
    }

    fn text_key(&mut self, k: KeyEvent) {
        let Mode::Text(t) = &mut self.mode else {
            return;
        };
        let lines = t.body.lines().count() as u16;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Browse,
            KeyCode::Down | KeyCode::Char('j') => t.scroll = (t.scroll + 1).min(lines),
            KeyCode::Up | KeyCode::Char('k') => t.scroll = t.scroll.saturating_sub(1),
            KeyCode::PageDown => t.scroll = (t.scroll + 15).min(lines),
            KeyCode::PageUp => t.scroll = t.scroll.saturating_sub(15),
            KeyCode::Home | KeyCode::Char('g') => t.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => t.scroll = lines,
            KeyCode::Char('y') => {
                copy_to_clipboard(&t.body);
                self.status = Some((
                    "Copied to the terminal clipboard (if supported).".into(),
                    true,
                ));
            }
            KeyCode::Char('e') => {
                let (title, body) = (t.title.clone(), t.body.clone());
                let dir = self.settings.results_dir.clone();
                let r = (|| -> plan_burner_core::Result<std::path::PathBuf> {
                    std::fs::create_dir_all(&dir)?;
                    let p = std::path::Path::new(&dir).join(format!(
                        "export-{}.txt",
                        plan_burner_core::engine::sanitize_filename(&title)
                    ));
                    std::fs::write(&p, body)?;
                    Ok(p)
                })();
                self.status = Some(match r {
                    Ok(p) => (format!("Exported to {}", p.display()), true),
                    Err(e) => (e.to_string(), false),
                });
            }
            _ => {}
        }
    }

    fn pick_key(&mut self, k: KeyEvent) {
        let Mode::Pick {
            task,
            accounts,
            idx,
        } = &mut self.mode
        else {
            return;
        };
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => *idx = (*idx + 1).min(accounts.len() - 1),
            KeyCode::Up | KeyCode::Char('k') => *idx = idx.saturating_sub(1),
            KeyCode::Enter => {
                let (t, a) = (*task, accounts[*idx]);
                self.mode = Mode::Browse;
                let r = self.svc.run_now(t, a);
                self.report(r, "Started (all pre-flight checks still apply).");
            }
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Browse,
            _ => {}
        }
    }

    /// "Burn now" for one account or (`None`) all of them; with approval it jumps to Approvals.
    fn run_burn(&mut self, id: Option<i64>, with_approval: bool) {
        match self.svc.burn_now(id, with_approval) {
            Ok(rep) => {
                let did = rep.proposed + rep.started > 0;
                self.status = Some((rep.message, did));
                if with_approval && rep.proposed > 0 {
                    self.set_view(View::Approvals);
                }
            }
            Err(e) => self.status = Some((e.to_string(), false)),
        }
        self.refresh();
    }

    fn confirm_key(&mut self, k: KeyEvent) {
        let Mode::Confirm(c) = std::mem::replace(&mut self.mode, Mode::Browse) else {
            return;
        };
        if !matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.status = Some((MSG_CANCELLED.into(), true));
            return;
        }
        match c {
            Confirm::BurnNow(id) => self.run_burn(id, false),
            Confirm::StopRunning(_) => {
                let r = self.svc.cancel_active();
                self.report(r, "Task cancelled.");
            }
            Confirm::RejectBatch { id, .. } => {
                let r = self.svc.reject_batch(id);
                self.report(r, "Batch rejected; tasks returned to the queue.");
            }
            Confirm::Stop => {
                let r = self.svc.emergency_stop();
                self.report(
                    r,
                    "Emergency stop requested; waiting for process termination.",
                );
            }
            Confirm::DeleteTask(id) => {
                let r = self.svc.task_action(id, TaskAction::Delete);
                self.report(r, "Task deleted.");
            }
            Confirm::DeleteAccount(id) => {
                let r = self.svc.delete_account(id);
                self.report(r, "Account deleted.");
            }
            Confirm::DeleteTemplate(id) => {
                let r = self.svc.delete_template(id);
                self.report(r, "Template deleted.");
            }
        }
    }
}

/// Copies text through the terminal (OSC 52); terminals that don't support it ignore it.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let b64 = base64(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        s.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    s
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new()?;
    let mut term = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    let res = (|| -> std::io::Result<()> {
        while !app.quit {
            app.pump();
            term.draw(|f| ui::draw(f, &app))?;
            if event::poll(Duration::from_millis(250))? {
                match event::read()? {
                    Event::Key(k) => app.key(k),
                    Event::Paste(s) => app.paste(&s),
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    res?;
    Ok(())
}

/// Accounts a task may run on now: its pinned account (if any) of its provider (if any). The caller
/// starts the task only when exactly one matches; an ambiguous choice is never made automatically.
fn run_now_candidates<'a>(
    accounts: impl Iterator<Item = (i64, &'a str)>,
    pinned: Option<i64>,
    provider: Option<&str>,
) -> Vec<i64> {
    accounts
        .filter(|(id, p)| pinned.is_none_or(|x| x == *id) && provider.is_none_or(|x| x == *p))
        .map(|(id, _)| id)
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Logs on stderr would corrupt the screen: log to a file, and only when asked.
    if let Some(path) = std::env::var_os("PLAN_BURNER_TUI_LOG") {
        if let Ok(f) = std::fs::File::create(path) {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .with_writer(std::sync::Mutex::new(f))
                .with_ansi(false)
                .init();
        }
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        println!("{}", plan_burner_core::brand::name_version());
        return Ok(());
    }
    // Presentation helpers are shared with the GUI through `core::present`.
    let _ = present::fmt_minutes;
    run()
}

#[cfg(test)]
mod tests;
