//! Subscription-only billing guard. A successful CLI invocation does NOT prove subscription billing,
//! so subscription mode requires: no API/cloud-billing env vars, a local login present, and the user's
//! explicit confirmation. Anything else blocks execution.
use crate::models::BillingMode;
use std::collections::HashMap;
use std::path::Path;

/// Environment variables that switch Claude Code to pay-as-you-go / cloud billing.
pub const BILLING_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "AWS_BEARER_TOKEN_BEDROCK",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
];

/// Outcome of inspecting Claude Code settings files for billing-relevant configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsScan {
    /// Settings that switch to API / cloud billing (⇒ `BillingMode::Api`).
    pub api: Vec<String>,
    /// Files that exist but could not be verified (⇒ at best `Unverified`).
    pub unverified: Vec<String>,
}

impl SettingsScan {
    fn merge(&mut self, o: SettingsScan) {
        self.api.extend(o.api);
        self.unverified.extend(o.unverified);
    }
}

/// Inspects one settings file. Missing file = clean. Values are never copied into messages, only key names.
pub fn scan_settings_file(path: &Path) -> SettingsScan {
    let mut r = SettingsScan::default();
    let name = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return r,
        Err(e) => {
            r.unverified
                .push(format!("{name}: cannot read ({})", e.kind()));
            return r;
        }
    };
    let Ok(serde_json::Value::Object(o)) = serde_json::from_str::<serde_json::Value>(&text) else {
        r.unverified
            .push(format!("{name}: not a valid JSON settings object"));
        return r;
    };
    let non_empty = |k: &str| match o.get(k) {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
        Some(_) => true,
    };
    for k in [
        "apiKeyHelper",
        "awsAuthRefresh",
        "awsCredentialExport",
        "gcpAuthRefresh",
    ] {
        if non_empty(k) {
            r.api.push(format!("{name} sets `{k}`"));
        }
    }
    if o.get("forceLoginMethod").and_then(|v| v.as_str()) == Some("console") {
        r.api.push(format!(
            "{name} sets forceLoginMethod=console (API billing)"
        ));
    }
    match o.get("env") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Object(env)) => {
            for k in BILLING_ENV_VARS {
                let set = env.get(*k).is_some_and(|v| match v {
                    serde_json::Value::String(s) => !s.is_empty(),
                    serde_json::Value::Null => false,
                    _ => true,
                });
                if set {
                    r.api.push(format!("{name} sets env {k}"));
                }
            }
        }
        Some(_) => r.unverified.push(format!("{name}: `env` is not an object")),
    }
    r
}

/// Account-level settings Claude Code reads: `<config dir>/settings.json` (default `$HOME/.claude`) and the
/// system-wide managed settings.
pub fn scan_account_settings(
    config_dir: Option<&str>,
    env: &HashMap<String, String>,
) -> SettingsScan {
    let mut r = SettingsScan::default();
    let dir = config_dir
        .map(std::path::PathBuf::from)
        .or_else(|| env.get("HOME").map(|h| Path::new(h).join(".claude")));
    match dir {
        Some(d) => r.merge(scan_settings_file(&d.join("settings.json"))),
        None => r
            .unverified
            .push("cannot locate the Claude settings directory".into()),
    }
    r.merge(scan_settings_file(Path::new(
        "/etc/claude-code/managed-settings.json",
    )));
    r
}

/// Project-level settings (`.claude/settings.json`, `.claude/settings.local.json`) for a task working
/// directory and its ancestors up to the enclosing git repository root (or just the directory itself).
pub fn scan_project_settings(workdir: &Path) -> SettingsScan {
    let mut r = SettingsScan::default();
    for d in workdir.ancestors() {
        for f in ["settings.json", "settings.local.json"] {
            r.merge(scan_settings_file(&d.join(".claude").join(f)));
        }
        if d.join(".git").exists() {
            break;
        }
    }
    r
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingAssessment {
    pub mode: BillingMode,
    pub reasons: Vec<String>,
}

/// Which billing-related env vars are set (names only; values are never read into messages).
pub fn offending_env(env: &HashMap<String, String>) -> Vec<&'static str> {
    BILLING_ENV_VARS
        .iter()
        .copied()
        .filter(|k| env.get(*k).is_some_and(|v| !v.is_empty()))
        .collect()
}

/// `login_present`: a credentials file exists in the account's config dir (contents are never read).
pub fn assess(
    env: &HashMap<String, String>,
    login_present: bool,
    user_confirmed: bool,
) -> BillingAssessment {
    let bad = offending_env(env);
    if !bad.is_empty() {
        return BillingAssessment {
            mode: BillingMode::Api,
            reasons: vec![format!(
                "API/cloud billing environment variable(s) set: {}",
                bad.join(", ")
            )],
        };
    }
    let mut reasons = vec![];
    if !login_present {
        reasons.push("no subscription login found for this account".to_string());
    }
    if !user_confirmed {
        reasons.push("you have not confirmed this account uses a subscription login".to_string());
    }
    if reasons.is_empty() {
        BillingAssessment {
            mode: BillingMode::Subscription,
            reasons,
        }
    } else {
        BillingAssessment {
            mode: BillingMode::Unverified,
            reasons,
        }
    }
}

/// Like [`assess`], additionally taking the result of scanning Claude Code settings files: API-billing
/// settings make the mode `Api`; unverifiable settings files make it `Unverified`.
pub fn assess_with_settings(
    env: &HashMap<String, String>,
    login_present: bool,
    user_confirmed: bool,
    scan: &SettingsScan,
) -> BillingAssessment {
    let mut a = assess(env, login_present, user_confirmed);
    if !scan.api.is_empty() {
        a.mode = BillingMode::Api;
        a.reasons.extend(scan.api.iter().cloned());
    } else if !scan.unverified.is_empty() {
        if a.mode == BillingMode::Subscription {
            a.mode = BillingMode::Unverified;
        }
        a.reasons.extend(scan.unverified.iter().cloned());
    }
    a
}

pub fn credentials_file_exists(config_dir: &Path) -> bool {
    config_dir.join(".credentials.json").is_file()
}

/// Environment for child processes: parent env minus every billing variable, plus the account profile dir.
pub fn scrubbed_env(
    parent: &HashMap<String, String>,
    config_dir: Option<&str>,
) -> HashMap<String, String> {
    let mut e: HashMap<String, String> = parent
        .iter()
        .filter(|(k, _)| !BILLING_ENV_VARS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(d) = config_dir {
        e.insert("CLAUDE_CONFIG_DIR".into(), d.to_string());
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn api_key_detected() {
        let a = assess(&env(&[("ANTHROPIC_API_KEY", "x")]), true, true);
        assert_eq!(a.mode, BillingMode::Api);
    }
    #[test]
    fn cloud_billing_detected() {
        assert_eq!(
            assess(&env(&[("CLAUDE_CODE_USE_BEDROCK", "1")]), true, true).mode,
            BillingMode::Api
        );
    }
    #[test]
    fn unverifiable_billing_blocks() {
        assert_eq!(assess(&env(&[]), true, false).mode, BillingMode::Unverified);
        assert_eq!(assess(&env(&[]), false, true).mode, BillingMode::Unverified);
    }
    #[test]
    fn subscription_requires_login_and_confirmation() {
        assert_eq!(
            assess(&env(&[]), true, true).mode,
            BillingMode::Subscription
        );
    }
    #[test]
    fn empty_env_value_is_not_api() {
        assert_eq!(
            assess(&env(&[("ANTHROPIC_API_KEY", "")]), true, true).mode,
            BillingMode::Subscription
        );
    }
    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn extended_env_denylist() {
        for k in [
            "ANTHROPIC_FOUNDRY_API_KEY",
            "ANTHROPIC_FOUNDRY_RESOURCE",
            "ANTHROPIC_VERTEX_PROJECT_ID",
            "ANTHROPIC_CUSTOM_HEADERS",
            "ANTHROPIC_BEDROCK_BASE_URL",
        ] {
            assert_eq!(
                assess(&env(&[(k, "x")]), true, true).mode,
                BillingMode::Api,
                "{k}"
            );
            assert!(
                !scrubbed_env(&env(&[(k, "x")]), None).contains_key(k),
                "{k}"
            );
        }
    }

    #[test]
    fn config_dir_settings_with_api_billing_block() {
        let none = env(&[]);
        for (i, body) in [
            r#"{"apiKeyHelper":"/bin/get-key"}"#,
            r#"{"env":{"ANTHROPIC_API_KEY":"sk-x"}}"#,
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"t"}}"#,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://proxy"}}"#,
            r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#,
            r#"{"env":{"CLAUDE_CODE_USE_VERTEX":"1"}}"#,
            r#"{"forceLoginMethod":"console"}"#,
            r#"{"awsAuthRefresh":"aws sso login"}"#,
        ]
        .iter()
        .enumerate()
        {
            let d = tempfile::tempdir().unwrap();
            write(d.path(), "settings.json", body);
            let scan = scan_account_settings(d.path().to_str(), &none);
            let a = assess_with_settings(&none, true, true, &scan);
            assert_eq!(a.mode, BillingMode::Api, "case {i}: {body}");
            assert!(
                !a.reasons.iter().any(|r| r.contains("sk-x")),
                "no secret values in reasons"
            );
        }
    }

    #[test]
    fn benign_or_missing_settings_stay_subscription() {
        let none = env(&[]);
        let d = tempfile::tempdir().unwrap();
        let scan = scan_account_settings(d.path().to_str(), &none);
        assert_eq!(
            assess_with_settings(&none, true, true, &scan).mode,
            BillingMode::Subscription
        );
        write(
            d.path(),
            "settings.json",
            r#"{"theme":"dark","env":{"FOO":"bar","ANTHROPIC_API_KEY":""},"apiKeyHelper":"","forceLoginMethod":"claudeai"}"#,
        );
        let scan = scan_account_settings(d.path().to_str(), &none);
        assert_eq!(
            assess_with_settings(&none, true, true, &scan).mode,
            BillingMode::Subscription
        );
    }

    #[test]
    fn unreadable_or_malformed_settings_are_unverified() {
        let none = env(&[]);
        for body in ["{not json", "[]", r#"{"env":"x"}"#] {
            let d = tempfile::tempdir().unwrap();
            write(d.path(), "settings.json", body);
            let scan = scan_account_settings(d.path().to_str(), &none);
            assert_eq!(
                assess_with_settings(&none, true, true, &scan).mode,
                BillingMode::Unverified,
                "{body}"
            );
        }
    }

    #[test]
    fn project_settings_found_up_to_git_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let sub = root.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(scan_project_settings(&sub), SettingsScan::default());
        write(
            root.path(),
            ".claude/settings.local.json",
            r#"{"env":{"ANTHROPIC_API_KEY":"k"}}"#,
        );
        assert!(!scan_project_settings(&sub).api.is_empty());
        write(&sub, ".claude/settings.json", r#"{"apiKeyHelper":"x"}"#);
        assert!(scan_project_settings(&sub).api.len() >= 2);
    }

    #[test]
    fn child_env_is_scrubbed() {
        let e = scrubbed_env(
            &env(&[
                ("ANTHROPIC_API_KEY", "k"),
                ("PATH", "/bin"),
                ("CLAUDE_CODE_USE_VERTEX", "1"),
            ]),
            Some("/p"),
        );
        assert!(!e.contains_key("ANTHROPIC_API_KEY"));
        assert!(!e.contains_key("CLAUDE_CODE_USE_VERTEX"));
        assert_eq!(e["PATH"], "/bin");
        assert_eq!(e["CLAUDE_CONFIG_DIR"], "/p");
    }
}
