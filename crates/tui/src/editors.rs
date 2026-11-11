//! Maps the core form models (`core::forms`, `Settings`) to and from the generic [`Form`].
//! Only layout and text parsing live here; every rule is applied by `core` when saving.
use crate::form::{Field, Form};
use plan_burner_core::forms::{AccountForm, TaskForm, UsageForm};
use plan_burner_core::import::ImportFormat;
use plan_burner_core::models::{Account, AccountLimits, ExecutionPolicy, RunMode};
use plan_burner_core::settings::Settings;
use std::str::FromStr;

type R<T> = Result<T, String>;

/// The label to name in a message: the field's own (what the user sees), else `fallback`.
fn shown<'a>(f: &'a Form, key: &str, fallback: &'a str) -> &'a str {
    f.label(key).unwrap_or(fallback)
}

fn num<T: FromStr>(f: &Form, key: &str, label: &str) -> R<T> {
    f.text(key)
        .trim()
        .parse()
        .map_err(|_| format!("{} must be a number", shown(f, key, label)))
}

fn opt_num<T: FromStr>(f: &Form, key: &str, label: &str) -> R<Option<T>> {
    match f.opt(key) {
        None => Ok(None),
        Some(t) => t
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| format!("{} must be a number", shown(f, key, label))),
    }
}

/// A plain text field where blank = `None`.
fn blank_num<T: FromStr>(f: &Form, key: &str, label: &str) -> R<Option<T>> {
    let t = f.text(key);
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse()
        .map(Some)
        .map_err(|_| format!("{} must be a number", shown(f, key, label)))
}

fn n<T: ToString>(v: T) -> String {
    v.to_string()
}

fn run_mode_labels() -> Vec<String> {
    RunMode::ALL.iter().map(|m| m.label().to_string()).collect()
}

fn run_mode_idx(m: RunMode) -> usize {
    RunMode::ALL.iter().position(|x| *x == m).unwrap_or(0)
}

// ---------------------------------------------------------------- settings

const APPROVAL: [&str; 2] = ["always", "once_per_batch"];

pub fn settings_form(s: &Settings) -> Form {
    let policies: Vec<String> = ExecutionPolicy::ALL
        .iter()
        .map(|p| p.as_str().to_string())
        .collect();
    let pol = ExecutionPolicy::ALL
        .iter()
        .position(|p| *p == s.execution_mode)
        .unwrap_or(0);
    Form::new(
        "Settings  (Ctrl-S save, Esc discard)",
        vec![
            Field::check("enabled", "Automation enabled", s.enabled),
            Field::choice("execution_mode", "Execution policy", policies, pol),
            Field::choice(
                "approval_policy",
                "Approval",
                APPROVAL.iter().map(|a| a.to_string()).collect(),
                APPROVAL
                    .iter()
                    .position(|a| *a == s.approval_policy)
                    .unwrap_or(0),
            ),
            Field::text(
                "threshold",
                "Start when capacity above (%)",
                &n(s.minimum_capacity_threshold),
            ),
            Field::text("reserved", "Reserved capacity (%)", &n(s.reserved_capacity)),
            Field::text(
                "max_tasks",
                "Max tasks per batch",
                &n(s.maximum_tasks_per_batch),
            ),
            Field::text(
                "max_minutes",
                "Max execution minutes per batch",
                &n(s.maximum_execution_minutes),
            ),
            Field::text(
                "idle",
                "Idle requirement (min, 0 = off)",
                &n(s.idle_requirement),
            ),
            Field::text("hours", "Allowed hours (HH:MM-HH:MM)", &s.allowed_hours),
            Field::text(
                "burn_session",
                "Burn 5 h window when it resets within (h, blank = off)",
                &s.burn_session_hours.map(n).unwrap_or_default(),
            ),
            Field::text(
                "burn_weekly",
                "Burn weekly window when it resets within (h, blank = off)",
                &s.burn_weekly_hours.map(n).unwrap_or_default(),
            ),
            Field::choice(
                "run_mode",
                "Default run mode",
                run_mode_labels(),
                run_mode_idx(s.default_run_mode),
            ),
            Field::text(
                "max_age",
                "Max usage age (min)",
                &n(s.max_usage_age_minutes),
            ),
            Field::text(
                "retention",
                "Keep history (days, 0 = forever)",
                &n(s.retention_days),
            ),
            Field::text(
                "interval_normal",
                "Check interval, normal (min)",
                &n(s.interval_normal_minutes),
            ),
            Field::text(
                "interval_waiting",
                "Check interval, waiting (min)",
                &n(s.interval_waiting_minutes),
            ),
            Field::text(
                "working_root",
                "Allowed working directory root",
                &s.working_root,
            ),
            Field::text("results_dir", "Results folder", &s.results_dir),
            Field::text("quiet_hours", "Quiet hours (HH:MM-HH:MM)", &s.quiet_hours),
            Field::check("notify_desktop", "Desktop notifications", s.notify_desktop),
            Field::check(
                "notify_telegram",
                "Telegram notifications (token from env)",
                s.notify_telegram,
            ),
            Field::text("telegram_chat", "Telegram chat id", &s.telegram_chat_id),
            Field::check(
                "notify_names",
                "Include task names externally",
                s.notify_include_names,
            ),
        ],
    )
}

pub fn read_settings(f: &Form, base: &Settings) -> R<Settings> {
    let mut s = base.clone();
    s.enabled = f.checked("enabled");
    s.execution_mode = ExecutionPolicy::ALL[f.choice_idx("execution_mode")];
    s.approval_policy = APPROVAL[f.choice_idx("approval_policy")].to_string();
    s.minimum_capacity_threshold = num(f, "threshold", "Start threshold")?;
    s.reserved_capacity = num(f, "reserved", "Reserved capacity")?;
    s.maximum_tasks_per_batch = num(f, "max_tasks", "Max tasks per batch")?;
    s.maximum_execution_minutes = num(f, "max_minutes", "Max execution minutes")?;
    s.idle_requirement = num(f, "idle", "Idle requirement")?;
    s.allowed_hours = f.text("hours").trim().to_string();
    s.burn_session_hours = blank_num(f, "burn_session", "Burn 5 h window")?;
    s.burn_weekly_hours = blank_num(f, "burn_weekly", "Burn weekly window")?;
    s.default_run_mode = RunMode::ALL[f.choice_idx("run_mode")];
    s.max_usage_age_minutes = num(f, "max_age", "Max usage age")?;
    s.retention_days = num(f, "retention", "Keep history")?;
    s.interval_normal_minutes = num(f, "interval_normal", "Normal interval")?;
    s.interval_waiting_minutes = num(f, "interval_waiting", "Waiting interval")?;
    s.working_root = f.text("working_root").trim().to_string();
    s.results_dir = f.text("results_dir").trim().to_string();
    s.quiet_hours = f.text("quiet_hours").trim().to_string();
    s.notify_desktop = f.checked("notify_desktop");
    s.notify_telegram = f.checked("notify_telegram");
    s.telegram_chat_id = f.text("telegram_chat").trim().to_string();
    s.notify_include_names = f.checked("notify_names");
    s.validate().map_err(|e| e.to_string())?;
    s.validate_working_root().map_err(|e| e.to_string())?;
    Ok(s)
}

// ---------------------------------------------------------------- account

pub fn provider_types() -> Vec<String> {
    plan_burner_core::provider::registry()
        .iter()
        .map(|a| a.provider_type().to_string())
        .collect()
}

pub fn account_form(a: &AccountForm, general: &Settings) -> Form {
    let providers = provider_types();
    let pidx = providers.iter().position(|p| *p == a.provider).unwrap_or(0);
    let l = &a.limits;
    let g = general;
    let title = if a.id.is_some() {
        "Edit account  (Ctrl-S save, Esc cancel)"
    } else {
        "Add account  (Ctrl-S save, Esc cancel, Ctrl-D discover logins)"
    };
    let mut f = Form::new(
        title,
        vec![
            Field::heading("h_identity", "Identity"),
            {
                let fld = Field::choice("provider", "Provider", providers, pidx);
                if a.id.is_some() {
                    fld.disabled()
                } else {
                    fld
                }
            },
            Field::text("label", "Label", &a.label),
            Field::text("plan", "Plan (blank = from login)", &a.plan),
            Field::heading("h_connection", "Connection"),
            Field::text(
                "config_dir",
                "Profile dir (CLAUDE_CONFIG_DIR)",
                &a.config_dir,
            ),
            Field::text(
                "cli_command",
                "CLI command (default: claude)",
                &a.cli_command,
            ),
            Field::text(
                "credentials_file",
                "Credentials file (optional)",
                &a.credentials_file,
            ),
            Field::heading("h_usage", "Usage tracking"),
            Field::check("auto_usage", "Automatic usage", a.auto_usage),
            Field::text("user_agent", "User-Agent override", &a.user_agent),
            Field::text("script", "Script (absolute path)", &a.script),
            Field::multi(
                "script_args",
                "Fixed arguments (one per line)",
                &a.script_args,
                3,
            ),
            Field::text("script_timeout", "Pull timeout (s)", &n(a.script_timeout)),
            Field::heading("h_availability", "Availability"),
            Field::text(
                "schedule",
                "Availability schedule (mon-fri 09:00-17:00; …)",
                &a.availability_schedule,
            ),
            Field::heading("h_limits", "Eligibility limits"),
            Field::check(
                "ignore_limits",
                "Eligible ignoring limits (threshold, reserve, idle, hours)",
                l.ignore_limits,
            ),
            Field::opt(
                "lim_start",
                "Start when capacity above (%)",
                l.start_threshold.map(n).as_deref(),
                &n(g.minimum_capacity_threshold),
            ),
            Field::opt(
                "lim_reserved",
                "Reserved capacity (%)",
                l.reserved_capacity.map(n).as_deref(),
                &n(g.reserved_capacity),
            ),
            Field::opt(
                "lim_idle",
                "Run on idle (min, 0 = any time)",
                l.idle_requirement.map(n).as_deref(),
                &n(g.idle_requirement),
            ),
            Field::opt(
                "lim_hours",
                "Allowed hours",
                l.allowed_hours.as_deref(),
                &g.allowed_hours,
            ),
            Field::opt(
                "lim_age",
                "Max usage age (min)",
                l.max_usage_age_minutes.map(n).as_deref(),
                &n(g.max_usage_age_minutes),
            ),
            Field::opt(
                "lim_batch",
                "Max tasks per batch",
                l.max_tasks_per_batch.map(n).as_deref(),
                &n(g.maximum_tasks_per_batch),
            ),
            Field::opt(
                "lim_minutes",
                "Max batch minutes",
                l.max_execution_minutes.map(n).as_deref(),
                &n(g.maximum_execution_minutes),
            ),
            Field::check(
                "ignore_session",
                "Ignore the 5 h window (decide on weekly only)",
                l.ignore_session,
            ),
            Field::opt(
                "lim_sess_thr",
                "5 h: start when above (%)",
                l.session_threshold.map(n).as_deref(),
                &n(l.start_threshold.unwrap_or(g.minimum_capacity_threshold)),
            ),
            Field::opt(
                "lim_sess_res",
                "5 h: reserved capacity (%)",
                l.session_reserved.map(n).as_deref(),
                &n(l.reserved_capacity.unwrap_or(g.reserved_capacity)),
            ),
            Field::opt(
                "lim_burn_sess",
                "Burn 5 h window within (h before reset, 0 = off)",
                l.burn_session_hours.map(n).as_deref(),
                &g.burn_session_hours.map(n).unwrap_or_else(|| "off".into()),
            ),
            Field::opt(
                "lim_burn_week",
                "Burn weekly window within (h before reset, 0 = off)",
                l.burn_weekly_hours.map(n).as_deref(),
                &g.burn_weekly_hours.map(n).unwrap_or_else(|| "off".into()),
            ),
        ],
    );
    account_visibility(&mut f);
    f
}

/// Marks the fields that differ from the values the form was opened with and shows the count in the
/// title (the same `AccountForm::changed_fields` the GUI uses). Keeps the last marks when the form
/// does not parse yet.
pub fn account_marks(f: &mut Form, base: &AccountForm) {
    let Ok(cur) = read_account(f, base) else {
        return;
    };
    let changed = cur.changed_fields();
    for fld in &mut f.fields {
        fld.marked = changed.contains(&fld.key);
    }
    let head = f.title.split("  ●").next().unwrap_or("").to_string();
    f.title = if changed.is_empty() {
        head
    } else {
        format!("{head}  ● {} unsaved", changed.len())
    };
}

/// Shows the fields that apply to the chosen provider (call after every change).
pub fn account_visibility(f: &mut Form) {
    let custom = provider_types()
        .get(f.choice_idx("provider"))
        .is_some_and(|p| p == "custom");
    for k in [
        "config_dir",
        "cli_command",
        "credentials_file",
        "user_agent",
    ] {
        f.set_visible(k, !custom);
    }
    for k in ["script", "script_args", "script_timeout"] {
        f.set_visible(k, custom);
    }
    for k in [
        "ignore_session",
        "lim_sess_thr",
        "lim_sess_res",
        "lim_burn_sess",
    ] {
        f.set_visible(k, !custom);
    }
    if let Some(fld) = f.field_mut("auto_usage") {
        fld.label = if custom {
            "Usage from script (call `usage` periodically)".into()
        } else {
            "Automatic usage (login token, unofficial endpoint)".into()
        };
    }
}

pub fn read_account(f: &Form, base: &AccountForm) -> R<AccountForm> {
    let mut a = base.clone();
    a.provider = provider_types()[f.choice_idx("provider")].clone();
    a.label = f.text("label");
    a.plan = f.text("plan");
    a.config_dir = f.text("config_dir");
    a.cli_command = f.text("cli_command");
    a.credentials_file = f.text("credentials_file");
    a.auto_usage = f.checked("auto_usage");
    a.user_agent = f.text("user_agent");
    a.script = f.text("script");
    a.script_args = f.text("script_args");
    a.script_timeout = num(f, "script_timeout", "Pull timeout")?;
    a.availability_schedule = f.text("schedule");
    a.limits = AccountLimits {
        start_threshold: opt_num(f, "lim_start", "Start threshold")?,
        reserved_capacity: opt_num(f, "lim_reserved", "Reserved capacity")?,
        idle_requirement: opt_num(f, "lim_idle", "Run on idle")?,
        allowed_hours: f.opt("lim_hours"),
        max_usage_age_minutes: opt_num(f, "lim_age", "Max usage age")?,
        max_tasks_per_batch: opt_num(f, "lim_batch", "Max tasks per batch")?,
        max_execution_minutes: opt_num(f, "lim_minutes", "Max batch minutes")?,
        session_threshold: opt_num(f, "lim_sess_thr", "5 h start threshold")?,
        session_reserved: opt_num(f, "lim_sess_res", "5 h reserved capacity")?,
        ignore_session: f.checked("ignore_session"),
        ignore_limits: f.checked("ignore_limits"),
        burn_session_hours: opt_num(f, "lim_burn_sess", "Burn 5 h window")?,
        burn_weekly_hours: opt_num(f, "lim_burn_week", "Burn weekly window")?,
    };
    Ok(a)
}

// ------------------------------------------------------------------- task

pub fn task_form(
    t: &TaskForm,
    accounts: &[Account],
    templates: &[plan_burner_core::templates::Template],
) -> Form {
    let providers = provider_types();
    let pidx =
        t.t.provider_type
            .as_ref()
            .and_then(|p| providers.iter().position(|x| x == p))
            .unwrap_or(0);
    let mut accts = vec!["Any account".to_string()];
    accts.extend(accounts.iter().map(|a| a.display()));
    let aidx = t
        .pinned
        .and_then(|id| accounts.iter().position(|a| a.id == id))
        .map_or(0, |i| i + 1);
    Form::new(
        if t.id.is_some() {
            "Edit task  (Ctrl-S save, Esc cancel)"
        } else {
            "New task  (Ctrl-S save, Esc cancel, Ctrl-L suggest estimate, Ctrl-O save as template)"
        },
        {
            let mut v = vec![];
            if t.id.is_none() {
                let mut opts = vec![if templates.is_empty() {
                    "(no templates yet – create one under 6 Templates, or Ctrl-O here)".to_string()
                } else {
                    "(none)".to_string()
                }];
                opts.extend(templates.iter().map(|x| x.name.clone()));
                let fld = Field::choice("template", "Load template (←/→)", opts, 0);
                v.push(if templates.is_empty() {
                    fld.disabled()
                } else {
                    fld
                });
            }
            v
        }
        .into_iter()
        .chain(vec![
            Field::text("name", "Name", &t.t.name),
            Field::text("description", "Description", &t.t.description),
            Field::multi("prompt", "Prompt", &t.t.prompt, 6),
            Field::check("any_provider", "Any provider", t.any_provider),
            Field::choice("provider", "Provider", providers, pidx),
            Field::choice("account", "Account", accts, aidx),
            Field::check(
                "fallback",
                "Allow fallback to another account",
                t.t.allow_fallback,
            ),
            Field::choice(
                "run_mode",
                "Run mode",
                run_mode_labels(),
                run_mode_idx(t.t.run_mode),
            ),
            Field::text("model", "Model", &t.t.model),
            Field::choice(
                "priority",
                "Priority",
                vec!["Low".into(), "Normal".into(), "High".into()],
                t.t.priority.clamp(0, 2) as usize,
            ),
            Field::text("est", "Est. consumption (%)", &n(t.t.estimated_consumption)),
            Field::text("est_min", "Est. minutes", &n(t.t.estimated_minutes)),
            Field::check("budget_on", "Maximum budget", t.budget_on),
            Field::text("budget", "  budget (%)", &n(t.budget)),
            Field::text("timeout", "Timeout (min)", &n(t.timeout_min)),
            Field::text("retries", "Retries", &n(t.t.retry_max)),
            Field::text(
                "working_dir",
                "Working dir (inside allowed root)",
                &t.t.working_dir,
            ),
            Field::text(
                "deps",
                "Depends on task ids (comma separated; ids are in the # column)",
                &t.deps,
            ),
            Field::text(
                "schedule",
                "Schedule (blank, after:<RFC3339>, daily:HH:MM)",
                t.t.schedule.as_deref().unwrap_or(""),
            ),
            Field::text(
                "repeat",
                "Repeat (blank = once; every:6h, daily:09:00, weekly:mon-fri@18:30)",
                &t.repeat,
            ),
            Field::text("expires", "Expires (RFC 3339, blank = never)", &t.expires),
            Field::check(
                "requires_approval",
                "Always require approval for this task",
                t.t.requires_approval,
            ),
            Field::text("template_name", "Save-as-template name", &t.template_name),
        ])
        .collect(),
    )
}

pub fn read_task(f: &Form, base: &TaskForm, accounts: &[Account]) -> R<TaskForm> {
    let mut t = base.clone();
    let providers = provider_types();
    t.t.name = f.text("name");
    t.t.description = f.text("description");
    t.t.prompt = f.text("prompt");
    t.any_provider = f.checked("any_provider");
    let ai = f.choice_idx("account");
    t.pinned = (ai > 0)
        .then(|| accounts.get(ai - 1).map(|a| a.id))
        .flatten();
    t.t.provider_type = match t.pinned.and_then(|id| accounts.iter().find(|a| a.id == id)) {
        Some(a) => Some(a.provider_type.clone()),
        None => providers.get(f.choice_idx("provider")).cloned(),
    };
    t.t.allow_fallback = f.checked("fallback");
    t.t.run_mode = RunMode::ALL[f.choice_idx("run_mode")];
    t.t.model = f.text("model").trim().to_string();
    t.t.priority = f.choice_idx("priority") as i64;
    t.t.estimated_consumption = num(f, "est", "Est. consumption")?;
    t.t.estimated_minutes = num(f, "est_min", "Est. minutes")?;
    t.budget_on = f.checked("budget_on");
    t.budget = num(f, "budget", "Budget")?;
    t.timeout_min = num(f, "timeout", "Timeout")?;
    t.t.retry_max = num(f, "retries", "Retries")?;
    t.t.working_dir = f.text("working_dir").trim().to_string();
    t.deps = f.text("deps");
    let sched = f.text("schedule").trim().to_string();
    t.t.schedule = (!sched.is_empty()).then_some(sched);
    t.repeat = f.text("repeat").trim().to_string();
    t.expires = f.text("expires");
    t.t.requires_approval = f.checked("requires_approval");
    t.template_name = f.text("template_name").trim().to_string();
    Ok(t)
}

pub fn template_form(t: &plan_burner_core::forms::TemplateForm) -> Form {
    let providers = provider_types();
    let tk = &t.task;
    let pidx =
        tk.t.provider_type
            .as_ref()
            .and_then(|p| providers.iter().position(|x| x == p))
            .unwrap_or(0);
    Form::new(
        if t.id.is_some() {
            "Edit template  (Ctrl-S save, Esc cancel)"
        } else {
            "New template  (Ctrl-S save, Esc cancel)"
        },
        vec![
            Field::text("tpl_name", "Template name", &t.name),
            Field::text("description", "Description", &tk.t.description),
            Field::multi("prompt", "Prompt", &tk.t.prompt, 6),
            Field::check("any_provider", "Any provider", tk.any_provider),
            Field::choice("provider", "Provider", providers, pidx),
            Field::check(
                "fallback",
                "Allow fallback to another account",
                tk.t.allow_fallback,
            ),
            Field::choice(
                "run_mode",
                "Run mode",
                run_mode_labels(),
                run_mode_idx(tk.t.run_mode),
            ),
            Field::text("model", "Model", &tk.t.model),
            Field::choice(
                "priority",
                "Priority",
                vec!["Low".into(), "Normal".into(), "High".into()],
                tk.t.priority.clamp(0, 2) as usize,
            ),
            Field::text(
                "est",
                "Est. consumption (%)",
                &n(tk.t.estimated_consumption),
            ),
            Field::text("est_min", "Est. minutes", &n(tk.t.estimated_minutes)),
            Field::check("budget_on", "Maximum budget", tk.budget_on),
            Field::text("budget", "  budget (%)", &n(tk.budget)),
            Field::text("timeout", "Timeout (min)", &n(tk.timeout_min)),
            Field::text("retries", "Retries", &n(tk.t.retry_max)),
            Field::text(
                "working_dir",
                "Working dir (inside allowed root)",
                &tk.t.working_dir,
            ),
            Field::check(
                "requires_approval",
                "Always require approval for tasks from this template",
                tk.t.requires_approval,
            ),
        ],
    )
}

/// Templates share the task field keys; target, dependencies, schedule and expiry are simply absent.
pub fn read_template(
    f: &Form,
    base: &plan_burner_core::forms::TemplateForm,
) -> R<plan_burner_core::forms::TemplateForm> {
    let mut out = base.clone();
    out.name = f.text("tpl_name");
    out.task = read_task(f, &base.task, &[])?;
    Ok(out)
}

// ------------------------------------------------------------ usage/actual

pub fn usage_form(u: &UsageForm, account: &str) -> Form {
    Form::new(
        &format!("Update usage: {account}  (Ctrl-S save, Esc cancel) – manual entries, not provider measurements; unchecked = unknown (blocks execution)"),
        vec![
            Field::check("session_known", "Session remaining known", u.session_known),
            Field::text("session", "  session remaining (%)", &n(u.session)),
            Field::text("session_h", "  session resets in (h)", &n(u.session_reset_h)),
            Field::check("weekly_known", "Weekly remaining known", u.weekly_known),
            Field::text("weekly", "  weekly remaining (%)", &n(u.weekly)),
            Field::text("weekly_h", "  weekly resets in (h)", &n(u.weekly_reset_h)),
        ],
    )
}

pub fn read_usage(f: &Form, base: &UsageForm) -> R<UsageForm> {
    let mut u = base.clone();
    u.session_known = f.checked("session_known");
    u.session = num(f, "session", "Session remaining")?;
    u.session_reset_h = num(f, "session_h", "Session reset")?;
    u.weekly_known = f.checked("weekly_known");
    u.weekly = num(f, "weekly", "Weekly remaining")?;
    u.weekly_reset_h = num(f, "weekly_h", "Weekly reset")?;
    Ok(u)
}

const IMPORT_FORMATS: [&str; 3] = ["auto (by extension / content)", "json", "markdown"];

pub fn import_form() -> Form {
    Form::new(
        "Import tasks  (Ctrl-S import, Esc cancel) – JSON or Markdown file; tasks are queued as pending, nothing starts",
        vec![
            Field::text("path", "File path", ""),
            Field::choice(
                "format",
                "Format",
                IMPORT_FORMATS.iter().map(|s| s.to_string()).collect(),
                0,
            ),
            Field::check("dry_run", "Check only (create nothing)", false),
        ],
    )
}

/// (path, format, dry run) as entered.
pub fn read_import(f: &Form) -> (String, ImportFormat, bool) {
    let format = match f.choice_idx("format") {
        1 => ImportFormat::Json,
        2 => ImportFormat::Markdown,
        _ => ImportFormat::Auto,
    };
    (f.text("path"), format, f.checked("dry_run"))
}

pub fn actual_form(task: &str, current: Option<f64>) -> Form {
    Form::new(
        &format!("Record measured usage: {task}  (Ctrl-S save, Esc cancel)"),
        vec![Field::text(
            "actual",
            "Measured consumption (%)",
            &n(current.unwrap_or(0.0)),
        )],
    )
}

pub fn read_actual(f: &Form) -> R<f64> {
    let v: f64 = num(f, "actual", "Measured consumption")?;
    if !(0.0..=100.0).contains(&v) {
        return Err("Measured consumption must be 0-100".into());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn settings_round_trip_through_the_form() {
        let s = Settings {
            enabled: true,
            reserved_capacity: 33.0,
            allowed_hours: "22:00-07:00".into(),
            ..Settings::default()
        };
        let f = settings_form(&s);
        assert_eq!(read_settings(&f, &s).unwrap(), s);
    }

    #[test]
    fn settings_errors_are_named() {
        let s = Settings::default();
        let mut f = settings_form(&s);
        f.set_text("reserved", "lots");
        assert!(read_settings(&f, &s).unwrap_err().contains("Reserved"));
        f.set_text("reserved", "130");
        assert!(read_settings(&f, &s).is_err(), "core validation applies");
    }

    #[test]
    fn account_form_marks_modified_fields_and_counts_them() {
        let c = plan_burner_core::db::open_memory().unwrap();
        let a = plan_burner_core::accounts::create(&c, "claude", "A", "Pro", None).unwrap();
        let base = AccountForm::from_account(&a);
        let g = Settings::default();
        let mut f = account_form(&base, &g);
        account_marks(&mut f, &base);
        assert!(f.fields.iter().all(|x| !x.marked));
        f.set_text("label", "B");
        f.set_text("lim_start", "40");
        account_marks(&mut f, &base);
        let marked: Vec<_> = f
            .fields
            .iter()
            .filter(|x| x.marked)
            .map(|x| x.key)
            .collect();
        assert_eq!(marked, vec!["label", "lim_start"]);
        assert!(f.title.contains("2 unsaved"));
        f.set_text("label", "A");
        account_marks(&mut f, &base);
        assert!(f.title.contains("1 unsaved") && !f.title.contains("2 unsaved"));
    }

    #[test]
    fn account_round_trip_with_limits_and_provider_visibility() {
        let c = plan_burner_core::db::open_memory().unwrap();
        let a = plan_burner_core::accounts::create(&c, "claude", "A", "Pro", None).unwrap();
        let mut base = AccountForm::from_account(&a);
        base.limits.start_threshold = Some(40.0);
        base.limits.ignore_session = true;
        base.limits.allowed_hours = Some(String::new());
        base.limits.burn_session_hours = Some(2.0);
        base.limits.burn_weekly_hours = Some(0.0);
        let g = Settings::default();
        let mut f = account_form(&base, &g);
        let back = read_account(&f, &base).unwrap();
        assert_eq!(back.limits, base.limits);
        assert_eq!(back.label, "A");
        // overriding turns an inherited limit on, seeded with the general value
        f.focus = f
            .fields
            .iter()
            .position(|x| x.key == "lim_reserved")
            .unwrap();
        f.handle(key(KeyCode::Char(' ')));
        assert_eq!(
            read_account(&f, &base).unwrap().limits.reserved_capacity,
            Some(g.reserved_capacity)
        );
        // custom provider hides the Claude-only fields and shows the script ones
        let custom = AccountForm {
            provider: "custom".into(),
            ..AccountForm::new()
        };
        let f = account_form(&custom, &g);
        let vis = |k: &str| f.fields.iter().find(|x| x.key == k).unwrap().visible;
        assert!(vis("script") && !vis("config_dir") && !vis("lim_sess_thr"));
        let f = account_form(&AccountForm::new(), &g);
        let vis = |k: &str| f.fields.iter().find(|x| x.key == k).unwrap().visible;
        assert!(!vis("script") && vis("config_dir") && vis("lim_sess_thr"));
    }

    #[test]
    fn invalid_limit_numbers_are_rejected() {
        let g = Settings::default();
        let base = AccountForm::new();
        let mut f = account_form(&base, &g);
        f.set_text("lim_start", "abc");
        assert!(read_account(&f, &base).unwrap_err().contains("Start"));
    }

    #[test]
    fn task_round_trip_pins_account_and_provider() {
        let c = plan_burner_core::db::open_memory().unwrap();
        let a = plan_burner_core::accounts::create(&c, "claude", "A", "", None).unwrap();
        let accts = vec![a.clone()];
        let mut base = TaskForm::new_task();
        base.t.name = "n".into();
        base.t.prompt = "line1\nline2".into();
        let mut f = task_form(&base, &accts, &[]);
        f.set_choice("account", 1);
        let back = read_task(&f, &base, &accts).unwrap();
        assert_eq!(back.pinned, Some(a.id));
        assert_eq!(back.t.provider_type.as_deref(), Some("claude"));
        assert_eq!(back.t.prompt, "line1\nline2");
        f.set_text("est", "x");
        assert!(read_task(&f, &base, &accts).is_err());
        // The full save path goes through the same core code as the GUI.
        f.set_text("est", "7");
        let tf = read_task(&f, &base, &accts).unwrap();
        let t = tf.save(&c).unwrap();
        assert_eq!(t.estimated_consumption, 7.0);
        assert_eq!(t.account_id, Some(a.id));
    }

    #[test]
    fn load_template_field_is_always_present_on_new_tasks() {
        let new = TaskForm::new_task();
        let f = task_form(&new, &[], &[]);
        let fld = f.fields.iter().find(|x| x.key == "template").unwrap();
        assert!(
            fld.visible && !fld.enabled,
            "visible but empty-state disabled"
        );
        let c = plan_burner_core::db::open_memory().unwrap();
        let id = plan_burner_core::templates::save(
            &c,
            "t",
            &plan_burner_core::tasks::NewTask {
                prompt: "p".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let tpls = vec![plan_burner_core::templates::get(&c, id).unwrap()];
        let f = task_form(&new, &[], &tpls);
        assert!(
            f.fields
                .iter()
                .find(|x| x.key == "template")
                .unwrap()
                .enabled
        );
        let edit = TaskForm { id: Some(1), ..new };
        assert!(
            task_form(&edit, &[], &tpls)
                .fields
                .iter()
                .all(|x| x.key != "template"),
            "only offered when creating"
        );
    }

    #[test]
    fn template_form_round_trips_all_defaults() {
        let mut t = plan_burner_core::forms::TemplateForm::new();
        t.name = "daily".into();
        t.task.t.prompt = "do it".into();
        t.task.t.priority = 2;
        t.task.budget_on = true;
        t.task.budget = 9.0;
        t.task.t.retry_max = 3;
        let f = template_form(&t);
        let back = read_template(&f, &t).unwrap();
        assert_eq!(back.name, "daily");
        let (_, n) = back.to_template().unwrap();
        assert_eq!(
            (n.priority, n.maximum_budget, n.retry_max),
            (2, Some(9.0), 3)
        );
        assert_eq!(n.account_id, None);
    }

    #[test]
    fn usage_and_actual_forms() {
        let u = UsageForm::for_account(1, Some(30.0), None);
        let f = usage_form(&u, "claude · A");
        let back = read_usage(&f, &u).unwrap();
        assert_eq!(back, u);
        let mut f = actual_form("t", Some(12.0));
        assert_eq!(read_actual(&f).unwrap(), 12.0);
        f.set_text("actual", "140");
        assert!(read_actual(&f).is_err());
    }
}
