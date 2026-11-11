//! Pre-flight checks and the child-process runner (timeout, cancellation, confirmed termination).
use crate::models::{Account, BillingMode, ExecutionPolicy, RunMode};
use crate::provider::CommandSpec;
use crate::settings::Settings;
use crate::tasks::Task;
use crate::usage::{check_capacity, CapacityBlock, CapacityRules, UsageSnapshot};
use chrono::{DateTime, Utc};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Emergency STOP or pause is active.
    AutomationHalted,
    /// Global automation switch is off (only blocks automatic launches).
    AutomationDisabled,
    ProviderUnavailable(String),
    Billing(BillingMode, Vec<String>),
    Capacity(CapacityBlock),
    NeedsApproval,
    WorkingDir(String),
    Expired,
    /// The account is switched off or outside its availability schedule.
    AccountUnavailable(String),
}

impl std::fmt::Display for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Block::AutomationHalted => write!(f, "automation is paused or stopped"),
            Block::AutomationDisabled => write!(f, "automation is disabled"),
            Block::ProviderUnavailable(s) => write!(f, "provider unavailable: {s}"),
            Block::Billing(m, r) => {
                write!(f, "subscription-only billing not verified (mode: {m})")?;
                if !r.is_empty() {
                    write!(f, ": {}", r.join("; "))?;
                }
                // Tasks only ever run on a verified subscription; say how to get there.
                if r.iter().any(|x| x.contains("not confirmed")) {
                    write!(f, ". Tick \"I confirm this account uses a subscription\" on the account")
                } else if *m == BillingMode::Api {
                    write!(f, ". Remove the API-key / cloud-billing settings, then log in with your subscription")
                } else {
                    write!(f, ". Log in to the account with your subscription (not an API key) and confirm it on the account")
                }
            }
            Block::Capacity(c) => write!(f, "{c}"),
            Block::NeedsApproval => write!(
                f,
                "approval required: approve the batch under Approvals (execution policy \"approval\" is the default)"
            ),
            Block::WorkingDir(s) => write!(f, "working directory rejected: {s}"),
            Block::Expired => write!(f, "task expired"),
            Block::AccountUnavailable(s) => write!(f, "account unavailable: {s}"),
        }
    }
}

pub struct Preflight<'a> {
    pub settings: &'a Settings,
    pub account: &'a Account,
    pub task: &'a Task,
    pub snapshot: Option<&'a UsageSnapshot>,
    /// Freshly detected billing mode (not the stored one).
    pub billing: BillingMode,
    pub billing_reasons: Vec<String>,
    pub cli_found: bool,
    pub launches_allowed: bool,
    /// Launch was initiated by the scheduler rather than by the user clicking Run.
    pub automatic: bool,
    pub approved: bool,
    pub now: DateTime<Utc>,
    /// Local weekday/time, for the account availability schedule.
    pub clock: crate::availability::LocalClock,
    /// Percent already consumed since `snapshot` was captured (earlier executions on this account).
    pub consumed_since_snapshot: f64,
}

/// The policy that applies: the more restrictive of the global mode and the account's mode.
pub fn effective_policy(settings: &Settings, account: &Account) -> ExecutionPolicy {
    use ExecutionPolicy::*;
    match (settings.execution_mode, account.execution_policy) {
        (Manual, _) | (_, Manual) => Manual,
        (Approval, _) | (_, Approval) => Approval,
        _ => Automatic,
    }
}

/// The run mode that applies: one-off override > task > account > global default; `Inherit` entries are
/// skipped, and if all are `Inherit` the provider's own default applies.
pub fn effective_run_mode(
    settings: &Settings,
    account: &Account,
    task: &Task,
    run_override: Option<RunMode>,
) -> RunMode {
    [
        run_override.unwrap_or_default(),
        task.run_mode,
        account.run_mode,
        settings.default_run_mode,
    ]
    .into_iter()
    .find(|m| *m != RunMode::Inherit)
    .unwrap_or(RunMode::Inherit)
}

/// Runs every check from PLAN §4 "Before each task". Returns all blocking reasons (empty = go).
pub fn preflight(p: &Preflight) -> Vec<Block> {
    let mut b = Vec::new();
    // 1. global automation state
    if !p.launches_allowed {
        b.push(Block::AutomationHalted);
    }
    if p.automatic && !p.settings.enabled {
        b.push(Block::AutomationDisabled);
    }
    // 1b. account availability (on-demand switch + weekly schedule); applies to manual runs too
    if let Err(why) = p.account.availability(p.clock) {
        b.push(Block::AccountUnavailable(why));
    }
    // 2. provider availability
    if !p.cli_found {
        let detail = p
            .billing_reasons
            .iter()
            .find(|r| r.contains("CLI not found"))
            .cloned()
            .unwrap_or_else(|| "CLI not found".into());
        b.push(Block::ProviderUnavailable(format!(
            "{detail}; install it or set the account's CLI command"
        )));
    }
    // 3. subscription-only authentication (API billing is never an acceptable fallback)
    if p.billing != BillingMode::Subscription {
        b.push(Block::Billing(p.billing, p.billing_reasons.clone()));
    }
    // 4-8. freshness, session, weekly, reserve, estimate (+ consumption since the snapshot)
    // The account's overrides (and its "ignore limits" switch) are applied here, so every caller agrees.
    let eff = p.settings.effective_for(p.account);
    let (session_threshold, session_reserved, check_session) = eff.session_rules(p.account);
    let rules = CapacityRules {
        max_age_minutes: eff.max_usage_age_minutes as i64,
        threshold: eff.minimum_capacity_threshold,
        reserved: eff.reserved_capacity,
        session_threshold,
        session_reserved,
        check_session,
        burn_session_hours: eff.burn_session_hours,
        burn_weekly_hours: eff.burn_weekly_hours,
    };
    let needed = p.task.estimated_consumption + p.consumed_since_snapshot;
    if let Err(c) = check_capacity(p.snapshot, p.now, rules, needed, None) {
        b.push(Block::Capacity(c));
    }
    // 9. maximum budget
    if p.task
        .maximum_budget
        .is_some_and(|m| p.task.estimated_consumption > m)
    {
        b.push(Block::Capacity(CapacityBlock::ExceedsBudget));
    }
    if p.task.expiration_at.is_some_and(|e| e <= p.now) {
        b.push(Block::Expired);
    }
    // 10. approval
    let policy = effective_policy(p.settings, p.account);
    let auto_ok = p.automatic && policy == ExecutionPolicy::Automatic && !p.task.requires_approval;
    if !p.approved && !auto_ok {
        b.push(Block::NeedsApproval);
    }
    if p.automatic && policy == ExecutionPolicy::Manual {
        b.push(Block::NeedsApproval);
    }
    b
}

/// Resolves a task working directory and ensures it stays inside the configured working root.
pub fn resolve_workdir(root: &str, dir: &str) -> std::result::Result<PathBuf, String> {
    if root.is_empty() {
        return Err("no working root is set: choose one under Settings > Working root".into());
    }
    let configured = root;
    let root = Path::new(root)
        .canonicalize()
        .map_err(|e| format!("Settings > Working root \"{configured}\" cannot be used: {e}"))?;
    let wanted = if dir.is_empty() {
        root.clone()
    } else {
        let d = Path::new(dir);
        let joined = if d.is_absolute() {
            d.to_path_buf()
        } else {
            root.join(d)
        };
        joined
            .canonicalize()
            .map_err(|e| format!("the task's working dir \"{dir}\" cannot be used: {e}"))?
    };
    if !wanted.starts_with(&root) {
        return Err(format!(
            "{} is outside the working root {} (Settings > Working root)",
            wanted.display(),
            root.display()
        ));
    }
    if !wanted.is_dir() {
        return Err(format!("{} is not a directory", wanted.display()));
    }
    Ok(wanted)
}

/// File-name-safe slug.
pub fn sanitize_filename(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect();
    if t.is_empty() {
        "task".into()
    } else {
        t
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    Exited(i32),
    TimedOut,
    Cancelled,
    SpawnFailed(String),
    /// Waiting on the child failed (`try_wait` error); the child and its group were killed.
    WaitFailed(String),
}

#[derive(Debug, Clone)]
pub struct ProcessOutcome {
    pub reason: EndReason,
    pub stdout: String,
    pub stderr: String,
    /// True only when the OS confirmed the child was reaped. Must be true before reporting a successful stop.
    pub terminated_confirmed: bool,
    pub duration: Duration,
}

const OUTPUT_CAP: u64 = 16 * 1024 * 1024;

fn read_capped<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut r).take(OUTPUT_CAP).read_to_end(&mut buf);
        // Drain the remainder so the child never blocks on a full pipe.
        let _ = std::io::copy(&mut r, &mut std::io::sink());
        String::from_utf8_lossy(&buf).into_owned()
    })
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: i32) {
    // SAFETY: plain kill(2) on the child's own process group (created with process_group(0)).
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

/// Runs the process with structured args, optional timeout and cancellation.
/// Termination is graceful (SIGTERM) then forced (SIGKILL), and is confirmed via wait().
pub fn run_process(
    spec: &CommandSpec,
    timeout: Duration,
    cancel: &Arc<AtomicBool>,
) -> ProcessOutcome {
    let start = Instant::now();
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ProcessOutcome {
                reason: EndReason::SpawnFailed(e.to_string()),
                stdout: String::new(),
                stderr: String::new(),
                terminated_confirmed: true,
                duration: start.elapsed(),
            }
        }
    };
    let pid = child.id();
    let stdin_data = spec.stdin.clone();
    let mut stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(s) = stdin.as_mut() {
            let _ = s.write_all(stdin_data.as_bytes());
        }
        drop(stdin);
    });
    let out_h = read_capped(child.stdout.take().unwrap());
    let err_h = read_capped(child.stderr.take().unwrap());

    let reason: EndReason;
    let mut confirmed = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                reason = EndReason::Exited(status.code().unwrap_or(-1));
                confirmed = true;
                // Background descendants must not outlive the task and keep using capacity (or
                // holding the output pipes open): the group is killed once its leader has exited.
                #[cfg(unix)]
                signal_group(pid, libc::SIGKILL);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                // State unknown: don't leave the process running unsupervised.
                reason = EndReason::WaitFailed(e.to_string());
                #[cfg(unix)]
                signal_group(pid, libc::SIGKILL);
                let _ = child.kill();
                for _ in 0..50 {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        confirmed = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                break;
            }
        }
        let cancelled = cancel.load(Ordering::SeqCst);
        if cancelled || start.elapsed() >= timeout {
            reason = if cancelled {
                EndReason::Cancelled
            } else {
                EndReason::TimedOut
            };
            #[cfg(unix)]
            signal_group(pid, libc::SIGTERM);
            let grace = Instant::now();
            loop {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    confirmed = true;
                    break;
                }
                if grace.elapsed() > Duration::from_secs(3) {
                    #[cfg(unix)]
                    signal_group(pid, libc::SIGKILL);
                    let _ = child.kill();
                    // confirm reaping
                    for _ in 0..50 {
                        if matches!(child.try_wait(), Ok(Some(_))) {
                            confirmed = true;
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = writer.join();
    // Pipes close when the child (and its group) exit; if descendants keep them open, don't hang forever.
    let stdout = join_with_deadline(out_h);
    let stderr = join_with_deadline(err_h);
    ProcessOutcome {
        reason,
        stdout,
        stderr,
        terminated_confirmed: confirmed,
        duration: start.elapsed(),
    }
}

fn join_with_deadline(h: std::thread::JoinHandle<String>) -> String {
    let t = Instant::now();
    while !h.is_finished() {
        if t.elapsed() > Duration::from_secs(5) {
            return String::new();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    h.join().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::*;
    use crate::tasks::NewTask;
    use chrono::Duration as D;

    fn account(policy: ExecutionPolicy) -> Account {
        Account {
            id: 1,
            provider_id: 1,
            provider_type: "claude".into(),
            label: "A".into(),
            subscription_plan: "".into(),
            billing_mode: BillingMode::Subscription,
            connection_status: ConnectionStatus::Connected,
            config_dir: None,
            subscription_confirmed: true,
            execution_policy: policy,
            credentials_file: None,
            usage_source: UsageSource::Manual,
            user_agent: None,
            custom: None,
            custom_status: None,
            cli_command: None,
            available: true,
            availability_schedule: String::new(),
            run_mode: RunMode::Inherit,
            limits: Default::default(),
        }
    }
    fn task(est: f64) -> Task {
        let db = crate::db::open_memory().unwrap();
        crate::tasks::create(
            &db,
            &NewTask {
                name: "t".into(),
                prompt: "p".into(),
                estimated_consumption: est,
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap()
    }
    fn snap(now: DateTime<Utc>) -> UsageSnapshot {
        UsageSnapshot {
            id: 1,
            account_id: 1,
            session_remaining: Some(90.0),
            weekly_remaining: Some(90.0),
            session_reset_at: Some(now + D::hours(2)),
            weekly_reset_at: Some(now + D::days(2)),
            confidence: Confidence::Manual,
            source: "manual".into(),
            captured_at: now - D::minutes(2),
        }
    }
    struct Fx {
        settings: Settings,
        account: Account,
        task: Task,
        snap: Option<UsageSnapshot>,
        now: DateTime<Utc>,
    }
    fn fx() -> Fx {
        let now = Utc::now();
        Fx {
            settings: Settings {
                enabled: true,
                execution_mode: ExecutionPolicy::Automatic,
                ..Settings::default()
            },
            account: account(ExecutionPolicy::Automatic),
            task: Task {
                requires_approval: false,
                ..task(5.0)
            },
            snap: Some(snap(now)),
            now,
        }
    }
    fn pf(f: &Fx) -> Preflight<'_> {
        Preflight {
            settings: &f.settings,
            account: &f.account,
            task: &f.task,
            snapshot: f.snap.as_ref(),
            billing: BillingMode::Subscription,
            billing_reasons: vec![],
            cli_found: true,
            launches_allowed: true,
            automatic: true,
            approved: false,
            now: f.now,
            clock: crate::availability::LocalClock::default(),
            consumed_since_snapshot: 0.0,
        }
    }

    #[test]
    fn run_mode_precedence() {
        use RunMode::*;
        let mut f = fx();
        let m = |f: &Fx, o| effective_run_mode(&f.settings, &f.account, &f.task, o);
        assert_eq!(m(&f, None), Inherit);
        f.settings.default_run_mode = Plan;
        assert_eq!(m(&f, None), Plan);
        f.account.run_mode = Review;
        assert_eq!(m(&f, None), Review);
        f.task.run_mode = AutoEdit;
        assert_eq!(m(&f, None), AutoEdit);
        assert_eq!(m(&f, Some(Plan)), Plan);
        assert_eq!(m(&f, Some(Inherit)), AutoEdit, "Inherit override defers");
    }

    #[test]
    fn unavailable_account_blocks_even_manual_and_approved_runs() {
        let mut f = fx();
        f.account.available = false;
        for (automatic, approved) in [(true, true), (false, true), (false, false)] {
            let mut p = pf(&f);
            p.automatic = automatic;
            p.approved = approved;
            assert!(
                preflight(&p)
                    .iter()
                    .any(|b| matches!(b, Block::AccountUnavailable(_))),
                "automatic={automatic} approved={approved}"
            );
        }
        f.account.available = true;
        assert!(!preflight(&pf(&f))
            .iter()
            .any(|b| matches!(b, Block::AccountUnavailable(_))));
    }

    #[test]
    fn schedule_blocks_outside_window_only() {
        let mut f = fx();
        f.account.availability_schedule = "mon-fri 09:00-17:00".into();
        let mut p = pf(&f);
        p.clock = crate::availability::LocalClock::new(5, 10 * 60); // Saturday
        assert!(preflight(&p)
            .iter()
            .any(|b| matches!(b, Block::AccountUnavailable(_))));
        p.clock = crate::availability::LocalClock::new(2, 10 * 60); // Wednesday
        assert!(!preflight(&p)
            .iter()
            .any(|b| matches!(b, Block::AccountUnavailable(_))));
    }

    #[test]
    fn block_messages_say_how_to_fix_them() {
        let mut f = fx();
        f.account.subscription_confirmed = false;
        let mut p = pf(&f);
        p.cli_found = false;
        p.billing = BillingMode::Unverified;
        let br = vec![
            "`/usr/bin/claude` CLI not found".to_string(),
            "you have not confirmed this account uses a subscription login".to_string(),
        ];
        p.billing_reasons = br;
        let msgs: Vec<String> = preflight(&p).iter().map(|b| b.to_string()).collect();
        let all = msgs.join(" | ");
        assert!(
            all.contains("/usr/bin/claude") && all.contains("CLI command"),
            "{all}"
        );
        assert!(all.contains("Tick \"I confirm"), "{all}");
        assert!(Block::NeedsApproval.to_string().contains("Approvals"));
        let api = Block::Billing(BillingMode::Api, vec!["API key set".into()]).to_string();
        assert!(api.contains("Remove the API-key"), "{api}");
    }

    #[test]
    fn all_clear_in_automatic_mode() {
        let f = fx();
        assert!(preflight(&pf(&f)).is_empty());
    }
    #[test]
    fn account_limits_override_general_settings() {
        let mut f = fx();
        f.snap.as_mut().unwrap().weekly_remaining = Some(50.0);
        assert!(preflight(&pf(&f))
            .iter()
            .any(|b| matches!(b, Block::Capacity(CapacityBlock::BelowThreshold { .. }))));
        f.account.limits.start_threshold = Some(40.0);
        f.account.limits.session_threshold = Some(40.0);
        f.account.limits.reserved_capacity = Some(10.0);
        assert!(preflight(&pf(&f)).is_empty());
        // 5 h window: its own rule, and it can be ignored entirely
        f.snap.as_mut().unwrap().session_remaining = Some(30.0);
        assert!(preflight(&pf(&f)).iter().any(|b| matches!(
            b,
            Block::Capacity(CapacityBlock::BelowThreshold {
                which: "5 h session",
                ..
            })
        )));
        f.account.limits.session_threshold = Some(20.0);
        f.account.limits.session_reserved = Some(5.0);
        assert!(preflight(&pf(&f)).is_empty());
        f.account.limits.ignore_session = true;
        f.snap.as_mut().unwrap().session_remaining = None;
        assert!(preflight(&pf(&f)).is_empty());
    }

    #[test]
    fn burn_before_reset_global_and_account_override() {
        let mut f = fx();
        // 30% left in both windows: below the threshold; session resets in 2 h, weekly in 2 days
        f.snap.as_mut().unwrap().session_remaining = Some(30.0);
        f.snap.as_mut().unwrap().weekly_remaining = Some(30.0);
        assert!(!preflight(&pf(&f)).is_empty());
        // global burn windows cover both resets
        f.settings.burn_session_hours = Some(3.0);
        f.settings.burn_weekly_hours = Some(72.0);
        assert!(preflight(&pf(&f)).is_empty());
        // an account can switch it off again (Some(0) = off)...
        f.account.limits.burn_weekly_hours = Some(0.0);
        assert!(preflight(&pf(&f)).iter().any(|b| matches!(
            b,
            Block::Capacity(CapacityBlock::BelowThreshold {
                which: "weekly",
                ..
            })
        )));
        // ...and stale usage still blocks while burning
        f.account.limits.burn_weekly_hours = None;
        f.snap.as_mut().unwrap().captured_at = f.now - D::hours(3);
        assert!(preflight(&pf(&f))
            .iter()
            .any(|b| matches!(b, Block::Capacity(CapacityBlock::Stale { .. }))));
    }

    #[test]
    fn ignore_limits_never_bypasses_safety() {
        let mut f = fx();
        f.account.limits.ignore_limits = true;
        f.snap.as_mut().unwrap().session_remaining = Some(8.0);
        f.snap.as_mut().unwrap().weekly_remaining = Some(8.0);
        assert!(
            preflight(&pf(&f)).is_empty(),
            "thresholds and reserve are ignored"
        );
        // ...but not the real remaining capacity,
        f.task.estimated_consumption = 9.0;
        assert!(preflight(&pf(&f))
            .iter()
            .any(|b| matches!(b, Block::Capacity(CapacityBlock::Insufficient { .. }))));
        f.task.estimated_consumption = 5.0;
        // staleness,
        f.snap.as_mut().unwrap().captured_at = f.now - D::hours(3);
        assert!(preflight(&pf(&f))
            .iter()
            .any(|b| matches!(b, Block::Capacity(CapacityBlock::Stale { .. }))));
        f.snap = None;
        assert!(preflight(&pf(&f)).contains(&Block::Capacity(CapacityBlock::NoUsageData)));
        // billing,
        let f2 = fx();
        let mut f2 = Fx {
            account: Account {
                limits: crate::models::AccountLimits {
                    ignore_limits: true,
                    ..Default::default()
                },
                ..f2.account.clone()
            },
            ..f2
        };
        let mut p = pf(&f2);
        p.billing = BillingMode::Api;
        assert!(preflight(&p)
            .iter()
            .any(|b| matches!(b, Block::Billing(..))));
        // availability and halted state.
        f2.account.available = false;
        assert!(preflight(&pf(&f2))
            .iter()
            .any(|b| matches!(b, Block::AccountUnavailable(_))));
        f2.account.available = true;
        let mut p = pf(&f2);
        p.launches_allowed = false;
        assert!(preflight(&p).contains(&Block::AutomationHalted));
    }

    #[test]
    fn api_billing_blocks_even_when_approved() {
        let f = fx();
        let mut p = pf(&f);
        p.billing = BillingMode::Api;
        p.approved = true;
        assert!(preflight(&p)
            .iter()
            .any(|b| matches!(b, Block::Billing(BillingMode::Api, _))));
    }
    #[test]
    fn unverified_billing_blocks() {
        let f = fx();
        let mut p = pf(&f);
        p.billing = BillingMode::Unverified;
        assert!(!preflight(&p).is_empty());
    }
    #[test]
    fn stale_or_missing_usage_blocks() {
        let mut f = fx();
        f.snap = None;
        assert!(preflight(&pf(&f)).contains(&Block::Capacity(CapacityBlock::NoUsageData)));
        let mut f = fx();
        f.snap.as_mut().unwrap().captured_at = f.now - D::hours(3);
        assert!(preflight(&pf(&f))
            .iter()
            .any(|b| matches!(b, Block::Capacity(CapacityBlock::Stale { .. }))));
    }
    #[test]
    fn approval_required_by_default_policy() {
        let mut f = fx();
        f.settings.execution_mode = ExecutionPolicy::Approval;
        assert!(preflight(&pf(&f)).contains(&Block::NeedsApproval));
        let mut p = pf(&f);
        p.approved = true;
        assert!(preflight(&p).is_empty());
    }
    #[test]
    fn task_flag_requires_approval_even_in_automatic() {
        let mut f = fx();
        f.task.requires_approval = true;
        assert!(preflight(&pf(&f)).contains(&Block::NeedsApproval));
    }
    #[test]
    fn stricter_account_policy_wins() {
        let mut f = fx();
        f.account.execution_policy = ExecutionPolicy::Approval;
        assert!(preflight(&pf(&f)).contains(&Block::NeedsApproval));
    }
    #[test]
    fn disabled_blocks_automatic_but_not_manual_approved() {
        let mut f = fx();
        f.settings.enabled = false;
        assert!(preflight(&pf(&f)).contains(&Block::AutomationDisabled));
        let mut p = pf(&f);
        p.automatic = false;
        p.approved = true;
        assert!(preflight(&p).is_empty());
    }
    #[test]
    fn halted_blocks_everything() {
        let f = fx();
        let mut p = pf(&f);
        p.launches_allowed = false;
        p.approved = true;
        assert!(preflight(&p).contains(&Block::AutomationHalted));
    }
    #[test]
    fn consumption_since_snapshot_counts() {
        let f = fx();
        let mut p = pf(&f);
        p.consumed_since_snapshot = 80.0;
        assert!(preflight(&p)
            .iter()
            .any(|b| matches!(b, Block::Capacity(CapacityBlock::Insufficient { .. }))));
    }
    #[test]
    fn budget_exceeded_blocks() {
        let mut f = fx();
        f.task.maximum_budget = Some(2.0);
        assert!(preflight(&pf(&f)).contains(&Block::Capacity(CapacityBlock::ExceedsBudget)));
    }

    #[test]
    fn workdir_restricted_to_root() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let root = d.path().to_str().unwrap();
        assert!(resolve_workdir(root, "sub").is_ok());
        assert!(resolve_workdir(root, "").is_ok());
        assert!(resolve_workdir(root, "..").is_err());
        assert!(resolve_workdir(root, "/etc").is_err());
        assert!(resolve_workdir("", "sub").is_err());
    }
    #[test]
    fn filenames_sanitized() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize_filename(""), "task");
    }

    #[cfg(unix)]
    fn sh(script: &str, stdin: &str) -> CommandSpec {
        CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: Default::default(),
            cwd: std::env::temp_dir(),
            stdin: stdin.into(),
        }
    }
    #[cfg(unix)]
    #[test]
    fn process_captures_output_and_stdin() {
        let c = Arc::new(AtomicBool::new(false));
        let o = run_process(
            &sh("cat; echo err >&2", "hello"),
            Duration::from_secs(10),
            &c,
        );
        assert_eq!(o.reason, EndReason::Exited(0));
        assert_eq!(o.stdout, "hello");
        assert_eq!(o.stderr.trim(), "err");
        assert!(o.terminated_confirmed);
    }
    #[cfg(unix)]
    #[test]
    fn process_timeout_kills_and_confirms() {
        let c = Arc::new(AtomicBool::new(false));
        let o = run_process(&sh("sleep 30", ""), Duration::from_millis(300), &c);
        assert_eq!(o.reason, EndReason::TimedOut);
        assert!(o.terminated_confirmed);
        assert!(o.duration < Duration::from_secs(10));
    }
    #[cfg(unix)]
    #[test]
    fn process_cancellation_kills_and_confirms() {
        let c = Arc::new(AtomicBool::new(false));
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            c2.store(true, Ordering::SeqCst);
        });
        let o = run_process(&sh("sleep 30", ""), Duration::from_secs(60), &c);
        assert_eq!(o.reason, EndReason::Cancelled);
        assert!(o.terminated_confirmed);
    }
    #[cfg(unix)]
    fn pid_alive(pid: i32) -> bool {
        // A zombie awaiting an init that doesn't reap counts as dead.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s
                .rsplit(')')
                .next()
                .is_some_and(|r| !r.trim_start().starts_with('Z')),
            Err(_) => false,
        }
    }
    #[cfg(unix)]
    #[test]
    fn leaked_descendants_are_killed_after_normal_exit() {
        let c = Arc::new(AtomicBool::new(false));
        // The background sleep inherits stdout/stderr and outlives its parent shell.
        let o = run_process(
            &sh("sleep 60 & echo $!; exit 0", ""),
            Duration::from_secs(30),
            &c,
        );
        assert_eq!(o.reason, EndReason::Exited(0));
        assert!(o.duration < Duration::from_secs(4), "{:?}", o.duration);
        let pid: i32 = o.stdout.trim().parse().expect("pid of background job");
        std::thread::sleep(Duration::from_millis(200));
        assert!(!pid_alive(pid), "descendant {pid} survived");
    }
    #[cfg(unix)]
    #[test]
    fn spawn_failure_is_reported() {
        let mut s = sh("true", "");
        s.program = "/nonexistent/claude".into();
        let o = run_process(
            &s,
            Duration::from_secs(1),
            &Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(o.reason, EndReason::SpawnFailed(_)));
    }
}
