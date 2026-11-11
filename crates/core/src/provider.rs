//! Provider adapter interface. One adapter type serves many accounts of the same provider.
use crate::models::{Account, BillingMode, RunMode};
use std::collections::HashMap;
use std::path::PathBuf;

/// Description of the process to launch for a task. Structured arguments only; the prompt goes via stdin.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: PathBuf,
    pub stdin: String,
}

/// What to run for one task (everything except the account and process environment).
#[derive(Debug, Clone, Copy)]
pub struct TaskLaunch<'a> {
    pub model: &'a str,
    pub prompt: &'a str,
    pub task_id: i64,
    /// Already resolved (see `engine::effective_run_mode`); the adapter translates it.
    pub mode: RunMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub cli_found: bool,
    pub billing: BillingMode,
    pub reasons: Vec<String>,
}

pub trait ProviderAdapter: Send + Sync {
    fn provider_type(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    /// Inspect local configuration (never network, never secrets) and judge the account's billing mode.
    fn detect(&self, account: &Account, env: &HashMap<String, String>) -> Detection;
    /// Active detection that may start external processes (too slow for UI refreshes). The runner calls it
    /// before launching **every** task; `None` means "use `detect`".
    fn probe(&self, _account: &Account, _env: &HashMap<String, String>) -> Option<Detection> {
        None
    }
    /// Billing-relevant configuration found in a task's working directory (e.g. project-level settings that
    /// switch to API billing). Any entry blocks the task. Called by the runner before every execution.
    fn workdir_billing_reasons(&self, _cwd: &std::path::Path) -> Vec<String> {
        Vec::new()
    }
    /// Build the launch spec. Must scrub billing env vars.
    fn command(
        &self,
        account: &Account,
        launch: &TaskLaunch,
        cwd: PathBuf,
        env: &HashMap<String, String>,
    ) -> CommandSpec;
    /// True if the output/exit code of a failed run indicates the provider's rate or usage limit was hit.
    fn is_rate_limited(&self, output: &str, exit_code: i32) -> bool;
}

/// Adapters registered with the app. Future providers are added here.
pub fn registry() -> Vec<Box<dyn ProviderAdapter>> {
    vec![
        Box::new(crate::claude::ClaudeAdapter::default()),
        Box::new(crate::custom::CustomAdapter),
    ]
}

pub fn find<'a>(
    reg: &'a [Box<dyn ProviderAdapter>],
    provider_type: &str,
) -> Option<&'a dyn ProviderAdapter> {
    reg.iter()
        .find(|a| a.provider_type() == provider_type)
        .map(|b| b.as_ref())
}
