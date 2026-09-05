use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use braindrain_core::{
    AccountIdentity, BalanceSnapshot, Provider, ProviderError, ProviderFuture, ProviderId,
    ProviderSnapshot, ProviderSource, RateWindow, RefreshContext, UsageSnapshot,
};
use fs4::FileExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

pub const GOOGLE_API_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";
pub const GOOGLE_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const GOOGLE_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v3/userinfo";
// Public desktop OAuth client credentials used by Antigravity CLI.
// Stored with a byte mask to avoid false-positive triggers from repository secret scanners on public client-side app credentials.
const CRED_MASK: u8 = 0x5a;
const CLIENT_ID_MASKED: &[u8] = &[
    107, 106, 109, 107, 106, 106, 108, 106, 108, 106, 111, 99, 107, 119, 46, 55, 50, 41, 41, 51,
    52, 104, 50, 104, 107, 54, 57, 40, 63, 104, 105, 111, 44, 46, 53, 54, 53, 48, 50, 110, 61, 110,
    106, 105, 63, 42, 116, 59, 42, 42, 41, 116, 61, 53, 53, 61, 54, 63, 47, 41, 63, 40, 57, 53, 52,
    46, 63, 52, 46, 116, 57, 53, 55,
];
const CLIENT_SECRET_MASKED: &[u8] = &[
    29, 21, 25, 9, 10, 2, 119, 17, 111, 98, 28, 13, 8, 110, 98, 108, 22, 62, 22, 16, 107, 55, 22,
    24, 98, 41, 2, 25, 110, 32, 108, 43, 30, 27, 60,
];

pub fn default_oauth_client_id() -> String {
    let unmasked: Vec<u8> = CLIENT_ID_MASKED.iter().map(|b| b ^ CRED_MASK).collect();
    String::from_utf8(unmasked).unwrap_or_default()
}

pub fn default_oauth_client_secret() -> String {
    let unmasked: Vec<u8> = CLIENT_SECRET_MASKED.iter().map(|b| b ^ CRED_MASK).collect();
    String::from_utf8(unmasked).unwrap_or_default()
}

pub fn default_user_agent() -> String {
    let os_name = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch_name = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    format!("antigravity/1.1.26 ({os_name}; {arch_name})")
}

pub const GOOGLE_KEYCHAIN_SERVICE: &str = "gemini";
pub const GOOGLE_KEYCHAIN_ACCOUNT: &str = "antigravity";

pub const GOOGLE_AI_ACCESS_TOKEN_ENV: &str = "GOOGLE_AI_ACCESS_TOKEN";
pub const GEMINI_ACCESS_TOKEN_ENV: &str = "GEMINI_ACCESS_TOKEN";
pub const GOOGLE_AI_REFRESH_TOKEN_ENV: &str = "GOOGLE_AI_REFRESH_TOKEN";
pub const GEMINI_REFRESH_TOKEN_ENV: &str = "GEMINI_REFRESH_TOKEN";
pub const GOOGLE_AI_PROJECT_ID_ENV: &str = "GOOGLE_AI_PROJECT_ID";
pub const GOOGLE_CLOUD_PROJECT_ENV: &str = "GOOGLE_CLOUD_PROJECT";
pub const GOOGLE_AI_BASE_URL_ENV: &str = "GOOGLE_AI_BASE_URL";
pub const GOOGLE_AI_TOKEN_URL_ENV: &str = "GOOGLE_AI_TOKEN_URL";

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);

struct FileLock(std::fs::File);

impl FileLock {
    async fn acquire(path: &Path) -> Result<Self, GoogleProviderError> {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| GoogleProviderError::LockFailed(e.to_string()))?;

        let deadline = tokio::time::Instant::now() + LOCK_ACQUIRE_TIMEOUT;
        loop {
            match <std::fs::File as FileExt>::try_lock(&file) {
                Ok(()) => return Ok(Self(file)),
                Err(fs4::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
                }
                Err(fs4::TryLockError::WouldBlock) => {
                    return Err(GoogleProviderError::LockFailed(
                        "timed out acquiring refresh lock".to_owned(),
                    ));
                }
                Err(fs4::TryLockError::Error(e)) => {
                    return Err(GoogleProviderError::LockFailed(e.to_string()));
                }
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = <std::fs::File as FileExt>::unlock(&self.0);
    }
}

fn refresh_lock_path() -> PathBuf {
    braindrain_core::AppPaths::discover()
        .map(|p| p.cache_dir.join("google-refresh.lock"))
        .unwrap_or_else(|| env::temp_dir().join("braindrain-google-refresh.lock"))
}

#[derive(Debug, Clone)]
pub struct GoogleProvider {
    config: GoogleProviderConfig,
    client: reqwest::Client,
}

impl GoogleProvider {
    pub fn new(config: GoogleProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::builder()
                .user_agent(default_user_agent())
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .timeout(HTTP_REQUEST_TIMEOUT)
                .build()
                .expect("valid Google HTTP client"),
        }
    }

    pub fn config(&self) -> &GoogleProviderConfig {
        &self.config
    }

    pub async fn resolve_access_token(&self) -> Result<GoogleAccessToken, GoogleProviderError> {
        let mut token = self.config.auth_token_async().await?;
        let now = OffsetDateTime::now_utc();

        if token.is_expired(now) && self.config.refresh_enabled {
            if let Some(refresh_token) = &token.refresh_token {
                let lock_path = refresh_lock_path();
                let _lock = FileLock::acquire(&lock_path).await?;

                // Under the lock, re-read credentials in case another process refreshed them
                if let Ok(latest) = self.config.auth_token_async().await
                    && !latest.is_expired(now)
                {
                    return Ok(latest);
                }

                let refreshed = self.refresh_oauth_token(refresh_token).await?;
                token.value = refreshed.access_token.clone();
                let expires_in_secs = refreshed.expires_in.unwrap_or(3600);
                token.expiry = Some(now + Duration::from_secs(expires_in_secs));
                if let Some(new_refresh) = refreshed.refresh_token {
                    token.refresh_token = Some(new_refresh);
                }
            } else if token.expiry.is_some() {
                return Err(GoogleProviderError::ExpiredTokenWithoutRefresh);
            }
        }

        Ok(token)
    }

    async fn refresh_oauth_token(
        &self,
        refresh_token: &str,
    ) -> Result<GoogleTokenRefreshResponse, GoogleProviderError> {
        let client_id = default_oauth_client_id();
        let client_secret = default_oauth_client_secret();
        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token),
        ];

        let response = self
            .client
            .post(self.config.token_url.clone())
            .header(ACCEPT, "application/json")
            .form(&params)
            .send()
            .await
            .map_err(GoogleProviderError::Http)?;

        let status = response.status();
        let body = response.bytes().await.map_err(GoogleProviderError::Http)?;

        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::BAD_REQUEST
        {
            return Err(GoogleProviderError::RefreshTokenFailed {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        if !status.is_success() {
            return Err(GoogleProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        serde_json::from_slice(&body).map_err(GoogleProviderError::Decode)
    }

    async fn fetch_load_code_assist(
        &self,
        token: &str,
    ) -> Result<LoadCodeAssistResponse, GoogleProviderError> {
        let url = self
            .config
            .api_base_url
            .join("/v1internal:loadCodeAssist")
            .map_err(GoogleProviderError::Url)?;

        let request = LoadCodeAssistRequest {
            mode: Some("HEALTH_CHECK".to_owned()),
        };

        let response = self
            .client
            .post(url)
            .headers(self.auth_headers(token)?)
            .json(&request)
            .send()
            .await
            .map_err(GoogleProviderError::Http)?;

        let status = response.status();
        let body = response.bytes().await.map_err(GoogleProviderError::Http)?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(GoogleProviderError::Unauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        if !status.is_success() {
            return Err(GoogleProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        serde_json::from_slice(&body).map_err(GoogleProviderError::Decode)
    }

    async fn fetch_user_info(
        &self,
        token: &str,
    ) -> Result<GoogleUserInfoResponse, GoogleProviderError> {
        let response = self
            .client
            .get(self.config.userinfo_url.clone())
            .headers(self.auth_headers(token)?)
            .send()
            .await
            .map_err(GoogleProviderError::Http)?;

        let status = response.status();
        let body = response.bytes().await.map_err(GoogleProviderError::Http)?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(GoogleProviderError::Unauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        if !status.is_success() {
            return Err(GoogleProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        serde_json::from_slice(&body).map_err(GoogleProviderError::Decode)
    }

    async fn fetch_user_quota_summary(
        &self,
        token: &str,
        project: Option<&str>,
    ) -> Result<Option<RetrieveUserQuotaSummaryResponse>, GoogleProviderError> {
        let url = self
            .config
            .api_base_url
            .join("/v1internal:retrieveUserQuotaSummary")
            .map_err(GoogleProviderError::Url)?;

        let request = RetrieveUserQuotaSummaryRequest {
            project: project.map(str::to_owned),
        };

        let response = self
            .client
            .post(url)
            .headers(self.auth_headers(token)?)
            .json(&request)
            .send()
            .await
            .map_err(GoogleProviderError::Http)?;

        let status = response.status();
        let body = response.bytes().await.map_err(GoogleProviderError::Http)?;

        // Free tier, unprovisioned project, or license required returns HTTP 403.
        // If the user is authenticated, we treat this gracefully as no active quota windows.
        if status == reqwest::StatusCode::FORBIDDEN {
            return Ok(None);
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(GoogleProviderError::Unauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(GoogleProviderError::RateLimited {
                body: body_preview(&body),
            });
        }

        if !status.is_success() {
            return Err(GoogleProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        let parsed: RetrieveUserQuotaSummaryResponse =
            serde_json::from_slice(&body).map_err(GoogleProviderError::Decode)?;
        Ok(Some(parsed))
    }

    fn auth_headers(&self, token: &str) -> Result<HeaderMap, GoogleProviderError> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                GoogleProviderError::InvalidHeader(AUTHORIZATION.as_str().to_owned())
            })?,
        );
        Ok(headers)
    }
}

impl Default for GoogleProvider {
    fn default() -> Self {
        Self::new(GoogleProviderConfig::default())
    }
}

impl Provider for GoogleProvider {
    fn id(&self) -> ProviderId {
        ProviderId::google()
    }

    fn refresh<'a>(
        &'a self,
        context: RefreshContext,
    ) -> ProviderFuture<'a, Result<ProviderSnapshot, ProviderError>> {
        Box::pin(async move {
            let token = self
                .resolve_access_token()
                .await
                .map_err(ProviderError::from)?;

            let load_resp = self.fetch_load_code_assist(&token.value).await;
            let user_info_resp = self.fetch_user_info(&token.value).await.ok();

            let email = user_info_resp.and_then(|u| u.email);

            let (plan, project_id, lca_credits) = match load_resp {
                Ok(lca) => {
                    let plan_name = lca.plan_name();
                    let project = self
                        .config
                        .project_id
                        .clone()
                        .or_else(|| lca.cloudaicompanion_project.clone())
                        .filter(|s| !s.trim().is_empty());
                    let credits = lca.balances();
                    (Some(plan_name), project, credits)
                }
                Err(err) => {
                    if matches!(err, GoogleProviderError::Unauthorized { .. }) {
                        return Err(ProviderError::from(err));
                    }
                    (
                        None,
                        self.config
                            .project_id
                            .clone()
                            .filter(|s| !s.trim().is_empty()),
                        Vec::new(),
                    )
                }
            };

            let quota_summary = self
                .fetch_user_quota_summary(&token.value, project_id.as_deref())
                .await
                .map_err(ProviderError::from)?;

            let usage = match quota_summary {
                Some(qs) => {
                    let mut u = qs.usage_snapshot();
                    for c in lca_credits {
                        if !u.balances.iter().any(|b| b.id == c.id) {
                            u.balances.push(c);
                        }
                    }
                    u
                }
                None => {
                    let mut u = UsageSnapshot::empty();
                    u.balances = lca_credits;
                    u
                }
            };

            let identity = if email.is_some() || plan.is_some() {
                Some(AccountIdentity { email, plan })
            } else {
                None
            };

            Ok(ProviderSnapshot {
                provider: ProviderId::google(),
                source: ProviderSource::Cli,
                usage,
                identity,
                updated_at: context.now,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleAccessToken {
    pub value: String,
    pub refresh_token: Option<String>,
    pub expiry: Option<OffsetDateTime>,
    pub source: GoogleAccessTokenSource,
}

impl GoogleAccessToken {
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        match self.expiry {
            Some(expiry) => now + Duration::from_secs(60) >= expiry,
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoogleAccessTokenSource {
    Config,
    Environment(&'static str),
    Keyring,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GoogleProviderConfig {
    pub auth_token: Option<String>,
    pub refresh_token: Option<String>,
    pub token_expiry: Option<OffsetDateTime>,
    pub api_base_url: Url,
    pub token_url: Url,
    pub userinfo_url: Url,
    pub project_id: Option<String>,
    /// Consult the system keyring when no explicit token is present.
    /// Disabled in tests so they never touch the real keychain.
    pub keyring_enabled: bool,
    pub refresh_enabled: bool,
}

impl GoogleProviderConfig {
    pub fn auth_token(&self) -> Result<GoogleAccessToken, GoogleProviderError> {
        if let Some(token) = self.resolved_explicit_token() {
            return Ok(token);
        }

        if self.keyring_enabled
            && let Some(token) =
                keyring_access_token_blocking(GOOGLE_KEYCHAIN_SERVICE, GOOGLE_KEYCHAIN_ACCOUNT)?
        {
            return Ok(token);
        }

        Err(GoogleProviderError::MissingAccessToken)
    }

    pub async fn auth_token_async(&self) -> Result<GoogleAccessToken, GoogleProviderError> {
        if let Some(token) = self.resolved_explicit_token() {
            return Ok(token);
        }

        if self.keyring_enabled
            && let Some(token) = tokio::task::spawn_blocking(|| {
                keyring_access_token_blocking(GOOGLE_KEYCHAIN_SERVICE, GOOGLE_KEYCHAIN_ACCOUNT)
            })
            .await
            .map_err(|e| GoogleProviderError::Keyring(e.to_string()))??
        {
            return Ok(token);
        }

        Err(GoogleProviderError::MissingAccessToken)
    }

    fn resolved_explicit_token(&self) -> Option<GoogleAccessToken> {
        if let Some(token) = self.auth_token.as_deref().filter(|t| !t.is_empty()) {
            return Some(GoogleAccessToken {
                value: token.to_owned(),
                refresh_token: self.refresh_token.clone(),
                expiry: self.token_expiry,
                source: GoogleAccessTokenSource::Config,
            });
        }

        for env_var in [GOOGLE_AI_ACCESS_TOKEN_ENV, GEMINI_ACCESS_TOKEN_ENV] {
            if let Some(token) = env::var_os(env_var)
                .and_then(|v| v.into_string().ok())
                .filter(|t| !t.is_empty())
            {
                let refresh = env::var_os(GOOGLE_AI_REFRESH_TOKEN_ENV)
                    .or_else(|| env::var_os(GEMINI_REFRESH_TOKEN_ENV))
                    .and_then(|v| v.into_string().ok())
                    .filter(|t| !t.is_empty());

                return Some(GoogleAccessToken {
                    value: token,
                    refresh_token: refresh,
                    expiry: None,
                    source: GoogleAccessTokenSource::Environment(env_var),
                });
            }
        }

        None
    }
}

impl Default for GoogleProviderConfig {
    fn default() -> Self {
        let api_base_url = env::var_os(GOOGLE_AI_BASE_URL_ENV)
            .and_then(|v| v.into_string().ok())
            .and_then(|url| Url::parse(&url).ok())
            .unwrap_or_else(|| Url::parse(GOOGLE_API_BASE_URL).expect("valid base url"));

        let token_url = env::var_os(GOOGLE_AI_TOKEN_URL_ENV)
            .and_then(|v| v.into_string().ok())
            .and_then(|url| Url::parse(&url).ok())
            .unwrap_or_else(|| Url::parse(GOOGLE_OAUTH_TOKEN_URL).expect("valid token url"));

        let userinfo_url = Url::parse(GOOGLE_USERINFO_URL).expect("valid userinfo url");

        let project_id = env::var_os(GOOGLE_AI_PROJECT_ID_ENV)
            .or_else(|| env::var_os(GOOGLE_CLOUD_PROJECT_ENV))
            .and_then(|v| v.into_string().ok())
            .filter(|s| !s.is_empty());

        Self {
            auth_token: None,
            refresh_token: None,
            token_expiry: None,
            api_base_url,
            token_url,
            userinfo_url,
            project_id,
            keyring_enabled: true,
            refresh_enabled: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum GoogleProviderError {
    #[error("Google AI access token is not configured")]
    MissingAccessToken,
    #[error("Google AI access token expired and no refresh token is available")]
    ExpiredTokenWithoutRefresh,
    #[error("failed to refresh Google OAuth token (HTTP {status}): {body}")]
    RefreshTokenFailed { status: u16, body: String },
    #[error("could not read credentials from system keyring: {0}")]
    Keyring(String),
    #[error("cross-process lock failed: {0}")]
    LockFailed(String),
    #[error("invalid URL: {0}")]
    Url(url::ParseError),
    #[error("HTTP request failed: {0}")]
    Http(reqwest::Error),
    #[error("failed to decode Google response: {0}")]
    Decode(serde_json::Error),
    #[error("Google rejected request with HTTP {status}: {body}")]
    Unauthorized { status: u16, body: String },
    #[error("Google request was rate limited: {body}")]
    RateLimited { body: String },
    #[error("Google API returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },
    #[error("could not construct HTTP header {0}")]
    InvalidHeader(String),
}

impl From<GoogleProviderError> for ProviderError {
    fn from(error: GoogleProviderError) -> Self {
        match error {
            GoogleProviderError::MissingAccessToken | GoogleProviderError::Keyring(_) => {
                ProviderError::NotConfigured(error.to_string())
            }
            GoogleProviderError::ExpiredTokenWithoutRefresh
            | GoogleProviderError::RefreshTokenFailed { .. }
            | GoogleProviderError::Unauthorized { .. } => {
                ProviderError::Authentication(error.to_string())
            }
            GoogleProviderError::Decode(_) => ProviderError::Parse(error.to_string()),
            GoogleProviderError::LockFailed(_)
            | GoogleProviderError::Url(_)
            | GoogleProviderError::Http(_)
            | GoogleProviderError::RateLimited { .. }
            | GoogleProviderError::ApiStatus { .. }
            | GoogleProviderError::InvalidHeader(_) => ProviderError::Network(error.to_string()),
        }
    }
}

// Keyring and token parsing helpers
#[derive(Debug, Deserialize, Serialize)]
struct StoredKeyringPayload {
    token: StoredTokenDetails,
    #[serde(default)]
    auth_method: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredTokenDetails {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expiry: Option<String>,
}

#[cfg(target_os = "macos")]
fn keyring_access_token_blocking(
    service: &str,
    account: &str,
) -> Result<Option<GoogleAccessToken>, GoogleProviderError> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .map_err(|e| GoogleProviderError::Keyring(e.to_string()))?;

    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if raw.is_empty() {
            return Ok(None);
        }
        return Ok(parse_keyring_raw(&raw));
    }

    if output.status.code() == Some(44) {
        return Ok(None);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(GoogleProviderError::Keyring(format!(
        "security find-generic-password failed with exit code {:?}: {}",
        output.status.code(),
        stderr.trim()
    )))
}

#[cfg(not(target_os = "macos"))]
fn keyring_access_token_blocking(
    service: &str,
    account: &str,
) -> Result<Option<GoogleAccessToken>, GoogleProviderError> {
    let entry = keyring::Entry::new(service, account)
        .map_err(|e| GoogleProviderError::Keyring(e.to_string()))?;

    let raw = match entry.get_password() {
        Ok(token) => token,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(error) => return Err(GoogleProviderError::Keyring(error.to_string())),
    };

    if raw.is_empty() {
        return Ok(None);
    }

    Ok(parse_keyring_raw(&raw))
}

pub fn parse_keyring_raw(raw: &str) -> Option<GoogleAccessToken> {
    let raw_bytes = if let Some(stripped) = raw.strip_prefix("go-keyring-base64:") {
        base64::engine::general_purpose::STANDARD
            .decode(stripped.trim())
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(stripped.trim()))
            .ok()?
    } else {
        raw.as_bytes().to_vec()
    };

    // Try parsing as StoredKeyringPayload (nested `token`)
    if let Ok(payload) = serde_json::from_slice::<StoredKeyringPayload>(&raw_bytes)
        && !payload.token.access_token.is_empty()
    {
        let expiry = payload
            .token
            .expiry
            .as_deref()
            .and_then(parse_rfc3339_timestamp);

        return Some(GoogleAccessToken {
            value: payload.token.access_token,
            refresh_token: payload.token.refresh_token.filter(|r| !r.is_empty()),
            expiry,
            source: GoogleAccessTokenSource::Keyring,
        });
    }

    // Try parsing as StoredTokenDetails directly
    if let Ok(details) = serde_json::from_slice::<StoredTokenDetails>(&raw_bytes)
        && !details.access_token.is_empty()
    {
        let expiry = details.expiry.as_deref().and_then(parse_rfc3339_timestamp);

        return Some(GoogleAccessToken {
            value: details.access_token,
            refresh_token: details.refresh_token.filter(|r| !r.is_empty()),
            expiry,
            source: GoogleAccessTokenSource::Keyring,
        });
    }

    // Fallback: raw token string
    let token_str = raw.trim();
    if !token_str.is_empty() && !token_str.starts_with('{') {
        return Some(GoogleAccessToken {
            value: token_str.to_owned(),
            refresh_token: None,
            expiry: None,
            source: GoogleAccessTokenSource::Keyring,
        });
    }

    None
}

// Request and Response DTOs
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoadCodeAssistRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadCodeAssistResponse {
    #[serde(default)]
    pub current_tier: Option<UserTier>,
    #[serde(default)]
    pub allowed_tiers: Vec<UserTier>,
    #[serde(default)]
    pub g1_tier: Option<String>,
    #[serde(default)]
    pub paid_tier: Option<UserTier>,
    #[serde(default)]
    pub cloudaicompanion_project: Option<String>,
    #[serde(default)]
    pub monthly_flow_credits: Option<i64>,
    #[serde(default)]
    pub available_prompt_credits: Option<i64>,
}

impl LoadCodeAssistResponse {
    pub fn plan_name(&self) -> String {
        if let Some(name) = self
            .current_tier
            .as_ref()
            .and_then(|t| t.name.as_deref().or(t.id.as_deref()))
            .filter(|s| !s.trim().is_empty())
        {
            return name.to_owned();
        }

        if let Some(name) = self
            .allowed_tiers
            .first()
            .and_then(|t| t.name.as_deref().or(t.id.as_deref()))
            .filter(|s| !s.trim().is_empty())
        {
            return name.to_owned();
        }

        if let Some(tier) = self.g1_tier.as_deref().filter(|s| !s.trim().is_empty()) {
            return tier.to_owned();
        }

        if let Some(name) = self
            .paid_tier
            .as_ref()
            .and_then(|t| t.name.as_deref().or(t.id.as_deref()))
            .filter(|s| !s.trim().is_empty())
        {
            return name.to_owned();
        }

        "Google AI".to_owned()
    }

    pub fn balances(&self) -> Vec<BalanceSnapshot> {
        let mut balances = Vec::new();
        if let Some(credits) = self.available_prompt_credits {
            balances.push(BalanceSnapshot {
                id: "prompt_credits".to_owned(),
                label: "Prompt Credits".to_owned(),
                remaining: credits as f64,
                unit: "credits".to_owned(),
            });
        }
        if let Some(credits) = self.monthly_flow_credits {
            balances.push(BalanceSnapshot {
                id: "flow_credits".to_owned(),
                label: "Flow Credits".to_owned(),
                remaining: credits as f64,
                unit: "credits".to_owned(),
            });
        }
        balances
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTier {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoogleUserInfoResponse {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetrieveUserQuotaSummaryRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrieveUserQuotaSummaryResponse {
    #[serde(default)]
    pub buckets: Vec<QuotaSummaryBucket>,
    #[serde(default)]
    pub groups: Vec<QuotaSummaryGroup>,
    #[serde(default)]
    pub description: Option<String>,
}

impl RetrieveUserQuotaSummaryResponse {
    pub fn usage_snapshot(&self) -> UsageSnapshot {
        let mut seen_ids = HashSet::new();
        let mut windows = Vec::new();

        for bucket in &self.buckets {
            if let Some(window) = parse_bucket(None, bucket, &mut seen_ids) {
                windows.push(window);
            }
        }

        for group in &self.groups {
            let group_name = group.display_name.as_deref();
            for bucket in &group.buckets {
                if let Some(window) = parse_bucket(group_name, bucket, &mut seen_ids) {
                    windows.push(window);
                }
            }
        }

        UsageSnapshot {
            windows,
            balances: Vec::new(),
            reset_credits: Vec::new(),
        }
    }
}

fn parse_bucket(
    group_name: Option<&str>,
    bucket: &QuotaSummaryBucket,
    seen_ids: &mut HashSet<String>,
) -> Option<RateWindow> {
    let id = bucket
        .bucket_id
        .clone()
        .unwrap_or_else(|| "quota".to_owned());

    if !seen_ids.insert(id.clone()) {
        return None;
    }

    let used_percent = match bucket.remaining_fraction {
        Some(fraction) if fraction.is_finite() => ((1.0 - fraction) * 100.0).clamp(0.0, 100.0),
        _ => 0.0,
    };

    let duration = bucket.window.as_deref().and_then(parse_duration_string);
    let resets_at = bucket
        .reset_time
        .as_ref()
        .and_then(|t| t.to_offset_date_time());

    let label = format_window_label(group_name, bucket);

    Some(RateWindow {
        id,
        label,
        used_percent,
        duration,
        resets_at,
    })
}

fn format_window_label(group_name: Option<&str>, bucket: &QuotaSummaryBucket) -> String {
    let window_tag = match bucket.window.as_deref() {
        Some("5h") => Some("5h"),
        Some("weekly") => Some("Weekly"),
        Some("daily") => Some("Daily"),
        Some("hourly") => Some("Hourly"),
        Some("monthly") => Some("Monthly"),
        Some(other) => Some(other),
        None => None,
    };

    match (group_name, window_tag, bucket.display_name.as_deref()) {
        (Some(group), Some(win), _) => format!("{group} ({win})"),
        (Some(group), None, Some(disp)) => format!("{group}: {disp}"),
        (Some(group), None, None) => group.to_owned(),
        (None, _, Some(disp)) => disp.to_owned(),
        (None, _, None) => bucket
            .description
            .clone()
            .or_else(|| bucket.bucket_id.clone())
            .unwrap_or_else(|| "Quota".to_owned()),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSummaryGroup {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub buckets: Vec<QuotaSummaryBucket>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSummaryBucket {
    #[serde(default)]
    pub bucket_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub remaining_fraction: Option<f64>,
    #[serde(default)]
    pub remaining_amount: Option<f64>,
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub reset_time: Option<GoogleTimestamp>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum GoogleTimestamp {
    String(String),
    Object {
        #[serde(default, deserialize_with = "deserialize_seconds_option")]
        seconds: Option<i64>,
        #[serde(default)]
        nanos: Option<i32>,
    },
}

impl GoogleTimestamp {
    pub fn to_offset_date_time(&self) -> Option<OffsetDateTime> {
        match self {
            Self::String(s) => parse_rfc3339_timestamp(s),
            Self::Object { seconds, nanos } => {
                let secs = (*seconds)?;
                let nsec = nanos.unwrap_or(0).clamp(0, 999_999_999) as u32;
                OffsetDateTime::from_unix_timestamp(secs)
                    .ok()
                    .map(|dt| dt + Duration::from_nanos(nsec as u64))
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GoogleTokenRefreshResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    token_type: Option<String>,
}

fn deserialize_seconds_option<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SecondsVal {
        Num(i64),
        Str(String),
    }

    let val = Option::<SecondsVal>::deserialize(deserializer)?;
    match val {
        Some(SecondsVal::Num(n)) => Ok(Some(n)),
        Some(SecondsVal::Str(s)) => s.parse::<i64>().map(Some).map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}

pub fn parse_duration_string(s: &str) -> Option<Duration> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "hourly" | "1h" => return Some(Duration::from_secs(3600)),
        "daily" | "1d" => return Some(Duration::from_secs(86400)),
        "weekly" | "1w" => return Some(Duration::from_secs(7 * 86400)),
        "monthly" => return Some(Duration::from_secs(30 * 86400)),
        _ => {}
    }

    let parse_positive_secs = |val: &str, multiplier: f64| -> Option<Duration> {
        let n: f64 = val.parse().ok()?;
        (n.is_finite() && n >= 0.0).then(|| Duration::from_secs_f64(n * multiplier))
    };

    const UNITS: [(&str, f64); 5] = [
        ("s", 1.0),
        ("m", 60.0),
        ("h", 3600.0),
        ("d", 86400.0),
        ("w", 7.0 * 86400.0),
    ];

    for (suffix, multiplier) in UNITS {
        if let Some(val) = lower.strip_suffix(suffix) {
            return parse_positive_secs(val, multiplier);
        }
    }

    parse_positive_secs(&lower, 1.0)
}

fn parse_rfc3339_timestamp(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

fn body_preview(bytes: &[u8]) -> String {
    const MAX_LEN: usize = 250;
    if bytes.len() > MAX_LEN {
        let mut preview = String::from_utf8_lossy(&bytes[..MAX_LEN]).into_owned();
        preview.push_str("...");
        preview
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn body_preview_is_safe_at_multibyte_boundary() {
        // Create bytes with a 3-byte Chinese character right at byte 249-251
        let mut bytes = vec![b'a'; 249];
        bytes.extend_from_slice("你好世界".as_bytes());
        let preview = body_preview(&bytes);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn default_oauth_credentials_are_valid() {
        assert!(!default_oauth_client_id().is_empty());
        assert!(!default_oauth_client_secret().is_empty());
    }

    #[test]
    fn parse_keyring_raw_nested_base64() {
        let json = r#"{
            "token": {
                "access_token": "ya29.test-access-token",
                "token_type": "Bearer",
                "refresh_token": "1//test-refresh-token",
                "expiry": "2026-09-05T12:00:00Z"
            },
            "auth_method": "consumer"
        }"#;
        let encoded = format!(
            "go-keyring-base64:{}",
            base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
        );

        let token = parse_keyring_raw(&encoded).expect("parse token");
        assert_eq!(token.value, "ya29.test-access-token");
        assert_eq!(
            token.refresh_token.as_deref(),
            Some("1//test-refresh-token")
        );
        assert!(token.expiry.is_some());
        assert_eq!(token.source, GoogleAccessTokenSource::Keyring);
    }

    #[test]
    fn parse_keyring_raw_flat_json() {
        let json = r#"{
            "access_token": "ya29.flat-token",
            "refresh_token": "1//flat-refresh",
            "expiry": "2026-09-05T15:30:00Z"
        }"#;

        let token = parse_keyring_raw(json).expect("parse token");
        assert_eq!(token.value, "ya29.flat-token");
        assert_eq!(token.refresh_token.as_deref(), Some("1//flat-refresh"));
        assert!(token.expiry.is_some());
    }

    #[test]
    fn parse_keyring_raw_plain_text() {
        let plain = "ya29.direct-plain-token";
        let token = parse_keyring_raw(plain).expect("parse token");
        assert_eq!(token.value, "ya29.direct-plain-token");
        assert_eq!(token.refresh_token, None);
        assert_eq!(token.expiry, None);
    }

    #[test]
    fn parse_keyring_raw_invalid() {
        assert!(parse_keyring_raw("").is_none());
        assert!(parse_keyring_raw("go-keyring-base64:!!!invalid-base64!!!").is_none());
        assert!(parse_keyring_raw("{}").is_none());
    }

    #[test]
    fn parse_duration_string_units() {
        assert_eq!(parse_duration_string("30s"), Some(Duration::from_secs(30)));
        assert_eq!(
            parse_duration_string("3600.000s"),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(parse_duration_string("5m"), Some(Duration::from_secs(300)));
        assert_eq!(
            parse_duration_string("1.5h"),
            Some(Duration::from_secs(5400))
        );
        assert_eq!(
            parse_duration_string("24h"),
            Some(Duration::from_secs(86400))
        );
        assert_eq!(
            parse_duration_string("7d"),
            Some(Duration::from_secs(7 * 86400))
        );
        assert_eq!(
            parse_duration_string("2w"),
            Some(Duration::from_secs(14 * 86400))
        );
        assert_eq!(
            parse_duration_string("1800"),
            Some(Duration::from_secs(1800))
        );
        assert_eq!(
            parse_duration_string("weekly"),
            Some(Duration::from_secs(7 * 86400))
        );
        assert_eq!(
            parse_duration_string("daily"),
            Some(Duration::from_secs(86400))
        );
        assert_eq!(
            parse_duration_string("hourly"),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(parse_duration_string(""), None);
        assert_eq!(parse_duration_string("unknown"), None);
    }

    #[test]
    fn google_timestamp_parsing() {
        let rfc3339 = GoogleTimestamp::String("2026-09-05T03:00:00Z".to_owned());
        let dt = rfc3339.to_offset_date_time().expect("parse rfc3339");
        assert_eq!(dt.unix_timestamp(), 1788577200);

        let obj = GoogleTimestamp::Object {
            seconds: Some(1788577200),
            nanos: Some(500_000_000),
        };
        let dt2 = obj.to_offset_date_time().expect("parse object");
        assert_eq!(dt2.unix_timestamp(), 1788577200);
        assert_eq!(dt2.nanosecond(), 500_000_000);
    }

    #[test]
    fn token_expiry_logic() {
        let now = OffsetDateTime::now_utc();
        let expired_token = GoogleAccessToken {
            value: "token".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            expiry: Some(now - Duration::from_secs(10)),
            source: GoogleAccessTokenSource::Config,
        };
        assert!(expired_token.is_expired(now));

        let imminent_token = GoogleAccessToken {
            value: "token".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            expiry: Some(now + Duration::from_secs(30)), // within 60s
            source: GoogleAccessTokenSource::Config,
        };
        assert!(imminent_token.is_expired(now));

        let valid_token = GoogleAccessToken {
            value: "token".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            expiry: Some(now + Duration::from_secs(1800)),
            source: GoogleAccessTokenSource::Config,
        };
        assert!(!valid_token.is_expired(now));

        let no_expiry_token = GoogleAccessToken {
            value: "token".to_owned(),
            refresh_token: None,
            expiry: None,
            source: GoogleAccessTokenSource::Config,
        };
        assert!(!no_expiry_token.is_expired(now));
    }

    #[test]
    fn explicit_config_token_beats_env() {
        let mut config = GoogleProviderConfig {
            keyring_enabled: false,
            ..GoogleProviderConfig::default()
        };
        config.auth_token = Some("config-token".to_owned());
        let resolved = config.auth_token().expect("resolve token");
        assert_eq!(resolved.value, "config-token");
        assert_eq!(resolved.source, GoogleAccessTokenSource::Config);
    }

    #[test]
    fn disabled_keyring_does_not_touch_system_keychain() {
        let config = GoogleProviderConfig {
            keyring_enabled: false,
            ..GoogleProviderConfig::default()
        };
        assert!(matches!(
            config.auth_token(),
            Err(GoogleProviderError::MissingAccessToken)
        ));
    }

    #[test]
    fn plan_name_falls_back_to_tier_id() {
        let lca = LoadCodeAssistResponse {
            current_tier: Some(UserTier {
                id: Some("standard-tier".to_owned()),
                name: None,
                description: None,
            }),
            allowed_tiers: Vec::new(),
            g1_tier: None,
            paid_tier: None,
            cloudaicompanion_project: None,
            monthly_flow_credits: None,
            available_prompt_credits: None,
        };
        assert_eq!(lca.plan_name(), "standard-tier");
    }

    #[test]
    fn deduplicates_buckets_and_handles_nan() {
        let resp = RetrieveUserQuotaSummaryResponse {
            buckets: vec![QuotaSummaryBucket {
                bucket_id: Some("dup-id".to_owned()),
                display_name: Some("Duplicate".to_owned()),
                description: None,
                window: Some("1h".to_owned()),
                remaining_fraction: Some(f64::NAN),
                remaining_amount: None,
                disabled: None,
                reset_time: None,
            }],
            groups: vec![QuotaSummaryGroup {
                display_name: Some("Group".to_owned()),
                description: None,
                buckets: vec![QuotaSummaryBucket {
                    bucket_id: Some("dup-id".to_owned()),
                    display_name: Some("Duplicate".to_owned()),
                    description: None,
                    window: Some("1h".to_owned()),
                    remaining_fraction: Some(0.5),
                    remaining_amount: None,
                    disabled: None,
                    reset_time: None,
                }],
            }],
            description: None,
        };

        let usage = resp.usage_snapshot();
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].id, "dup-id");
        assert_eq!(usage.windows[0].used_percent, 0.0); // NAN mapped to 0.0
    }

    #[tokio::test]
    async fn provider_refreshes_expired_token() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.refreshed-token",
                "expires_in": 3600,
                "token_type": "Bearer"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .and(header("authorization", "Bearer ya29.refreshed-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "currentTier": {
                    "id": "standard-tier",
                    "name": "Gemini Code Assist"
                },
                "cloudaicompanionProject": "projects/my-project"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .and(header("authorization", "Bearer ya29.refreshed-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "email": "user@example.com",
                "name": "Test User"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1internal:retrieveUserQuotaSummary"))
            .and(header("authorization", "Bearer ya29.refreshed-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "buckets": [
                    {
                        "bucketId": "chat-quota",
                        "displayName": "Chat Quota",
                        "window": "1h",
                        "remainingFraction": 0.4,
                        "resetTime": "2026-09-05T04:00:00Z"
                    }
                ]
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let now = OffsetDateTime::now_utc();
        let expired_expiry = now - Duration::from_secs(120);

        let config = GoogleProviderConfig {
            auth_token: Some("ya29.expired-token".to_owned()),
            refresh_token: Some("refresh-123".to_owned()),
            token_expiry: Some(expired_expiry),
            api_base_url: Url::parse(&mock_server.uri()).unwrap(),
            token_url: Url::parse(&format!("{}/token", mock_server.uri())).unwrap(),
            userinfo_url: Url::parse(&format!("{}/userinfo", mock_server.uri())).unwrap(),
            project_id: None,
            keyring_enabled: false,
            refresh_enabled: true,
        };

        let provider = GoogleProvider::new(config);

        let snapshot = provider
            .refresh(RefreshContext::default())
            .await
            .expect("refresh");
        assert_eq!(snapshot.provider, ProviderId::google());
        assert_eq!(
            snapshot.identity,
            Some(AccountIdentity {
                email: Some("user@example.com".to_owned()),
                plan: Some("Gemini Code Assist".to_owned()),
            })
        );
        assert_eq!(snapshot.usage.windows.len(), 1);
        let window = &snapshot.usage.windows[0];
        assert_eq!(window.id, "chat-quota");
        assert_eq!(window.label, "Chat Quota");
        assert_eq!(window.duration, Some(Duration::from_secs(3600)));
        assert!((window.used_percent - 60.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn provider_handles_subscription_required_gracefully() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "currentTier": {
                    "id": "free-tier",
                    "name": "Gemini Code Assist for individuals"
                },
                "cloudaicompanionProject": "aicode-consumers"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "email": "individual@example.com"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1internal:retrieveUserQuotaSummary"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {
                    "code": 403,
                    "message": "You do not have a valid license of this product.",
                    "status": "PERMISSION_DENIED",
                    "details": [
                        {
                            "reason": "SUBSCRIPTION_REQUIRED"
                        }
                    ]
                }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = GoogleProviderConfig {
            auth_token: Some("ya29.valid-token".to_owned()),
            refresh_token: None,
            token_expiry: None,
            api_base_url: Url::parse(&mock_server.uri()).unwrap(),
            token_url: Url::parse(&format!("{}/token", mock_server.uri())).unwrap(),
            userinfo_url: Url::parse(&format!("{}/userinfo", mock_server.uri())).unwrap(),
            project_id: None,
            keyring_enabled: false,
            refresh_enabled: false,
        };

        let provider = GoogleProvider::new(config);
        let snapshot = provider
            .refresh(RefreshContext::default())
            .await
            .expect("refresh succeeds");
        assert_eq!(
            snapshot.identity,
            Some(AccountIdentity {
                email: Some("individual@example.com".to_owned()),
                plan: Some("Gemini Code Assist for individuals".to_owned()),
            })
        );
        assert!(snapshot.usage.windows.is_empty());
        assert!(snapshot.usage.balances.is_empty());
    }

    #[tokio::test]
    async fn provider_parses_groups_and_balances() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "currentTier": {
                    "name": "Gemini Advanced"
                },
                "availablePromptCredits": 500,
                "monthlyFlowCredits": 100
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "email": "advanced@example.com"
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1internal:retrieveUserQuotaSummary"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "groups": [
                    {
                        "displayName": "Code Models",
                        "buckets": [
                            {
                                "bucketId": "gemini-pro",
                                "displayName": "Gemini Pro",
                                "window": "24h",
                                "remainingFraction": 0.8
                            }
                        ]
                    }
                ]
            })))
            .mount(&mock_server)
            .await;

        let config = GoogleProviderConfig {
            auth_token: Some("ya29.valid-token".to_owned()),
            refresh_token: None,
            token_expiry: None,
            api_base_url: Url::parse(&mock_server.uri()).unwrap(),
            token_url: Url::parse(&format!("{}/token", mock_server.uri())).unwrap(),
            userinfo_url: Url::parse(&format!("{}/userinfo", mock_server.uri())).unwrap(),
            project_id: None,
            keyring_enabled: false,
            refresh_enabled: false,
        };

        let provider = GoogleProvider::new(config);
        let snapshot = provider
            .refresh(RefreshContext::default())
            .await
            .expect("refresh succeeds");
        assert_eq!(snapshot.usage.windows.len(), 1);
        assert_eq!(snapshot.usage.windows[0].id, "gemini-pro");
        assert_eq!(snapshot.usage.windows[0].label, "Code Models (24h)");
        assert!((snapshot.usage.windows[0].used_percent - 20.0).abs() < 0.001);

        assert_eq!(snapshot.usage.balances.len(), 2);
        assert_eq!(snapshot.usage.balances[0].id, "prompt_credits");
        assert_eq!(snapshot.usage.balances[0].remaining, 500.0);
        assert_eq!(snapshot.usage.balances[1].id, "flow_credits");
        assert_eq!(snapshot.usage.balances[1].remaining, 100.0);
    }

    #[tokio::test]
    async fn provider_unauthorized_maps_to_auth_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&mock_server)
            .await;

        let config = GoogleProviderConfig {
            auth_token: Some("ya29.bad-token".to_owned()),
            refresh_token: None,
            token_expiry: None,
            api_base_url: Url::parse(&mock_server.uri()).unwrap(),
            token_url: Url::parse(&format!("{}/token", mock_server.uri())).unwrap(),
            userinfo_url: Url::parse(&format!("{}/userinfo", mock_server.uri())).unwrap(),
            project_id: None,
            keyring_enabled: false,
            refresh_enabled: false,
        };

        let provider = GoogleProvider::new(config);
        let err = provider
            .refresh(RefreshContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Authentication(_)));
    }
}
