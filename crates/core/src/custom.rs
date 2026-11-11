//! "Custom" provider: one user-supplied script per account, called as
//! `program [fixed args…] <kind> [--name=value …]`. See docs/CUSTOM_PROVIDER.md (protocol v1).
//!
//! Kinds: `usage` and `status` (pull), `run` (push: executes the agent embedded in the script).
//! The script is arbitrary user code run with the user's privileges. It is never started through a shell,
//! the prompt only travels on stdin, and billing-related env vars are scrubbed. The app cannot police
//! what a script bills, so `status` is fail-closed and the user must still confirm subscription billing.
use crate::engine::{run_process, EndReason};
use crate::models::{Account, BillingMode, Confidence};
use crate::provider::{CommandSpec, Detection, ProviderAdapter, TaskLaunch};
use crate::usage::{self, NewSnapshot};
use crate::{accounts, db, safety, Error, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub const PROTOCOL_VERSION: u64 = 1;
/// A cached `status` older than this is ignored (fail-closed). The app re-polls every few minutes.
pub const STATUS_MAX_AGE_MINUTES: i64 = 60;
/// Exit code a `run` script uses to report a rate/usage limit (EX_TEMPFAIL).
pub const EXIT_RATE_LIMITED: i32 = 75;
/// Exit code for "kind not supported" (EX_USAGE).
pub const EXIT_UNSUPPORTED: i32 = 64;
const PULL_OUTPUT_CAP: usize = 1024 * 1024;

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomConfig {
    /// Absolute path of the script/executable.
    pub program: String,
    /// Fixed leading arguments (before the kind).
    #[serde(default)]
    pub args: Vec<String>,
    /// Timeout for pull kinds (`usage`, `status`).
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl CustomConfig {
    /// Checks that can be done without touching the filesystem (used when saving).
    pub fn validate_syntax(&self) -> Result<()> {
        let p = self.program.trim();
        if p.is_empty() || !Path::new(p).is_absolute() {
            return Err(Error::Invalid(
                "script program must be an absolute path".into(),
            ));
        }
        if self.program.contains('\0') || self.args.iter().any(|a| a.contains('\0')) {
            return Err(Error::Invalid("NUL bytes are not allowed".into()));
        }
        if !(1..=600).contains(&self.timeout_secs) {
            return Err(Error::Invalid(
                "script timeout must be 1-600 seconds".into(),
            ));
        }
        Ok(())
    }

    /// Full check before execution: the file exists and is executable.
    pub fn validate_runtime(&self) -> std::result::Result<(), String> {
        self.validate_syntax().map_err(|e| e.to_string())?;
        let p = Path::new(&self.program);
        let meta = std::fs::metadata(p).map_err(|e| format!("{}: {e}", self.program))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", self.program));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                return Err(format!("{} is not executable", self.program));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Usage,
    Status,
    Run,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Usage => "usage",
            Kind::Status => "status",
            Kind::Run => "run",
        }
    }
}

fn env_name(param: &str) -> String {
    format!(
        "PLAN_BURNER_{}",
        param.to_ascii_uppercase().replace('-', "_")
    )
}

/// The single place that builds argv and extra environment for a call.
/// argv = fixed args, then the kind, then `--name=value` for each parameter.
pub fn invocation(
    cfg: &CustomConfig,
    kind: Kind,
    account_label: &str,
    params: &[(&str, &str)],
) -> (Vec<String>, HashMap<String, String>) {
    let clean = |s: &str| s.replace('\0', "");
    let mut args = cfg.args.clone();
    args.push(kind.as_str().to_string());
    let mut env = HashMap::from([
        (
            "PLAN_BURNER_PROTOCOL".to_string(),
            PROTOCOL_VERSION.to_string(),
        ),
        ("PLAN_BURNER_KIND".to_string(), kind.as_str().to_string()),
        ("PLAN_BURNER_ACCOUNT".to_string(), clean(account_label)),
    ]);
    for (k, v) in params {
        args.push(format!("--{k}={}", clean(v)));
        env.insert(env_name(k), clean(v));
    }
    (args, env)
}

// ---------------------------------------------------------------- protocol: status

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomStatus {
    pub billing_mode: BillingMode,
    pub authenticated: bool,
    /// Kinds the script supports; `None` = unspecified (assume all).
    pub supports: Option<Vec<String>>,
    pub message: String,
}

impl CustomStatus {
    pub fn supports_kind(&self, k: Kind) -> bool {
        self.supports
            .as_ref()
            .is_none_or(|s| s.iter().any(|x| x == k.as_str()))
    }
}

fn check_version(v: &Value) -> std::result::Result<(), String> {
    match v.get("version").and_then(Value::as_u64) {
        Some(PROTOCOL_VERSION) => Ok(()),
        other => Err(format!("unsupported protocol version {other:?}")),
    }
}

pub fn parse_status_output(s: &str) -> std::result::Result<CustomStatus, String> {
    let v: Value = serde_json::from_str(s).map_err(|_| "status output is not JSON".to_string())?;
    check_version(&v)?;
    let billing_mode = v
        .get("billing_mode")
        .and_then(Value::as_str)
        .and_then(BillingMode::parse)
        .ok_or("status: billing_mode must be subscription|api|unverified")?;
    let authenticated = v
        .get("authenticated")
        .and_then(Value::as_bool)
        .ok_or("status: authenticated must be a boolean")?;
    let supports = match v.get("supports") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(
            a.iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .ok_or("status: supports must be strings")
                })
                .collect::<std::result::Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err("status: supports must be an array".into()),
    };
    Ok(CustomStatus {
        billing_mode,
        authenticated,
        supports,
        message: v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(200)
            .collect(),
    })
}

// ---------------------------------------------------------------- protocol: usage

#[derive(Debug, Clone, PartialEq)]
pub struct UsageData {
    pub session_remaining: Option<f64>,
    pub weekly_remaining: Option<f64>,
    pub session_reset_at: Option<DateTime<Utc>>,
    pub weekly_reset_at: Option<DateTime<Utc>>,
    pub confidence: Confidence,
    pub plan: Option<String>,
}

fn pct(v: &Value, key: &str) -> std::result::Result<Option<f64>, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => match x.as_f64() {
            Some(f) if (0.0..=100.0).contains(&f) => Ok(Some(f)),
            _ => Err(format!(
                "usage: {key} must be a number between 0 and 100 or null"
            )),
        },
    }
}

fn when(v: &Value, key: &str) -> std::result::Result<Option<DateTime<Utc>>, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => x.as_str().and_then(db::parse_ts).map(Some).ok_or(format!(
            "usage: {key} must be an RFC 3339 timestamp or null"
        )),
    }
}

pub fn parse_usage_output(s: &str) -> std::result::Result<UsageData, String> {
    let v: Value = serde_json::from_str(s).map_err(|_| "usage output is not JSON".to_string())?;
    check_version(&v)?;
    let confidence = match v.get("confidence").and_then(Value::as_str) {
        None => Confidence::Manual,
        Some("provider") => Confidence::Provider,
        Some("estimated") => Confidence::Estimated,
        Some("manual") => Confidence::Manual,
        Some(_) => return Err("usage: confidence must be provider|estimated|manual".into()),
    };
    let d = UsageData {
        session_remaining: pct(&v, "session_remaining")?,
        weekly_remaining: pct(&v, "weekly_remaining")?,
        session_reset_at: when(&v, "session_reset_at")?,
        weekly_reset_at: when(&v, "weekly_reset_at")?,
        confidence,
        plan: v
            .get("plan")
            .and_then(Value::as_str)
            .map(|p| p.chars().take(64).collect()),
    };
    if d.session_remaining.is_none() && d.weekly_remaining.is_none() {
        return Err("usage: both session_remaining and weekly_remaining are unknown".into());
    }
    Ok(d)
}

// ---------------------------------------------------------------- execution

fn base_env(
    parent: &HashMap<String, String>,
    extra: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut e = safety::scrubbed_env(parent, None);
    e.extend(extra);
    e
}

fn short(s: &str) -> String {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// Runs a pull kind and returns stdout. Error text never includes script stdout.
pub fn call_pull(
    cfg: &CustomConfig,
    kind: Kind,
    account_label: &str,
    parent_env: &HashMap<String, String>,
) -> std::result::Result<String, String> {
    cfg.validate_runtime()?;
    let (args, extra) = invocation(cfg, kind, account_label, &[]);
    let spec = CommandSpec {
        program: PathBuf::from(&cfg.program),
        args,
        env: base_env(parent_env, extra),
        cwd: Path::new(&cfg.program)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/")),
        stdin: String::new(),
    };
    let out = run_process(
        &spec,
        std::time::Duration::from_secs(cfg.timeout_secs),
        &Arc::new(AtomicBool::new(false)),
    );
    match out.reason {
        EndReason::Exited(0) if out.stdout.len() > PULL_OUTPUT_CAP => {
            Err(format!("{} output too large", kind.as_str()))
        }
        EndReason::Exited(0) => Ok(out.stdout),
        EndReason::Exited(EXIT_UNSUPPORTED) => {
            Err(format!("script does not support `{}`", kind.as_str()))
        }
        EndReason::Exited(c) => Err(format!(
            "script `{}` exited with {c}: {}",
            kind.as_str(),
            short(&out.stderr)
        )),
        EndReason::TimedOut => Err(format!("script `{}` timed out", kind.as_str())),
        EndReason::Cancelled => Err("cancelled".into()),
        EndReason::SpawnFailed(e) => Err(format!("could not start script: {e}")),
        EndReason::WaitFailed(e) => Err(format!("lost track of script ({e}); it was killed")),
    }
}

/// Polls the script: always `status` (billing must stay fresh), and `usage` when the account uses it.
/// Records the usage snapshot and caches the status. Both are attempted; errors are joined.
pub fn poll(
    conn: &rusqlite::Connection,
    a: &Account,
    env: &HashMap<String, String>,
    now: DateTime<Utc>,
) -> std::result::Result<(), String> {
    let cfg = a.custom.as_ref().ok_or("no script configured")?;
    let mut errors: Vec<String> = vec![];
    let mut supports_usage = true;
    match call_pull(cfg, Kind::Status, &a.label, env).and_then(|o| parse_status_output(&o)) {
        Ok(s) => {
            supports_usage = s.supports_kind(Kind::Usage);
            accounts::set_custom_status(conn, a.id, &s, now).map_err(|e| e.to_string())?;
        }
        Err(e) => errors.push(e),
    }
    if a.usage_source == crate::models::UsageSource::CustomScript && supports_usage {
        match call_pull(cfg, Kind::Usage, &a.label, env).and_then(|o| parse_usage_output(&o)) {
            Ok(u) => {
                let snap = NewSnapshot {
                    account_id: a.id,
                    session_remaining: u.session_remaining,
                    weekly_remaining: u.weekly_remaining,
                    session_reset_at: u.session_reset_at,
                    weekly_reset_at: u.weekly_reset_at,
                    confidence: u.confidence,
                    source: "custom_script".into(),
                };
                usage::record(conn, &snap, now).map_err(|e| e.to_string())?;
                if a.subscription_plan.is_empty() {
                    if let Some(p) = u.plan.filter(|p| !p.is_empty()) {
                        let _ = accounts::set_plan(conn, a.id, &p);
                    }
                }
            }
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

// ---------------------------------------------------------------- detection

/// Pure fail-closed mapping from (config check, status, age, user confirmation) to a detection.
pub fn detection_from_status(
    cfg_check: std::result::Result<(), String>,
    status: Option<&CustomStatus>,
    age: Option<Duration>,
    confirmed: bool,
) -> Detection {
    let mut reasons = vec![];
    let mut cli_found = cfg_check.is_ok();
    if let Err(e) = cfg_check {
        reasons.push(e);
    }
    let billing = match (status, age) {
        (None, _) | (_, None) => {
            reasons.push("no status from the script yet (use Refresh now)".into());
            BillingMode::Unverified
        }
        (Some(_), Some(a)) if a > Duration::minutes(STATUS_MAX_AGE_MINUTES) => {
            reasons.push("script status is stale".into());
            BillingMode::Unverified
        }
        (Some(s), Some(_)) => {
            if !s.supports_kind(Kind::Run) {
                cli_found = false;
                reasons.push("script does not support `run` (data-only account)".into());
            }
            match s.billing_mode {
                BillingMode::Api => {
                    reasons.push("script reports API billing".into());
                    BillingMode::Api
                }
                _ if !s.authenticated => {
                    reasons.push("script reports it is not authenticated".into());
                    BillingMode::Unverified
                }
                BillingMode::Unverified => {
                    reasons.push(if s.message.is_empty() {
                        "script cannot verify subscription billing".into()
                    } else {
                        format!("script: {}", s.message)
                    });
                    BillingMode::Unverified
                }
                BillingMode::Subscription if !confirmed => {
                    reasons.push("you have not confirmed this account uses a subscription".into());
                    BillingMode::Unverified
                }
                BillingMode::Subscription => BillingMode::Subscription,
            }
        }
    };
    Detection {
        cli_found,
        billing,
        reasons,
    }
}

#[derive(Debug, Default, Clone)]
pub struct CustomAdapter;

impl ProviderAdapter for CustomAdapter {
    fn provider_type(&self) -> &'static str {
        "custom"
    }
    fn display_name(&self) -> &'static str {
        "Custom (script)"
    }

    /// Cheap: uses only the cached `status`.
    fn detect(&self, account: &Account, _env: &HashMap<String, String>) -> Detection {
        let check = match &account.custom {
            Some(c) => c.validate_runtime(),
            None => Err("no script configured".to_string()),
        };
        let now = Utc::now();
        let (s, age) = match &account.custom_status {
            Some((s, t)) => (Some(s), Some(now - *t)),
            None => (None, None),
        };
        detection_from_status(check, s, age, account.subscription_confirmed)
    }

    /// Active: runs the script's `status` right now (called before every task).
    fn probe(&self, account: &Account, env: &HashMap<String, String>) -> Option<Detection> {
        let Some(cfg) = &account.custom else {
            return Some(self.detect(account, env));
        };
        let check = cfg.validate_runtime();
        if check.is_err() {
            return Some(detection_from_status(check, None, None, false));
        }
        Some(
            match call_pull(cfg, Kind::Status, &account.label, env)
                .and_then(|o| parse_status_output(&o))
            {
                Ok(s) => detection_from_status(
                    Ok(()),
                    Some(&s),
                    Some(Duration::zero()),
                    account.subscription_confirmed,
                ),
                Err(e) => Detection {
                    cli_found: true,
                    billing: BillingMode::Unverified,
                    reasons: vec![e],
                },
            },
        )
    }

    fn command(
        &self,
        account: &Account,
        launch: &TaskLaunch,
        cwd: PathBuf,
        env: &HashMap<String, String>,
    ) -> CommandSpec {
        let Some(cfg) = &account.custom else {
            return CommandSpec {
                program: PathBuf::new(),
                args: vec![],
                env: HashMap::new(),
                cwd,
                stdin: String::new(),
            };
        };
        let id = launch.task_id.to_string();
        let mut params = vec![("task-id", id.as_str())];
        if !launch.model.is_empty() {
            params.push(("model", launch.model));
        }
        // The neutral run mode is handed to the script (`--mode=<m>` / PLAN_BURNER_MODE); it translates it.
        if launch.mode != crate::models::RunMode::Inherit {
            params.push(("mode", launch.mode.as_str()));
        }
        let (args, extra) = invocation(cfg, Kind::Run, &account.label, &params);
        CommandSpec {
            program: PathBuf::from(&cfg.program),
            args,
            env: base_env(env, extra),
            cwd,
            stdin: launch.prompt.to_string(),
        }
    }

    fn is_rate_limited(&self, _output: &str, exit_code: i32) -> bool {
        exit_code == EXIT_RATE_LIMITED
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::models::{ExecutionStatus, TaskStatus, UsageSource};
    use crate::runner::{run_batch_conn, BatchRequest, Event, Runtime};
    use crate::tasks::{self, NewTask};
    use std::os::unix::fs::PermissionsExt;

    const USAGE_OK: &str = r#"{"version":1,"session_remaining":95.5,"weekly_remaining":90,"session_reset_at":"2099-01-01T00:00:00Z","confidence":"provider","plan":"team"}"#;
    const STATUS_SUB: &str = r#"{"version":1,"billing_mode":"subscription","authenticated":true}"#;

    /// Writes a dispatcher script. Variants are driven by files next to it (so tests can flip them).
    fn write_script(dir: &Path) -> PathBuf {
        let p = dir.join("agent.sh");
        std::fs::write(
            &p,
            r#"#!/bin/sh
D="$(dirname "$0")"
KIND="$PLAN_BURNER_KIND"
[ "$1" = "$KIND" ] || [ "$2" = "$KIND" ] || { echo "argv/env mismatch" >&2; exit 2; }
case "$KIND" in
  usage) cat "$D/usage.json";;
  status) cat "$D/status.json";;
  run)
    P=$(cat)
    echo "$@" > "$D/last_args"
    echo "$P" > "$D/last_prompt"
    env | grep '^PLAN_BURNER_' | sort > "$D/last_env"
    env | grep -c 'ANTHROPIC_API_KEY' > "$D/leak" || true
    case "$P" in
      *LIMIT*) exit 75;; *FAIL*) exit 1;; *SLEEP*) sleep 30;;
    esac
    echo "result for: $P";;
  *) exit 64;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("usage.json"), USAGE_OK).unwrap();
        std::fs::write(dir.join("status.json"), STATUS_SUB).unwrap();
        p
    }

    fn cfg(p: &Path) -> CustomConfig {
        CustomConfig {
            program: p.to_string_lossy().into_owned(),
            args: vec!["--profile=work".into()],
            timeout_secs: 5,
        }
    }

    struct Fx {
        dir: tempfile::TempDir,
        conn: rusqlite::Connection,
        acct: i64,
        rt: Runtime,
        rx: std::sync::mpsc::Receiver<Event>,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path());
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let conn = db::open_memory().unwrap();
        let mut s = crate::settings::load(&conn).unwrap();
        s.working_root = work.to_string_lossy().into_owned();
        s.results_dir = dir.path().join("results").to_string_lossy().into_owned();
        crate::settings::save(&conn, &s).unwrap();
        let a = accounts::create(&conn, "custom", "Mine", "", None).unwrap();
        let a = accounts::update(
            &conn,
            a.id,
            &accounts::AccountUpdate {
                label: "Mine".into(),
                plan: "".into(),
                config_dir: None,
                credentials_file: None,
                usage_source: UsageSource::CustomScript,
                user_agent: None,
                cli_command: None,
                availability_schedule: String::new(),
                custom: Some(cfg(&script)),
            },
        )
        .unwrap();
        accounts::set_subscription_confirmed(&conn, a.id, true).unwrap();
        let now = Utc::now();
        poll(
            &conn,
            &accounts::get(&conn, a.id).unwrap(),
            &HashMap::new(),
            now,
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let adapters: Vec<Box<dyn ProviderAdapter>> = vec![Box::new(CustomAdapter)];
        let env = HashMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "sk-leak".to_string()),
        ]);
        Fx {
            dir,
            conn,
            acct: a.id,
            rt: Runtime {
                adapters: Arc::new(adapters),
                controller: Arc::new(crate::control::Controller::default()),
                env,
                events: tx,
            },
            rx,
        }
    }

    fn task(f: &Fx, prompt: &str) -> i64 {
        tasks::create(
            &f.conn,
            &NewTask {
                name: "t".into(),
                prompt: prompt.into(),
                provider_type: Some("custom".into()),
                model: "m1".into(),
                timeout_secs: 20,
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
        .id
    }

    fn go(f: &Fx, ids: Vec<i64>) -> usize {
        run_batch_conn(
            &f.conn,
            &BatchRequest {
                account_id: f.acct,
                task_ids: ids,
                automatic: false,
                mode_override: None,
                burn: false,
            },
            &f.rt,
        )
        .unwrap()
    }

    #[test]
    fn config_validation() {
        let ok = CustomConfig {
            program: "/bin/sh".into(),
            args: vec![],
            timeout_secs: 30,
        };
        assert!(ok.validate_syntax().is_ok() && ok.validate_runtime().is_ok());
        for bad in [
            CustomConfig {
                program: "relative.sh".into(),
                ..ok.clone()
            },
            CustomConfig {
                program: "".into(),
                ..ok.clone()
            },
            CustomConfig {
                args: vec!["a\0b".into()],
                ..ok.clone()
            },
            CustomConfig {
                timeout_secs: 0,
                ..ok.clone()
            },
            CustomConfig {
                timeout_secs: 601,
                ..ok.clone()
            },
        ] {
            assert!(bad.validate_syntax().is_err(), "{bad:?}");
        }
        assert!(CustomConfig {
            program: "/nonexistent/x".into(),
            ..ok.clone()
        }
        .validate_runtime()
        .is_err());
        assert!(
            CustomConfig {
                program: "/etc/passwd".into(),
                ..ok
            }
            .validate_runtime()
            .is_err(),
            "not executable"
        );
    }

    #[test]
    fn invocation_layout() {
        let c = CustomConfig {
            program: "/x".into(),
            args: vec!["--profile=work".into()],
            timeout_secs: 30,
        };
        let (args, env) = invocation(&c, Kind::Run, "Acc", &[("task-id", "7"), ("model", "opus")]);
        assert_eq!(
            args,
            vec!["--profile=work", "run", "--task-id=7", "--model=opus"]
        );
        assert_eq!(env["PLAN_BURNER_KIND"], "run");
        assert_eq!(env["PLAN_BURNER_PROTOCOL"], "1");
        assert_eq!(env["PLAN_BURNER_ACCOUNT"], "Acc");
        assert_eq!(env["PLAN_BURNER_TASK_ID"], "7");
        assert_eq!(env["PLAN_BURNER_MODEL"], "opus");
        let (a2, _) = invocation(&c, Kind::Usage, "A", &[("x", "a\0b")]);
        assert_eq!(a2.last().unwrap(), "--x=ab");
    }

    #[test]
    fn usage_parsing() {
        let u = parse_usage_output(USAGE_OK).unwrap();
        assert_eq!(
            (u.session_remaining, u.weekly_remaining),
            (Some(95.5), Some(90.0))
        );
        assert_eq!(u.confidence, Confidence::Provider);
        assert_eq!(u.plan.as_deref(), Some("team"));
        let p = parse_usage_output(r#"{"version":1,"weekly_remaining":10}"#).unwrap();
        assert_eq!(p.session_remaining, None);
        assert_eq!(p.confidence, Confidence::Manual, "default");
        for bad in [
            "garbage",
            r#"{"version":2,"weekly_remaining":10}"#,
            r#"{"weekly_remaining":10}"#,
            r#"{"version":1}"#,
            r#"{"version":1,"session_remaining":null,"weekly_remaining":null}"#,
            r#"{"version":1,"session_remaining":101}"#,
            r#"{"version":1,"session_remaining":"5"}"#,
            r#"{"version":1,"session_remaining":5,"session_reset_at":"tomorrow"}"#,
            r#"{"version":1,"session_remaining":5,"confidence":"unknown"}"#,
        ] {
            assert!(parse_usage_output(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn status_parsing() {
        let s = parse_status_output(r#"{"version":1,"billing_mode":"api","authenticated":true,"supports":["usage"],"message":"hi"}"#).unwrap();
        assert_eq!(s.billing_mode, BillingMode::Api);
        assert!(s.supports_kind(Kind::Usage) && !s.supports_kind(Kind::Run));
        assert!(parse_status_output(STATUS_SUB)
            .unwrap()
            .supports_kind(Kind::Run));
        for bad in [
            "x",
            r#"{"version":1,"billing_mode":"free","authenticated":true}"#,
            r#"{"version":1,"billing_mode":"api"}"#,
            r#"{"version":1,"billing_mode":"api","authenticated":true,"supports":"run"}"#,
            r#"{"version":9,"billing_mode":"api","authenticated":true}"#,
        ] {
            assert!(parse_status_output(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn detection_is_fail_closed() {
        let ok = || Ok(());
        let st = |b, auth| CustomStatus {
            billing_mode: b,
            authenticated: auth,
            supports: None,
            message: String::new(),
        };
        let fresh = Some(Duration::minutes(1));
        let d = detection_from_status(
            ok(),
            Some(&st(BillingMode::Subscription, true)),
            fresh,
            true,
        );
        assert_eq!(d.billing, BillingMode::Subscription);
        assert!(d.cli_found);
        let b =
            |s: Option<&CustomStatus>, age, conf| detection_from_status(ok(), s, age, conf).billing;
        assert_eq!(
            b(Some(&st(BillingMode::Subscription, true)), fresh, false),
            BillingMode::Unverified,
            "unconfirmed"
        );
        assert_eq!(
            b(Some(&st(BillingMode::Subscription, false)), fresh, true),
            BillingMode::Unverified,
            "not authenticated"
        );
        assert_eq!(
            b(
                Some(&st(BillingMode::Subscription, true)),
                Some(Duration::hours(3)),
                true
            ),
            BillingMode::Unverified,
            "stale"
        );
        assert_eq!(b(None, None, true), BillingMode::Unverified, "no status");
        assert_eq!(
            b(Some(&st(BillingMode::Api, true)), fresh, true),
            BillingMode::Api
        );
        assert_eq!(
            b(Some(&st(BillingMode::Unverified, true)), fresh, true),
            BillingMode::Unverified
        );
        let mut data_only = st(BillingMode::Subscription, true);
        data_only.supports = Some(vec!["usage".into()]);
        assert!(!detection_from_status(ok(), Some(&data_only), fresh, true).cli_found);
        assert!(!detection_from_status(Err("x".into()), None, None, true).cli_found);
    }

    #[test]
    fn poll_records_snapshot_and_status() {
        let f = fx();
        let u = usage::latest(&f.conn, f.acct).unwrap().unwrap();
        assert_eq!(
            (u.session_remaining, u.weekly_remaining),
            (Some(95.5), Some(90.0))
        );
        assert_eq!(u.source, "custom_script");
        assert_eq!(u.confidence, Confidence::Provider);
        let a = accounts::get(&f.conn, f.acct).unwrap();
        assert_eq!(a.subscription_plan, "team");
        assert_eq!(
            a.custom_status.unwrap().0.billing_mode,
            BillingMode::Subscription
        );
    }

    #[test]
    fn pull_failures_are_reported_without_script_output() {
        let f = fx();
        let d = f.dir.path();
        let a = accounts::get(&f.conn, f.acct).unwrap();
        let before = usage::history(&f.conn, f.acct, 10).unwrap().len();
        std::fs::write(d.join("usage.json"), "SECRET not json").unwrap();
        let e = poll(&f.conn, &a, &HashMap::new(), Utc::now()).unwrap_err();
        assert!(e.contains("not JSON") && !e.contains("SECRET"), "{e}");
        assert_eq!(
            usage::history(&f.conn, f.acct, 10).unwrap().len(),
            before,
            "nothing recorded"
        );
        std::fs::write(d.join("usage.json"), "x".repeat(2 * 1024 * 1024)).unwrap();
        assert!(poll(&f.conn, &a, &HashMap::new(), Utc::now())
            .unwrap_err()
            .contains("too large"));
        std::fs::remove_file(d.join("status.json")).unwrap();
        let e = poll(&f.conn, &a, &HashMap::new(), Utc::now()).unwrap_err();
        assert!(e.contains("exited with"), "{e}");
    }

    #[test]
    fn pull_timeout() {
        let f = fx();
        let d = f.dir.path().join("slow.sh");
        std::fs::write(&d, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        let c = CustomConfig {
            timeout_secs: 1,
            ..cfg(&d)
        };
        assert!(call_pull(&c, Kind::Status, "A", &HashMap::new())
            .unwrap_err()
            .contains("timed out"));
    }

    #[test]
    fn run_passes_prompt_on_stdin_and_scrubs_env() {
        let f = fx();
        let t = task(&f, "hello world");
        assert_eq!(go(&f, vec![t]), 1);
        let d = f.dir.path();
        assert_eq!(
            std::fs::read_to_string(d.join("last_prompt"))
                .unwrap()
                .trim(),
            "hello world"
        );
        let args = std::fs::read_to_string(d.join("last_args")).unwrap();
        assert!(
            !args.contains("hello"),
            "prompt must not be in argv: {args}"
        );
        assert!(args.starts_with("--profile=work run --task-id="), "{args}");
        assert!(args.contains("--model=m1"));
        let env = std::fs::read_to_string(d.join("last_env")).unwrap();
        assert!(env.contains("PLAN_BURNER_KIND=run") && env.contains("PLAN_BURNER_MODEL=m1"));
        assert_eq!(
            std::fs::read_to_string(d.join("leak")).unwrap().trim(),
            "0",
            "API key scrubbed"
        );
        let res = tasks::get(&f.conn, t).unwrap();
        assert_eq!(res.status, TaskStatus::Completed);
        assert!(std::fs::read_to_string(res.result_path.unwrap())
            .unwrap()
            .contains("result for: hello world"));
    }

    #[test]
    fn run_exit_codes() {
        let f = fx();
        let (lim, next) = (task(&f, "LIMIT"), task(&f, "ok"));
        go(&f, vec![lim, next]);
        assert_eq!(
            tasks::get(&f.conn, lim).unwrap().status,
            TaskStatus::WaitingForCapacity
        );
        assert_eq!(
            tasks::get(&f.conn, next).unwrap().status,
            TaskStatus::Pending,
            "batch stopped"
        );
        assert!(f
            .rx
            .try_iter()
            .any(|e| matches!(e, Event::RateLimited { .. })));
        let bad = task(&f, "FAIL");
        go(&f, vec![bad]);
        assert_eq!(tasks::get(&f.conn, bad).unwrap().status, TaskStatus::Failed);
    }

    #[test]
    fn run_timeout_and_stop() {
        let f = fx();
        let t = tasks::create(
            &f.conn,
            &NewTask {
                name: "s".into(),
                prompt: "SLEEP".into(),
                provider_type: Some("custom".into()),
                timeout_secs: 1,
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
        .id;
        go(&f, vec![t]);
        let s: String = f
            .conn
            .query_row("SELECT status FROM executions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(s, ExecutionStatus::TimedOut.as_str());
        let t2 = task(&f, "SLEEP");
        let ctl = f.rt.controller.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(800));
            ctl.emergency_stop();
        });
        go(&f, vec![t2]);
        h.join().unwrap();
        assert_eq!(
            tasks::get(&f.conn, t2).unwrap().status,
            TaskStatus::Cancelled
        );
    }

    #[test]
    fn status_is_probed_before_every_task_and_api_stops_batch() {
        let f = fx();
        let (a, b) = (task(&f, "one"), task(&f, "two"));
        // Flip billing to "api" as soon as the first run starts.
        let d = f.dir.path().to_path_buf();
        let script = d.join("agent.sh");
        let body = std::fs::read_to_string(&script).unwrap().replace(
            "    P=$(cat)\n",
            "    P=$(cat)\n    echo '{\"version\":1,\"billing_mode\":\"api\",\"authenticated\":true}' > \"$D/status.json\"\n",
        );
        std::fs::write(&script, body).unwrap();
        assert_eq!(go(&f, vec![a, b]), 1);
        assert_eq!(
            tasks::get(&f.conn, a).unwrap().status,
            TaskStatus::Completed
        );
        assert_ne!(
            tasks::get(&f.conn, b).unwrap().status,
            TaskStatus::Completed,
            "second task blocked by fresh probe"
        );
        assert_eq!(
            accounts::get(&f.conn, f.acct).unwrap().billing_mode,
            BillingMode::Api
        );
    }

    #[test]
    fn unconfirmed_or_data_only_never_runs() {
        let f = fx();
        accounts::set_subscription_confirmed(&f.conn, f.acct, false).unwrap();
        let t = task(&f, "x");
        assert_eq!(go(&f, vec![t]), 0);
        assert!(!f.dir.path().join("last_prompt").exists());
        accounts::set_subscription_confirmed(&f.conn, f.acct, true).unwrap();
        std::fs::write(
            f.dir.path().join("status.json"),
            r#"{"version":1,"billing_mode":"subscription","authenticated":true,"supports":["usage","status"]}"#,
        )
        .unwrap();
        assert_eq!(go(&f, vec![t]), 0);
        assert!(
            !f.dir.path().join("last_prompt").exists(),
            "data-only account"
        );
    }

    #[test]
    fn changing_script_resets_confirmation_and_status() {
        let f = fx();
        let a = accounts::get(&f.conn, f.acct).unwrap();
        assert!(a.subscription_confirmed && a.custom_status.is_some());
        let mut c = a.custom.clone().unwrap();
        c.args.push("--other".into());
        let u = accounts::update(
            &f.conn,
            a.id,
            &accounts::AccountUpdate {
                label: a.label.clone(),
                plan: a.subscription_plan.clone(),
                config_dir: None,
                credentials_file: None,
                usage_source: a.usage_source,
                user_agent: None,
                cli_command: None,
                availability_schedule: String::new(),
                custom: Some(c),
            },
        )
        .unwrap();
        assert!(!u.subscription_confirmed);
        assert!(u.custom_status.is_none());
        assert_eq!(u.billing_mode, BillingMode::Unverified);
    }

    #[test]
    fn rate_limit_is_exit_75_only() {
        assert!(CustomAdapter.is_rate_limited("", 75));
        assert!(!CustomAdapter.is_rate_limited("usage limit reached", 1));
    }
}
