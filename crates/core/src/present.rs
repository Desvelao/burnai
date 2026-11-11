//! Presentation facts shared by every client (texts, severities, counts), so the GUI and the TUI say
//! the same thing (AGENTS.md rule 9). No rendering and no colours here: clients map [`Level`].
use crate::claude_usage::{self, CredentialStatus};
use crate::models::{Account, BillingMode, Confidence};
use crate::tasks::Task;
use crate::usage::UsageSnapshot;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// How a client should colour a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Good,
    Warn,
    Bad,
    Muted,
}

/// True when "burn before reset" is in force for `account` (its override, else the general setting).
pub fn burn_active(settings: &crate::settings::Settings, account: &Account) -> bool {
    let e = settings.effective_for(account);
    e.burn_session_hours.is_some_and(|h| h > 0.0) || e.burn_weekly_hours.is_some_and(|h| h > 0.0)
}

/// "#3 Write docs, #7 Fix build" for a task's dependency ids (looked up in `all`); "none" when empty.
pub fn dependency_names(all: &[Task], deps: &[i64]) -> String {
    if deps.is_empty() {
        return "none".into();
    }
    deps.iter()
        .map(|id| match all.iter().find(|t| t.id == *id) {
            Some(t) => format!("#{id} {}", t.name),
            None => format!("#{id} (deleted)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// State of the account's Claude Code login file (never shows token contents).
pub fn credentials_summary(
    a: &Account,
    env: &HashMap<String, String>,
    now: DateTime<Utc>,
) -> (String, Level) {
    match claude_usage::credentials_path(a, env) {
        None => ("no credentials location".into(), Level::Bad),
        Some(p) => match claude_usage::check_credentials(&p, now) {
            CredentialStatus::Missing => (format!("missing: {}", p.display()), Level::Bad),
            CredentialStatus::Malformed => ("credentials file unreadable".into(), Level::Bad),
            CredentialStatus::Expired => (
                "token expired: run `claude` to log in again".into(),
                Level::Warn,
            ),
            CredentialStatus::Ok { subscription_type } => (
                format!(
                    "login found{}",
                    subscription_type
                        .map(|t| format!(" ({t})"))
                        .unwrap_or_default()
                ),
                Level::Good,
            ),
        },
    }
}

pub fn billing_label(b: BillingMode) -> &'static str {
    match b {
        BillingMode::Subscription => "Subscription",
        BillingMode::Unverified => "Billing NOT verified",
        BillingMode::Api => "API billing: blocked",
    }
}

pub fn billing_level(b: BillingMode) -> Level {
    match b {
        BillingMode::Subscription => Level::Good,
        BillingMode::Unverified => Level::Warn,
        BillingMode::Api => Level::Bad,
    }
}

pub fn priority_label(p: i64) -> &'static str {
    match p {
        0 => "Low",
        2 => "High",
        _ => "Normal",
    }
}

pub fn fmt_minutes(m: i64) -> String {
    if m < 0 {
        return "passed".into();
    }
    if m >= 24 * 60 {
        format!("{} d {} h", m / 1440, (m % 1440) / 60)
    } else if m >= 60 {
        format!("{} h {} min", m / 60, m % 60)
    } else {
        format!("{m} min")
    }
}

/// Where a usage reading came from, in plain words.
pub fn provenance(s: &UsageSnapshot) -> &'static str {
    if s.source == "custom_script" {
        "Reported by your custom script; the app cannot verify it."
    } else if s.confidence == Confidence::Provider {
        "Read via the unofficial claude.ai usage endpoint; can break or be blocked."
    } else {
        "Entered by you; not a provider measurement."
    }
}

/// `(age in minutes, stale)` for a reading under the account's effective freshness limit.
pub fn reading_age(s: &UsageSnapshot, now: DateTime<Utc>, max_age_minutes: u32) -> (i64, bool) {
    let age = (now - s.captured_at).num_minutes();
    (age, age > max_age_minutes as i64)
}

/// Open-task counts by target, plus the task running now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueueCounts {
    /// `(account id, open tasks pinned to it)` in account order.
    pub pinned: Vec<(i64, usize)>,
    pub any_account: usize,
    pub any_provider: usize,
    pub running: Option<String>,
}

pub fn queue_counts(accounts: &[Account], tasks: &[Task]) -> QueueCounts {
    let open = |t: &&Task| !t.status.is_terminal();
    QueueCounts {
        pinned: accounts
            .iter()
            .map(|a| {
                (
                    a.id,
                    tasks
                        .iter()
                        .filter(open)
                        .filter(|t| t.account_id == Some(a.id))
                        .count(),
                )
            })
            .collect(),
        any_account: tasks
            .iter()
            .filter(open)
            .filter(|t| t.provider_type.is_some() && t.account_id.is_none())
            .count(),
        any_provider: tasks
            .iter()
            .filter(open)
            .filter(|t| t.provider_type.is_none())
            .count(),
        running: tasks
            .iter()
            .find(|t| t.status == crate::models::TaskStatus::Running)
            .map(|t| t.name.clone()),
    }
}

/// Accounts a task can be run on right now by hand: those of its provider (all when any provider).
pub fn run_now_options<'a>(accounts: &'a [Account], t: &Task) -> Vec<&'a Account> {
    accounts
        .iter()
        .filter(|a| {
            t.provider_type
                .as_deref()
                .is_none_or(|p| p == a.provider_type)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TaskStatus;

    #[test]
    fn labels_and_times() {
        assert_eq!(fmt_minutes(-3), "passed");
        assert_eq!(fmt_minutes(45), "45 min");
        assert_eq!(fmt_minutes(125), "2 h 5 min");
        assert_eq!(fmt_minutes(3000), "2 d 2 h");
        assert_eq!(priority_label(0), "Low");
        assert_eq!(priority_label(5), "Normal");
        assert_eq!(billing_level(BillingMode::Api), Level::Bad);
        assert!(billing_label(BillingMode::Unverified).contains("NOT"));
    }

    #[test]
    fn counts_and_options() {
        let c = crate::db::open_memory().unwrap();
        let a = crate::accounts::create(&c, "claude", "A", "", None).unwrap();
        let b = crate::accounts::create(&c, "custom", "B", "", None).unwrap();
        let mk = |provider: Option<&str>, account: Option<i64>| {
            crate::tasks::create(
                &c,
                &crate::tasks::NewTask {
                    name: "t".into(),
                    prompt: "p".into(),
                    provider_type: provider.map(Into::into),
                    account_id: account,
                    ..Default::default()
                },
                TaskStatus::Pending,
            )
            .unwrap()
        };
        mk(Some("claude"), Some(a.id));
        mk(Some("claude"), None);
        let any = mk(None, None);
        let accts = vec![a.clone(), b.clone()];
        let q = queue_counts(&accts, &crate::tasks::list(&c).unwrap());
        assert_eq!(q.pinned, vec![(a.id, 1), (b.id, 0)]);
        assert_eq!((q.any_account, q.any_provider), (1, 1));
        assert!(q.running.is_none());
        assert_eq!(
            run_now_options(&accts, &any).len(),
            2,
            "any provider => all"
        );
        let claude = crate::tasks::list(&c)
            .unwrap()
            .into_iter()
            .find(|t| t.provider_type.is_some() && t.account_id.is_none())
            .unwrap();
        assert_eq!(run_now_options(&accts, &claude)[0].id, a.id);
        assert_eq!(run_now_options(&accts, &claude).len(), 1);
    }

    #[test]
    fn dependencies_are_shown_by_id_and_name() {
        let c = crate::db::open_memory().unwrap();
        let mk = |n: &str| {
            crate::tasks::create(
                &c,
                &crate::tasks::NewTask {
                    name: n.into(),
                    prompt: "x".into(),
                    ..Default::default()
                },
                crate::models::TaskStatus::Pending,
            )
            .unwrap()
        };
        let (a, b) = (mk("Write docs"), mk("Fix build"));
        let all = crate::tasks::list(&c).unwrap();
        assert_eq!(dependency_names(&all, &[]), "none");
        assert_eq!(
            dependency_names(&all, &[a.id, b.id]),
            format!("#{} Write docs, #{} Fix build", a.id, b.id)
        );
        assert!(dependency_names(&all, &[9999]).contains("deleted"));
    }

    #[test]
    fn credentials_without_a_login_file_are_reported() {
        let c = crate::db::open_memory().unwrap();
        let mut a = crate::accounts::create(&c, "claude", "A", "", None).unwrap();
        a.credentials_file = Some("/nonexistent/creds.json".into());
        let (t, l) = credentials_summary(&a, &HashMap::new(), Utc::now());
        assert!(t.starts_with("missing"));
        assert_eq!(l, Level::Bad);
    }
}
