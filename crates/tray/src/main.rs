//! Tray: a minimal control surface for the scheduler daemon. It never drives the scheduler itself; no business
//! rules live here. Everything goes through `plan_burner_core::service::Service`, which forwards actions to the
//! daemon over the local control socket. It can open the GUI and start the daemon, nothing else.
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{MenuItem, Status, ToolTip, Tray};
use plan_burner_core::brand;
use plan_burner_core::control::RunState;
use plan_burner_core::icon::{self, IconSpec, IconState};
use plan_burner_core::notify::{Kind, Notice};
use plan_burner_core::service::{SchedulerStatus, Service};
use plan_burner_core::{launch, session, settings};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

enum Cmd {
    SetEnabled(bool),
    Pause,
    Resume,
    Stop,
    ClearStop,
    OpenGui,
    StartScheduler,
    Quit,
}

/// What the menu shows; refreshed by the main loop.
#[derive(Clone)]
struct View {
    enabled: bool,
    state: RunState,
    pending: usize,
    /// Whether the scheduler daemon is up.
    scheduler: SchedulerStatus,
    /// The last tray action that failed (e.g. "EMERGENCY STOP: …"); cleared by the next success.
    last_error: Option<String>,
}

impl Cmd {
    fn label(&self) -> &'static str {
        match self {
            Cmd::SetEnabled(true) => "Enable automation",
            Cmd::SetEnabled(false) => "Disable automation",
            Cmd::Pause => "Pause",
            Cmd::Resume => "Resume",
            Cmd::Stop => "EMERGENCY STOP",
            Cmd::ClearStop => "Clear STOP",
            Cmd::OpenGui => "Open GUI",
            Cmd::StartScheduler => "Start scheduler",
            Cmd::Quit => "Quit",
        }
    }
}

/// How often the (DB-reading) view and tick interval are recomputed when nothing was clicked.
const VIEW_REFRESH: Duration = Duration::from_secs(2);

/// True when cached state must be recomputed: never computed, forced (a command ran), or older than `every`.
fn refresh_due(last: Option<Instant>, force: bool, every: Duration) -> bool {
    force || last.is_none_or(|t| t.elapsed() >= every)
}

/// Tooltip text for a view (one line, no secrets).
/// Informational lines for the top of the menu: what needs attention and why actions may fail.
fn status_notes(v: &View) -> Vec<String> {
    let mut n = Vec::new();
    if v.pending > 0 {
        n.push(format!(
            "{} batch(es) awaiting approval: open the GUI or TUI",
            v.pending
        ));
    }
    match v.scheduler {
        SchedulerStatus::Offline => {
            n.push("The scheduler is not running: start it, or the actions below will fail".into())
        }
        SchedulerStatus::Unreachable => {
            n.push("The scheduler does not respond: the actions below will fail".into())
        }
        SchedulerStatus::Running => {}
    }
    n
}

fn tooltip_text(v: &View) -> String {
    let mut d = format!(
        "{} · automation {}",
        state_label(v.state),
        if v.enabled { "on" } else { "off" }
    );
    if v.pending > 0 {
        d.push_str(&format!(" · {} approval(s) waiting", v.pending));
    }
    match v.scheduler {
        SchedulerStatus::Running => {}
        other => d.push_str(&format!(" · {}", other.label())),
    }
    if let Some(e) = &v.last_error {
        d.push_str(&format!(" · LAST ACTION FAILED: {e}"));
    }
    d
}

impl View {
    fn icon(&self) -> IconSpec {
        IconSpec {
            state: IconState::derive(
                self.enabled,
                self.state,
                self.scheduler != SchedulerStatus::Running,
            ),
            attention: self.pending > 0,
        }
    }
}

const ICON_SIZES: [u32; 5] = [16, 22, 32, 48, 64];

fn pixmaps(spec: IconSpec) -> Vec<ksni::Icon> {
    ICON_SIZES
        .iter()
        .map(|&s| ksni::Icon {
            width: s as i32,
            height: s as i32,
            data: icon::to_argb_be(&icon::render_rgba(s, spec)),
        })
        .collect()
}

fn state_label(s: RunState) -> &'static str {
    match s {
        RunState::Running => "Running",
        RunState::Paused => "Paused",
        RunState::Waiting => "Waiting",
        RunState::Stopped => "STOPPED",
    }
}

struct BurnerTray {
    tx: Sender<Cmd>,
    view: View,
    /// Rendered for `view.icon()`; refreshed only when the icon spec changes.
    icons: Vec<ksni::Icon>,
}

impl BurnerTray {
    fn new(tx: Sender<Cmd>, view: View) -> Self {
        let icons = pixmaps(view.icon());
        BurnerTray { tx, view, icons }
    }

    fn set_view(&mut self, v: View) {
        if v.icon() != self.view.icon() {
            self.icons = pixmaps(v.icon());
        }
        self.view = v;
    }
}

impl BurnerTray {
    fn item(&self, label: &str, cmd: fn() -> Cmd) -> MenuItem<Self> {
        StandardItem {
            label: label.into(),
            activate: Box::new(move |t: &mut Self| {
                let _ = t.tx.send(cmd());
            }),
            ..Default::default()
        }
        .into()
    }
}

impl Tray for BurnerTray {
    fn id(&self) -> String {
        brand::SLUG.into()
    }
    fn title(&self) -> String {
        brand::NAME.into()
    }
    // No themed icon name: the hosts then use our state-aware pixmaps.
    fn icon_name(&self) -> String {
        String::new()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }
    fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }
    fn status(&self) -> Status {
        if self.view.state == RunState::Stopped
            || self.view.pending > 0
            || self.view.last_error.is_some()
        {
            Status::NeedsAttention
        } else {
            Status::Active
        }
    }
    fn tool_tip(&self) -> ToolTip {
        let icons = self.icons.clone();
        let d = tooltip_text(&self.view);
        ToolTip {
            icon_pixmap: icons,
            title: brand::name_version(),
            description: d,
            ..Default::default()
        }
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let v = &self.view;
        let mut m: Vec<MenuItem<Self>> = vec![StandardItem {
            label: format!("{} — {}", brand::NAME, state_label(v.state)),
            enabled: false,
            ..Default::default()
        }
        .into()];
        for note in status_notes(v) {
            m.push(
                StandardItem {
                    label: note,
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        if let Some(e) = &v.last_error {
            m.push(
                StandardItem {
                    label: format!("⚠ {e}"),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        m.extend::<Vec<MenuItem<Self>>>(vec![CheckmarkItem {
            label: "Automation enabled".into(),
            checked: v.enabled,
            activate: Box::new(|t: &mut Self| {
                let _ = t.tx.send(Cmd::SetEnabled(!t.view.enabled));
            }),
            ..Default::default()
        }
        .into()]);
        m.push(if v.state == RunState::Paused {
            self.item("Resume", || Cmd::Resume)
        } else {
            self.item("Pause (until resumed)", || Cmd::Pause)
        });
        m.push(MenuItem::Separator);
        // Kept away from Pause and behind a second click, so a slip cannot kill a running task.
        m.push(if v.state == RunState::Stopped {
            self.item("Clear STOP", || Cmd::ClearStop)
        } else {
            SubMenu {
                label: "EMERGENCY STOP…".into(),
                submenu: vec![self.item("Yes, stop everything now", || Cmd::Stop)],
                ..Default::default()
            }
            .into()
        });
        m.push(MenuItem::Separator);
        m.push(self.item("Open GUI", || Cmd::OpenGui));
        if v.scheduler == SchedulerStatus::Offline {
            m.push(self.item("Start scheduler", || Cmd::StartScheduler));
        }
        m.push(MenuItem::Separator);
        m.push(self.item("Quit", || Cmd::Quit));
        m
    }
}

fn view_of(svc: &Service) -> View {
    View {
        enabled: settings::load(&svc.conn)
            .map(|s| s.enabled)
            .unwrap_or(false),
        state: svc.run_state(chrono::Utc::now()),
        pending: svc.pending_batches().len(),
        scheduler: svc.scheduler_status(),
        last_error: None,
    }
}

/// `None` = the tray should quit; otherwise the action's result (error text on failure).
fn apply(svc: &mut Service, cmd: &Cmd) -> Option<Result<(), String>> {
    Some(match cmd {
        Cmd::SetEnabled(on) => svc.set_enabled(*on).map_err(|e| e.to_string()),
        Cmd::Pause => svc.pause_now().map_err(|e| e.to_string()),
        Cmd::Resume => svc.resume().map_err(|e| e.to_string()),
        Cmd::Stop => svc.emergency_stop().map_err(|e| e.to_string()),
        Cmd::ClearStop => svc.clear_stop().map_err(|e| e.to_string()),
        Cmd::OpenGui => launch::spawn_gui(),
        Cmd::StartScheduler => launch::spawn_daemon(),
        Cmd::Quit => return None,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        println!("{}", brand::name_version());
        return Ok(());
    }
    if let Some(i) = std::env::args().position(|a| a == "--export-icon") {
        return export_icon(&std::env::args().skip(i + 1).collect::<Vec<_>>());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let (mut svc, _) = session::open_client_service()?;
    if svc.scheduler_status() != SchedulerStatus::Running {
        tracing::warn!("{}", svc.scheduler_status().label());
    }
    let (tx, rx) = channel();
    let handle = BurnerTray::new(tx, view_of(&svc)).spawn().map_err(|e| {
        format!("cannot show the tray icon (is a StatusNotifier host running?): {e}")
    })?;

    // DB-reading state is recomputed on commands and every VIEW_REFRESH, not on every 500 ms wake-up.
    let mut last_refresh: Option<Instant> = None;
    let mut last_error: Option<String> = None;
    let mut failures = 0usize;
    loop {
        let mut force = false;
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(cmd) => {
                force = true;
                match apply(&mut svc, &cmd) {
                    None => break,
                    Some(Ok(())) => {
                        if !matches!(cmd, Cmd::OpenGui | Cmd::StartScheduler) {
                            last_error = None;
                        }
                    }
                    Some(Err(e)) => {
                        // Never fail silently, least of all EMERGENCY STOP: log, notify and show in the tray.
                        failures += 1;
                        tracing::warn!("tray action {:?} failed: {e}", cmd.label());
                        svc.notify(&Notice {
                            kind: Kind::ActionFailed,
                            account: cmd.label().to_string(),
                            count: failures, // distinct dedup key per failure
                            names: vec![],
                            detail: e.clone(),
                            hint: "check the tray menu or open the app".into(),
                        });
                        last_error = Some(format!("{}: {e}", cmd.label()));
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        for e in svc.poll() {
            if let Some((msg, ok)) = session::event_status(&e) {
                if ok {
                    tracing::info!("{msg}");
                } else {
                    tracing::warn!("{msg}");
                }
            }
        }
        if refresh_due(last_refresh, force, VIEW_REFRESH) {
            last_refresh = Some(Instant::now());
            let mut v = view_of(&svc);
            v.last_error = last_error.clone();
            if handle.update(move |t| t.set_view(v)).is_none() {
                break; // tray service ended
            }
        }
    }
    drop(handle.shutdown());
    Ok(())
}

/// `--export-icon <out.png> [--size N] [--state running|waiting|disabled|paused|stopped|nodriver] [--attention]`
/// Writes the icon as a PNG and exits (packaging, visual checks). Needs no D-Bus or database.
fn export_icon(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let out = args.first().ok_or("--export-icon needs an output path")?;
    let (mut size, mut state, mut attention) = (256u32, IconState::Running, false);
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--size" => size = it.next().ok_or("--size needs a value")?.parse()?,
            "--state" => {
                let v = it.next().ok_or("--state needs a value")?;
                state = IconState::parse(v).ok_or_else(|| format!("unknown state {v:?}"))?;
            }
            "--attention" => attention = true,
            other => return Err(format!("unknown option {other:?}").into()),
        }
    }
    if !(8..=1024).contains(&size) {
        return Err("--size must be between 8 and 1024".into());
    }
    let rgba = icon::render_rgba(size, IconSpec { state, attention });
    std::fs::write(out, icon::encode_png(size, size, &rgba))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View {
            enabled: true,
            state: RunState::Running,
            pending: 0,
            scheduler: SchedulerStatus::Running,
            last_error: None,
        }
    }

    #[test]
    fn menu_notes_explain_approvals_and_missing_driver() {
        let mut v = view();
        assert!(status_notes(&v).is_empty());
        v.pending = 2;
        assert!(status_notes(&v)[0].contains("2 batch(es) awaiting approval"));
        v.scheduler = SchedulerStatus::Offline;
        assert!(status_notes(&v).iter().any(|n| n.contains("not running")));
        v.scheduler = SchedulerStatus::Unreachable;
        assert!(status_notes(&v)
            .iter()
            .any(|n| n.contains("does not respond")));
    }

    #[test]
    fn refresh_only_when_forced_or_old() {
        let every = Duration::from_secs(2);
        assert!(refresh_due(None, false, every), "first time");
        assert!(
            !refresh_due(Some(Instant::now()), false, every),
            "fresh cache"
        );
        assert!(
            refresh_due(Some(Instant::now()), true, every),
            "forced after a command"
        );
        let old = Instant::now() - Duration::from_secs(3);
        assert!(refresh_due(Some(old), false, every));
    }

    #[test]
    fn failed_action_is_visible_in_tooltip_and_raises_attention() {
        let mut v = view();
        assert!(!tooltip_text(&v).contains("FAILED"));
        v.last_error = Some("EMERGENCY STOP: scheduler not running".into());
        let t = tooltip_text(&v);
        assert!(
            t.contains("LAST ACTION FAILED") && t.contains("EMERGENCY STOP"),
            "{t}"
        );
        let (tx, _rx) = channel();
        let tray = BurnerTray::new(tx, v);
        assert_eq!(tray.status(), Status::NeedsAttention);
    }

    #[test]
    fn action_labels_name_the_action() {
        assert_eq!(Cmd::Stop.label(), "EMERGENCY STOP");
        assert_eq!(Cmd::SetEnabled(false).label(), "Disable automation");
    }
}
