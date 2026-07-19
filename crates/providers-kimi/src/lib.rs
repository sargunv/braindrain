use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use braindrain_core::{
    AccountIdentity, Provider, ProviderError, ProviderFuture, ProviderId, ProviderSnapshot,
    ProviderSource, RateWindow, RefreshContext, UsageSnapshot,
};
use fs4::{FileExt, TryLockError};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

/// Kimi Code subscription API used by kimi-cli 1.49.0.
///
/// Source: MoonshotAI/kimi-cli@4a550effdfcb29a25a5d325bf935296cc50cd417,
/// `auth/platforms.py` and `ui/shell/usage.py`.
pub const KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding/v1";
pub const KIMI_CODE_USAGE_PATH: &str = "usages";
pub const KIMI_CODE_OAUTH_TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";
pub const KIMI_CODE_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
pub const KIMI_SHARE_DIR_ENV: &str = "KIMI_SHARE_DIR";
pub const KIMI_API_KEY_ENV: &str = "KIMI_API_KEY";
pub const KIMI_CODE_BASE_URL_ENV: &str = "KIMI_CODE_BASE_URL";
pub const KIMI_CODE_CREDENTIALS_PATH: &str = "credentials/kimi-code.json";
const KIMI_CODE_LOCK_PATH: &str = "credentials/kimi-code.lock";
const KIMI_DEVICE_ID_PATH: &str = "device_id";
const MIN_REFRESH_THRESHOLD_SECONDS: f64 = 300.0;
const REFRESH_THRESHOLD_RATIO: f64 = 0.5;
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct KimiProvider {
    config: KimiProviderConfig,
    client: reqwest::Client,
}

impl KimiProvider {
    pub fn new(config: KimiProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::builder()
                .user_agent(format!("braindrain/{}", env!("CARGO_PKG_VERSION")))
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .timeout(HTTP_REQUEST_TIMEOUT)
                .build()
                .expect("valid Kimi HTTP client"),
        }
    }

    pub fn config(&self) -> &KimiProviderConfig {
        &self.config
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.config.credentials_path()
    }

    pub fn access_token(&self) -> Result<KimiAccessToken, KimiProviderError> {
        self.config.access_token()
    }

    pub fn usage_url(&self) -> Url {
        self.config.usage_url()
    }

    async fn fetch_usage_with_token(
        &self,
        token: &str,
    ) -> Result<KimiUsageResponse, KimiProviderError> {
        let response = self
            .client
            .get(self.config.usage_url())
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, bearer_value(token)?)
            .send()
            .await
            .map_err(KimiProviderError::Http)?;
        let status = response.status();
        let body = response.bytes().await.map_err(KimiProviderError::Http)?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(KimiProviderError::Unauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        if !status.is_success() {
            return Err(KimiProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        serde_json::from_slice(&body).map_err(KimiProviderError::Decode)
    }

    async fn fresh_access_token(&self, force: bool) -> Result<KimiAccessToken, KimiProviderError> {
        if let Some(token) = self
            .config
            .api_key
            .as_deref()
            .filter(|token| !token.is_empty())
        {
            return Ok(KimiAccessToken {
                value: token.to_owned(),
                source: KimiAccessTokenSource::Config,
            });
        }

        let path = self.config.credentials_path();
        if path.exists() {
            let mut token = KimiOAuthToken::load(&path)?;
            if force || token.needs_refresh(now_unix_seconds()) {
                token = self.refresh_file_token(&path, token, force).await?;
            }
            if token.access_token.is_empty() {
                return Err(KimiProviderError::MissingAccessToken { path });
            }
            return Ok(KimiAccessToken {
                value: token.access_token,
                source: KimiAccessTokenSource::KimiCli,
            });
        }

        if let Some(token) = env::var_os(KIMI_API_KEY_ENV)
            .and_then(|token| token.into_string().ok())
            .filter(|token| !token.is_empty())
        {
            return Ok(KimiAccessToken {
                value: token,
                source: KimiAccessTokenSource::Environment(KIMI_API_KEY_ENV),
            });
        }

        Err(KimiProviderError::MissingCredentials { path })
    }

    async fn refresh_file_token(
        &self,
        path: &Path,
        original: KimiOAuthToken,
        force: bool,
    ) -> Result<KimiOAuthToken, KimiProviderError> {
        let lock_path = self.config.lock_path();
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|source| KimiProviderError::WriteCredentials {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|source| KimiProviderError::WriteCredentials {
                path: lock_path.clone(),
                source,
            })?;
        let deadline = tokio::time::Instant::now() + LOCK_ACQUIRE_TIMEOUT;
        loop {
            match <std::fs::File as FileExt>::try_lock(&lock) {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(KimiProviderError::LockCredentialsTimeout { path: lock_path });
                }
                Err(TryLockError::Error(source)) => {
                    return Err(KimiProviderError::LockCredentials {
                        path: lock_path,
                        source,
                    });
                }
            }
        }

        // Re-read after taking kimi-cli's compatible cross-process lock: another
        // process may have rotated the refresh token while we were waiting.
        let latest = KimiOAuthToken::load(path).unwrap_or(original.clone());
        if latest.refresh_token != original.refresh_token {
            return Ok(latest);
        }
        if !force && !latest.needs_refresh(now_unix_seconds()) {
            return Ok(latest);
        }
        if latest.refresh_token.is_empty() {
            return Err(KimiProviderError::MissingRefreshToken {
                path: path.to_path_buf(),
            });
        }

        let refreshed = self.request_refresh(&latest.refresh_token).await?;
        let merged = latest.apply_refresh(refreshed);
        merged.save_atomic(path)?;
        Ok(merged)
    }

    async fn request_refresh(
        &self,
        refresh_token: &str,
    ) -> Result<KimiTokenRefreshResponse, KimiProviderError> {
        let mut headers = kimi_common_headers(&self.config.share_dir());
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let response = self
            .client
            .post(self.config.oauth_token_url.clone())
            .headers(headers)
            .form(&[
                ("client_id", KIMI_CODE_CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .map_err(KimiProviderError::Http)?;
        let status = response.status();
        let body = response.bytes().await.map_err(KimiProviderError::Http)?;
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(KimiProviderError::RefreshUnauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        if !status.is_success() {
            return Err(KimiProviderError::RefreshStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        serde_json::from_slice(&body).map_err(KimiProviderError::Decode)
    }

    async fn fetch_usage(&self) -> Result<KimiUsageResponse, KimiProviderError> {
        let token = self.fresh_access_token(false).await?;
        match self.fetch_usage_with_token(&token.value).await {
            Err(KimiProviderError::Unauthorized { .. })
                if token.source == KimiAccessTokenSource::KimiCli =>
            {
                let refreshed = self.fresh_access_token(true).await?;
                self.fetch_usage_with_token(&refreshed.value).await
            }
            result => result,
        }
    }
}

impl Default for KimiProvider {
    fn default() -> Self {
        Self::new(KimiProviderConfig::default())
    }
}

impl Provider for KimiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::kimi()
    }

    fn refresh<'a>(
        &'a self,
        context: RefreshContext,
    ) -> ProviderFuture<'a, Result<ProviderSnapshot, ProviderError>> {
        Box::pin(async move {
            let usage = self.fetch_usage().await.map_err(ProviderError::from)?;
            Ok(ProviderSnapshot {
                provider: ProviderId::kimi(),
                source: ProviderSource::OAuth,
                usage: usage.usage_snapshot(),
                identity: usage.identity(),
                updated_at: context.now,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiAccessToken {
    pub value: String,
    pub source: KimiAccessTokenSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiAccessTokenSource {
    Config,
    KimiCli,
    Environment(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub struct KimiProviderConfig {
    pub api_key: Option<String>,
    pub base_url: Url,
    pub credentials_path: Option<PathBuf>,
    pub share_dir: Option<PathBuf>,
    pub oauth_token_url: Url,
}

impl KimiProviderConfig {
    pub fn share_dir(&self) -> PathBuf {
        self.share_dir
            .clone()
            .or_else(|| env::var_os(KIMI_SHARE_DIR_ENV).map(PathBuf::from))
            .unwrap_or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".kimi")
            })
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.credentials_path
            .clone()
            .unwrap_or_else(|| self.share_dir().join(KIMI_CODE_CREDENTIALS_PATH))
    }

    fn lock_path(&self) -> PathBuf {
        self.share_dir().join(KIMI_CODE_LOCK_PATH)
    }

    pub fn usage_url(&self) -> Url {
        Url::parse(&format!(
            "{}/{}",
            self.base_url.as_str().trim_end_matches('/'),
            KIMI_CODE_USAGE_PATH
        ))
        .expect("Kimi usage path is valid")
    }

    pub fn access_token(&self) -> Result<KimiAccessToken, KimiProviderError> {
        if let Some(token) = self.api_key.as_deref().filter(|token| !token.is_empty()) {
            return Ok(KimiAccessToken {
                value: token.to_owned(),
                source: KimiAccessTokenSource::Config,
            });
        }
        let path = self.credentials_path();
        if path.exists() {
            let token = KimiOAuthToken::load(&path)?;
            if token.access_token.is_empty() {
                return Err(KimiProviderError::MissingAccessToken { path });
            }
            return Ok(KimiAccessToken {
                value: token.access_token,
                source: KimiAccessTokenSource::KimiCli,
            });
        }
        if let Some(token) = env::var_os(KIMI_API_KEY_ENV)
            .and_then(|token| token.into_string().ok())
            .filter(|token| !token.is_empty())
        {
            return Ok(KimiAccessToken {
                value: token,
                source: KimiAccessTokenSource::Environment(KIMI_API_KEY_ENV),
            });
        }
        Err(KimiProviderError::MissingCredentials { path })
    }
}

impl Default for KimiProviderConfig {
    fn default() -> Self {
        let base_url = env::var(KIMI_CODE_BASE_URL_ENV)
            .ok()
            .and_then(|url| Url::parse(&url).ok())
            .unwrap_or_else(|| Url::parse(KIMI_CODE_BASE_URL).expect("valid Kimi Code base URL"));
        Self {
            api_key: None,
            base_url,
            credentials_path: None,
            share_dir: None,
            oauth_token_url: Url::parse(KIMI_CODE_OAUTH_TOKEN_URL)
                .expect("valid Kimi OAuth token URL"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KimiOAuthToken {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_at: f64,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    expires_in: f64,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl KimiOAuthToken {
    fn load(path: &Path) -> Result<Self, KimiProviderError> {
        let data = fs::read(path).map_err(|source| KimiProviderError::ReadCredentials {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_slice(&data).map_err(|source| KimiProviderError::ParseCredentials {
            path: path.to_path_buf(),
            source,
        })
    }

    fn needs_refresh(&self, now: f64) -> bool {
        if self.refresh_token.is_empty() {
            return false;
        }
        let threshold =
            (self.expires_in * REFRESH_THRESHOLD_RATIO).max(MIN_REFRESH_THRESHOLD_SECONDS);
        self.expires_at <= 0.0 || self.expires_at - now < threshold
    }

    fn apply_refresh(mut self, refreshed: KimiTokenRefreshResponse) -> Self {
        self.access_token = refreshed.access_token;
        self.refresh_token = refreshed.refresh_token;
        self.scope = refreshed.scope;
        self.token_type = refreshed.token_type;
        self.expires_in = refreshed.expires_in;
        self.expires_at = now_unix_seconds() + refreshed.expires_in;
        self
    }

    fn save_atomic(&self, path: &Path) -> Result<(), KimiProviderError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| KimiProviderError::WriteCredentials {
            path: parent.to_path_buf(),
            source,
        })?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_path = parent.join(format!(
            ".kimi-code.json.{}.{}.tmp",
            std::process::id(),
            nonce
        ));
        let data = serde_json::to_vec(self).map_err(KimiProviderError::Encode)?;
        let mut temp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|source| KimiProviderError::WriteCredentials {
                path: temp_path.clone(),
                source,
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| KimiProviderError::WriteCredentials {
                    path: temp_path.clone(),
                    source,
                })?;
        }
        let result = (|| {
            temp.write_all(&data)?;
            temp.sync_all()?;
            drop(temp);
            fs::rename(&temp_path, path)
        })();
        if let Err(source) = result {
            let _ = fs::remove_file(&temp_path);
            return Err(KimiProviderError::WriteCredentials {
                path: path.to_path_buf(),
                source,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct KimiTokenRefreshResponse {
    access_token: String,
    refresh_token: String,
    expires_in: f64,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    token_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct KimiUsageResponse {
    #[serde(default)]
    usage: Option<KimiUsageDetail>,
    #[serde(default)]
    limits: Vec<KimiLimit>,
    #[serde(default, rename = "totalQuota")]
    total_quota: Option<Value>,
    #[serde(default)]
    user: Option<KimiUser>,
}

impl KimiUsageResponse {
    fn usage_snapshot(&self) -> UsageSnapshot {
        let mut windows = Vec::new();
        if let Some(window) = self
            .usage
            .as_ref()
            .and_then(|detail| detail.rate_window("weekly", "Weekly limit", None))
        {
            windows.push(window);
        }
        for (index, limit) in self.limits.iter().enumerate() {
            if let Some(window) = limit.rate_window(index) {
                windows.push(window);
            }
        }
        if let Some(window) = self
            .total_quota
            .as_ref()
            .and_then(total_quota_detail)
            .and_then(|detail| detail.rate_window("total-quota", "Membership quota", None))
        {
            windows.push(window);
        }
        UsageSnapshot {
            windows,
            balances: Vec::new(),
            reset_credits: Vec::new(),
        }
    }

    fn identity(&self) -> Option<AccountIdentity> {
        let level = self
            .user
            .as_ref()
            .and_then(|user| user.membership.as_ref())
            .and_then(|membership| membership.level.as_deref())
            .filter(|level| !level.is_empty());
        Some(AccountIdentity {
            email: None,
            plan: Some(match level {
                Some(level) => format!("Kimi Coding Plan ({level})"),
                None => "Kimi Coding Plan".to_owned(),
            }),
        })
    }
}

fn total_quota_detail(value: &Value) -> Option<KimiUsageDetail> {
    let object = value.as_object()?;
    let detail = object.get("detail").unwrap_or(value);
    serde_json::from_value(detail.clone()).ok()
}

#[derive(Debug, Clone, Default, Deserialize)]
struct KimiUser {
    #[serde(default)]
    membership: Option<KimiMembership>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct KimiMembership {
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct KimiLimit {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    detail: Option<KimiUsageDetail>,
    #[serde(default)]
    window: Option<KimiWindow>,
    #[serde(flatten)]
    root: KimiUsageDetail,
}

impl KimiLimit {
    fn rate_window(&self, index: usize) -> Option<RateWindow> {
        let detail = self.detail.as_ref().unwrap_or(&self.root);
        let default_label = self
            .name
            .as_deref()
            .or(self.title.as_deref())
            .or(self.scope.as_deref())
            .map(ToOwned::to_owned)
            .or_else(|| self.window.as_ref().and_then(KimiWindow::label))
            .unwrap_or_else(|| format!("Limit #{}", index + 1));
        detail.rate_window(
            &format!("limit-{}", index + 1),
            &default_label,
            self.window.as_ref(),
        )
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct KimiUsageDetail {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    limit: Option<Value>,
    #[serde(default)]
    used: Option<Value>,
    #[serde(default)]
    remaining: Option<Value>,
    #[serde(default, alias = "resetAt", alias = "reset_time", alias = "resetTime")]
    reset_at: Option<Value>,
    #[serde(default, alias = "resetIn", alias = "ttl")]
    reset_in: Option<Value>,
}

impl KimiUsageDetail {
    fn rate_window(
        &self,
        id: &str,
        default_label: &str,
        window: Option<&KimiWindow>,
    ) -> Option<RateWindow> {
        let limit = self.limit.as_ref().and_then(value_to_f64)?;
        if limit <= 0.0 {
            return None;
        }
        let used = self
            .used
            .as_ref()
            .and_then(value_to_f64)
            .or_else(|| {
                self.remaining
                    .as_ref()
                    .and_then(value_to_f64)
                    .map(|remaining| limit - remaining)
            })
            .unwrap_or(0.0);
        let used_percent = ((used.clamp(0.0, limit) / limit) * 100.0).clamp(0.0, 100.0);
        let resets_at = self.reset_at.as_ref().and_then(value_to_time).or_else(|| {
            self.reset_in
                .as_ref()
                .and_then(value_to_f64)
                .and_then(|seconds| {
                    OffsetDateTime::now_utc().checked_add(time::Duration::seconds_f64(seconds))
                })
        });
        Some(RateWindow {
            id: id.to_owned(),
            label: self
                .name
                .as_deref()
                .or(self.title.as_deref())
                .unwrap_or(default_label)
                .to_owned(),
            used_percent,
            duration: window.and_then(KimiWindow::duration),
            resets_at,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct KimiWindow {
    #[serde(default)]
    duration: Option<Value>,
    #[serde(default, rename = "timeUnit", alias = "time_unit")]
    time_unit: Option<String>,
}

impl KimiWindow {
    fn duration(&self) -> Option<Duration> {
        let value = self.duration.as_ref().and_then(value_to_f64)?;
        if value <= 0.0 {
            return None;
        }
        let multiplier = match self
            .time_unit
            .as_deref()
            .unwrap_or("")
            .to_ascii_uppercase()
            .as_str()
        {
            unit if unit.contains("MINUTE") => 60.0,
            unit if unit.contains("HOUR") => 3_600.0,
            unit if unit.contains("DAY") => 86_400.0,
            _ => 1.0,
        };
        Some(Duration::from_secs_f64(value * multiplier))
    }

    fn label(&self) -> Option<String> {
        let duration = self.duration.as_ref().and_then(value_to_f64)?;
        let unit = self.time_unit.as_deref().unwrap_or("").to_ascii_uppercase();
        if unit.contains("MINUTE") && duration >= 60.0 && duration % 60.0 == 0.0 {
            return Some(format!("{}h limit", duration / 60.0));
        }
        if unit.contains("MINUTE") {
            return Some(format!("{duration}m limit"));
        }
        if unit.contains("HOUR") {
            return Some(format!("{duration}h limit"));
        }
        if unit.contains("DAY") {
            return Some(format!("{duration}d limit"));
        }
        Some(format!("{duration}s limit"))
    }
}

#[derive(Debug, Error)]
pub enum KimiProviderError {
    #[error("Kimi Code credentials were not found at {path}; run `kimi login` first")]
    MissingCredentials { path: PathBuf },
    #[error("Kimi Code credentials at {path} contain no access token")]
    MissingAccessToken { path: PathBuf },
    #[error("Kimi Code credentials at {path} contain no refresh token")]
    MissingRefreshToken { path: PathBuf },
    #[error("could not read Kimi Code credentials at {path}: {source}")]
    ReadCredentials {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse Kimi Code credentials at {path}: {source}")]
    ParseCredentials {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("could not write Kimi Code credentials at {path}: {source}")]
    WriteCredentials {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not lock Kimi Code credentials at {path}: {source}")]
    LockCredentials {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("timed out waiting for Kimi Code credential lock at {path}")]
    LockCredentialsTimeout { path: PathBuf },
    #[error("could not encode Kimi Code credentials: {0}")]
    Encode(serde_json::Error),
    #[error("Kimi Code request failed: {0}")]
    Http(reqwest::Error),
    #[error("Kimi Code response could not be decoded: {0}")]
    Decode(serde_json::Error),
    #[error("Kimi Code rejected the usage request with HTTP {status}: {body}")]
    Unauthorized { status: u16, body: String },
    #[error("Kimi Code usage request returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },
    #[error("Kimi Code rejected the token refresh with HTTP {status}: {body}")]
    RefreshUnauthorized { status: u16, body: String },
    #[error("Kimi Code token refresh returned HTTP {status}: {body}")]
    RefreshStatus { status: u16, body: String },
    #[error("could not construct HTTP header {0}")]
    InvalidHeader(String),
}

impl From<KimiProviderError> for ProviderError {
    fn from(error: KimiProviderError) -> Self {
        match error {
            KimiProviderError::MissingCredentials { .. }
            | KimiProviderError::MissingAccessToken { .. }
            | KimiProviderError::MissingRefreshToken { .. }
            | KimiProviderError::ReadCredentials { .. }
            | KimiProviderError::ParseCredentials { .. } => {
                ProviderError::NotConfigured(error.to_string())
            }
            KimiProviderError::Unauthorized { .. }
            | KimiProviderError::RefreshUnauthorized { .. } => {
                ProviderError::Authentication(error.to_string())
            }
            KimiProviderError::Decode(_) => ProviderError::Parse(error.to_string()),
            KimiProviderError::WriteCredentials { .. }
            | KimiProviderError::LockCredentials { .. }
            | KimiProviderError::LockCredentialsTimeout { .. }
            | KimiProviderError::Encode(_)
            | KimiProviderError::Http(_)
            | KimiProviderError::ApiStatus { .. }
            | KimiProviderError::RefreshStatus { .. }
            | KimiProviderError::InvalidHeader(_) => ProviderError::Network(error.to_string()),
        }
    }
}

fn kimi_common_headers(share_dir: &Path) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-msh-platform", "braindrain".to_owned()),
        ("x-msh-version", env!("CARGO_PKG_VERSION").to_owned()),
        (
            "x-msh-device-name",
            env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_owned()),
        ),
        (
            "x-msh-device-model",
            format!("{} {}", env::consts::OS, env::consts::ARCH),
        ),
        ("x-msh-os-version", env::consts::OS.to_owned()),
        (
            "x-msh-device-id",
            fs::read_to_string(share_dir.join(KIMI_DEVICE_ID_PATH))
                .unwrap_or_else(|_| "unknown".to_owned())
                .trim()
                .to_owned(),
        ),
    ] {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&ascii_header_value(&value)),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

fn ascii_header_value(value: &str) -> String {
    let value: String = value.chars().filter(char::is_ascii).collect();
    let value = value.trim();
    if value.is_empty() {
        "unknown".to_owned()
    } else {
        value.to_owned()
    }
}

fn bearer_value(token: &str) -> Result<HeaderValue, KimiProviderError> {
    HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| KimiProviderError::InvalidHeader(AUTHORIZATION.as_str().to_owned()))
}

fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn value_to_time(value: &Value) -> Option<OffsetDateTime> {
    match value {
        Value::String(value) => OffsetDateTime::parse(value, &Rfc3339).ok(),
        _ => {
            let raw = value_to_f64(value)?;
            let seconds = if raw.abs() >= 1_000_000_000_000.0 {
                raw / 1_000.0
            } else {
                raw
            };
            OffsetDateTime::from_unix_timestamp_nanos((seconds * 1_000_000_000.0) as i128).ok()
        }
    }
}

fn now_unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn body_preview(body: &[u8]) -> String {
    const MAX_BODY_PREVIEW: usize = 512;
    if body.len() > MAX_BODY_PREVIEW {
        let mut preview = String::from_utf8_lossy(&body[..MAX_BODY_PREVIEW]).into_owned();
        preview.push_str("...");
        preview
    } else {
        String::from_utf8_lossy(body).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use braindrain_core::{Provider, RefreshContext};
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config(server: &MockServer, credentials_path: PathBuf) -> KimiProviderConfig {
        KimiProviderConfig {
            api_key: None,
            base_url: Url::parse(&format!("{}/coding/v1/", server.uri())).expect("base URL"),
            credentials_path: Some(credentials_path.clone()),
            share_dir: credentials_path
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            oauth_token_url: Url::parse(&format!("{}/api/oauth/token", server.uri()))
                .expect("OAuth URL"),
        }
    }

    #[test]
    fn defaults_match_first_party_kimi_code_platform() {
        let config = KimiProviderConfig::default();
        assert_eq!(config.base_url.as_str(), "https://api.kimi.com/coding/v1");
        assert_eq!(
            config.usage_url().as_str(),
            "https://api.kimi.com/coding/v1/usages"
        );
    }

    #[test]
    fn discovers_first_party_credentials_and_preserves_unknown_fields() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join(KIMI_CODE_CREDENTIALS_PATH);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_at": 2_000_000_000.0,
                "scope": "kimi-code",
                "token_type": "Bearer",
                "expires_in": 900.0,
                "future_field": {"keep": true}
            })
            .to_string(),
        )
        .expect("write credentials");
        let token = KimiOAuthToken::load(&path).expect("load token");
        assert_eq!(token.access_token, "access");
        assert_eq!(token.extra["future_field"]["keep"], true);
    }

    #[test]
    fn parses_first_party_usage_shape() {
        let response: KimiUsageResponse = serde_json::from_value(serde_json::json!({
            "user": {"membership": {"level": "ultra"}},
            "usage": {
                "limit": 1000,
                "used": 250,
                "reset_at": "2026-07-26T00:00:00Z"
            },
            "limits": [
                {
                    "window": {"duration": 300, "timeUnit": "MINUTE"},
                    "detail": {
                        "limit": "100",
                        "remaining": "40",
                        "resetTime": "2026-07-19T05:00:00Z"
                    }
                }
            ],
            "totalQuota": {
                "detail": {
                    "limit": 10000,
                    "used": 10000,
                    "resetTime": "2026-08-01T00:00:00Z"
                }
            }
        }))
        .expect("parse usage");
        let snapshot = response.usage_snapshot();
        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(snapshot.windows[0].id, "weekly");
        assert_eq!(snapshot.windows[0].used_percent, 25.0);
        assert_eq!(snapshot.windows[1].label, "5h limit");
        assert_eq!(snapshot.windows[1].used_percent, 60.0);
        assert_eq!(
            snapshot.windows[1].duration,
            Some(Duration::from_secs(18_000))
        );
        assert_eq!(
            snapshot.windows[1]
                .resets_at
                .expect("reset")
                .unix_timestamp(),
            1_784_437_200
        );
        assert_eq!(snapshot.windows[2].id, "total-quota");
        assert_eq!(snapshot.windows[2].used_percent, 100.0);
        assert_eq!(
            response.identity().expect("identity").plan.as_deref(),
            Some("Kimi Coding Plan (ultra)")
        );

        let scalar: KimiUsageResponse = serde_json::from_value(serde_json::json!({
            "totalQuota": false
        }))
        .expect("scalar total quota remains forward-compatible");
        assert!(scalar.usage_snapshot().windows.is_empty());
    }

    #[tokio::test]
    async fn provider_fetches_subscription_usage_with_oauth_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/coding/v1/usages"))
            .and(header("authorization", "Bearer test-access"))
            .and(header("user-agent", "braindrain/0.1.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "usage": {"limit": 100, "used": 42}
            })))
            .mount(&server)
            .await;
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join(KIMI_CODE_CREDENTIALS_PATH);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            serde_json::json!({
                "access_token": "test-access",
                "refresh_token": "test-refresh",
                "expires_at": now_unix_seconds() + 3600.0,
                "expires_in": 900.0
            })
            .to_string(),
        )
        .expect("write credentials");
        let provider = KimiProvider::new(test_config(&server, path));
        let snapshot = provider
            .refresh(RefreshContext {
                now: OffsetDateTime::from_unix_timestamp(1_784_419_200).expect("timestamp"),
            })
            .await
            .expect("refresh");
        assert_eq!(snapshot.provider, ProviderId::kimi());
        assert_eq!(snapshot.source, ProviderSource::OAuth);
        assert_eq!(snapshot.usage.windows[0].used_percent, 42.0);
        assert_eq!(
            snapshot.identity.expect("identity").plan.as_deref(),
            Some("Kimi Coding Plan")
        );
    }

    #[tokio::test]
    async fn refreshes_expiring_token_with_first_party_form_and_writes_rotation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .and(body_string_contains(
                "client_id=17e5f671-d194-4dfb-9706-5516cb48c098",
            ))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=old-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 900,
                "scope": "kimi-code",
                "token_type": "Bearer"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/coding/v1/usages"))
            .and(header("authorization", "Bearer new-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "usage": {"limit": 100, "remaining": 90}
            })))
            .mount(&server)
            .await;
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join(KIMI_CODE_CREDENTIALS_PATH);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            serde_json::json!({
                "access_token": "old-access",
                "refresh_token": "old-refresh",
                "expires_at": 1,
                "expires_in": 900,
                "future_field": "preserved"
            })
            .to_string(),
        )
        .expect("write credentials");
        let provider = KimiProvider::new(test_config(&server, path.clone()));
        provider.fetch_usage().await.expect("fetch usage");
        let stored = KimiOAuthToken::load(&path).expect("stored token");
        assert_eq!(stored.access_token, "new-access");
        assert_eq!(stored.refresh_token, "new-refresh");
        assert_eq!(stored.extra["future_field"], "preserved");
    }

    #[tokio::test]
    async fn forced_refresh_uses_rotation_written_before_lock() {
        let server = MockServer::start().await;
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join(KIMI_CODE_CREDENTIALS_PATH);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let original: KimiOAuthToken = serde_json::from_value(serde_json::json!({
            "access_token": "old-access",
            "refresh_token": "old-refresh",
            "expires_at": 1,
            "expires_in": 900
        }))
        .expect("original");
        fs::write(
            &path,
            serde_json::json!({
                "access_token": "rotated-access",
                "refresh_token": "rotated-refresh",
                "expires_at": now_unix_seconds() + 900.0,
                "expires_in": 900
            })
            .to_string(),
        )
        .expect("write rotation");
        let provider = KimiProvider::new(test_config(&server, path.clone()));
        let token = provider
            .refresh_file_token(&path, original, true)
            .await
            .expect("use external rotation");
        assert_eq!(token.access_token, "rotated-access");
        assert_eq!(token.refresh_token, "rotated-refresh");
    }

    #[test]
    fn body_preview_is_safe_at_multibyte_boundary() {
        let body = "界".repeat(300);
        let preview = body_preview(body.as_bytes());
        assert!(preview.ends_with("..."));
        assert!(preview.len() <= 518);
    }
}
