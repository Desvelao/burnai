//! Scheduler daemon: the only process that owns the driver lock, runs the scheduler and serves the control
//! socket. Headless, no UI types and no business rules (those live in `plan_burner_core`). The GUI, TUI, tray
//! and CLI are clients that read the shared DB and forward every action to this process.
use plan_burner_core::{brand, session, settings, tasks};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_signal_handlers() {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

/// Fallback pacing when the DB cannot be read.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(300);
/// How often the control socket and runner events are serviced.
const LOOP_STEP: Duration = Duration::from_millis(200);
/// How long a signalled daemon waits for the active run's process to exit before giving up.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(15);
/// How often the tick interval is recomputed from the queue and settings.
const INTERVAL_REFRESH: Duration = Duration::from_secs(2);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        println!("{}", brand::name_version());
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    install_signal_handlers();
    let (mut svc, _) = session::open_driver_service()?;
    tracing::info!("{} scheduler daemon started", brand::NAME);

    let mut last_tick: Option<Instant> = None;
    let mut last_interval: Option<Instant> = None;
    let mut interval = DEFAULT_INTERVAL;
    while !SHUTDOWN.load(Ordering::SeqCst) {
        for e in svc.poll() {
            if let Some((msg, ok)) = session::event_status(&e) {
                if ok {
                    tracing::info!("{msg}");
                } else {
                    tracing::warn!("{msg}");
                }
            }
        }
        if last_interval.is_none_or(|t| t.elapsed() >= INTERVAL_REFRESH) {
            last_interval = Some(Instant::now());
            interval = match (tasks::list(&svc.conn), settings::load(&svc.conn)) {
                (Ok(t), Ok(s)) => session::tick_interval(&t, &s),
                _ => DEFAULT_INTERVAL,
            };
        }
        if last_tick.is_none_or(|t| t.elapsed() >= interval) {
            last_tick = Some(Instant::now());
            for e in session::poll_and_tick(&mut svc) {
                tracing::warn!("{e}");
            }
        }
        std::thread::sleep(LOOP_STEP);
    }
    tracing::info!("shutting down");
    if !svc.shutdown(SHUTDOWN_WAIT) {
        tracing::warn!("the active task did not stop within {SHUTDOWN_WAIT:?}");
    }
    Ok(())
}
