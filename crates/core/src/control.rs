//! Temporary automation state and the emergency STOP.
use crate::error::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RunState {
    /// A task is executing.
    Running,
    /// Paused by the user (immediately, or until the next usage reset).
    Paused,
    /// Idle, waiting for conditions / user.
    Waiting,
    /// Emergency STOP was pressed. Nothing launches until the user explicitly clears it.
    Stopped,
}

#[derive(Debug)]
struct Inner {
    paused: bool,
    paused_until: Option<DateTime<Utc>>,
    stopped: bool,
    executing: bool,
}

#[derive(Debug)]
pub struct Controller {
    inner: Mutex<Inner>,
    /// Shared with running processes; set by STOP and by cancel requests.
    cancel: Arc<AtomicBool>,
}

impl Default for Controller {
    fn default() -> Self {
        Controller {
            inner: Mutex::new(Inner {
                paused: false,
                paused_until: None,
                stopped: false,
                executing: false,
            }),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Controller {
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn state(&self, now: DateTime<Utc>) -> RunState {
        let mut i = self.inner.lock().unwrap();
        if i.paused && i.paused_until.is_some_and(|t| t <= now) {
            i.paused = false;
            i.paused_until = None;
        }
        if i.stopped {
            RunState::Stopped
        } else if i.paused {
            RunState::Paused
        } else if i.executing {
            RunState::Running
        } else {
            RunState::Waiting
        }
    }

    /// New launches are allowed (an active run does not matter here; the runner enforces one at a time).
    pub fn can_launch(&self, now: DateTime<Utc>) -> bool {
        matches!(self.state(now), RunState::Running | RunState::Waiting)
    }

    pub fn set_executing(&self, v: bool) {
        self.inner.lock().unwrap().executing = v;
    }

    /// Pause immediately: no new launches; a running task is allowed to finish.
    pub fn pause_now(&self) {
        let mut i = self.inner.lock().unwrap();
        i.paused = true;
        i.paused_until = None;
    }

    /// Pause until the given reset time (usually the account's next session reset).
    pub fn pause_until(&self, t: DateTime<Utc>) {
        let mut i = self.inner.lock().unwrap();
        i.paused = true;
        i.paused_until = Some(t);
    }

    pub fn resume(&self) {
        let mut i = self.inner.lock().unwrap();
        i.paused = false;
        i.paused_until = None;
    }

    /// Emergency STOP: block launches and ask any active process to terminate.
    /// The caller must still wait for the process-termination confirmation before reporting success.
    pub fn emergency_stop(&self) {
        self.inner.lock().unwrap().stopped = true;
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Explicit user action to re-arm after STOP.
    pub fn clear_stop(&self) {
        self.inner.lock().unwrap().stopped = false;
        self.cancel.store(false, Ordering::SeqCst);
    }

    /// Cancel only the active run (e.g. the user cancels one task) without entering Stopped.
    pub fn cancel_active(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// (stopped, paused, paused_until) as currently held, for persistence.
    pub fn persistent_state(&self) -> (bool, bool, Option<DateTime<Utc>>) {
        let i = self.inner.lock().unwrap();
        (i.stopped, i.paused, i.paused_until)
    }

    /// Re-applies a persisted state after a restart. A stop is restored as a stop (it also raises the
    /// cancel flag exactly as `emergency_stop` does); a pause whose end time already passed is dropped.
    pub fn restore(
        &self,
        stopped: bool,
        paused: bool,
        paused_until: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) {
        let mut i = self.inner.lock().unwrap();
        i.stopped = stopped;
        let expired = paused_until.is_some_and(|t| t <= now);
        i.paused = paused && !expired;
        i.paused_until = if i.paused { paused_until } else { None };
        if stopped {
            self.cancel.store(true, Ordering::SeqCst);
        }
    }

    pub fn reset_cancel(&self) {
        if !self.inner.lock().unwrap().stopped {
            self.cancel.store(false, Ordering::SeqCst);
        }
    }
}

/// Writes the controller's stop/pause state so a restarted daemon cannot launch after an EMERGENCY STOP.
pub fn save_state(conn: &Connection, c: &Controller) -> Result<()> {
    let (stopped, paused, until) = c.persistent_state();
    conn.execute(
        "UPDATE control_state SET stopped=?1, paused=?2, paused_until=?3 WHERE id = 1",
        params![stopped, paused, until.map(|t| t.to_rfc3339())],
    )?;
    Ok(())
}

/// Loads the persisted state into `c` (call once, by the driver, right after taking the lock).
pub fn restore_state(conn: &Connection, c: &Controller, now: DateTime<Utc>) -> Result<()> {
    let (stopped, paused, until): (bool, bool, Option<String>) = conn.query_row(
        "SELECT stopped, paused, paused_until FROM control_state WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let until = until
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| t.with_timezone(&Utc));
    c.restore(stopped, paused, until, now);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn stop_blocks_launches_until_cleared() {
        let c = Controller::default();
        let now = Utc::now();
        assert!(c.can_launch(now));
        c.emergency_stop();
        assert!(!c.can_launch(now));
        assert!(c.cancel_flag().load(Ordering::SeqCst));
        c.resume();
        assert!(!c.can_launch(now), "resume does not clear STOP");
        c.clear_stop();
        assert!(c.can_launch(now));
        assert!(!c.cancel_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn pause_until_reset_expires() {
        let c = Controller::default();
        let now = Utc::now();
        c.pause_until(now + Duration::hours(1));
        assert_eq!(c.state(now), RunState::Paused);
        assert_eq!(c.state(now + Duration::hours(2)), RunState::Waiting);
        c.pause_now();
        assert_eq!(c.state(now + Duration::days(9)), RunState::Paused);
    }

    #[test]
    fn stop_and_pause_survive_a_restart() {
        let d = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&d.path().join("t.db")).unwrap();
        let now = Utc::now();
        let c = Controller::default();
        c.emergency_stop();
        c.pause_until(now + Duration::hours(1));
        save_state(&conn, &c).unwrap();

        let fresh = Controller::default();
        assert!(fresh.can_launch(now));
        restore_state(&conn, &fresh, now).unwrap();
        assert_eq!(fresh.state(now), RunState::Stopped);
        assert!(!fresh.can_launch(now));
        assert!(fresh.cancel_flag().load(Ordering::SeqCst));
        fresh.resume();
        assert!(
            !fresh.can_launch(now),
            "resume does not clear a restored STOP"
        );
        fresh.clear_stop();
        save_state(&conn, &fresh).unwrap();

        let again = Controller::default();
        restore_state(&conn, &again, now).unwrap();
        assert!(again.can_launch(now), "a cleared STOP stays cleared");
    }

    #[test]
    fn restored_pause_expires_and_past_pause_is_dropped() {
        let d = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&d.path().join("t.db")).unwrap();
        let now = Utc::now();
        let c = Controller::default();
        c.pause_until(now + Duration::hours(1));
        save_state(&conn, &c).unwrap();
        let r = Controller::default();
        restore_state(&conn, &r, now).unwrap();
        assert_eq!(r.state(now), RunState::Paused);
        assert_eq!(r.state(now + Duration::hours(2)), RunState::Waiting);
        let late = Controller::default();
        restore_state(&conn, &late, now + Duration::hours(2)).unwrap();
        assert_eq!(late.state(now + Duration::hours(2)), RunState::Waiting);
    }
}
