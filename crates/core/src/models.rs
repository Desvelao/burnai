use serde::{Deserialize, Serialize};

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($var:ident => $s:expr),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $($var),+ }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$var),+];
            pub fn as_str(&self) -> &'static str { match self { $($name::$var => $s),+ } }
            pub fn parse(s: &str) -> Option<$name> { match s { $($s => Some($name::$var),)+ _ => None } }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

str_enum!(TaskStatus {
    Draft => "draft",
    Pending => "pending",
    WaitingForCapacity => "waiting_for_capacity",
    AwaitingApproval => "awaiting_approval",
    Running => "running",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    Paused => "paused",
    Expired => "expired",
});

impl TaskStatus {
    pub fn label(&self) -> &'static str {
        match self {
            TaskStatus::Draft => "Draft",
            TaskStatus::Pending => "Pending",
            TaskStatus::WaitingForCapacity => "Waiting for capacity",
            TaskStatus::AwaitingApproval => "Awaiting approval",
            TaskStatus::Running => "Running",
            TaskStatus::Completed => "Completed",
            TaskStatus::Failed => "Failed",
            TaskStatus::Cancelled => "Cancelled",
            TaskStatus::Paused => "Paused",
            TaskStatus::Expired => "Expired",
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Expired
        )
    }
    /// Statuses from which a task may be proposed for execution.
    pub fn is_runnable(&self) -> bool {
        matches!(
            self,
            TaskStatus::Pending | TaskStatus::WaitingForCapacity | TaskStatus::AwaitingApproval
        )
    }
}

str_enum!(
    /// How an account is billed. Only `Subscription` may execute tasks.
    BillingMode {
        Unverified => "unverified",
        Subscription => "subscription",
        Api => "api",
    }
);

str_enum!(ExecutionPolicy {
    Manual => "manual",
    Approval => "approval",
    Automatic => "automatic",
});

str_enum!(Confidence {
    Unknown => "unknown",
    Estimated => "estimated",
    Manual => "manual",
    Provider => "provider",
});

str_enum!(ConnectionStatus {
    Disconnected => "disconnected",
    Connected => "connected",
    Error => "error",
});

str_enum!(
    /// Where an account's usage readings come from.
    UsageSource {
        Manual => "manual",
        ClaudeOauth => "claude_oauth",
        CustomScript => "custom_script",
    }
);

str_enum!(ExecutionStatus {
    Running => "running",
    Completed => "completed",
    Failed => "failed",
    TimedOut => "timed_out",
    Cancelled => "cancelled",
    RateLimited => "rate_limited",
    Interrupted => "interrupted",
});

str_enum!(
    /// Provider-neutral statement of how much a task may do. Each adapter translates it into its own
    /// CLI flags / protocol parameters; `Inherit` means "pass nothing, use the provider's own default".
    /// There is deliberately no "bypass all safeguards" level.
    RunMode {
    Inherit => "inherit",
    Plan => "plan",
    Review => "review",
    AutoEdit => "auto_edit",
});

// The `str_enum!` macro can't mark a `#[default]` variant, so the impl is written by hand.
#[allow(clippy::derivable_impls)]
impl Default for RunMode {
    fn default() -> Self {
        RunMode::Inherit
    }
}

impl RunMode {
    pub fn label(&self) -> &'static str {
        match self {
            RunMode::Inherit => "Provider default",
            RunMode::Plan => "Plan only (read-only)",
            RunMode::Review => "Ask before changes",
            RunMode::AutoEdit => "Auto-edit files",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: i64,
    pub provider_type: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: i64,
    pub provider_id: i64,
    pub provider_type: String,
    pub label: String,
    pub subscription_plan: String,
    pub billing_mode: BillingMode,
    pub connection_status: ConnectionStatus,
    pub config_dir: Option<String>,
    pub subscription_confirmed: bool,
    pub execution_policy: ExecutionPolicy,
    /// Explicit credentials file; `None` = `<config_dir or ~/.claude>/.credentials.json`.
    pub credentials_file: Option<String>,
    pub usage_source: UsageSource,
    /// User-Agent override for the usage endpoint; `None` = built-in default.
    pub user_agent: Option<String>,
    /// Script configuration (provider `custom` only).
    pub custom: Option<crate::custom::CustomConfig>,
    /// Last `status` result from the custom script and when it was obtained.
    pub custom_status: Option<(crate::custom::CustomStatus, chrono::DateTime<chrono::Utc>)>,
    /// Command used to launch the provider CLI (Claude accounts); `None` = default (`claude`).
    pub cli_command: Option<String>,
    /// On-demand master switch; when false the account is never used (see `availability`).
    pub available: bool,
    /// Weekly availability schedule (see `availability`); empty = always.
    pub availability_schedule: String,
    /// Run-mode override for this account (`Inherit` = use the global default).
    pub run_mode: RunMode,
    /// Per-account eligibility overrides (`None` = inherit the general settings).
    pub limits: AccountLimits,
}

/// Per-account overrides of the general eligibility settings. Percentages are 0-100 "remaining".
/// Everything here only changes *thresholds*: stale/unknown usage, billing safety, remaining capacity
/// for the task, availability and STOP are never affected (see `Settings::effective_for`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountLimits {
    /// "Start when capacity above (%)".
    #[serde(default)]
    pub start_threshold: Option<f64>,
    #[serde(default)]
    pub reserved_capacity: Option<f64>,
    /// Idle minutes required before running on this account; `Some(0)` = never require idle.
    #[serde(default)]
    pub idle_requirement: Option<u32>,
    /// "HH:MM-HH:MM"; `Some("")` = always.
    #[serde(default)]
    pub allowed_hours: Option<String>,
    #[serde(default)]
    pub max_usage_age_minutes: Option<u32>,
    #[serde(default)]
    pub max_tasks_per_batch: Option<u32>,
    #[serde(default)]
    pub max_execution_minutes: Option<u32>,
    /// Session (5 h) window start threshold; `None` = same as `start_threshold`/general.
    #[serde(default)]
    pub session_threshold: Option<f64>,
    #[serde(default)]
    pub session_reserved: Option<f64>,
    /// Ignore the session (5 h) window entirely and decide on the weekly window only.
    #[serde(default)]
    pub ignore_session: bool,
    /// Eligible regardless of thresholds, reserve, idle and allowed hours (never staleness/billing).
    #[serde(default)]
    pub ignore_limits: bool,
    /// Burn-before-reset override for the session (5 h) window; `Some(0)` = off, `None` = inherit.
    #[serde(default)]
    pub burn_session_hours: Option<f64>,
    /// Burn-before-reset override for the weekly window; `Some(0)` = off, `None` = inherit.
    #[serde(default)]
    pub burn_weekly_hours: Option<f64>,
}

impl Account {
    /// True if the app polls this account's usage/status on a timer.
    pub fn polls(&self) -> bool {
        self.usage_source != UsageSource::Manual
            || (self.provider_type == "custom" && self.custom.is_some())
    }

    pub fn display(&self) -> String {
        format!("{} · {}", self.provider_type, self.label)
    }
}
