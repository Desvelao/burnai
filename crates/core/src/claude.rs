//! Claude Code CLI adapter. Uses only documented CLI behaviour (`claude -p`, `--model`,
//! `CLAUDE_CONFIG_DIR`). There is no supported programmatic usage-meter API, so usage is entered
//! manually or, optionally, via `claude_usage` (unofficial endpoint; see docs/KNOWN_LIMITATIONS.md).
//! This adapter only launches the CLI and inspects local files for existence.
use crate::models::{Account, RunMode};
use crate::provider::{CommandSpec, Detection, ProviderAdapter, TaskLaunch};
use crate::safety;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    pub binary: PathBuf,
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        ClaudeAdapter {
            binary: PathBuf::from("claude"),
        }
    }
}

fn on_path(program: &Path, env: &HashMap<String, String>) -> bool {
    if program.components().count() > 1 {
        return program.is_file();
    }
    env.get("PATH")
        .map(|p| std::env::split_paths(p).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}

impl ClaudeAdapter {
    /// The program to launch for `account`: its configured command, else the default (`claude`).
    fn binary_for(&self, account: &Account) -> PathBuf {
        account
            .cli_command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.binary.clone())
    }
}

impl ProviderAdapter for ClaudeAdapter {
    fn provider_type(&self) -> &'static str {
        "claude"
    }
    fn display_name(&self) -> &'static str {
        "Claude (Claude Code CLI)"
    }

    fn detect(&self, account: &Account, env: &HashMap<String, String>) -> Detection {
        let bin = self.binary_for(account);
        let cli_found = on_path(&bin, env);
        // The login that matters is the one in the profile the task process runs under
        // (CLAUDE_CONFIG_DIR = config_dir, else ~/.claude). `credentials_file` is a usage-polling
        // override only and must never vouch for a profile that has no login of its own.
        let login = account
            .config_dir
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| env.get("HOME").map(|h| PathBuf::from(h).join(".claude")))
            .is_some_and(|d| safety::credentials_file_exists(&d));
        let scan = safety::scan_account_settings(account.config_dir.as_deref(), env);
        let mut a = safety::assess_with_settings(env, login, account.subscription_confirmed, &scan);
        if !cli_found {
            a.reasons.push(format!("`{}` CLI not found", bin.display()));
        }
        Detection {
            cli_found,
            billing: a.mode,
            reasons: a.reasons,
        }
    }

    fn workdir_billing_reasons(&self, cwd: &std::path::Path) -> Vec<String> {
        let scan = safety::scan_project_settings(cwd);
        // Unverifiable project settings are treated like API billing: refuse rather than guess.
        scan.api.into_iter().chain(scan.unverified).collect()
    }

    fn command(
        &self,
        account: &Account,
        launch: &TaskLaunch,
        cwd: PathBuf,
        env: &HashMap<String, String>,
    ) -> CommandSpec {
        let mut args = vec!["-p".to_string(), "--output-format".into(), "text".into()];
        if !launch.model.is_empty() {
            // One argument, so a value can never be parsed as a separate flag.
            args.push(format!("--model={}", launch.model));
        }
        if let Some(m) = permission_mode(launch.mode) {
            args.push("--permission-mode".into());
            args.push(m.into());
        }
        CommandSpec {
            program: self.binary_for(account),
            args,
            env: safety::scrubbed_env(env, account.config_dir.as_deref()),
            cwd,
            stdin: launch.prompt.to_string(),
        }
    }

    fn is_rate_limited(&self, output: &str, exit_code: i32) -> bool {
        // Model-generated text often mentions "rate limit", "429" or "quota", so only the CLI's own
        // status line counts: a failing run whose last few lines are an anchored, short limit message.
        exit_code != 0
            && output
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .rev()
                .take(3)
                .any(is_limit_line)
    }
}

/// Translates the neutral run mode into Claude Code's `--permission-mode` value (`None` = pass nothing,
/// so the user's own Claude configuration decides).
fn permission_mode(m: RunMode) -> Option<&'static str> {
    match m {
        RunMode::Inherit => None,
        RunMode::Plan => Some("plan"),
        RunMode::Review => Some("default"),
        RunMode::AutoEdit => Some("acceptEdits"),
    }
}

/// Claude Code limit messages, e.g. `Claude AI usage limit reached|1700000000`,
/// `5-hour limit reached ∙ resets 3pm`, `Weekly limit reached`, `API Error: 429 {"type":"error","error":{"type":"rate_limit_error",..}}`.
fn is_limit_line(line: &str) -> bool {
    if line.chars().count() > 300 {
        return false;
    }
    let l = line.to_lowercase();
    if l.starts_with("claude ai usage limit reached") || l.starts_with("claude usage limit reached")
    {
        return true;
    }
    if l.starts_with("api error") {
        return l.contains("rate_limit_error")
            || l.contains("overloaded_error")
            || l.starts_with("api error: 429");
    }
    // "<short label> limit reached [∙ resets ...]": at most 4 words before "limit reached".
    let head = l.split(['∙', '|']).next().unwrap_or("").trim();
    head.strip_suffix("limit reached")
        .is_some_and(|label| label.split_whitespace().count() <= 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::*;

    fn acct() -> Account {
        Account {
            id: 1,
            provider_id: 1,
            provider_type: "claude".into(),
            label: "A".into(),
            subscription_plan: "Pro".into(),
            billing_mode: BillingMode::Unverified,
            connection_status: ConnectionStatus::Disconnected,
            config_dir: Some("/nonexistent-profile".into()),
            subscription_confirmed: true,
            execution_policy: ExecutionPolicy::Approval,
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

    fn launch<'a>(model: &'a str, prompt: &'a str, mode: RunMode) -> TaskLaunch<'a> {
        TaskLaunch {
            model,
            prompt,
            task_id: 1,
            mode,
        }
    }

    #[test]
    fn usage_credentials_override_does_not_count_as_a_profile_login() {
        let d = tempfile::tempdir().unwrap();
        let other = d.path().join("other-login.json");
        std::fs::write(&other, "{}").unwrap();
        let profile = d.path().join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        let mut a = acct();
        a.config_dir = Some(profile.to_string_lossy().into());
        a.credentials_file = Some(other.to_string_lossy().into());
        let det = ClaudeAdapter::default().detect(&a, &HashMap::new());
        assert_eq!(det.billing, BillingMode::Unverified);
        // The profile's own login makes it verifiable again.
        std::fs::write(profile.join(".credentials.json"), "{}").unwrap();
        let det = ClaudeAdapter::default().detect(&a, &HashMap::new());
        assert_eq!(det.billing, BillingMode::Subscription);
    }

    #[test]
    fn model_is_one_argument_even_if_it_looks_like_a_flag() {
        let env = HashMap::new();
        let ad = ClaudeAdapter::default();
        let spec = ad.command(
            &acct(),
            &launch("--dangerously-skip-permissions", "p", RunMode::Inherit),
            "/w".into(),
            &env,
        );
        assert!(spec
            .args
            .contains(&"--model=--dangerously-skip-permissions".to_string()));
        assert!(!spec
            .args
            .contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!spec.args.contains(&"--model".to_string()));
    }

    #[test]
    fn run_mode_is_translated_to_permission_mode() {
        let env = HashMap::new();
        let ad = ClaudeAdapter::default();
        for (mode, want) in [
            (RunMode::Inherit, None),
            (RunMode::Plan, Some("plan")),
            (RunMode::Review, Some("default")),
            (RunMode::AutoEdit, Some("acceptEdits")),
        ] {
            let spec = ad.command(&acct(), &launch("", "p", mode), "/w".into(), &env);
            let got = spec
                .args
                .iter()
                .position(|a| a == "--permission-mode")
                .map(|i| spec.args[i + 1].as_str());
            assert_eq!(got, want, "{mode:?}");
            assert_eq!(spec.stdin, "p", "prompt stays on stdin");
        }
    }

    #[test]
    fn command_defaults_to_claude_and_honours_account_override() {
        let env = HashMap::new();
        let ad = ClaudeAdapter::default();
        let spec = ad.command(
            &acct(),
            &launch("", "p", RunMode::Inherit),
            "/w".into(),
            &env,
        );
        assert_eq!(spec.program, PathBuf::from("claude"));
        let mut a = acct();
        a.cli_command = Some("/opt/claude-wrap".into());
        let spec = ad.command(&a, &launch("", "p", RunMode::Inherit), "/w".into(), &env);
        assert_eq!(spec.program, PathBuf::from("/opt/claude-wrap"));
        let d = ad.detect(&a, &env);
        assert!(!d.cli_found);
        assert!(d.reasons.iter().any(|r| r.contains("/opt/claude-wrap")));
    }

    #[test]
    fn command_has_no_prompt_in_args_and_no_api_env() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_API_KEY".to_string(), "k".to_string());
        env.insert("PATH".to_string(), "/bin".to_string());
        let spec = ClaudeAdapter::default().command(
            &acct(),
            &launch("sonnet", "secret prompt; rm -rf /", RunMode::Review),
            "/w".into(),
            &env,
        );
        assert!(spec.args.iter().all(|a| !a.contains("secret")));
        assert_eq!(spec.stdin, "secret prompt; rm -rf /");
        assert!(!spec.env.contains_key("ANTHROPIC_API_KEY"));
        assert_eq!(spec.env["CLAUDE_CONFIG_DIR"], "/nonexistent-profile");
    }

    #[test]
    fn detect_flags_api_auth() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_API_KEY".to_string(), "k".to_string());
        assert_eq!(
            ClaudeAdapter::default().detect(&acct(), &env).billing,
            BillingMode::Api
        );
    }

    #[test]
    fn detect_without_login_is_unverified() {
        let d = ClaudeAdapter::default().detect(&acct(), &HashMap::new());
        assert_eq!(d.billing, BillingMode::Unverified);
        assert!(!d.cli_found);
    }

    #[test]
    fn rate_limit_detection() {
        let a = ClaudeAdapter::default();
        assert!(a.is_rate_limited("Claude AI usage limit reached|1700000000", 1));
        assert!(!a.is_rate_limited("all good", 1));
        for m in [
            "5-hour limit reached ∙ resets 3pm",
            "Weekly limit reached ∙ resets Oct 9",
            "Opus weekly limit reached",
            "API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}",
            "API Error: 529 {\"error\":{\"type\":\"overloaded_error\"}}",
        ] {
            assert!(a.is_rate_limited(m, 1), "{m}");
        }
        // Only the tail counts, and only for failing runs.
        assert!(a.is_rate_limited(
            "working...\nlots of output\nClaude AI usage limit reached|1700000000\n",
            1
        ));
        assert!(!a.is_rate_limited("Claude AI usage limit reached|1700000000", 0));
    }

    #[test]
    fn rate_limit_ignores_generated_text() {
        let a = ClaudeAdapter::default();
        for m in [
            "I added exponential backoff for HTTP 429 responses and a rate limit of 5 req/s.",
            "The quota check fails when the limit reached flag is set.",
            "fn handle_429() { /* rate limit */ }",
            "Error: tests failed (3 failed, 12 passed)",
            "We should explain why the usage limit reached message appears in the app",
        ] {
            assert!(!a.is_rate_limited(m, 1), "{m}");
        }
        // A limit message buried in earlier output is not the CLI's status line.
        let buried = "Claude AI usage limit reached|1\na\nb\nc\nd\n";
        assert!(!a.is_rate_limited(buried, 1));
    }
}
