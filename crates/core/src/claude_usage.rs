//! Claude subscription usage via the user's own Claude Code login, modelled on
//! `xfce4-ai-providers-usage-plugin`.
//!
//! **Unofficial**: `claude.ai/api/oauth/usage` is undocumented and may change or block clients at any time.
//! The OAuth access token is read from the account's credentials file on every poll, held only in memory,
//! sent only to the usage endpoint, and never logged, stored, or returned to the UI. This module never
//! refreshes tokens and never writes the credentials file (a rotated refresh token could log the CLI out).
use crate::models::{Account, Confidence, UsageSource};
use crate::usage::{self, NewSnapshot};
use crate::{db, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const USAGE_URL: &str = "https://claude.ai/api/oauth/usage";
/// Browser-like UA; the endpoint sits behind Cloudflare bot management. Configurable per account.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
pub const SOURCE_LABEL: &str = "claude_oauth_usage";

/// OAuth access token. Debug/Display never reveal it.
pub struct Token(String);
impl Token {
    fn expose(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[derive(Debug)]
pub struct Credentials {
    pub token: Token,
    /// Unix seconds; 0 = unknown.
    pub expires_at: i64,
    pub subscription_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialStatus {
    Missing,
    Malformed,
    Expired,
    Ok { subscription_type: Option<String> },
}

/// Milliseconds-since-epoch values (> 1e11) are normalised to seconds.
pub fn normalize_expires_at(raw: i64) -> i64 {
    if raw > 100_000_000_000 {
        raw / 1000
    } else {
        raw
    }
}

pub fn parse_credentials(json: &str) -> Option<Credentials> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let o = v.get("claudeAiOauth")?;
    let token = o.get("accessToken")?.as_str().filter(|s| !s.is_empty())?;
    Some(Credentials {
        token: Token(token.to_string()),
        expires_at: o
            .get("expiresAt")
            .and_then(|e| e.as_i64().or_else(|| e.as_f64().map(|f| f as i64)))
            .map(normalize_expires_at)
            .unwrap_or(0),
        subscription_type: o
            .get("subscriptionType")
            .and_then(|s| s.as_str())
            .map(str::to_string),
    })
}

pub fn read_credentials(path: &Path) -> Option<Credentials> {
    parse_credentials(&std::fs::read_to_string(path).ok()?)
}

/// Where this account's credentials live.
pub fn credentials_path(a: &Account, env: &HashMap<String, String>) -> Option<PathBuf> {
    if let Some(f) = &a.credentials_file {
        return Some(PathBuf::from(f));
    }
    let dir = a
        .config_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| env.get("HOME").map(|h| Path::new(h).join(".claude")))?;
    Some(dir.join(".credentials.json"))
}

/// Inspects the credentials without exposing the token.
pub fn check_credentials(path: &Path, now: DateTime<Utc>) -> CredentialStatus {
    let Ok(text) = std::fs::read_to_string(path) else {
        return CredentialStatus::Missing;
    };
    match parse_credentials(&text) {
        None => CredentialStatus::Malformed,
        Some(c) if c.expires_at > 0 && c.expires_at <= now.timestamp() => CredentialStatus::Expired,
        Some(c) => CredentialStatus::Ok {
            subscription_type: c.subscription_type,
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Percent used, 0-100.
    pub used: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageReading {
    pub session: Option<Window>,
    pub weekly: Option<Window>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    /// No usable credentials file/token, or the token is expired/rejected (401). Re-login with `claude`.
    AuthExpired(String),
    /// HTTP 403: typically a Cloudflare bot challenge, not a token problem.
    Blocked(String),
    Network(String),
    Parse(String),
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsageError::AuthExpired(m) => write!(f, "login expired or missing: {m}"),
            UsageError::Blocked(m) => write!(f, "blocked by the usage endpoint (403): {m}"),
            UsageError::Network(m) => write!(f, "network error: {m}"),
            UsageError::Parse(m) => write!(f, "unexpected usage response: {m}"),
        }
    }
}

fn window(v: &serde_json::Value, key: &str) -> Option<Window> {
    let w = v.get(key)?.as_object()?;
    let used = w.get("utilization")?.as_f64()?;
    Some(Window {
        used: used.clamp(0.0, 100.0),
        resets_at: w
            .get("resets_at")
            .and_then(|r| r.as_str())
            .and_then(db::parse_ts),
    })
}

/// Defensive: missing or wrong-typed fields just yield `None` for that window.
pub fn parse_usage_json(body: &str) -> std::result::Result<UsageReading, UsageError> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|_| UsageError::Parse("not JSON".into()))?;
    let r = UsageReading {
        session: window(&v, "five_hour"),
        weekly: window(&v, "seven_day"),
    };
    if r.session.is_none() && r.weekly.is_none() {
        return Err(UsageError::Parse("no five_hour/seven_day data".into()));
    }
    Ok(r)
}

/// Client-hint headers a real Chrome would send for this UA; `None` if the UA is not Chrome-like.
pub fn client_hints_for_ua(ua: &str) -> Option<[(&'static str, String); 3]> {
    let rest = ua.split("Chrome/").nth(1)?;
    let major: u64 = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
        .filter(|m| *m > 0)?;
    Some([
        (
            "sec-ch-ua",
            format!(
                "\"Chromium\";v=\"{major}\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"{major}\""
            ),
        ),
        ("sec-ch-ua-mobile", "?0".to_string()),
        ("sec-ch-ua-platform", "\"Linux\"".to_string()),
    ])
}

pub fn fetch_usage(
    token: &Token,
    user_agent: Option<&str>,
    url: &str,
) -> std::result::Result<UsageReading, UsageError> {
    let ua = user_agent
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_USER_AGENT);
    // The bearer token only ever goes to the https URL asked for: never over plaintext (loopback http is
    // allowed in tests only) and never forwarded by a redirect.
    if !url.starts_with("https://") && !(cfg!(test) && url.starts_with("http://127.0.0.1")) {
        return Err(UsageError::Network("usage URL must be https".into()));
    }
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let mut req = agent
        .get(url)
        .timeout(Duration::from_secs(10))
        .set("Authorization", &format!("Bearer {}", token.expose()))
        .set("Accept", "application/json")
        .set("User-Agent", ua);
    if let Some(hints) = client_hints_for_ua(ua) {
        for (k, v) in hints {
            req = req.set(k, &v);
        }
    }
    match req.call() {
        Ok(resp) if (300..400).contains(&resp.status()) => Err(UsageError::Network(format!(
            "unexpected redirect (HTTP {}); not followed",
            resp.status()
        ))),
        Ok(resp) => {
            let body = resp
                .into_string()
                .map_err(|e| UsageError::Network(e.to_string()))?;
            parse_usage_json(&body)
        }
        Err(ureq::Error::Status(401, _)) => Err(UsageError::AuthExpired(
            "token rejected (401); run `claude` to log in again".into(),
        )),
        Err(ureq::Error::Status(403, _)) => Err(UsageError::Blocked(
            "possibly a bot challenge; try another User-Agent or enter usage manually".into(),
        )),
        Err(ureq::Error::Status(c, _)) => Err(UsageError::Network(format!("HTTP {c}"))),
        // The URL/token are never included in error text.
        Err(ureq::Error::Transport(t)) => Err(UsageError::Network(format!("{:?}", t.kind()))),
    }
}

/// Reads the credentials fresh, fetches usage and records a snapshot (confidence: provider-reported,
/// unofficial endpoint). On any failure nothing is recorded, so older data goes stale and blocks runs.
pub fn poll_account(
    conn: &Connection,
    a: &Account,
    env: &HashMap<String, String>,
    url: &str,
    now: DateTime<Utc>,
) -> std::result::Result<UsageReading, UsageError> {
    if a.usage_source != UsageSource::ClaudeOauth {
        return Err(UsageError::AuthExpired(
            "automatic usage is not enabled for this account".into(),
        ));
    }
    let path = credentials_path(a, env)
        .ok_or_else(|| UsageError::AuthExpired("no credentials location (HOME unset)".into()))?;
    let creds = read_credentials(&path)
        .ok_or_else(|| UsageError::AuthExpired(format!("no usable login in {}", path.display())))?;
    if creds.expires_at > 0 && creds.expires_at <= now.timestamp() {
        return Err(UsageError::AuthExpired(
            "access token expired; run `claude` to log in again".into(),
        ));
    }
    let reading = fetch_usage(&creds.token, a.user_agent.as_deref(), url)?;
    let snap = NewSnapshot {
        account_id: a.id,
        session_remaining: reading.session.as_ref().map(|w| 100.0 - w.used),
        weekly_remaining: reading.weekly.as_ref().map(|w| 100.0 - w.used),
        session_reset_at: reading.session.as_ref().and_then(|w| w.resets_at),
        weekly_reset_at: reading.weekly.as_ref().and_then(|w| w.resets_at),
        confidence: Confidence::Provider,
        source: SOURCE_LABEL.into(),
    };
    usage::record(conn, &snap, now).map_err(|e| UsageError::Parse(e.to_string()))?;
    if a.subscription_plan.is_empty() {
        if let Some(t) = creds.subscription_type.filter(|t| !t.is_empty()) {
            let _ = crate::accounts::set_plan(conn, a.id, &t);
        }
    }
    Ok(reading)
}

#[derive(Debug, Clone)]
pub struct DiscoveredProfile {
    pub profile_dir: String,
    pub label_guess: String,
    pub subscription_type: Option<String>,
}

/// Scans `~/.claude*` and `$CLAUDE_CONFIG_DIR` for directories with a parseable `.credentials.json`.
pub fn discover_profiles(env: &HashMap<String, String>) -> Vec<DiscoveredProfile> {
    let mut dirs: Vec<PathBuf> = vec![];
    if let Some(h) = env.get("HOME") {
        if let Ok(rd) = std::fs::read_dir(h) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(".claude") && e.path().is_dir() {
                    dirs.push(e.path());
                }
            }
        }
    }
    if let Some(d) = env.get("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(d));
    }
    dirs.sort();
    dirs.dedup();
    dirs.into_iter()
        .filter_map(|d| {
            let c = read_credentials(&d.join(".credentials.json"))?;
            Some(DiscoveredProfile {
                label_guess: d
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                profile_dir: d.to_string_lossy().into_owned(),
                subscription_type: c.subscription_type,
            })
        })
        .collect()
}

/// Convenience used by the service: poll with the real endpoint.
pub fn poll_account_default(
    conn: &Connection,
    a: &Account,
    env: &HashMap<String, String>,
) -> Result<std::result::Result<UsageReading, UsageError>> {
    Ok(poll_account(conn, a, env, USAGE_URL, Utc::now()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const GOOD: &str = r#"{"five_hour":{"utilization":37.5,"resets_at":"2030-01-01T05:00:00Z"},"seven_day":{"utilization":60,"resets_at":"2030-01-05T00:00:00Z"}}"#;

    /// One-shot HTTP stub. Returns (base url, captured raw request).
    fn stub(status: u16, body: &'static str) -> (String, Arc<Mutex<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/oauth/usage", l.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(String::new()));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = l.accept() {
                let mut buf = vec![0u8; 8192];
                let n = c.read(&mut buf).unwrap_or(0);
                *s2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                let _ = write!(
                    c,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, seen)
    }

    fn setup(
        token_json: &str,
    ) -> (
        tempfile::TempDir,
        Connection,
        Account,
        HashMap<String, String>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), token_json).unwrap();
        let c = db::open_memory().unwrap();
        let a =
            accounts::create(&c, "claude", "A", "", Some(dir.path().to_str().unwrap())).unwrap();
        let a = accounts::update(
            &c,
            a.id,
            &accounts::AccountUpdate {
                custom: None,
                label: "A".into(),
                plan: "".into(),
                config_dir: a.config_dir.clone(),
                credentials_file: None,
                usage_source: UsageSource::ClaudeOauth,
                user_agent: Some("Mozilla/5.0 Chrome/140.0.0.0 TestUA".into()),
                cli_command: None,
                availability_schedule: String::new(),
            },
        )
        .unwrap();
        (dir, c, a, HashMap::new())
    }
    fn creds(exp_ms: i64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-secret-token","refreshToken":"r","expiresAt":{exp_ms},"subscriptionType":"max"}}}}"#
        )
    }
    fn future_ms() -> i64 {
        (Utc::now().timestamp() + 3600) * 1000
    }

    #[test]
    fn credentials_parsing_and_expiry_normalisation() {
        let c = parse_credentials(&creds(1_900_000_000_000)).unwrap();
        assert_eq!(c.expires_at, 1_900_000_000);
        assert_eq!(c.subscription_type.as_deref(), Some("max"));
        assert_eq!(normalize_expires_at(1_900_000_000), 1_900_000_000);
        assert!(parse_credentials("{}").is_none());
        assert!(parse_credentials("not json").is_none());
        assert!(parse_credentials(r#"{"claudeAiOauth":{"accessToken":""}}"#).is_none());
        assert_eq!(
            parse_credentials(r#"{"claudeAiOauth":{"accessToken":"x"}}"#)
                .unwrap()
                .expires_at,
            0
        );
    }

    #[test]
    fn token_is_redacted_in_debug() {
        let c = parse_credentials(&creds(0)).unwrap();
        assert!(!format!("{c:?}").contains("sk-secret-token"));
    }

    #[test]
    fn usage_json_parsing_is_defensive() {
        let r = parse_usage_json(GOOD).unwrap();
        assert_eq!(r.session.as_ref().unwrap().used, 37.5);
        assert!(r.weekly.as_ref().unwrap().resets_at.is_some());
        let p = parse_usage_json(r#"{"five_hour":{"utilization":10},"seven_day":"oops"}"#).unwrap();
        assert!(p.session.is_some() && p.weekly.is_none());
        assert!(parse_usage_json("{}").is_err());
        assert!(parse_usage_json("<html>").is_err());
        let c = parse_usage_json(r#"{"five_hour":{"utilization":250}}"#).unwrap();
        assert_eq!(c.session.unwrap().used, 100.0);
    }

    #[test]
    fn client_hints() {
        let h = client_hints_for_ua(DEFAULT_USER_AGENT).unwrap();
        assert!(h[0].1.contains("v=\"131\""));
        assert_eq!(h[1].1, "?0");
        assert!(client_hints_for_ua("curl/8.0").is_none());
        assert!(client_hints_for_ua("Chrome/abc").is_none());
    }

    #[test]
    fn credential_status_without_exposing_token() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.json");
        let now = Utc::now();
        assert_eq!(check_credentials(&p, now), CredentialStatus::Missing);
        std::fs::write(&p, "junk").unwrap();
        assert_eq!(check_credentials(&p, now), CredentialStatus::Malformed);
        std::fs::write(&p, creds(1_000_000_000_000)).unwrap();
        assert_eq!(check_credentials(&p, now), CredentialStatus::Expired);
        std::fs::write(&p, creds(future_ms())).unwrap();
        assert_eq!(
            check_credentials(&p, now),
            CredentialStatus::Ok {
                subscription_type: Some("max".into())
            }
        );
    }

    #[test]
    fn success_records_provider_snapshot_with_headers() {
        let (_d, c, a, env) = setup(&creds(future_ms()));
        let (url, seen) = stub(200, GOOD);
        let r = poll_account(&c, &a, &env, &url, Utc::now()).unwrap();
        assert_eq!(r.session.unwrap().used, 37.5);
        let s = usage::latest(&c, a.id).unwrap().unwrap();
        assert_eq!(s.session_remaining, Some(62.5));
        assert_eq!(s.weekly_remaining, Some(40.0));
        assert_eq!(s.confidence, Confidence::Provider);
        assert_eq!(s.source, SOURCE_LABEL);
        assert!(s.session_reset_at.is_some());
        let req = seen.lock().unwrap().to_lowercase();
        assert!(req.contains("authorization: bearer sk-secret-token"));
        assert!(req.contains("user-agent: mozilla/5.0 chrome/140.0.0.0 testua"));
        assert!(req.contains("sec-ch-ua: \"chromium\";v=\"140\""));
        assert_eq!(
            accounts::get(&c, a.id).unwrap().subscription_plan,
            "max",
            "plan auto-filled"
        );
    }

    #[test]
    fn redirects_are_not_followed_and_token_not_forwarded() {
        // Target server that must never be contacted.
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let loc = format!("http://{}/steal", target.local_addr().unwrap());
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/oauth/usage", l.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = l.accept() {
                let mut buf = vec![0u8; 8192];
                let _ = c.read(&mut buf);
                let _ = write!(
                    c,
                    "HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        let (_d, c, a, env) = setup(&creds(future_ms()));
        assert!(matches!(
            poll_account(&c, &a, &env, &url, Utc::now()),
            Err(UsageError::Network(_))
        ));
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            target.accept().is_err(),
            "redirect target must receive no connection"
        );
        assert!(usage::latest(&c, a.id).unwrap().is_none());
    }

    #[test]
    fn non_https_url_is_refused_before_any_request() {
        let (_d, c, a, env) = setup(&creds(future_ms()));
        assert!(matches!(
            poll_account(&c, &a, &env, "http://example.com/x", Utc::now()),
            Err(UsageError::Network(m)) if m.contains("https")
        ));
        assert!(matches!(
            poll_account(&c, &a, &env, "ftp://127.0.0.1/x", Utc::now()),
            Err(UsageError::Network(_))
        ));
    }

    #[test]
    fn http_errors_map_and_record_nothing() {
        let (_d, c, a, env) = setup(&creds(future_ms()));
        let (u401, _) = stub(401, "{}");
        assert!(matches!(
            poll_account(&c, &a, &env, &u401, Utc::now()),
            Err(UsageError::AuthExpired(_))
        ));
        let (u403, _) = stub(403, "<html>Just a moment</html>");
        assert!(matches!(
            poll_account(&c, &a, &env, &u403, Utc::now()),
            Err(UsageError::Blocked(_))
        ));
        let (u200, _) = stub(200, "{\"x\":1}");
        assert!(matches!(
            poll_account(&c, &a, &env, &u200, Utc::now()),
            Err(UsageError::Parse(_))
        ));
        assert!(matches!(
            poll_account(&c, &a, &env, "http://127.0.0.1:1/x", Utc::now()),
            Err(UsageError::Network(_))
        ));
        assert!(usage::latest(&c, a.id).unwrap().is_none());
    }

    #[test]
    fn expired_or_missing_token_makes_no_request() {
        let (_d, c, a, env) = setup(&creds(1_000_000_000_000));
        // Unroutable URL: would be a Network error if a request were attempted.
        let e = poll_account(&c, &a, &env, "http://127.0.0.1:1/x", Utc::now()).unwrap_err();
        assert!(matches!(e, UsageError::AuthExpired(_)), "{e}");
        let d2 = tempfile::tempdir().unwrap();
        let mut a2 = a.clone();
        a2.config_dir = Some(d2.path().to_str().unwrap().into());
        assert!(matches!(
            poll_account(&c, &a2, &env, "http://127.0.0.1:1/x", Utc::now()),
            Err(UsageError::AuthExpired(_))
        ));
    }

    #[test]
    fn failed_poll_leaves_the_old_snapshot_which_goes_stale_and_blocks() {
        use crate::usage::{check_capacity, CapacityBlock, CapacityRules, NewSnapshot};
        let rules = CapacityRules::uniform(30, 10.0, 5.0);
        let captured = Utc::now() - chrono::Duration::hours(2);
        // Failure modes: expired credentials, HTTP 401, unreachable endpoint. None may record anything.
        for mode in ["expired", "http401", "network"] {
            let exp = if mode == "expired" {
                1_000_000_000_000
            } else {
                future_ms()
            };
            let (_d, c, a, env) = setup(&creds(exp));
            usage::record(
                &c,
                &NewSnapshot {
                    account_id: a.id,
                    session_remaining: Some(90.0),
                    weekly_remaining: Some(90.0),
                    session_reset_at: Some(Utc::now() + chrono::Duration::hours(3)),
                    weekly_reset_at: Some(Utc::now() + chrono::Duration::days(3)),
                    confidence: Confidence::Provider,
                    source: "test".into(),
                },
                captured,
            )
            .unwrap();
            let before = usage::latest(&c, a.id).unwrap().unwrap();
            let url = match mode {
                "http401" => stub(401, "{}").0,
                _ => "http://127.0.0.1:1/x".to_string(),
            };
            assert!(
                poll_account(&c, &a, &env, &url, Utc::now()).is_err(),
                "{mode}"
            );
            let after = usage::latest(&c, a.id).unwrap().unwrap();
            assert_eq!(after.id, before.id, "{mode}: no new snapshot recorded");
            assert!(
                matches!(
                    check_capacity(Some(&after), Utc::now(), rules, 5.0, None),
                    Err(CapacityBlock::Stale { .. })
                ),
                "{mode}: old reading must be stale and block"
            );
        }
    }

    #[test]
    fn manual_accounts_are_not_polled() {
        let (_d, c, mut a, env) = setup(&creds(future_ms()));
        a.usage_source = UsageSource::Manual;
        assert!(poll_account(&c, &a, &env, "http://127.0.0.1:1/x", Utc::now()).is_err());
    }

    #[test]
    fn credential_file_is_never_modified() {
        let (d, c, a, env) = setup(&creds(future_ms()));
        let before = std::fs::read(d.path().join(".credentials.json")).unwrap();
        let (url, _) = stub(200, GOOD);
        poll_account(&c, &a, &env, &url, Utc::now()).unwrap();
        assert_eq!(
            std::fs::read(d.path().join(".credentials.json")).unwrap(),
            before
        );
    }

    #[test]
    fn discovery_finds_profiles() {
        let home = tempfile::tempdir().unwrap();
        for (n, ok) in [
            (".claude", true),
            (".claude-work", true),
            (".claude-bad", false),
            (".other", true),
        ] {
            let d = home.path().join(n);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(
                d.join(".credentials.json"),
                if ok { creds(0) } else { "x".into() },
            )
            .unwrap();
        }
        let env = HashMap::from([(
            "HOME".to_string(),
            home.path().to_string_lossy().into_owned(),
        )]);
        let found = discover_profiles(&env);
        let names: Vec<_> = found.iter().map(|p| p.label_guess.as_str()).collect();
        assert_eq!(names, vec![".claude", ".claude-work"]);
        assert_eq!(found[0].subscription_type.as_deref(), Some("max"));
    }
}
