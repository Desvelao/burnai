//! Notifications: in-app (DB), desktop, optional Telegram. External messages never contain prompts
//! and, unless the user opts in, never contain task names. Telegram's bot token comes from the
//! `PLAN_BURNER_TELEGRAM_TOKEN` environment variable and is never stored or logged.
use crate::settings::{in_window, Settings};
use crate::{db, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    CapacityAvailable,
    ApprovalRequested,
    ExecutionStarted,
    ExecutionCompleted,
    CapacityInsufficient,
    MonitoringError,
    /// A front-end action (e.g. the tray's EMERGENCY STOP) failed; `account` carries the action name.
    ActionFailed,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::CapacityAvailable => "capacity_available",
            Kind::ApprovalRequested => "approval_requested",
            Kind::ExecutionStarted => "execution_started",
            Kind::ExecutionCompleted => "execution_completed",
            Kind::CapacityInsufficient => "capacity_insufficient",
            Kind::MonitoringError => "monitoring_error",
            Kind::ActionFailed => "action_failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub kind: Kind,
    /// Account label, e.g. "claude · Work".
    pub account: String,
    pub count: usize,
    /// Task names (may be sensitive). Only used externally when `notify_include_names` is on.
    pub names: Vec<String>,
    pub detail: String,
    /// Short, fixed next-step text that is safe to send off-device (no task data, no error text).
    pub hint: String,
}

impl Notice {
    pub fn dedup_key(&self) -> String {
        format!("{}:{}:{}", self.kind.as_str(), self.account, self.count)
    }

    pub fn internal_text(&self) -> String {
        let base = self.base();
        let mut s = base;
        if !self.names.is_empty() {
            s.push_str(&format!(" ({})", self.names.join(", ")));
        }
        if !self.detail.is_empty() {
            s.push_str(&format!(": {}", self.detail));
        }
        s
    }

    /// Text safe to send off-device. Never includes prompts; names only by explicit opt-in.
    pub fn external_text(&self, include_names: bool) -> String {
        let mut s = self.base();
        if include_names && !self.names.is_empty() {
            s.push_str(&format!(" ({})", self.names.join(", ")));
        }
        s
    }

    fn base(&self) -> String {
        let mut s = self.headline();
        if !self.hint.is_empty() {
            s.push_str(&format!(": {}", self.hint));
        }
        s
    }

    fn headline(&self) -> String {
        match self.kind {
            Kind::CapacityAvailable => format!("Capacity available on {}", self.account),
            Kind::ApprovalRequested => format!(
                "Approval requested for {} task(s) on {}",
                self.count, self.account
            ),
            Kind::ExecutionStarted => format!(
                "Execution started on {} ({} task(s))",
                self.account, self.count
            ),
            Kind::ExecutionCompleted => format!(
                "Execution finished on {}: {} task(s) completed",
                self.account, self.count
            ),
            Kind::CapacityInsufficient => format!("Capacity insufficient on {}", self.account),
            Kind::MonitoringError => format!("Monitoring problem on {}", self.account),
            Kind::ActionFailed => format!("Action failed: {}", self.account),
        }
    }
}

pub trait ExternalSink {
    fn desktop(&self, title: &str, body: &str);
    fn telegram(&self, token: &str, chat_id: &str, text: &str) -> std::result::Result<(), String>;
}

pub struct SystemSinks;

impl ExternalSink for SystemSinks {
    fn desktop(&self, title: &str, body: &str) {
        if let Err(e) = notify_rust::Notification::new()
            .summary(title)
            .body(body)
            .show()
        {
            tracing::warn!("desktop notification failed: {e}");
        }
    }
    fn telegram(&self, token: &str, chat_id: &str, text: &str) -> std::result::Result<(), String> {
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        ureq::post(&url)
            .timeout(std::time::Duration::from_secs(10))
            .send_json(serde_json::json!({ "chat_id": chat_id, "text": text }))
            .map(|_| ())
            // Never include `url` (contains the token) in errors.
            .map_err(|e| match e {
                ureq::Error::Status(c, _) => format!("telegram HTTP {c}"),
                ureq::Error::Transport(_) => "telegram transport error".to_string(),
            })
    }
}

enum Job {
    Desktop {
        title: String,
        body: String,
    },
    Telegram {
        token: String,
        chat_id: String,
        text: String,
    },
}

/// Runs the slow external sinks (desktop, Telegram: a blocking HTTP POST of up to 10 s) on a background
/// thread behind a bounded queue, so the scheduler / control-socket thread never waits for them. The DB
/// insert in [`send`] stays synchronous. A full queue drops the external message with a warning.
pub struct BackgroundSink {
    tx: std::sync::mpsc::SyncSender<Job>,
}

const BACKGROUND_QUEUE: usize = 32;

impl BackgroundSink {
    pub fn new(inner: std::sync::Arc<dyn ExternalSink + Send + Sync>) -> BackgroundSink {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(BACKGROUND_QUEUE);
        std::thread::spawn(move || {
            for job in rx {
                match job {
                    Job::Desktop { title, body } => inner.desktop(&title, &body),
                    Job::Telegram {
                        token,
                        chat_id,
                        text,
                    } => {
                        if let Err(e) = inner.telegram(&token, &chat_id, &text) {
                            tracing::warn!("{e}");
                        }
                    }
                }
            }
        });
        BackgroundSink { tx }
    }
}

impl ExternalSink for BackgroundSink {
    fn desktop(&self, title: &str, body: &str) {
        let job = Job::Desktop {
            title: title.into(),
            body: body.into(),
        };
        if self.tx.try_send(job).is_err() {
            tracing::warn!("notification queue full or closed; desktop message dropped");
        }
    }
    /// `Ok` means queued, not delivered (failures are logged by the worker).
    fn telegram(&self, token: &str, chat_id: &str, text: &str) -> std::result::Result<(), String> {
        let job = Job::Telegram {
            token: token.into(),
            chat_id: chat_id.into(),
            text: text.into(),
        };
        self.tx
            .try_send(job)
            .map_err(|_| "notification queue full or closed; telegram message dropped".to_string())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    Deduplicated,
    Delivered { desktop: bool, telegram: bool },
}

pub const DEDUP_WINDOW_MINUTES: i64 = 30;

pub fn send(
    conn: &Connection,
    settings: &Settings,
    n: &Notice,
    now: DateTime<Utc>,
    local_minute: u32,
    telegram_token: Option<&str>,
    sink: &dyn ExternalSink,
) -> Result<Delivery> {
    let key = n.dedup_key();
    let cutoff = db::ts(now - Duration::minutes(DEDUP_WINDOW_MINUTES));
    let dup: i64 = conn.query_row(
        "SELECT COUNT(*) FROM notifications WHERE dedup_key=?1 AND created_at >= ?2",
        params![key, cutoff],
        |r| r.get(0),
    )?;
    if dup > 0 {
        return Ok(Delivery::Deduplicated);
    }
    conn.execute(
        "INSERT INTO notifications (notification_type, message, dedup_key, created_at) VALUES (?1,?2,?3,?4)",
        params![n.kind.as_str(), n.internal_text(), key, db::ts(now)],
    )?;
    let quiet = !settings.quiet_hours.is_empty() && in_window(&settings.quiet_hours, local_minute);
    let text = n.external_text(settings.notify_include_names);
    let mut d = Delivery::Delivered {
        desktop: false,
        telegram: false,
    };
    if !quiet {
        let mut desk = false;
        let mut tg = false;
        if settings.notify_desktop {
            sink.desktop(crate::brand::NAME, &text);
            desk = true;
        }
        if settings.notify_telegram {
            let tok = telegram_token.filter(|t| !t.is_empty());
            if settings.telegram_chat_id.is_empty() {
                warn_in_app(conn, now, "telegram_chat", "Telegram notifications are on but no chat id is set (Settings > Telegram chat id)")?;
            } else if let Some(tok) = tok {
                match sink.telegram(tok, &settings.telegram_chat_id, &text) {
                    Ok(()) => tg = true,
                    Err(e) => {
                        tracing::warn!("{e}");
                        warn_in_app(
                            conn,
                            now,
                            "telegram_send",
                            "A Telegram notification could not be delivered (see the log)",
                        )?;
                    }
                }
            } else {
                warn_in_app(conn, now, "telegram_token", "Telegram notifications are on but the bot token is missing (set PLAN_BURNER_TELEGRAM_TOKEN in the environment of the process that drives the scheduler)")?;
            }
        }
        d = Delivery::Delivered {
            desktop: desk,
            telegram: tg,
        };
    }
    Ok(d)
}

/// A notification-system problem shown in the app only (never sent externally, so a broken
/// external channel cannot recurse). Deduplicated like other notifications.
fn warn_in_app(conn: &Connection, now: DateTime<Utc>, key: &str, text: &str) -> Result<()> {
    let cutoff = db::ts(now - Duration::minutes(DEDUP_WINDOW_MINUTES));
    let dup: i64 = conn.query_row(
        "SELECT COUNT(*) FROM notifications WHERE dedup_key=?1 AND created_at >= ?2",
        params![key, cutoff],
        |r| r.get(0),
    )?;
    if dup == 0 {
        conn.execute(
            "INSERT INTO notifications (notification_type, message, dedup_key, created_at) VALUES (?1,?2,?3,?4)",
            params![Kind::ActionFailed.as_str(), text, key, db::ts(now)],
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Stored {
    pub id: i64,
    pub kind: String,
    pub message: String,
    pub created_at: String,
    pub read: bool,
}

pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<Stored>> {
    let mut st = conn.prepare(
        "SELECT id, notification_type, message, created_at, read_at IS NOT NULL FROM notifications ORDER BY id DESC LIMIT ?1",
    )?;
    let v = st
        .query_map([limit as i64], |r| {
            Ok(Stored {
                id: r.get(0)?,
                kind: r.get(1)?,
                message: r.get(2)?,
                created_at: r.get(3)?,
                read: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

pub fn mark_all_read(conn: &Connection, now: DateTime<Utc>) -> Result<()> {
    conn.execute(
        "UPDATE notifications SET read_at=?1 WHERE read_at IS NULL",
        [db::ts(now)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Mock {
        desktop: RefCell<Vec<String>>,
        tg: RefCell<Vec<String>>,
    }
    impl ExternalSink for Mock {
        fn desktop(&self, _t: &str, b: &str) {
            self.desktop.borrow_mut().push(b.into());
        }
        fn telegram(&self, _tok: &str, _c: &str, t: &str) -> std::result::Result<(), String> {
            self.tg.borrow_mut().push(t.into());
            Ok(())
        }
    }
    fn notice() -> Notice {
        Notice {
            kind: Kind::ApprovalRequested,
            account: "claude · Work".into(),
            count: 2,
            names: vec!["Secret refactor of payroll".into()],
            detail: String::new(),
            hint: String::new(),
        }
    }

    #[test]
    fn hint_is_part_of_the_external_text_but_detail_is_not() {
        let mut n = notice();
        n.detail = "secret path /home/me/project".into();
        n.hint = "review it under Approvals".into();
        let ext = n.external_text(false);
        assert!(ext.contains("review it under Approvals"));
        assert!(!ext.contains("secret path"));
        assert!(n.internal_text().contains("secret path"));
    }

    #[test]
    fn telegram_misconfiguration_is_reported_in_the_app() {
        let c = db::open_memory().unwrap();
        let s = Settings {
            notify_telegram: true,
            notify_desktop: false,
            ..Settings::default()
        };
        let m = Mock::default();
        send(&c, &s, &notice(), Utc::now(), 12 * 60, None, &m).unwrap();
        assert!(m.tg.borrow().is_empty());
        let msgs: Vec<String> = c
            .prepare("SELECT message FROM notifications")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(msgs.iter().any(|m| m.contains("no chat id")), "{msgs:?}");
        // with a chat id but no token, the missing token is named (and nothing secret is stored)
        let s2 = Settings {
            telegram_chat_id: "42".into(),
            ..s
        };
        let n2 = Notice {
            count: 9,
            ..notice()
        };
        send(&c, &s2, &n2, Utc::now(), 12 * 60, None, &m).unwrap();
        let msgs: Vec<String> = c
            .prepare("SELECT message FROM notifications")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            msgs.iter().any(|m| m.contains("bot token is missing")),
            "{msgs:?}"
        );
    }

    #[test]
    fn external_text_hides_names_by_default() {
        let n = notice();
        assert!(!n.external_text(false).contains("payroll"));
        assert!(n.external_text(true).contains("payroll"));
        assert!(n.internal_text().contains("payroll"));
    }

    #[test]
    fn dedup_suppresses_repeats_inside_window() {
        let c = db::open_memory().unwrap();
        let s = Settings::default();
        let m = Mock::default();
        let now = Utc::now();
        assert!(matches!(
            send(&c, &s, &notice(), now, 0, None, &m).unwrap(),
            Delivery::Delivered { .. }
        ));
        assert_eq!(
            send(&c, &s, &notice(), now + Duration::minutes(5), 0, None, &m).unwrap(),
            Delivery::Deduplicated
        );
        assert!(matches!(
            send(&c, &s, &notice(), now + Duration::minutes(31), 0, None, &m).unwrap(),
            Delivery::Delivered { .. }
        ));
        assert_eq!(m.desktop.borrow().len(), 2);
    }

    struct Slow {
        started: std::sync::mpsc::Sender<String>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl ExternalSink for Slow {
        fn desktop(&self, _t: &str, _b: &str) {}
        fn telegram(&self, _tok: &str, _c: &str, t: &str) -> std::result::Result<(), String> {
            let _ = self.started.send(t.to_string());
            let _ = self.release.lock().unwrap().recv(); // blocks like a slow HTTP POST
            Ok(())
        }
    }

    #[test]
    fn slow_external_sink_does_not_block_send() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let bg = BackgroundSink::new(std::sync::Arc::new(Slow {
            started: started_tx,
            release: std::sync::Mutex::new(release_rx),
        }));
        let c = db::open_memory().unwrap();
        let s = Settings {
            notify_desktop: false,
            notify_telegram: true,
            telegram_chat_id: "1".into(),
            ..Settings::default()
        };
        let t0 = std::time::Instant::now();
        // The first telegram call blocks the worker; later sends must still return immediately.
        for i in 0..3 {
            let n = Notice {
                count: i,
                ..notice()
            };
            send(&c, &s, &n, Utc::now(), 12 * 60, Some("tok"), &bg).unwrap();
        }
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "send must not wait for the sink"
        );
        assert!(started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok());
        // In-app rows were written synchronously even though delivery is still pending.
        assert_eq!(recent(&c, 10).unwrap().len(), 3);
        for _ in 0..3 {
            let _ = release_tx.send(());
        }
    }

    #[test]
    fn full_queue_drops_external_messages_without_blocking() {
        let (started_tx, _started_rx) = std::sync::mpsc::channel();
        let (_release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let bg = BackgroundSink::new(std::sync::Arc::new(Slow {
            started: started_tx,
            release: std::sync::Mutex::new(release_rx),
        }));
        let t0 = std::time::Instant::now();
        let results: Vec<_> = (0..(BACKGROUND_QUEUE + 5))
            .map(|i| bg.telegram("t", "c", &format!("m{i}")))
            .collect();
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        assert!(results.iter().any(|r| r.is_err()), "overflow is reported");
        assert!(results[0].is_ok());
    }

    #[test]
    fn quiet_hours_suppress_external_but_keep_in_app() {
        let c = db::open_memory().unwrap();
        let s = Settings {
            quiet_hours: "23:00-07:00".into(),
            notify_telegram: true,
            telegram_chat_id: "1".into(),
            ..Settings::default()
        };
        let m = Mock::default();
        let d = send(&c, &s, &notice(), Utc::now(), 23 * 60 + 30, Some("tok"), &m).unwrap();
        assert_eq!(
            d,
            Delivery::Delivered {
                desktop: false,
                telegram: false
            }
        );
        assert!(m.desktop.borrow().is_empty() && m.tg.borrow().is_empty());
        assert_eq!(recent(&c, 10).unwrap().len(), 1);
    }

    #[test]
    fn telegram_requires_opt_in_token_and_chat() {
        let c = db::open_memory().unwrap();
        let m = Mock::default();
        let mut s = Settings::default();
        send(&c, &s, &notice(), Utc::now(), 0, Some("tok"), &m).unwrap();
        assert!(m.tg.borrow().is_empty());
        let c2 = db::open_memory().unwrap();
        s.notify_telegram = true;
        s.telegram_chat_id = "42".into();
        send(&c2, &s, &notice(), Utc::now(), 0, None, &m).unwrap();
        assert!(m.tg.borrow().is_empty(), "no token => nothing sent");
        let c3 = db::open_memory().unwrap();
        send(&c3, &s, &notice(), Utc::now(), 0, Some("tok"), &m).unwrap();
        assert_eq!(m.tg.borrow().len(), 1);
        assert!(!m.tg.borrow()[0].contains("payroll"));
    }
}
