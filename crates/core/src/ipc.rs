//! Local control channel between the process that drives the scheduler (the tray daemon, or whichever
//! front-end holds `driver.lock`) and read-only front-ends.
//!
//! Unix socket `<data dir>/control.sock`, mode 0600, one line of JSON per request and per response.
//! The server never executes anything itself: requests are queued to the owner of the `Service`, which runs
//! them through the normal `Service` methods (billing guard, gates, pre-flight all still apply). Messages carry
//! no credentials (settings hold none).
use crate::approval::PendingBatch;
use crate::control::RunState;
use crate::service::StopReport;
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

pub const SOCKET_FILE: &str = "control.sock";
/// Largest request line; sized for bulk `import_tasks` payloads (still bounded).
const MAX_LINE: u64 = 2 * 1024 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_WAIT: Duration = Duration::from_secs(8);
/// Connections handled at the same time (each holds a thread for up to `CLIENT_TIMEOUT`/`SERVER_WAIT`).
const MAX_HANDLERS: usize = 16;

/// Releases a handler slot when the connection handler ends.
struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn write_response(out: &mut impl Write, resp: &Response) {
    if let Ok(mut s) = serde_json::to_string(resp) {
        s.push('\n');
        let _ = out.write_all(s.as_bytes());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Pause,
    PauseUntil {
        until: DateTime<Utc>,
    },
    Resume,
    Stop,
    ClearStop,
    CancelActive,
    Approve {
        batch_id: u64,
        tasks: Vec<i64>,
    },
    Reject {
        batch_id: u64,
    },
    Propose {
        account_id: i64,
    },
    RunNow {
        task_id: i64,
        account_id: i64,
    },
    /// Persist settings (no credentials: tokens live in env / the provider's own credentials file).
    SaveSettings {
        settings: crate::settings::Settings,
    },
    SetEnabled {
        on: bool,
    },
    CreateAccount {
        provider_type: String,
        label: String,
        plan: String,
        config_dir: Option<String>,
    },
    UpdateAccount {
        id: i64,
        update: crate::accounts::AccountUpdate,
    },
    DeleteAccount {
        id: i64,
    },
    SetAccountPolicy {
        id: i64,
        policy: crate::models::ExecutionPolicy,
    },
    SetSubscriptionConfirmed {
        id: i64,
        confirmed: bool,
    },
    SetAccountAvailable {
        id: i64,
        available: bool,
    },
    SetAccountRunMode {
        id: i64,
        mode: crate::models::RunMode,
    },
    SetAccountLimits {
        id: i64,
        limits: crate::models::AccountLimits,
    },
    SetAccountIgnoreLimits {
        id: i64,
        on: bool,
    },
    /// General "burn before reset" switch (default hours).
    SetBurn {
        on: bool,
    },
    SetAccountBurn {
        id: i64,
        on: bool,
    },
    /// One-shot batch with thresholds and reserve ignored. `account_id: None` = every account.
    BurnNow {
        account_id: Option<i64>,
        with_approval: bool,
    },
    SaveTask {
        form: crate::forms::TaskForm,
    },
    TaskAction {
        id: i64,
        action: TaskAction,
    },
    RecordUsage {
        form: crate::forms::UsageForm,
    },
    RecordActual {
        execution_id: i64,
        actual: f64,
    },
    SaveTemplate {
        name: String,
        task: crate::tasks::NewTask,
    },
    SaveTemplateForm {
        form: crate::forms::TemplateForm,
    },
    DuplicateTemplate {
        id: i64,
    },
    DeleteTemplate {
        id: i64,
    },
    /// Queue a new Pending task straight from a template's defaults.
    QueueTemplate {
        id: i64,
    },
    MarkNotificationsRead,
    /// All tasks in queue order.
    ListTasks,
    /// Bulk-create tasks from JSON / Markdown text (see `core::import`).
    ImportTasks {
        text: String,
        format: crate::import::ImportFormat,
        dry_run: bool,
    },
    /// Put these task ids first, in this order.
    ReorderTasks {
        ids: Vec<i64>,
    },
}

/// Queue operations on one task (forwarded as a single request).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskAction {
    Pause,
    Resume,
    Cancel,
    Retry,
    Duplicate,
    Delete,
    MoveBy { delta: i32 },
    MoveNextTo { target: i64, after: bool },
}

/// What a read-only front-end needs to render the driver's in-memory state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusView {
    pub run_state: RunState,
    pub running: bool,
    pub pending: Vec<PendingBatch>,
    pub last_stop: Option<StopReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Proposed {
        batch_id: Option<u64>,
    },
    /// Result of `BurnNow`.
    Burn {
        report: crate::service::BurnReport,
    },
    /// Result of `RecordActual`: true if the measured usage exceeded the task's budget.
    OverBudget {
        over: bool,
    },
    Account {
        account: Box<crate::models::Account>,
    },
    Status {
        status: StatusView,
    },
    /// Result of `ListTasks`.
    Tasks {
        tasks: Vec<crate::tasks::Task>,
    },
    /// Result of `ImportTasks`.
    Import {
        report: crate::import::ImportReport,
    },
    Err {
        message: String,
    },
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_FILE)
}

type Job = (Request, Sender<Response>);

/// Listens for requests on a background thread; the `Service` owner calls [`Server::drain`] regularly.
pub struct Server {
    rx: Receiver<Job>,
    path: PathBuf,
}

impl Server {
    /// Bind the socket. The caller must already hold the driver lock, so any existing socket file is stale.
    #[cfg(unix)]
    pub fn start(dir: &Path) -> Result<Server> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        use std::os::unix::net::UnixListener;
        let path = socket_path(dir);
        let _ = std::fs::remove_file(&path);
        // Bind inside a fresh 0700 directory, restrict the socket to its owner, then rename it into place.
        // The socket is therefore never reachable by others, without touching the process-wide umask
        // (which would race with files and directories other threads create meanwhile).
        let tmp = dir.join(format!(
            ".sock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&tmp)?;
        let bound = (|| -> Result<UnixListener> {
            let staged = tmp.join("s");
            let l = UnixListener::bind(&staged)?;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&staged, &path)?;
            Ok(l)
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        let listener = bound?;
        let (tx, rx) = channel::<Job>();
        let active = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                // Count before spawning so the cap holds even under a burst of connections.
                if active.fetch_add(1, Ordering::SeqCst) >= MAX_HANDLERS {
                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    write_response(
                        &mut stream,
                        &Response::Err {
                            message: "too many concurrent requests; try again shortly".into(),
                        },
                    );
                    continue;
                }
                let slot = Slot(active.clone());
                let tx = tx.clone();
                std::thread::spawn(move || {
                    serve_one(stream, tx);
                    drop(slot);
                });
            }
        });
        Ok(Server { rx, path })
    }

    #[cfg(not(unix))]
    pub fn start(_dir: &Path) -> Result<Server> {
        Err(Error::Blocked("IPC requires a Unix platform".into()))
    }

    /// Answer every queued request using `handle`. Never blocks.
    pub fn drain(&self, mut handle: impl FnMut(Request) -> Response) {
        for (req, reply) in self.rx.try_iter() {
            let _ = reply.send(handle(req));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn serve_one(stream: std::os::unix::net::UnixStream, jobs: Sender<Job>) {
    let _ = stream.set_read_timeout(Some(CLIENT_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_TIMEOUT));
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let mut line = String::new();
    let mut oversized = false;
    let resp = match BufReader::new(reader.take(MAX_LINE)).read_line(&mut line) {
        // Hit the read cap without reaching the end of the line.
        Ok(n) if n as u64 >= MAX_LINE && !line.ends_with('\n') => {
            oversized = true;
            Response::Err {
                message: format!("request too large (limit {MAX_LINE} bytes)"),
            }
        }
        Ok(n) if n > 0 => match serde_json::from_str::<Request>(line.trim()) {
            Ok(req) => {
                let (tx, rx) = channel();
                if jobs.send((req, tx)).is_err() {
                    Response::Err {
                        message: "driver is shutting down".into(),
                    }
                } else {
                    rx.recv_timeout(SERVER_WAIT).unwrap_or(Response::Err {
                        message: "driver did not respond in time".into(),
                    })
                }
            }
            Err(e) => Response::Err {
                message: format!("bad request: {e}"),
            },
        },
        _ => return,
    };
    write_response(&mut out, &resp);
    if oversized {
        // Read (and discard) what the client is still sending, bounded, so closing with unread data
        // does not reset the connection before the client has read our answer.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = std::io::copy(&mut stream.take(1024 * 1024), &mut std::io::sink());
    }
}

/// Send one request to the driver for `dir`. Any failure to reach it is `Error::Blocked`.
#[cfg(unix)]
pub fn call(dir: &Path, req: &Request) -> Result<Response> {
    use std::os::unix::net::UnixStream;
    let unreachable =
        |e: &dyn std::fmt::Display| Error::Blocked(format!("no scheduler driver reachable ({e})"));
    let mut s = UnixStream::connect(socket_path(dir)).map_err(|e| unreachable(&e))?;
    s.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    s.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    let mut msg = serde_json::to_string(req)?;
    msg.push('\n');
    s.write_all(msg.as_bytes())?;
    let mut line = String::new();
    BufReader::new(s.take(4 * 1024 * 1024))
        .read_line(&mut line)
        .map_err(|e| unreachable(&e))?;
    Ok(serde_json::from_str(line.trim())?)
}

#[cfg(not(unix))]
pub fn call(_dir: &Path, _req: &Request) -> Result<Response> {
    Err(Error::Blocked("IPC requires a Unix platform".into()))
}

/// True when something accepts connections on the control socket (it may still not answer).
#[cfg(unix)]
pub fn socket_reachable(dir: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path(dir)).is_ok()
}

#[cfg(not(unix))]
pub fn socket_reachable(_dir: &Path) -> bool {
    false
}

/// Like [`call`], but turns `Response::Err` into an error.
pub fn call_ok(dir: &Path, req: &Request) -> Result<Response> {
    match call(dir, req)? {
        Response::Err { message } => Err(Error::Blocked(message)),
        r => Ok(r),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn spawn_echo(dir: &Path) -> std::thread::JoinHandle<()> {
        let server = Server::start(dir).unwrap();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            while t0.elapsed() < Duration::from_secs(3) {
                server.drain(|r| match r {
                    Request::Propose { account_id } => Response::Proposed {
                        batch_id: Some(account_id as u64),
                    },
                    Request::Stop => Response::Err {
                        message: "nope".into(),
                    },
                    _ => Response::Ok,
                });
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    }

    #[test]
    fn round_trip_errors_and_socket_permissions() {
        let d = tempfile::tempdir().unwrap();
        let h = spawn_echo(d.path());
        let mode = std::fs::metadata(socket_path(d.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(matches!(
            call_ok(d.path(), &Request::Propose { account_id: 7 }).unwrap(),
            Response::Proposed { batch_id: Some(7) }
        ));
        assert!(matches!(
            call_ok(d.path(), &Request::Pause).unwrap(),
            Response::Ok
        ));
        assert!(call_ok(d.path(), &Request::Stop).is_err());
        h.join().unwrap();
        // Socket removed when the server is dropped; clients then fail closed.
        assert!(call(d.path(), &Request::Pause).is_err());
    }

    #[test]
    fn rejects_garbage_and_unknown_commands() {
        let d = tempfile::tempdir().unwrap();
        let h = spawn_echo(d.path());
        use std::os::unix::net::UnixStream;
        for bad in ["not json\n", "{\"cmd\":\"rm_rf\"}\n"] {
            let mut s = UnixStream::connect(socket_path(d.path())).unwrap();
            s.write_all(bad.as_bytes()).unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).unwrap();
            assert!(matches!(
                serde_json::from_str::<Response>(line.trim()).unwrap(),
                Response::Err { .. }
            ));
        }
        h.join().unwrap();
    }

    #[test]
    fn task_requests_round_trip_as_json_lines() {
        for r in [
            Request::ListTasks,
            Request::ReorderTasks { ids: vec![2, 1] },
            Request::ImportTasks {
                text: "- a: b".into(),
                format: crate::import::ImportFormat::Markdown,
                dry_run: true,
            },
        ] {
            let j = serde_json::to_string(&r).unwrap();
            assert!(!j.contains('\n'));
            assert_eq!(serde_json::from_str::<Request>(&j).unwrap(), r);
        }
        let j = r#"{"cmd":"import_tasks","text":"[]","format":"json","dry_run":false}"#;
        assert!(matches!(
            serde_json::from_str::<Request>(j).unwrap(),
            Request::ImportTasks { .. }
        ));
    }

    #[test]
    fn oversized_request_gets_a_clear_error() {
        use std::os::unix::net::UnixStream;
        let d = tempfile::tempdir().unwrap();
        let h = spawn_echo(d.path());
        let mut s = UnixStream::connect(socket_path(d.path())).unwrap();
        let big = vec![b'a'; (MAX_LINE as usize) + 5000];
        s.write_all(&big).unwrap();
        s.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        match serde_json::from_str::<Response>(line.trim()).unwrap() {
            Response::Err { message } => assert!(message.contains("too large"), "{message}"),
            other => panic!("{other:?}"),
        }
        drop(s);
        // A normal request still works afterwards.
        assert!(matches!(
            call_ok(d.path(), &Request::Pause).unwrap(),
            Response::Ok
        ));
        h.join().unwrap();
    }

    #[test]
    fn concurrent_handlers_are_capped() {
        use std::os::unix::net::UnixStream;
        let d = tempfile::tempdir().unwrap();
        let h = spawn_echo(d.path());
        // Idle connections each occupy a handler (they wait for a request line).
        let idle: Vec<_> = (0..MAX_HANDLERS)
            .map(|_| UnixStream::connect(socket_path(d.path())).unwrap())
            .collect();
        let mut extra = UnixStream::connect(socket_path(d.path())).unwrap();
        let mut line = String::new();
        BufReader::new(&mut extra).read_line(&mut line).unwrap();
        match serde_json::from_str::<Response>(line.trim()).unwrap() {
            Response::Err { message } => assert!(message.contains("too many"), "{message}"),
            other => panic!("{other:?}"),
        }
        // Slots are released when connections go away.
        drop(idle);
        drop(extra);
        let mut ok = false;
        for _ in 0..50 {
            if matches!(call(d.path(), &Request::Pause), Ok(Response::Ok)) {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ok, "capacity must free up after clients disconnect");
        h.join().unwrap();
    }

    #[test]
    fn no_driver_means_blocked() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            call(d.path(), &Request::Status),
            Err(Error::Blocked(_))
        ));
    }
}
