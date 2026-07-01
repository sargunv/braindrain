use std::env;

use braindrain_core::{
    AccountIdentity, Provider, ProviderCredentialField, ProviderCredentialSchema,
    ProviderCredentials, ProviderError, ProviderFuture, ProviderId, ProviderSnapshot,
    ProviderSource, RateWindow, RefreshContext, UsageSnapshot,
};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, COOKIE, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai";
pub const OPENCODE_GO_PAGE: &str = "go";
pub const OPENCODE_AUTH_COOKIE_ENV: &str = "OPENCODE_AUTH_COOKIE";
pub const OPENCODE_WORKSPACE_ID_ENV: &str = "OPENCODE_WORKSPACE_ID";
pub const OPENCODE_KEYCHAIN_SERVICE: &str = "braindrain-opencode-go";
pub const OPENCODE_KEYCHAIN_ACCOUNT: &str = "opencode-go";
pub const WORKSPACE_ID_FIELD: &str = "workspace_id";
pub const AUTH_COOKIE_FIELD: &str = "auth_cookie";

#[derive(Debug, Clone)]
pub struct OpenCodeGoProvider {
    config: OpenCodeGoProviderConfig,
    client: reqwest::Client,
}

impl OpenCodeGoProvider {
    pub fn new(config: OpenCodeGoProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { config, client }
    }

    pub fn config(&self) -> &OpenCodeGoProviderConfig {
        &self.config
    }

    pub fn credentials(&self) -> Result<OpenCodeGoCredentials, OpenCodeGoProviderError> {
        self.config.credentials()
    }

    pub async fn credentials_async(
        &self,
    ) -> Result<OpenCodeGoCredentials, OpenCodeGoProviderError> {
        self.config.credentials_async().await
    }

    pub fn credential_schema() -> ProviderCredentialSchema {
        ProviderCredentialSchema {
            provider: ProviderId::opencode_go(),
            fields: vec![
                ProviderCredentialField {
                    id: WORKSPACE_ID_FIELD.to_owned(),
                    label: "Workspace URL or ID".to_owned(),
                    secret: false,
                },
                ProviderCredentialField {
                    id: AUTH_COOKIE_FIELD.to_owned(),
                    label: "Auth cookie".to_owned(),
                    secret: true,
                },
            ],
        }
    }

    pub async fn store_credentials(
        credentials: ProviderCredentials,
    ) -> Result<(), OpenCodeGoProviderError> {
        let workspace_id = credentials
            .values
            .get(WORKSPACE_ID_FIELD)
            .map(|value| normalize_workspace_id(value))
            .filter(|value| !value.is_empty())
            .ok_or(OpenCodeGoProviderError::MissingField(WORKSPACE_ID_FIELD))?;
        let auth_cookie = credentials
            .values
            .get(AUTH_COOKIE_FIELD)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or(OpenCodeGoProviderError::MissingField(AUTH_COOKIE_FIELD))?;

        let payload = serde_json::to_string(&StoredCredentials {
            workspace_id,
            auth_cookie,
        })
        .map_err(OpenCodeGoProviderError::Serialize)?;

        tokio::task::spawn_blocking(move || -> Result<(), OpenCodeGoProviderError> {
            let entry = keyring::Entry::new(OPENCODE_KEYCHAIN_SERVICE, OPENCODE_KEYCHAIN_ACCOUNT)
                .map_err(keychain_error)?;
            entry.set_password(&payload).map_err(keychain_error)
        })
        .await
        .map_err(|error| OpenCodeGoProviderError::Keychain(error.to_string()))?
    }

    pub async fn delete_credentials() -> Result<(), OpenCodeGoProviderError> {
        tokio::task::spawn_blocking(|| -> Result<(), OpenCodeGoProviderError> {
            let entry = keyring::Entry::new(OPENCODE_KEYCHAIN_SERVICE, OPENCODE_KEYCHAIN_ACCOUNT)
                .map_err(keychain_error)?;
            match entry.delete_credential() {
                Ok(()) => Ok(()),
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(keychain_error(error)),
            }
        })
        .await
        .map_err(|error| OpenCodeGoProviderError::Keychain(error.to_string()))?
    }

    async fn fetch_usage(
        &self,
        now: OffsetDateTime,
    ) -> Result<UsageSnapshot, OpenCodeGoProviderError> {
        let credentials = self.config.credentials_async().await?;
        let workspace_id = normalize_workspace_id(&credentials.workspace_id);
        if workspace_id.is_empty() {
            return Err(OpenCodeGoProviderError::MissingCredentials);
        }

        let url = self
            .config
            .base_url
            .join(&format!("workspace/{workspace_id}/{OPENCODE_GO_PAGE}"))
            .map_err(OpenCodeGoProviderError::Url)?;

        // The workspace page is server-rendered and intermittently streams a
        // "Loading..." shell or only partially hydrates a window, so retry until
        // all three windows are present (best-effort after the final attempt).
        const MAX_ATTEMPTS: usize = 3;
        let mut best: Vec<RateWindow> = Vec::new();
        let mut last_byte_len = 0usize;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
            }
            let html = self.fetch_html(&url, &credentials.auth_cookie).await?;
            last_byte_len = html.len();
            let windows = build_windows(&html, now);
            if has_all_windows(&windows) {
                return Ok(UsageSnapshot {
                    windows,
                    balances: Vec::new(),
                    reset_credits: Vec::new(),
                });
            }
            if windows.len() > best.len() {
                best = windows;
            }
        }

        if best.is_empty() {
            return Err(OpenCodeGoProviderError::Parse(format!(
                "OpenCode Go usage data was not found on the workspace page after \
                 {MAX_ATTEMPTS} attempts (last response was {last_byte_len} bytes; \
                 the page may still be loading)"
            )));
        }

        Ok(UsageSnapshot {
            windows: best,
            balances: Vec::new(),
            reset_credits: Vec::new(),
        })
    }

    async fn fetch_html(
        &self,
        url: &Url,
        auth_cookie: &str,
    ) -> Result<String, OpenCodeGoProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("auth={auth_cookie}"))
                .map_err(|_| OpenCodeGoProviderError::CookieInvalid)?,
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/120.0.0.0 Safari/537.36",
            ),
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
        );

        let response = self
            .client
            .get(url.clone())
            .headers(headers)
            .send()
            .await
            .map_err(OpenCodeGoProviderError::Http)?;
        let status = response.status();

        if status.is_redirection()
            || status == StatusCode::UNAUTHORIZED
            || status == StatusCode::FORBIDDEN
        {
            return Err(OpenCodeGoProviderError::CookieInvalid);
        }
        if !status.is_success() {
            let body = body_preview(&response.bytes().await.unwrap_or_default());
            return Err(OpenCodeGoProviderError::ApiStatus {
                status: status.as_u16(),
                body,
            });
        }

        response.text().await.map_err(OpenCodeGoProviderError::Http)
    }
}

impl Default for OpenCodeGoProvider {
    fn default() -> Self {
        Self::new(OpenCodeGoProviderConfig::default())
    }
}

impl Provider for OpenCodeGoProvider {
    fn id(&self) -> ProviderId {
        ProviderId::opencode_go()
    }

    fn refresh<'a>(
        &'a self,
        context: RefreshContext,
    ) -> ProviderFuture<'a, Result<ProviderSnapshot, ProviderError>> {
        Box::pin(async move {
            let usage = self
                .fetch_usage(context.now)
                .await
                .map_err(ProviderError::from)?;
            Ok(ProviderSnapshot {
                provider: ProviderId::opencode_go(),
                source: ProviderSource::Web,
                usage,
                identity: Some(AccountIdentity {
                    email: None,
                    plan: Some("Zen Go".to_owned()),
                }),
                updated_at: context.now,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeGoCredentials {
    pub workspace_id: String,
    pub auth_cookie: String,
    pub source: OpenCodeGoCredentialsSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenCodeGoCredentialsSource {
    Config,
    Environment,
    Keyring,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenCodeGoProviderConfig {
    pub credentials: Option<OpenCodeGoCredentials>,
    pub base_url: Url,
}

impl OpenCodeGoProviderConfig {
    pub fn credentials(&self) -> Result<OpenCodeGoCredentials, OpenCodeGoProviderError> {
        if let Some(credentials) = self.resolved_credentials() {
            return Ok(credentials);
        }
        if let Some(credentials) = keyring_credentials_blocking()? {
            return Ok(credentials);
        }
        Err(OpenCodeGoProviderError::MissingCredentials)
    }

    async fn credentials_async(&self) -> Result<OpenCodeGoCredentials, OpenCodeGoProviderError> {
        if let Some(credentials) = self.resolved_credentials() {
            return Ok(credentials);
        }
        if let Some(credentials) = keyring_credentials_async().await? {
            return Ok(credentials);
        }
        Err(OpenCodeGoProviderError::MissingCredentials)
    }

    fn resolved_credentials(&self) -> Option<OpenCodeGoCredentials> {
        if let Some(credentials) = self.credentials.as_ref()
            && !credentials.workspace_id.is_empty()
            && !credentials.auth_cookie.is_empty()
        {
            return Some(credentials.clone());
        }

        let workspace_id = env::var_os(OPENCODE_WORKSPACE_ID_ENV)
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.is_empty());
        let auth_cookie = env::var_os(OPENCODE_AUTH_COOKIE_ENV)
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.is_empty());

        match (workspace_id, auth_cookie) {
            (Some(workspace_id), Some(auth_cookie)) => Some(OpenCodeGoCredentials {
                workspace_id,
                auth_cookie,
                source: OpenCodeGoCredentialsSource::Environment,
            }),
            _ => None,
        }
    }
}

impl Default for OpenCodeGoProviderConfig {
    fn default() -> Self {
        Self {
            credentials: None,
            base_url: Url::parse(OPENCODE_GO_BASE_URL).expect("valid OpenCode base URL"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RawWindow {
    status: String,
    reset_in_sec: u64,
    used_percent: f64,
}

fn rate_window(id: &str, label: &str, raw: &RawWindow, now: OffsetDateTime) -> RateWindow {
    let used_percent = if raw.status == "error" {
        100.0
    } else {
        raw.used_percent
    };
    RateWindow {
        id: id.to_owned(),
        label: label.to_owned(),
        used_percent,
        duration: None,
        resets_at: now.checked_add(time::Duration::seconds(raw.reset_in_sec as i64)),
    }
}

fn build_windows(html: &str, now: OffsetDateTime) -> Vec<RateWindow> {
    let mut windows = Vec::new();
    if let Some(window) = parse_window(html, "rollingUsage") {
        windows.push(rate_window("rolling", "Rolling", &window, now));
    }
    if let Some(window) = parse_window(html, "weeklyUsage") {
        windows.push(rate_window("weekly", "Weekly", &window, now));
    }
    if let Some(window) = parse_window(html, "monthlyUsage") {
        windows.push(rate_window("monthly", "Monthly", &window, now));
    }
    windows
}

fn has_all_windows(windows: &[RateWindow]) -> bool {
    let mut has_rolling = false;
    let mut has_weekly = false;
    let mut has_monthly = false;
    for window in windows {
        match window.id.as_str() {
            "rolling" => has_rolling = true,
            "weekly" => has_weekly = true,
            "monthly" => has_monthly = true,
            _ => {}
        }
    }
    has_rolling && has_weekly && has_monthly
}

fn parse_window(html: &str, name: &str) -> Option<RawWindow> {
    // The page renders several `<name>:` occurrences (e.g. `monthlyUsage:null`
    // in a budget component alongside the real `monthlyUsage:$R[N]={...}`), so
    // scan every occurrence and return the first that yields a real window.
    let needle = format!("{name}:");
    let mut search_from = 0;
    while let Some(relative) = html[search_from..].find(&needle) {
        let start = search_from + relative;
        let rest = &html[start + needle.len()..];
        if let Some(window) = parse_window_object(rest) {
            return Some(window);
        }
        search_from = start + needle.len();
    }
    None
}

fn parse_window_object(rest: &str) -> Option<RawWindow> {
    let mut rest = rest;
    if let Some(after_marker) = rest.strip_prefix("$R") {
        rest = after_marker;
        if let Some(close) = rest.find(']') {
            rest = &rest[close + 1..];
        }
    }
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    let body = rest.trim_start().strip_prefix('{')?;
    let close = body.find('}')?;
    let inner = &body[..close];

    let mut status = String::new();
    let mut reset_in_sec = 0;
    let mut used_percent = 0.0;
    let mut matched = false;

    for part in inner.split(',') {
        let Some((key, value)) = part.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "status" => {
                status = value.trim_matches('"').to_owned();
                matched = true;
            }
            "resetInSec" | "resetsInSeconds" | "reset_in_sec" => {
                if let Ok(parsed) = value.parse::<u64>() {
                    reset_in_sec = parsed;
                    matched = true;
                }
            }
            "usagePercent" | "usage_percent" => {
                if let Ok(parsed) = value.parse::<f64>() {
                    used_percent = parsed;
                    matched = true;
                }
            }
            _ => {}
        }
    }

    matched.then_some(RawWindow {
        status,
        reset_in_sec,
        used_percent,
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCredentials {
    workspace_id: String,
    auth_cookie: String,
}

fn normalize_workspace_id(value: &str) -> String {
    let trimmed = value.trim();
    if let Some(rest) = trimmed.split("/workspace/").nth(1) {
        let segment = rest.split('/').next().unwrap_or("").trim();
        return segment.to_owned();
    }
    trimmed.to_owned()
}

async fn keyring_credentials_async()
-> Result<Option<OpenCodeGoCredentials>, OpenCodeGoProviderError> {
    tokio::task::spawn_blocking(keyring_credentials_blocking)
        .await
        .map_err(|error| OpenCodeGoProviderError::Keychain(error.to_string()))?
}

fn keyring_credentials_blocking() -> Result<Option<OpenCodeGoCredentials>, OpenCodeGoProviderError>
{
    let entry = keyring::Entry::new(OPENCODE_KEYCHAIN_SERVICE, OPENCODE_KEYCHAIN_ACCOUNT)
        .map_err(keychain_error)?;
    let payload = match entry.get_password() {
        Ok(payload) => payload,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(error) => return Err(keychain_error(error)),
    };

    let stored: StoredCredentials =
        serde_json::from_str(&payload).map_err(OpenCodeGoProviderError::Serialize)?;
    if stored.workspace_id.is_empty() || stored.auth_cookie.is_empty() {
        return Ok(None);
    }

    Ok(Some(OpenCodeGoCredentials {
        workspace_id: stored.workspace_id,
        auth_cookie: stored.auth_cookie,
        source: OpenCodeGoCredentialsSource::Keyring,
    }))
}

fn keychain_error(error: keyring::Error) -> OpenCodeGoProviderError {
    OpenCodeGoProviderError::Keychain(error.to_string())
}

fn body_preview(body: &[u8]) -> String {
    const MAX_BODY_PREVIEW: usize = 512;
    let mut body = String::from_utf8_lossy(body).to_string();
    if body.len() > MAX_BODY_PREVIEW {
        body.truncate(MAX_BODY_PREVIEW);
        body.push_str("...");
    }
    body
}

#[derive(Debug, Error)]
pub enum OpenCodeGoProviderError {
    #[error("OpenCode Go credentials are not configured")]
    MissingCredentials,
    #[error("OpenCode Go auth cookie was rejected")]
    CookieInvalid,
    #[error("a required field is missing: {0}")]
    MissingField(&'static str),
    #[error("could not read OpenCode Go credentials from system keyring: {0}")]
    Keychain(String),
    #[error("could not serialize OpenCode Go credentials: {0}")]
    Serialize(serde_json::Error),
    #[error("could not build OpenCode Go URL: {0}")]
    Url(url::ParseError),
    #[error("OpenCode Go request failed: {0}")]
    Http(reqwest::Error),
    #[error("OpenCode Go request returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },
    #[error("could not parse OpenCode Go usage page: {0}")]
    Parse(String),
}

impl From<OpenCodeGoProviderError> for ProviderError {
    fn from(error: OpenCodeGoProviderError) -> Self {
        match error {
            OpenCodeGoProviderError::MissingCredentials
            | OpenCodeGoProviderError::MissingField(_) => {
                ProviderError::NotConfigured(error.to_string())
            }
            OpenCodeGoProviderError::CookieInvalid => {
                ProviderError::Authentication(error.to_string())
            }
            OpenCodeGoProviderError::Parse(_) => ProviderError::Parse(error.to_string()),
            OpenCodeGoProviderError::Keychain(_)
            | OpenCodeGoProviderError::Serialize(_)
            | OpenCodeGoProviderError::Url(_)
            | OpenCodeGoProviderError::Http(_)
            | OpenCodeGoProviderError::ApiStatus { .. } => {
                ProviderError::Network(error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use braindrain_core::Provider;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn workspace_page_html() -> String {
        "<html><script>rollingUsage:$R[35]={status:\"ok\",resetInSec:7600,usagePercent:29}\
        weeklyUsage:$R[36]={status:\"ok\",resetInSec:424577,usagePercent:41}\
        monthlyUsage:$R[37]={status:\"ok\",resetInSec:1140236,usagePercent:23}</script></html>"
            .to_owned()
    }

    #[test]
    fn parse_window_extracts_three_windows() {
        let html = workspace_page_html();
        let rolling = parse_window(&html, "rollingUsage").expect("rolling");
        let weekly = parse_window(&html, "weeklyUsage").expect("weekly");
        let monthly = parse_window(&html, "monthlyUsage").expect("monthly");

        assert_eq!(rolling.reset_in_sec, 7600);
        assert_eq!(rolling.used_percent, 29.0);
        assert_eq!(weekly.used_percent, 41.0);
        assert_eq!(monthly.reset_in_sec, 1_140_236);
    }

    #[test]
    fn parse_window_tolerates_reordered_fields() {
        let html = "rollingUsage:$R[1]={usagePercent:7,resetInSec:120,status:\"ok\"}";
        let window = parse_window(html, "rollingUsage").expect("rolling");
        assert_eq!(window.used_percent, 7.0);
        assert_eq!(window.reset_in_sec, 120);
        assert_eq!(window.status, "ok");
    }

    #[test]
    fn parse_window_returns_none_when_absent() {
        assert!(parse_window("nothing here", "rollingUsage").is_none());
    }

    #[test]
    fn parse_window_skips_null_occurrence_before_real_window() {
        // The page emits `monthlyUsage:null` (a budget component) before the
        // real hydrated window. The parser must skip the null and find the
        // object rather than giving up at the first occurrence.
        let html = "budget:{monthlyLimit:null,monthlyUsage:null,other:1}\
                   rollingUsage:$R[1]={status:\"ok\",resetInSec:60,usagePercent:5}\
                   monthlyUsage:$R[2]={status:\"ok\",resetInSec:1000,usagePercent:7}";

        let monthly = parse_window(html, "monthlyUsage").expect("real monthly window");
        assert_eq!(monthly.used_percent, 7.0);
        assert_eq!(monthly.reset_in_sec, 1000);

        let rolling = parse_window(html, "rollingUsage").expect("rolling");
        assert_eq!(rolling.used_percent, 5.0);
    }

    #[test]
    fn has_all_windows_requires_all_three() {
        let mk = |id: &str| RateWindow {
            id: id.to_owned(),
            label: id.to_owned(),
            used_percent: 1.0,
            duration: None,
            resets_at: None,
        };
        assert!(!has_all_windows(&[mk("rolling"), mk("weekly")]));
        assert!(has_all_windows(&[
            mk("rolling"),
            mk("weekly"),
            mk("monthly")
        ]));
    }

    #[test]
    fn normalize_workspace_id_extracts_from_url() {
        assert_eq!(
            normalize_workspace_id("https://opencode.ai/workspace/wrk_abc123/go"),
            "wrk_abc123"
        );
        assert_eq!(normalize_workspace_id("wrk_abc123"), "wrk_abc123");
        assert_eq!(
            normalize_workspace_id("https://opencode.ai/workspace/wrk_abc123/usage"),
            "wrk_abc123"
        );
    }

    #[test]
    fn rate_window_marks_error_status_as_exhausted() {
        let raw = RawWindow {
            status: "error".to_owned(),
            reset_in_sec: 60,
            used_percent: 12.0,
        };
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let window = rate_window("rolling", "Rolling", &raw, now);
        assert_eq!(window.used_percent, 100.0);
        assert!(window.duration.is_none());
    }

    #[tokio::test]
    async fn provider_fetches_usage_from_workspace_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/workspace/wrk_test/go"))
            .and(header("cookie", "auth=cookie-value"))
            .respond_with(ResponseTemplate::new(200).set_body_string(workspace_page_html()))
            .mount(&server)
            .await;

        let provider = OpenCodeGoProvider::new(OpenCodeGoProviderConfig {
            credentials: Some(OpenCodeGoCredentials {
                workspace_id: "wrk_test".to_owned(),
                auth_cookie: "cookie-value".to_owned(),
                source: OpenCodeGoCredentialsSource::Config,
            }),
            base_url: Url::parse(&server.uri()).expect("mock URL"),
        });

        let snapshot = provider
            .refresh(RefreshContext {
                now: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp"),
            })
            .await
            .expect("refresh OpenCode Go");

        assert_eq!(snapshot.provider, ProviderId::opencode_go());
        assert_eq!(snapshot.source, ProviderSource::Web);
        let ids: Vec<&str> = snapshot
            .usage
            .windows
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(ids, ["rolling", "weekly", "monthly"]);

        let rolling = &snapshot.usage.windows[0];
        assert_eq!(rolling.used_percent, 29.0);
        assert!(rolling.duration.is_none());
        assert_eq!(
            rolling.resets_at.expect("resets_at").unix_timestamp(),
            1_700_000_000 + 7600
        );

        let identity = snapshot.identity.expect("identity");
        assert_eq!(identity.plan.as_deref(), Some("Zen Go"));
    }

    #[tokio::test]
    async fn provider_reports_cookie_invalid_on_redirect() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/workspace/wrk_test/go"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/auth/authorize"))
            .mount(&server)
            .await;

        let provider = OpenCodeGoProvider::new(OpenCodeGoProviderConfig {
            credentials: Some(OpenCodeGoCredentials {
                workspace_id: "wrk_test".to_owned(),
                auth_cookie: "stale".to_owned(),
                source: OpenCodeGoCredentialsSource::Config,
            }),
            base_url: Url::parse(&server.uri()).expect("mock URL"),
        });

        let result = provider.refresh(RefreshContext::default()).await;
        let err = result.expect_err("expired cookie should fail");
        assert!(matches!(err, ProviderError::Authentication(_)));
    }
}
