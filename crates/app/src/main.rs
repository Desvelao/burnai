mod accounts_view;
mod dashboard;
mod other;
mod queue;
mod templates_view;
mod theme;
mod ui;

use eframe::egui;
use plan_burner_core::icon::{self, IconSpec, IconState};
use plan_burner_core::service::SchedulerStatus;
use plan_burner_core::{
    analytics, models::Account, provider, service::Service, session, settings, settings::Settings,
    tasks::Task,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a success message stays in the status bar.
const STATUS_TTL: Duration = Duration::from_secs(8);

/// Actions that ask "are you sure?" inline before they run.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    CancelTask(i64),
    StopRunning(i64),
    RejectBatch(u64),
    DiscardSettings,
    /// "Burn now without approvals" for one account (`Some`) or all of them (`None`).
    BurnNow(Option<i64>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Queue,
    Accounts,
    Approvals,
    History,
    Templates,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Account(i64),
    AnyAccount,
}

pub use plan_burner_core::forms::{opt_str, AccountForm, TaskForm, TemplateForm, UsageForm};

pub struct App {
    pub svc: Service,
    pub view: View,
    pub settings: Settings,
    pub accounts: Vec<Account>,
    pub tasks: Vec<Task>,
    pub dets: HashMap<i64, provider::Detection>,
    pub cache_at: Instant,
    /// Icon currently shown by the window (follows the scheduler state).
    icon_spec: IconSpec,
    pub status: Option<(String, bool)>,
    /// When `status` last changed (successes fade after `STATUS_TTL`; errors stay until dismissed).
    pub status_seen: Option<((String, bool), Instant)>,
    pub usage_form: Option<UsageForm>,
    pub account_form: Option<AccountForm>,
    pub task_form: Option<TaskForm>,
    /// The "Import tasks" panel on the queue page (`None` = closed).
    pub import: Option<crate::queue::ImportPanel>,
    pub filter: Filter,
    pub selected: Option<i64>,
    pub approval_sel: HashMap<u64, HashSet<i64>>,
    pub viewing_result: Option<(String, String)>,
    /// A two-step confirmation in progress for a high-impact action (see `Pending`).
    pub pending: Option<Pending>,
    pub confirm_delete: Option<i64>,
    pub confirm_delete_task: Option<i64>,
    pub confirm_delete_template: Option<i64>,
    pub template_form: Option<TemplateForm>,
    pub actual_inputs: HashMap<i64, f64>,
    /// Unsaved Settings-page edits as (settings when editing began, edited copy). The live
    /// `settings` (top bar, dashboard) never shows them until they are saved.
    pub settings_draft: Option<(settings::Settings, settings::Settings)>,
    /// Why the last Settings save was rejected, shown beside the Save button.
    pub settings_error: Option<String>,
    /// Why queued tasks are not running (refreshed with the snapshot).
    pub qstatus: plan_burner_core::service::QueueStatus,
}

impl App {
    /// True while the Settings page holds edits that differ from what is saved.
    pub fn settings_dirty(&self) -> bool {
        self.settings_draft.as_ref().is_some_and(|(b, d)| b != d)
    }

    fn new() -> Result<App, Box<dyn std::error::Error>> {
        let (svc, s) = session::open_client_service()?;
        let mut app = App {
            svc,
            view: View::Dashboard,
            settings: s,
            accounts: vec![],
            tasks: vec![],
            dets: HashMap::new(),
            cache_at: Instant::now() - Duration::from_secs(60),
            icon_spec: initial_icon(),
            status: None,
            status_seen: None,
            usage_form: None,
            account_form: None,
            task_form: None,
            import: None,
            filter: Filter::All,
            selected: None,
            approval_sel: HashMap::new(),
            viewing_result: None,
            pending: None,
            confirm_delete: None,
            confirm_delete_task: None,
            confirm_delete_template: None,
            template_form: None,
            actual_inputs: HashMap::new(),
            settings_draft: None,
            settings_error: None,
            qstatus: Default::default(),
        };
        app.refresh();
        Ok(app)
    }

    pub fn refresh(&mut self) {
        let prev = session::Snapshot {
            accounts: std::mem::take(&mut self.accounts),
            tasks: std::mem::take(&mut self.tasks),
            dets: std::mem::take(&mut self.dets),
        };
        let snap = session::snapshot(&self.svc, prev);
        self.accounts = snap.accounts;
        self.tasks = snap.tasks;
        self.dets = snap.dets;
        if let Ok(s) = settings::load(&self.svc.conn) {
            self.settings = s;
        }
        if let Ok(q) = self.svc.queue_status() {
            self.qstatus = q;
        }
        self.cache_at = Instant::now();
    }

    /// Successes fade after `STATUS_TTL`; errors stay until dismissed with the ✕ button.
    fn age_status(&mut self) {
        let (status, seen) =
            aged_status(self.status.take(), self.status_seen.take(), Instant::now());
        self.status = status;
        self.status_seen = seen;
    }

    pub fn report<T>(&mut self, r: plan_burner_core::Result<T>, ok_msg: &str) -> Option<T> {
        match r {
            Ok(v) => {
                if !ok_msg.is_empty() {
                    self.status = Some((ok_msg.to_string(), true));
                }
                self.refresh();
                Some(v)
            }
            Err(e) => {
                self.status = Some((e.to_string(), false));
                None
            }
        }
    }

    /// Keep the window/taskbar icon in step with the scheduler state (same icon as the tray).
    fn sync_icon(&mut self, ctx: &egui::Context) {
        let spec = IconSpec {
            state: IconState::derive(
                self.settings.enabled,
                self.svc.run_state(chrono::Utc::now()),
                self.svc.scheduler_status() != SchedulerStatus::Running,
            ),
            attention: !self.svc.pending_batches().is_empty(),
        };
        if spec != self.icon_spec {
            self.icon_spec = spec;
            ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::new(icon_data(spec)))));
        }
    }

    fn pump(&mut self) {
        for e in self.svc.poll() {
            if let Some(st) = session::event_status(&e) {
                self.status = Some(st);
            }
            self.cache_at = Instant::now() - Duration::from_secs(60);
        }
        if self.cache_at.elapsed() > Duration::from_secs(1) {
            self.refresh();
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top")
            .frame(egui::Frame::new().fill(theme::TITLE_BAR).inner_margin(egui::Margin::symmetric(14, 8)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(plan_burner_core::brand::NAME).strong().size(15.0));
                    ui.add_space(8.0);
                    let mut en = self.settings.enabled;
                    if ui.checkbox(&mut en, "Automation").changed() {
                        // Persist only this flag; unsaved settings-tab edits stay unsaved.
                        let r = self.svc.set_enabled(en);
                        if r.is_ok() {
                            self.settings.enabled = en;
                            if let Some((base, draft)) = &mut self.settings_draft {
                                // keep a pending Settings draft from undoing this toggle on Save
                                if draft.enabled == base.enabled {
                                    draft.enabled = en;
                                }
                                base.enabled = en;
                            }
                        }
                        self.report(r, if en { "Automation enabled." } else { "Automation disabled." });
                    }
                    self.burn_controls(ui, None);
                    let st = self.svc.run_state(chrono::Utc::now());
                    ui::badge(ui, &format!("Policy: {}", self.settings.execution_mode), ui::GREEN);
                    ui::badge(ui, ui::run_state_label(st), if st == plan_burner_core::control::RunState::Stopped { ui::RED } else { ui::GREY });
                    use plan_burner_core::control::RunState;
                    match st {
                        RunState::Paused => {
                            if ui::secondary_button(ui, "Resume").clicked() {
                                let r = self.svc.resume();
                                self.report(r, "Resumed.");
                            }
                        }
                        RunState::Waiting | RunState::Running => {
                            if ui::secondary_button(ui, "Pause").clicked() {
                                let r = self.svc.pause_now();
                                self.report(r, "Paused.");
                            }
                            if ui::secondary_button(ui, "Pause until reset").clicked() {
                                let r = self.svc.pause_until_next_reset();
                                self.report(r, "Paused until the next usage reset.");
                            }
                        }
                        // Stopped: "Clear STOP" is the only applicable action.
                        RunState::Stopped => {}
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui::danger_button(ui, "EMERGENCY STOP").clicked() {
                            let r = self.svc.emergency_stop();
                            self.report(r, "STOP engaged. Waiting for the running process (if any) to terminate…");
                        }
                        if self.svc.run_state(chrono::Utc::now()) == plan_burner_core::control::RunState::Stopped
                            && ui::secondary_button(ui, "Clear STOP").clicked()
                        {
                            let r = self.svc.clear_stop();
                            self.report(r, "STOP cleared.");
                        }
                    });
                });
                let sched = self.svc.scheduler_status();
                if sched != SchedulerStatus::Running {
                    ui.horizontal(|ui| {
                        ui.colored_label(ui::RED, format!("{}: nothing will run until it is started.", sched.label()));
                        if sched == SchedulerStatus::Offline && ui::secondary_button(ui, "Start scheduler").clicked() {
                            self.status = Some(match plan_burner_core::launch::spawn_daemon() {
                                Ok(()) => ("Starting the scheduler…".to_string(), true),
                                Err(e) => (e, false),
                            });
                        }
                    });
                }
                if let Some(r) = self.svc.stop_report() {
                    if r.process_confirmed.is_none() {
                        ui.colored_label(ui::AMBER, "Stopping: waiting for confirmation that the process has terminated…");
                    }
                }
            });
        let mut dismiss = false;
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(theme::TITLE_BAR)
                    .inner_margin(egui::Margin::symmetric(14, 5)),
            )
            .show(ctx, |ui| {
                if let Some((m, ok)) = &self.status {
                    ui.horizontal_wrapped(|ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(m).color(if *ok {
                                ui::GREEN
                            } else {
                                ui::RED
                            }))
                            .wrap(),
                        );
                        if !*ok && ui.small_button("✕ Dismiss").clicked() {
                            dismiss = true;
                        }
                    });
                } else {
                    ui.label(egui::RichText::new("Ready").color(theme::MUTED));
                }
            });
        if dismiss {
            self.status = None;
        }
    }

    /// Ctrl+1..7 switch pages, in sidebar order (like the TUI's 1-7).
    fn nav_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::Key;
        const KEYS: [Key; 7] = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
        ];
        for (k, v) in KEYS.iter().zip(NAV_ORDER) {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, *k)) {
                self.view = v;
            }
        }
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav")
            .exact_width(190.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(10, 14)),
            )
            .show(ctx, |ui| {
                let pending = self.svc.pending_batches().len();
                for (v, l, count) in [
                    (View::Dashboard, "Dashboard", 0),
                    (
                        View::Queue,
                        "Task queue",
                        usize::from(self.task_form.is_some()),
                    ),
                    (
                        View::Accounts,
                        "Accounts",
                        usize::from(self.account_form.is_some()),
                    ),
                    (View::Approvals, "Approvals", pending),
                    (View::History, "History & results", 0),
                    (
                        View::Templates,
                        "Templates",
                        usize::from(self.template_form.is_some()),
                    ),
                    (
                        View::Settings,
                        "Settings",
                        usize::from(self.settings_dirty()),
                    ),
                ] {
                    let active = self.view == v;
                    let r = egui::Frame::new()
                        .fill(if active {
                            theme::PRIMARY
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .corner_radius(egui::CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(14, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(l).color(if active {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_rgb(0xc4, 0xc4, 0xcc)
                                }));
                                if count > 0 {
                                    let t = if matches!(
                                        v,
                                        View::Queue | View::Accounts | View::Templates
                                    ) {
                                        "editing".to_string()
                                    } else if v == View::Settings {
                                        "unsaved".to_string()
                                    } else {
                                        count.to_string()
                                    };
                                    ui::badge(ui, &t, ui::AMBER);
                                }
                            });
                        });
                    // Focusable (Tab / Enter / Space), named for screen readers, with a visible focus ring.
                    let resp = r
                        .response
                        .interact(egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    resp.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, l)
                    });
                    if resp.has_focus() {
                        ui.painter().rect_stroke(
                            resp.rect,
                            egui::CornerRadius::same(4),
                            egui::Stroke::new(2.0_f32, ui::BLUE),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if resp.clicked() {
                        self.view = v;
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.label(
                        egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                            .small()
                            .color(theme::MUTED),
                    );
                });
            });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        self.age_status();
        self.nav_shortcuts(ctx);
        self.sync_icon(ctx);
        self.top_bar(ctx);
        self.sidebar(ctx);
        self.task_action_bar(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::WINDOW)
                    .inner_margin(egui::Margin::symmetric(24, 18)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.view {
                        View::Dashboard => self.dashboard(ui),
                        View::Queue if self.task_form.is_some() => self.task_window(ui),
                        View::Queue => self.queue(ui),
                        View::Accounts if self.account_form.is_some() => self.account_window(ui),
                        View::Accounts => self.accounts_view(ui),
                        View::Approvals => self.approvals(ui),
                        View::History => self.history(ui),
                        View::Templates if self.template_form.is_some() => self.template_editor(ui),
                        View::Templates => self.templates_view(ui),
                        View::Settings => self.settings_view(ui),
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

const WINDOW_ICON_SIZE: u32 = 64;

fn icon_data(spec: IconSpec) -> egui::IconData {
    egui::IconData {
        rgba: icon::render_rgba(WINDOW_ICON_SIZE, spec),
        width: WINDOW_ICON_SIZE,
        height: WINDOW_ICON_SIZE,
    }
}

fn initial_icon() -> IconSpec {
    IconSpec {
        state: IconState::Waiting,
        attention: false,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        println!("{}", plan_burner_core::brand::name_version());
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let app = App::new()?;
    // `analytics` is used by views; keep import used even if a view is compiled out.
    let _ = analytics::export_history_csv;
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 760.0])
            .with_title(plan_burner_core::brand::NAME)
            .with_app_id(plan_burner_core::brand::SLUG)
            .with_icon(icon_data(initial_icon())),
        ..Default::default()
    };
    eframe::run_native(
        plan_burner_core::brand::NAME,
        opts,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| format!("{e}").into())
}

/// Sidebar order; Ctrl+1..7 jump to these pages.
const NAV_ORDER: [View; 7] = [
    View::Dashboard,
    View::Queue,
    View::Accounts,
    View::Approvals,
    View::History,
    View::Templates,
    View::Settings,
];

type Status = (String, bool);

/// One step of status aging: remembers when a message first appeared and drops a success
/// message once it is older than `STATUS_TTL`. Errors are never dropped here.
fn aged_status(
    status: Option<Status>,
    seen: Option<(Status, Instant)>,
    now: Instant,
) -> (Option<Status>, Option<(Status, Instant)>) {
    match (status, seen) {
        (Some(cur), Some((was, at))) if cur == was => {
            if cur.1 && now.duration_since(at) > STATUS_TTL {
                (None, None)
            } else {
                (Some(cur), Some((was, at)))
            }
        }
        (Some(cur), _) => (Some(cur.clone()), Some((cur, now))),
        (None, _) => (None, None),
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn successes_fade_but_errors_stay_until_dismissed() {
        let t0 = Instant::now();
        let ok: Status = ("Saved.".into(), true);
        let err: Status = ("it broke".into(), false);
        // a new message starts its clock
        let (st, seen) = aged_status(Some(ok.clone()), None, t0);
        assert_eq!(st, Some(ok.clone()));
        // still fresh
        let (st, seen2) = aged_status(st, seen.clone(), t0 + Duration::from_secs(2));
        assert!(st.is_some() && seen2 == seen);
        // old success fades
        let (st, seen) = aged_status(st, seen2, t0 + Duration::from_secs(60));
        assert!(st.is_none() && seen.is_none());
        // errors never time out
        let (st, seen) = aged_status(Some(err.clone()), None, t0);
        let (st, _) = aged_status(st, seen, t0 + Duration::from_secs(3600));
        assert_eq!(st, Some(err));
    }
}
