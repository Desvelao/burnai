//! Client-independent form models. The GUI and TUI edit these structs and call the same mapping /
//! validation here, so no client carries its own rules (AGENTS.md rule 9).
use crate::accounts::AccountUpdate;
use crate::models::{Account, AccountLimits, Confidence, UsageSource};
use crate::tasks::{self, NewTask, Task};
use crate::usage::NewSnapshot;
use crate::{Error, Result};
use chrono::{DateTime, Duration, Utc};

/// Trimmed text, or `None` when blank.
pub fn opt_str(s: &str) -> Option<String> {
    (!s.trim().is_empty()).then(|| s.trim().to_string())
}

/// Account add/edit form.
#[derive(Debug, Clone)]
pub struct AccountForm {
    pub id: Option<i64>,
    pub provider: String,
    pub label: String,
    pub plan: String,
    pub config_dir: String,
    pub credentials_file: String,
    pub auto_usage: bool,
    /// Custom provider: script path, fixed args (one per line) and pull timeout.
    pub script: String,
    pub script_args: String,
    pub script_timeout: u64,
    pub user_agent: String,
    /// CLI command for Claude accounts; blank or `claude` = default.
    pub cli_command: String,
    /// Weekly availability schedule, e.g. `mon-fri 09:00-17:00; sat,sun 10:00-14:00`; blank = always.
    pub availability_schedule: String,
    /// Per-account eligibility overrides (`None` = inherit the general settings).
    pub limits: AccountLimits,
    pub discovered: Vec<crate::claude_usage::DiscoveredProfile>,
    /// Set once logins were looked up for this form (so a client scans only once on open).
    pub discovery_done: bool,
    /// The values the form was opened with (edit only); `changed_fields` compares against it.
    pub baseline: Option<Box<AccountForm>>,
}

impl Default for AccountForm {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountForm {
    pub fn new() -> Self {
        AccountForm {
            id: None,
            provider: "claude".into(),
            label: String::new(),
            plan: String::new(),
            config_dir: String::new(),
            credentials_file: String::new(),
            auto_usage: true,
            script: String::new(),
            script_args: String::new(),
            script_timeout: 30,
            user_agent: String::new(),
            cli_command: "claude".into(),
            availability_schedule: String::new(),
            limits: Default::default(),
            discovered: vec![],
            discovery_done: false,
            baseline: None,
        }
    }

    /// The edit form for an existing account.
    pub fn from_account(a: &Account) -> Self {
        let mut f = Self::from_account_unbased(a);
        f.baseline = Some(Box::new(f.clone()));
        f
    }

    fn from_account_unbased(a: &Account) -> Self {
        AccountForm {
            id: Some(a.id),
            provider: a.provider_type.clone(),
            label: a.label.clone(),
            plan: a.subscription_plan.clone(),
            config_dir: a.config_dir.clone().unwrap_or_default(),
            credentials_file: a.credentials_file.clone().unwrap_or_default(),
            auto_usage: a.usage_source != UsageSource::Manual,
            script: a
                .custom
                .as_ref()
                .map(|c| c.program.clone())
                .unwrap_or_default(),
            script_args: a
                .custom
                .as_ref()
                .map(|c| c.args.join("\n"))
                .unwrap_or_default(),
            script_timeout: a.custom.as_ref().map(|c| c.timeout_secs).unwrap_or(30),
            user_agent: a.user_agent.clone().unwrap_or_default(),
            cli_command: a.cli_command.clone().unwrap_or_else(|| "claude".into()),
            availability_schedule: a.availability_schedule.clone(),
            limits: a.limits.clone(),
            discovered: vec![],
            discovery_done: true,
            baseline: None,
        }
    }

    /// Keys of the fields that differ from the values the form was opened with (empty for a new
    /// account). Limit keys match the override rows: `lim_*`, `ignore_session`, `ignore_limits`.
    pub fn changed_fields(&self) -> Vec<&'static str> {
        let Some(b) = self.baseline.as_deref() else {
            return vec![];
        };
        let (l, o) = (&self.limits, &b.limits);
        [
            ("label", self.label != b.label),
            ("plan", self.plan != b.plan),
            ("config_dir", self.config_dir != b.config_dir),
            ("cli_command", self.cli_command != b.cli_command),
            (
                "credentials_file",
                self.credentials_file != b.credentials_file,
            ),
            ("auto_usage", self.auto_usage != b.auto_usage),
            ("user_agent", self.user_agent != b.user_agent),
            ("script", self.script != b.script),
            ("script_args", self.script_args != b.script_args),
            ("script_timeout", self.script_timeout != b.script_timeout),
            (
                "schedule",
                self.availability_schedule != b.availability_schedule,
            ),
            ("ignore_limits", l.ignore_limits != o.ignore_limits),
            ("lim_start", l.start_threshold != o.start_threshold),
            ("lim_reserved", l.reserved_capacity != o.reserved_capacity),
            ("lim_idle", l.idle_requirement != o.idle_requirement),
            ("lim_hours", l.allowed_hours != o.allowed_hours),
            (
                "lim_age",
                l.max_usage_age_minutes != o.max_usage_age_minutes,
            ),
            ("lim_batch", l.max_tasks_per_batch != o.max_tasks_per_batch),
            (
                "lim_minutes",
                l.max_execution_minutes != o.max_execution_minutes,
            ),
            ("ignore_session", l.ignore_session != o.ignore_session),
            ("lim_sess_thr", l.session_threshold != o.session_threshold),
            ("lim_sess_res", l.session_reserved != o.session_reserved),
            (
                "lim_burn_sess",
                l.burn_session_hours != o.burn_session_hours,
            ),
            ("lim_burn_week", l.burn_weekly_hours != o.burn_weekly_hours),
        ]
        .into_iter()
        .filter_map(|(k, changed)| changed.then_some(k))
        .collect()
    }

    /// How many eligibility limits this account overrides (flags count as overrides).
    pub fn limit_override_count(&self) -> usize {
        let l = &self.limits;
        [
            l.start_threshold.is_some(),
            l.reserved_capacity.is_some(),
            l.idle_requirement.is_some(),
            l.allowed_hours.is_some(),
            l.max_usage_age_minutes.is_some(),
            l.max_tasks_per_batch.is_some(),
            l.max_execution_minutes.is_some(),
            l.session_threshold.is_some(),
            l.session_reserved.is_some(),
            l.burn_session_hours.is_some(),
            l.burn_weekly_hours.is_some(),
            l.ignore_session,
            l.ignore_limits,
        ]
        .into_iter()
        .filter(|x| *x)
        .count()
    }

    /// One-line summaries shown next to collapsed sections.
    pub fn connection_summary(&self) -> String {
        if self.provider == "custom" {
            return match opt_str(&self.script) {
                Some(s) => format!("script {s}"),
                None => "no script yet".into(),
            };
        }
        let or = |s: &str, d: &str| opt_str(s).unwrap_or_else(|| d.to_string());
        format!(
            "CLI {} · profile {} · credentials file {}",
            or(&self.cli_command, "claude"),
            or(&self.config_dir, "default"),
            if opt_str(&self.credentials_file).is_some() {
                "set"
            } else {
                "default"
            }
        )
    }

    pub fn usage_summary(&self) -> String {
        match (self.auto_usage, self.provider == "custom") {
            (false, _) => "manual entry".into(),
            (true, true) => "from the script".into(),
            (true, false) => format!(
                "automatic · {} User-Agent",
                if opt_str(&self.user_agent).is_some() {
                    "custom"
                } else {
                    "default"
                }
            ),
        }
    }

    pub fn availability_summary(&self) -> String {
        opt_str(&self.availability_schedule).unwrap_or_else(|| "always available".into())
    }

    /// Fills the form from a discovered login (label/plan only when still empty).
    pub fn apply_discovered(&mut self, d: &crate::claude_usage::DiscoveredProfile) {
        self.config_dir = d.profile_dir.clone();
        if self.label.is_empty() {
            self.label = d.label_guess.trim_start_matches('.').to_string();
        }
        if self.plan.is_empty() {
            self.plan = d.subscription_type.clone().unwrap_or_default();
        }
    }

    /// The settings stored for this form, with the given label/plan.
    pub fn to_update(&self, label: &str, plan: &str) -> AccountUpdate {
        let is_custom = self.provider == "custom";
        let usage_source = match (self.auto_usage, is_custom) {
            (false, _) => UsageSource::Manual,
            (true, true) => UsageSource::CustomScript,
            (true, false) => UsageSource::ClaudeOauth,
        };
        let custom = is_custom.then(|| crate::custom::CustomConfig {
            program: self.script.trim().to_string(),
            args: self
                .script_args
                .lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect(),
            timeout_secs: self.script_timeout,
        });
        AccountUpdate {
            custom,
            label: label.to_string(),
            plan: plan.to_string(),
            config_dir: opt_str(&self.config_dir),
            credentials_file: opt_str(&self.credentials_file),
            usage_source,
            user_agent: opt_str(&self.user_agent),
            // Blank or the default name is stored as "unset" so the default can evolve.
            cli_command: opt_str(&self.cli_command).filter(|c| c != "claude"),
            availability_schedule: self.availability_schedule.trim().to_string(),
        }
    }
}

/// Manual usage entry form.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsageForm {
    pub account_id: i64,
    pub session_known: bool,
    pub session: f64,
    pub session_reset_h: f64,
    pub weekly_known: bool,
    pub weekly: f64,
    pub weekly_reset_h: f64,
}

impl UsageForm {
    /// Defaults from the account's current reading (unknown readings start unchecked at 100 %).
    pub fn for_account(account_id: i64, session: Option<f64>, weekly: Option<f64>) -> Self {
        UsageForm {
            account_id,
            session_known: session.is_some(),
            session: session.unwrap_or(100.0),
            session_reset_h: 5.0,
            weekly_known: weekly.is_some(),
            weekly: weekly.unwrap_or(100.0),
            weekly_reset_h: 72.0,
        }
    }

    /// A manual snapshot (never a provider measurement). Unchecked values are unknown.
    pub fn to_snapshot(&self, now: DateTime<Utc>) -> NewSnapshot {
        NewSnapshot {
            account_id: self.account_id,
            session_remaining: self.session_known.then_some(self.session),
            weekly_remaining: self.weekly_known.then_some(self.weekly),
            session_reset_at: Some(now + Duration::minutes((self.session_reset_h * 60.0) as i64)),
            weekly_reset_at: Some(now + Duration::minutes((self.weekly_reset_h * 60.0) as i64)),
            confidence: Confidence::Manual,
            source: "manual".into(),
        }
    }
}

/// Task new/edit form.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TaskForm {
    pub id: Option<i64>,
    pub t: NewTask,
    pub deps: String,
    pub budget_on: bool,
    pub budget: f64,
    pub timeout_min: f64,
    pub pinned: Option<i64>,
    pub any_provider: bool,
    pub template_name: String,
    /// Expiration as RFC 3339 (`2026-01-31T09:30:00Z`); blank = never.
    #[serde(default)]
    pub expires: String,
    /// Recurrence rule (`core::recurrence::GRAMMAR`); blank = runs once.
    #[serde(default)]
    pub repeat: String,
}

impl TaskForm {
    pub fn new_task() -> Self {
        TaskForm {
            id: None,
            t: NewTask::default(),
            deps: String::new(),
            budget_on: false,
            budget: 10.0,
            timeout_min: 20.0,
            pinned: None,
            any_provider: false,
            template_name: String::new(),
            expires: String::new(),
            repeat: String::new(),
        }
    }

    /// The dependency ids currently typed/selected (tokens that are not numbers are ignored here;
    /// `to_new_task` still rejects them on save).
    pub fn dep_ids(&self) -> Vec<i64> {
        self.deps
            .split(',')
            .filter_map(|t| t.trim().parse().ok())
            .collect()
    }

    /// Adds or removes one dependency (the GUI's multi-select); keeps the ids sorted and unique.
    pub fn toggle_dep(&mut self, id: i64) {
        let mut ids = self.dep_ids();
        if let Some(i) = ids.iter().position(|d| *d == id) {
            ids.remove(i);
        } else {
            ids.push(id);
        }
        ids.sort_unstable();
        ids.dedup();
        self.deps = ids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
    }

    /// The edit form for an existing task; `deps` are its current dependency ids.
    pub fn from_task(t: &Task, deps: &[i64]) -> Self {
        TaskForm {
            id: Some(t.id),
            t: NewTask {
                name: t.name.clone(),
                description: t.description.clone(),
                prompt: t.prompt.clone(),
                provider_type: t.provider_type.clone(),
                account_id: t.account_id,
                allow_fallback: t.allow_fallback,
                model: t.model.clone(),
                priority: t.priority,
                estimated_consumption: t.estimated_consumption,
                estimated_minutes: t.estimated_minutes,
                maximum_budget: t.maximum_budget,
                timeout_secs: t.timeout_secs,
                retry_max: t.retry_max,
                working_dir: t.working_dir.clone(),
                requires_approval: t.requires_approval,
                schedule: t.schedule.clone(),
                expiration_at: t.expiration_at,
                run_mode: t.run_mode,
            },
            deps: deps
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            budget_on: t.maximum_budget.is_some(),
            budget: t.maximum_budget.unwrap_or(10.0),
            timeout_min: t.timeout_secs as f64 / 60.0,
            pinned: t.account_id,
            any_provider: t.provider_type.is_none(),
            template_name: String::new(),
            expires: t.expiration_at.map(crate::db::ts).unwrap_or_default(),
            repeat: t.repeat.clone().unwrap_or_default(),
        }
    }

    /// A new-task form pre-filled from a template.
    pub fn from_template(tpl: &crate::templates::Template) -> Self {
        let mut f = TaskForm::new_task();
        f.apply_template(tpl);
        f
    }

    /// Applies every default of a saved template (the name only when still empty). The target
    /// account, schedule, expiry and dependencies are left alone.
    pub fn apply_template(&mut self, tpl: &crate::templates::Template) {
        let d = &tpl.task;
        self.t.description = d.description.clone();
        self.t.prompt = d.prompt.clone();
        self.t.model = d.model.clone();
        self.t.estimated_consumption = d.estimated_consumption;
        self.t.estimated_minutes = d.estimated_minutes;
        self.t.priority = d.priority;
        self.t.run_mode = d.run_mode;
        self.t.retry_max = d.retry_max;
        self.t.working_dir = d.working_dir.clone();
        self.t.requires_approval = d.requires_approval;
        self.t.allow_fallback = d.allow_fallback;
        self.t.provider_type = d.provider_type.clone();
        self.any_provider = d.provider_type.is_none();
        if self.any_provider || self.pinned.is_none() {
            self.pinned = None;
        }
        self.budget_on = d.maximum_budget.is_some();
        if let Some(b) = d.maximum_budget {
            self.budget = b;
        }
        self.timeout_min = d.timeout_secs as f64 / 60.0;
        if self.t.name.trim().is_empty() {
            self.t.name = tpl.name.clone();
        }
    }

    /// The task to store and its dependency ids, as entered in the form.
    pub fn to_new_task(&self) -> Result<(NewTask, Vec<i64>)> {
        let mut n = self.t.clone();
        n.account_id = if self.any_provider { None } else { self.pinned };
        if self.any_provider {
            n.provider_type = None;
            n.allow_fallback = false;
        }
        n.maximum_budget = self.budget_on.then_some(self.budget);
        n.timeout_secs = (self.timeout_min * 60.0) as i64;
        n.expiration_at = match self.expires.trim() {
            "" => None,
            e => Some(crate::db::parse_ts(e).ok_or_else(|| {
                Error::Invalid(
                    "expiration must be an RFC 3339 time, e.g. 2026-01-31T09:30:00Z".into(),
                )
            })?),
        };
        crate::recurrence::validate(Some(self.repeat.trim()))?;
        let deps = self
            .deps
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|t| {
                t.parse::<i64>().map_err(|_| {
                    Error::Invalid(format!(
                        "dependencies must be task ids (numbers); \"{t}\" is not one"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((n, deps))
    }

    /// Stores the task and its dependencies (a newly created task is removed again if the
    /// dependencies are rejected). Used by `Service::save_task`.
    pub fn save(&self, conn: &rusqlite::Connection) -> Result<Task> {
        let (n, deps) = self.to_new_task()?;
        let t = match self.id {
            Some(id) => tasks::update(conn, id, &n)?,
            None => tasks::create(conn, &n, crate::models::TaskStatus::Pending)?,
        };
        if let Err(e) = tasks::set_dependencies(conn, t.id, &deps) {
            if self.id.is_none() {
                tasks::delete(conn, t.id)?;
            }
            return Err(e);
        }
        tasks::set_repeat(conn, t.id, Some(&self.repeat))?;
        tasks::get(conn, t.id)
    }
}

/// Template create/edit form: a name plus the task defaults (a [`TaskForm`] whose target,
/// dependencies, schedule and expiry are ignored).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TemplateForm {
    pub id: Option<i64>,
    pub name: String,
    pub task: TaskForm,
}

impl TemplateForm {
    pub fn new() -> Self {
        let mut task = TaskForm::new_task();
        task.t.requires_approval = true;
        TemplateForm {
            id: None,
            name: String::new(),
            task,
        }
    }

    pub fn from_template(t: &crate::templates::Template) -> Self {
        let mut task = TaskForm::from_template(t);
        task.t.name = String::new();
        TemplateForm {
            id: Some(t.id),
            name: t.name.clone(),
            task,
        }
    }

    /// "Save as template" from a task form: defaults of the task, named after it unless a template
    /// name was typed.
    pub fn from_task(f: &TaskForm) -> Self {
        let name = if f.template_name.trim().is_empty() {
            f.t.name.clone()
        } else {
            f.template_name.clone()
        };
        TemplateForm {
            id: None,
            name,
            task: f.clone(),
        }
    }

    /// The template name and its task defaults (no account, schedule, expiry or dependencies).
    pub fn to_template(&self) -> Result<(String, NewTask)> {
        let mut t = self.task.clone();
        t.deps.clear();
        t.expires.clear();
        t.pinned = None;
        let (mut n, _) = t.to_new_task()?;
        n.account_id = None;
        n.schedule = None;
        n.expiration_at = None;
        n.name = self.name.trim().to_string();
        Ok((self.name.trim().to_string(), n))
    }
}

impl Default for TemplateForm {
    fn default() -> Self {
        Self::new()
    }
}

/// Which tasks of a pending batch are selected (default: all).
pub fn selected_ids(all: &[i64], deselected: &std::collections::HashSet<i64>) -> Vec<i64> {
    all.iter()
        .copied()
        .filter(|id| !deselected.contains(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::UsageSource;

    #[test]
    fn account_form_tracks_changes_against_the_opened_values() {
        let c = crate::db::open_memory().unwrap();
        let a = crate::accounts::create(&c, "claude", "A", "Pro", None).unwrap();
        let mut f = AccountForm::from_account(&a);
        assert!(f.changed_fields().is_empty());
        f.label = "B".into();
        f.limits.start_threshold = Some(40.0);
        f.availability_schedule = "mon-fri 09:00-17:00".into();
        assert_eq!(f.changed_fields(), vec!["label", "schedule", "lim_start"]);
        f.label = "A".into();
        assert_eq!(f.changed_fields(), vec!["schedule", "lim_start"]);
        assert_eq!(f.limit_override_count(), 1);
        assert!(AccountForm::new().changed_fields().is_empty());
        assert_eq!(f.availability_summary(), "mon-fri 09:00-17:00");
        assert_eq!(
            AccountForm::new().availability_summary(),
            "always available"
        );
        assert!(AccountForm::new()
            .connection_summary()
            .contains("CLI claude"));
    }

    #[test]
    fn toggling_dependencies_keeps_a_sorted_unique_list() {
        let mut f = TaskForm::new_task();
        f.toggle_dep(5);
        f.toggle_dep(2);
        f.toggle_dep(5);
        f.toggle_dep(9);
        assert_eq!(f.dep_ids(), vec![2, 9]);
        assert_eq!(f.deps, "2, 9");
        f.deps = "3, x, 1".into();
        assert_eq!(f.dep_ids(), vec![3, 1]);
    }

    #[test]
    fn account_form_maps_usage_source_and_blank_fields() {
        let mut f = AccountForm::new();
        f.config_dir = "  ".into();
        f.user_agent = " UA ".into();
        let u = f.to_update("L", "Pro");
        assert_eq!(u.usage_source, UsageSource::ClaudeOauth);
        assert_eq!((u.label.as_str(), u.plan.as_str()), ("L", "Pro"));
        assert_eq!(u.config_dir, None, "blank => unset");
        assert_eq!(u.user_agent.as_deref(), Some("UA"));
        assert_eq!(u.cli_command, None, "default `claude` is stored as unset");
        assert!(u.custom.is_none());
        f.cli_command = " /opt/claude-wrap ".into();
        assert_eq!(
            f.to_update("L", "").cli_command.as_deref(),
            Some("/opt/claude-wrap")
        );
        f.auto_usage = false;
        assert_eq!(f.to_update("L", "").usage_source, UsageSource::Manual);
        f.availability_schedule = "  mon-fri 09:00-17:00 ".into();
        assert_eq!(
            f.to_update("L", "").availability_schedule,
            "mon-fri 09:00-17:00"
        );
    }

    #[test]
    fn custom_account_form_builds_script_config() {
        let mut f = AccountForm::new();
        f.provider = "custom".into();
        f.script = " /opt/agent.sh ".into();
        f.script_args = "--a\n\n--b\n".into();
        f.script_timeout = 12;
        let u = f.to_update("C", "");
        assert_eq!(u.usage_source, UsageSource::CustomScript);
        let c = u.custom.expect("custom config");
        assert_eq!(c.program, "/opt/agent.sh");
        assert_eq!(c.args, vec!["--a", "--b"]);
        assert_eq!(c.timeout_secs, 12);
        f.auto_usage = false;
        assert_eq!(f.to_update("C", "").usage_source, UsageSource::Manual);
    }

    #[test]
    fn account_form_round_trips_an_account() {
        let c = crate::db::open_memory().unwrap();
        let a = crate::accounts::create(&c, "claude", "A", "Pro", None).unwrap();
        let mut f = AccountForm::from_account(&a);
        f.limits.start_threshold = Some(40.0);
        f.cli_command = "/opt/c".into();
        let u = f.to_update("A", "Pro");
        let a = crate::accounts::update(&c, a.id, &u).unwrap();
        let g = AccountForm::from_account(&a);
        assert_eq!(g.cli_command, "/opt/c");
        assert_eq!(g.id, Some(a.id));
    }

    fn task_form() -> TaskForm {
        TaskForm {
            id: None,
            t: NewTask {
                name: "n".into(),
                prompt: "p".into(),
                provider_type: Some("claude".into()),
                allow_fallback: true,
                ..Default::default()
            },
            deps: String::new(),
            budget_on: false,
            budget: 10.0,
            timeout_min: 2.0,
            pinned: Some(5),
            any_provider: false,
            template_name: String::new(),
            expires: String::new(),
            repeat: String::new(),
        }
    }

    #[test]
    fn task_form_pins_account_and_converts_units() {
        let mut f = task_form();
        f.budget_on = true;
        f.deps = " 1, 2 ,,3 ".into();
        let (n, deps) = f.to_new_task().unwrap();
        assert_eq!(n.account_id, Some(5));
        assert_eq!(n.provider_type.as_deref(), Some("claude"));
        assert_eq!(n.maximum_budget, Some(10.0));
        assert_eq!(n.timeout_secs, 120);
        assert_eq!(deps, vec![1, 2, 3]);
        f.budget_on = false;
        assert_eq!(f.to_new_task().unwrap().0.maximum_budget, None);
    }

    #[test]
    fn task_form_any_provider_clears_pin_provider_and_fallback() {
        let mut f = task_form();
        f.any_provider = true;
        let (n, _) = f.to_new_task().unwrap();
        assert_eq!(n.account_id, None, "no pin when any provider");
        assert_eq!(n.provider_type, None);
        assert!(!n.allow_fallback);
    }

    #[test]
    fn task_form_expiration_parses_or_is_rejected() {
        let mut f = task_form();
        f.expires = "2030-01-31T09:30:00Z".into();
        assert!(f.to_new_task().unwrap().0.expiration_at.is_some());
        f.expires = "  ".into();
        assert_eq!(f.to_new_task().unwrap().0.expiration_at, None);
        f.expires = "tomorrow".into();
        assert!(f.to_new_task().is_err());
    }

    #[test]
    fn task_form_rejects_non_numeric_dependencies() {
        let mut f = task_form();
        f.deps = "1, x".into();
        assert!(f.to_new_task().is_err());
    }

    #[test]
    fn task_form_save_and_edit_round_trip_with_dependencies() {
        let c = crate::db::open_memory().unwrap();
        let mut a = TaskForm::new_task();
        a.t.name = "a".into();
        a.t.prompt = "p".into();
        let ta = a.save(&c).unwrap();
        let mut b = TaskForm::new_task();
        b.t.name = "b".into();
        b.t.prompt = "p".into();
        b.deps = ta.id.to_string();
        let tb = b.save(&c).unwrap();
        assert_eq!(tasks::dependencies(&c, tb.id).unwrap(), vec![ta.id]);
        let back = TaskForm::from_task(&tb, &[ta.id]);
        assert_eq!(back.deps, ta.id.to_string());
        assert_eq!(back.id, Some(tb.id));
        // A rejected dependency rolls back a newly created task.
        let mut bad = TaskForm::new_task();
        bad.t.name = "c".into();
        bad.t.prompt = "p".into();
        bad.deps = "9999".into();
        let before = tasks::list(&c).unwrap().len();
        assert!(bad.save(&c).is_err());
        assert_eq!(tasks::list(&c).unwrap().len(), before);
    }

    #[test]
    fn usage_form_builds_manual_snapshot() {
        let now = Utc::now();
        let mut f = UsageForm::for_account(3, Some(40.0), None);
        let s = f.to_snapshot(now);
        assert_eq!(s.session_remaining, Some(40.0));
        assert_eq!(s.weekly_remaining, None, "unchecked = unknown");
        assert_eq!(s.confidence, Confidence::Manual);
        f.weekly_known = true;
        assert_eq!(f.to_snapshot(now).weekly_remaining, Some(100.0));
    }

    #[test]
    fn templates_carry_full_defaults_into_a_task_form() {
        let c = crate::db::open_memory().unwrap();
        let mut f = TemplateForm::new();
        f.name = "review".into();
        f.task.t.prompt = "review the diff".into();
        f.task.t.priority = 2;
        f.task.t.run_mode = crate::models::RunMode::Plan;
        f.task.budget_on = true;
        f.task.budget = 12.0;
        f.task.timeout_min = 5.0;
        f.task.t.retry_max = 1;
        f.task.any_provider = true;
        f.task.deps = "7".into(); // ignored by templates
        f.task.pinned = Some(3); // ignored by templates
        let (name, n) = f.to_template().unwrap();
        assert_eq!(n.account_id, None);
        assert_eq!(n.maximum_budget, Some(12.0));
        let id = crate::templates::save(&c, &name, &n).unwrap();
        let tpl = crate::templates::get(&c, id).unwrap();
        let mut t = TaskForm::new_task();
        t.pinned = Some(9);
        t.apply_template(&tpl);
        assert_eq!(t.t.name, "review", "empty name takes the template name");
        assert_eq!(t.t.priority, 2);
        assert_eq!(t.t.run_mode, crate::models::RunMode::Plan);
        assert!(t.budget_on && t.budget == 12.0);
        assert_eq!(t.timeout_min, 5.0);
        assert!(t.any_provider);
        assert_eq!(t.pinned, None, "any provider cannot stay pinned");
        let mut named = TaskForm::new_task();
        named.t.name = "mine".into();
        named.apply_template(&tpl);
        assert_eq!(named.t.name, "mine", "a typed name is kept");
        // editing round trip and save-as
        let back = TemplateForm::from_template(&tpl);
        assert_eq!(back.id, Some(id));
        assert_eq!(back.name, "review");
        assert_eq!(back.to_template().unwrap().1.priority, 2);
        let mut task = TaskForm::new_task();
        task.t.name = "t".into();
        task.t.prompt = "p".into();
        assert_eq!(TemplateForm::from_task(&task).name, "t");
        task.template_name = "custom".into();
        assert_eq!(TemplateForm::from_task(&task).name, "custom");
    }

    #[test]
    fn approval_selection_defaults_to_all() {
        let none = std::collections::HashSet::new();
        assert_eq!(selected_ids(&[1, 2, 3], &none), vec![1, 2, 3]);
        let off: std::collections::HashSet<i64> = [2].into();
        assert_eq!(selected_ids(&[1, 2, 3], &off), vec![1, 3]);
    }
}
