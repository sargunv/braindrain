use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use braindrain_core::{
    AccountIdentity, BalanceSnapshot, Provider, ProviderError, ProviderFuture, ProviderId,
    ProviderSnapshot, ProviderSource, RateWindow, RefreshContext, UsageSnapshot,
};
use fs4::{FileExt, TryLockError};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

/// Claude Code subscription usage endpoint.
///
/// The Claude Code CLI reads its own plan limits from this endpoint using the
/// OAuth access token it stores after `claude login`. Only the bearer token is
/// required; the `anthropic-beta` header is sent because the CLI sends it and
/// the endpoint has required it in the past.
pub const CLAUDE_OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// OAuth token endpoint and public client id used by Claude Code for the
/// `claudeAiOauth` subscription credential, read out of the CLI bundle
/// (`TOKEN_URL` / `CLIENT_ID` in its OAuth config). Note the token endpoint is
/// on `platform.claude.com`, not the `api.anthropic.com` base used for usage.
pub const CLAUDE_OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLAUDE_OAUTH_TOKEN_URL_ENV: &str = "CLAUDE_CODE_TOKEN_URL";
pub const CLAUDE_CODE_OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
pub const CLAUDE_CODE_USAGE_URL_ENV: &str = "CLAUDE_CODE_USAGE_URL";
pub const CLAUDE_CREDENTIALS_FILE: &str = ".credentials.json";
const CLAUDE_CREDENTIALS_LOCK_FILE: &str = ".credentials.json.braindrain.lock";
/// Keychain entry written by Claude Code on macOS.
pub const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

const FIVE_HOURS: Duration = Duration::from_secs(5 * 60 * 60);
const SEVEN_DAYS: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct ClaudeProvider {
    config: ClaudeProviderConfig,
    client: reqwest::Client,
}

impl ClaudeProvider {
    pub fn new(config: ClaudeProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::builder()
                .user_agent(format!("braindrain/{}", env!("CARGO_PKG_VERSION")))
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .timeout(HTTP_REQUEST_TIMEOUT)
                .build()
                .expect("valid Claude HTTP client"),
        }
    }

    pub fn config(&self) -> &ClaudeProviderConfig {
        &self.config
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.config.credentials_path()
    }

    pub fn usage_url(&self) -> Url {
        self.config.usage_url.clone()
    }

    pub async fn access_token(&self) -> Result<ClaudeAccessToken, ClaudeProviderError> {
        self.config.access_token().await
    }

    /// Resolves a usable access token, exchanging the refresh token when the
    /// stored one has expired.
    async fn resolve_access_token(&self) -> Result<ClaudeAccessToken, ClaudeProviderError> {
        if let Some(token) = self.config.static_access_token() {
            return Ok(token);
        }

        let path = self.config.credentials_path();
        if path.exists() {
            return self.file_access_token(&path).await;
        }

        // The keyring copy is read-only: rotating a credential braindrain
        // cannot write back would strand the CLI.
        if self.config.keyring_enabled
            && let Some(oauth) = system_keychain_credentials().await?
        {
            return oauth.into_access_token(ClaudeAccessTokenSource::Keyring, &path);
        }

        Err(ClaudeProviderError::MissingCredentials { path })
    }

    async fn file_access_token(
        &self,
        path: &Path,
    ) -> Result<ClaudeAccessToken, ClaudeProviderError> {
        let raw = load_raw_credentials(path)?;
        let oauth = oauth_from_raw(&raw, path)?;
        if oauth.access_token.is_empty() {
            return Err(ClaudeProviderError::MissingAccessToken {
                path: path.to_owned(),
            });
        }
        if !oauth.is_expired(now_unix_millis()) {
            return Ok(oauth.into_token_unchecked(ClaudeAccessTokenSource::ClaudeCode));
        }
        if !self.config.refresh_enabled {
            return Err(ClaudeProviderError::ExpiredAccessToken {
                path: path.to_owned(),
            });
        }
        self.refresh_file_token(path, oauth).await
    }

    async fn refresh_file_token(
        &self,
        path: &Path,
        expired: ClaudeOAuthCredential,
    ) -> Result<ClaudeAccessToken, ClaudeProviderError> {
        if expired.refresh_token.is_empty() {
            return Err(ClaudeProviderError::MissingRefreshToken {
                path: path.to_owned(),
            });
        }
        if expired.refresh_token_expired(now_unix_millis()) {
            return Err(ClaudeProviderError::ExpiredRefreshToken {
                path: path.to_owned(),
            });
        }

        // Rotation invalidates the previous access *and* refresh token, so two
        // concurrent exchanges leave the loser holding dead credentials. Hold a
        // lock across exchange-and-write so no two braindrain processes (daemon
        // and CLI) can race. This cannot coordinate with Claude Code itself,
        // which uses no lock file — see `refresh_enabled` to opt out entirely.
        let _lock = FileLock::acquire(&self.config.lock_path()).await?;

        // Re-check under the lock: another process may have refreshed while we
        // waited, in which case its token is good and we must not rotate again.
        let mut raw = load_raw_credentials(path)?;
        let latest = oauth_from_raw(&raw, path)?;
        if !latest.access_token.is_empty() && !latest.is_expired(now_unix_millis()) {
            return Ok(latest.into_token_unchecked(ClaudeAccessTokenSource::ClaudeCode));
        }
        // Use whatever refresh token is on disk now, not the one we read before
        // taking the lock.
        let refresh_token = if latest.refresh_token.is_empty() {
            expired.refresh_token.clone()
        } else {
            latest.refresh_token.clone()
        };

        let refreshed = self.request_refresh(&refresh_token, &latest.scopes).await?;

        apply_refresh(&mut raw, &refreshed, &refresh_token, now_unix_millis());
        save_raw_credentials(path, &raw)?;

        Ok(ClaudeAccessToken {
            value: refreshed.access_token,
            source: ClaudeAccessTokenSource::ClaudeCode,
            subscription_type: latest
                .subscription_type
                .or(expired.subscription_type)
                .filter(|subscription| !subscription.is_empty()),
        })
    }

    async fn request_refresh(
        &self,
        refresh_token: &str,
        scopes: &[String],
    ) -> Result<ClaudeTokenRefreshResponse, ClaudeProviderError> {
        let request = ClaudeTokenRefreshRequest {
            grant_type: "refresh_token",
            refresh_token,
            client_id: &self.config.client_id,
            scope: scopes.join(" "),
        };
        let response = self
            .client
            .post(self.config.token_url.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .json(&request)
            .send()
            .await
            .map_err(ClaudeProviderError::Http)?;
        let status = response.status();
        let body = response.bytes().await.map_err(ClaudeProviderError::Http)?;

        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::BAD_REQUEST
        {
            return Err(ClaudeProviderError::RefreshRejected {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        if !status.is_success() {
            return Err(ClaudeProviderError::RefreshStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        serde_json::from_slice(&body).map_err(ClaudeProviderError::Decode)
    }

    async fn fetch_usage(&self, token: &str) -> Result<ClaudeUsageResponse, ClaudeProviderError> {
        let response = self
            .client
            .get(self.config.usage_url.clone())
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, bearer_value(token)?)
            .header(
                HeaderName::from_static("anthropic-beta"),
                HeaderValue::from_static(CLAUDE_OAUTH_BETA),
            )
            .send()
            .await
            .map_err(ClaudeProviderError::Http)?;
        let status = response.status();
        let body = response.bytes().await.map_err(ClaudeProviderError::Http)?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ClaudeProviderError::Unauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ClaudeProviderError::RateLimited {
                body: body_preview(&body),
            });
        }
        if !status.is_success() {
            return Err(ClaudeProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        serde_json::from_slice(&body).map_err(ClaudeProviderError::Decode)
    }
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new(ClaudeProviderConfig::default())
    }
}

impl Provider for ClaudeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::claude()
    }

    fn refresh<'a>(
        &'a self,
        context: RefreshContext,
    ) -> ProviderFuture<'a, Result<ProviderSnapshot, ProviderError>> {
        Box::pin(async move {
            let token = self.resolve_access_token().await?;
            let usage = self.fetch_usage(&token.value).await?;
            Ok(ProviderSnapshot {
                provider: ProviderId::claude(),
                source: ProviderSource::OAuth,
                usage: usage.usage_snapshot(),
                identity: token.identity(),
                updated_at: context.now,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeAccessToken {
    pub value: String,
    pub source: ClaudeAccessTokenSource,
    /// Subscription tier recorded alongside the token, when the credential
    /// store provides one (`max`, `pro`, ...).
    pub subscription_type: Option<String>,
}

impl ClaudeAccessToken {
    fn identity(&self) -> Option<AccountIdentity> {
        Some(AccountIdentity {
            email: None,
            plan: Some(plan_label(self.subscription_type.as_deref())),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeAccessTokenSource {
    Config,
    ClaudeCode,
    Keyring,
    Environment(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeProviderConfig {
    pub oauth_token: Option<String>,
    pub usage_url: Url,
    pub token_url: Url,
    pub client_id: String,
    pub credentials_path: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
    /// Consult the system keyring when no credentials file is present. Claude
    /// Code stores credentials there on macOS; disabled in tests so they never
    /// touch the real keychain.
    pub keyring_enabled: bool,
    /// Exchange the refresh token when the access token has expired, writing
    /// the result back to the credentials file the CLI shares. Set false to
    /// keep braindrain strictly read-only.
    pub refresh_enabled: bool,
}

impl ClaudeProviderConfig {
    pub fn config_dir(&self) -> PathBuf {
        self.config_dir
            .clone()
            .or_else(|| env::var_os(CLAUDE_CONFIG_DIR_ENV).map(PathBuf::from))
            .unwrap_or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".claude")
            })
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.credentials_path
            .clone()
            .unwrap_or_else(|| self.config_dir().join(CLAUDE_CREDENTIALS_FILE))
    }

    /// Lock file kept beside the credentials it guards, so the lock follows a
    /// redirected `CLAUDE_CONFIG_DIR`.
    fn lock_path(&self) -> PathBuf {
        let credentials = self.credentials_path();
        let parent = credentials
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        parent.join(CLAUDE_CREDENTIALS_LOCK_FILE)
    }

    /// Tokens supplied directly, which carry no expiry and are never refreshed.
    fn static_access_token(&self) -> Option<ClaudeAccessToken> {
        let (value, source) = match self.oauth_token.as_deref().filter(|t| !t.is_empty()) {
            Some(token) => (token.to_owned(), ClaudeAccessTokenSource::Config),
            None => (
                env::var_os(CLAUDE_CODE_OAUTH_TOKEN_ENV)
                    .and_then(|token| token.into_string().ok())
                    .filter(|token| !token.is_empty())?,
                ClaudeAccessTokenSource::Environment(CLAUDE_CODE_OAUTH_TOKEN_ENV),
            ),
        };
        Some(ClaudeAccessToken {
            value,
            source,
            subscription_type: None,
        })
    }

    /// Read-only credential lookup: never performs a token exchange, so it is
    /// safe for local diagnostics.
    pub async fn access_token(&self) -> Result<ClaudeAccessToken, ClaudeProviderError> {
        if let Some(token) = self.static_access_token() {
            return Ok(token);
        }

        let path = self.credentials_path();
        if path.exists() {
            let raw = load_raw_credentials(&path)?;
            return oauth_from_raw(&raw, &path)?
                .into_access_token(ClaudeAccessTokenSource::ClaudeCode, &path);
        }

        if self.keyring_enabled
            && let Some(oauth) = system_keychain_credentials().await?
        {
            return oauth.into_access_token(ClaudeAccessTokenSource::Keyring, &path);
        }

        Err(ClaudeProviderError::MissingCredentials { path })
    }
}

impl Default for ClaudeProviderConfig {
    fn default() -> Self {
        let usage_url = env::var(CLAUDE_CODE_USAGE_URL_ENV)
            .ok()
            .and_then(|url| Url::parse(&url).ok())
            .unwrap_or_else(|| Url::parse(CLAUDE_OAUTH_USAGE_URL).expect("valid Claude usage URL"));
        let token_url = env::var(CLAUDE_OAUTH_TOKEN_URL_ENV)
            .ok()
            .and_then(|url| Url::parse(&url).ok())
            .unwrap_or_else(|| Url::parse(CLAUDE_OAUTH_TOKEN_URL).expect("valid Claude token URL"));
        Self {
            oauth_token: None,
            usage_url,
            token_url,
            client_id: CLAUDE_OAUTH_CLIENT_ID.to_owned(),
            credentials_path: None,
            config_dir: None,
            keyring_enabled: true,
            refresh_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeCredentials {
    #[serde(default, rename = "claudeAiOauth")]
    claude_ai_oauth: Option<ClaudeOAuthCredential>,
}

impl ClaudeCredentials {
    fn parse(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }
}

/// The `claudeAiOauth` object written by Claude Code.
#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeOAuthCredential {
    #[serde(default, rename = "accessToken")]
    access_token: String,
    #[serde(default, rename = "refreshToken")]
    refresh_token: String,
    /// Milliseconds since the Unix epoch.
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<f64>,
    #[serde(default, rename = "refreshTokenExpiresAt")]
    refresh_token_expires_at: Option<f64>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default, rename = "subscriptionType")]
    subscription_type: Option<String>,
}

impl ClaudeOAuthCredential {
    fn into_access_token(
        self,
        source: ClaudeAccessTokenSource,
        path: &PathBuf,
    ) -> Result<ClaudeAccessToken, ClaudeProviderError> {
        if self.access_token.is_empty() {
            return Err(ClaudeProviderError::MissingAccessToken {
                path: path.to_owned(),
            });
        }
        // An already-expired token cannot produce usage, so fail with a clear
        // instruction rather than making a doomed request. Callers that can
        // refresh handle expiry before reaching here.
        if self.is_expired(now_unix_millis()) {
            return Err(ClaudeProviderError::ExpiredAccessToken {
                path: path.to_owned(),
            });
        }
        Ok(self.into_token_unchecked(source))
    }

    fn into_token_unchecked(self, source: ClaudeAccessTokenSource) -> ClaudeAccessToken {
        ClaudeAccessToken {
            value: self.access_token,
            source,
            subscription_type: self
                .subscription_type
                .filter(|subscription| !subscription.is_empty()),
        }
    }

    fn is_expired(&self, now_millis: f64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at > 0.0 && expires_at <= now_millis)
    }

    fn refresh_token_expired(&self, now_millis: f64) -> bool {
        self.refresh_token_expires_at
            .is_some_and(|expires_at| expires_at > 0.0 && expires_at <= now_millis)
    }
}

/// Response from `POST /v1/oauth/token` with a `refresh_token` grant. Claude
/// Code treats a missing `refresh_token` as "keep the existing one", so
/// rotation is optional server-side.
#[derive(Debug, Clone, Deserialize)]
struct ClaudeTokenRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<f64>,
    #[serde(default)]
    refresh_token_expires_in: Option<f64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize)]
struct ClaudeTokenRefreshRequest<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
    client_id: &'a str,
    scope: String,
}

/// Applies a refresh result to the raw credentials JSON, mutating only the
/// fields Claude Code itself updates. Everything else in the file — including
/// `subscriptionType`, `rateLimitTier`, and any key a future CLI adds — is
/// carried through untouched.
fn apply_refresh(
    raw: &mut Value,
    refreshed: &ClaudeTokenRefreshResponse,
    previous_refresh_token: &str,
    now_millis: f64,
) {
    let Some(oauth) = raw.get_mut("claudeAiOauth").and_then(Value::as_object_mut) else {
        return;
    };

    oauth.insert(
        "accessToken".to_owned(),
        Value::String(refreshed.access_token.clone()),
    );
    oauth.insert(
        "refreshToken".to_owned(),
        Value::String(
            refreshed
                .refresh_token
                .clone()
                .filter(|token| !token.is_empty())
                .unwrap_or_else(|| previous_refresh_token.to_owned()),
        ),
    );
    if let Some(expires_in) = refreshed.expires_in {
        oauth.insert(
            "expiresAt".to_owned(),
            millis_value(now_millis + expires_in * 1_000.0),
        );
    }
    if let Some(expires_in) = refreshed.refresh_token_expires_in {
        oauth.insert(
            "refreshTokenExpiresAt".to_owned(),
            millis_value(now_millis + expires_in * 1_000.0),
        );
    }
    if let Some(scope) = refreshed.scope.as_deref() {
        oauth.insert(
            "scopes".to_owned(),
            Value::Array(
                scope
                    .split_whitespace()
                    .map(|scope| Value::String(scope.to_owned()))
                    .collect(),
            ),
        );
    }
}

/// Claude Code stores millisecond timestamps as JSON integers; match that so
/// the file keeps the same shape it had before braindrain touched it.
fn millis_value(millis: f64) -> Value {
    Value::Number(Number::from(millis.round() as i64))
}

/// Advisory cross-process lock guarding the read-exchange-write sequence.
/// Released on drop.
struct FileLock(std::fs::File);

impl FileLock {
    async fn acquire(path: &Path) -> Result<Self, ClaudeProviderError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ClaudeProviderError::LockCredentials {
                path: parent.to_owned(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| ClaudeProviderError::LockCredentials {
                path: path.to_owned(),
                source,
            })?;

        let deadline = tokio::time::Instant::now() + LOCK_ACQUIRE_TIMEOUT;
        loop {
            match <std::fs::File as FileExt>::try_lock(&file) {
                Ok(()) => return Ok(Self(file)),
                Err(TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(ClaudeProviderError::LockCredentialsTimeout {
                        path: path.to_owned(),
                    });
                }
                Err(TryLockError::Error(source)) => {
                    return Err(ClaudeProviderError::LockCredentials {
                        path: path.to_owned(),
                        source,
                    });
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

fn load_raw_credentials(path: &Path) -> Result<Value, ClaudeProviderError> {
    let data = fs::read(path).map_err(|source| ClaudeProviderError::ReadCredentials {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_slice(&data).map_err(|source| ClaudeProviderError::ParseCredentials {
        path: path.to_owned(),
        source,
    })
}

fn oauth_from_raw(raw: &Value, path: &Path) -> Result<ClaudeOAuthCredential, ClaudeProviderError> {
    raw.get("claudeAiOauth")
        .cloned()
        .and_then(|oauth| serde_json::from_value(oauth).ok())
        .ok_or_else(|| ClaudeProviderError::MissingAccessToken {
            path: path.to_owned(),
        })
}

/// Writes credentials via a private temp file and an atomic rename, so a
/// concurrent Claude Code read never observes a partial file.
fn save_raw_credentials(path: &Path, raw: &Value) -> Result<(), ClaudeProviderError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = parent.join(format!(
        ".credentials.json.braindrain.{}.{}.tmp",
        std::process::id(),
        nonce
    ));
    // Compact, no trailing newline: byte-for-byte the shape Claude Code writes.
    let data = serde_json::to_vec(raw).map_err(ClaudeProviderError::Encode)?;

    let write = (|| -> std::io::Result<()> {
        let mut temp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        temp.write_all(&data)?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&temp_path, path)
    })();

    if let Err(source) = write {
        let _ = fs::remove_file(&temp_path);
        return Err(ClaudeProviderError::WriteCredentials {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

async fn system_keychain_credentials() -> Result<Option<ClaudeOAuthCredential>, ClaudeProviderError>
{
    tokio::task::spawn_blocking(system_keychain_credentials_blocking)
        .await
        .map_err(|_| ClaudeProviderError::KeyringJoin)?
}

#[cfg(target_os = "macos")]
fn system_keychain_credentials_blocking()
-> Result<Option<ClaudeOAuthCredential>, ClaudeProviderError> {
    let account = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_default();
    // Claude Code writes this item with `/usr/bin/security`, whose update path
    // restores the item to the `apple-tool:` partition. Reading it directly
    // through Security.framework makes every later Claude credential update
    // invalidate BrainDrain's "Always Allow" grant. Use the same Apple tool so
    // the reader continues to match the ACL that Claude owns.
    let output = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            CLAUDE_KEYCHAIN_SERVICE,
            "-a",
            &account,
            "-w",
        ])
        .output()
        .map_err(ClaudeProviderError::KeychainCommand)?;

    decode_macos_keychain_output(output)
}

#[cfg(target_os = "macos")]
fn decode_macos_keychain_output(
    output: Output,
) -> Result<Option<ClaudeOAuthCredential>, ClaudeProviderError> {
    if output.status.success() {
        return Ok(ClaudeCredentials::parse(&output.stdout)
            .map_err(ClaudeProviderError::KeyringDecode)?
            .claude_ai_oauth);
    }
    // `security` returns the low byte of errSecItemNotFound (-25300).
    if output.status.code() == Some(44) {
        return Ok(None);
    }
    Err(ClaudeProviderError::KeychainCommandStatus {
        status: output.status,
    })
}

#[cfg(not(target_os = "macos"))]
fn system_keychain_credentials_blocking()
-> Result<Option<ClaudeOAuthCredential>, ClaudeProviderError> {
    let account = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_default();
    let entry = keyring::Entry::new(CLAUDE_KEYCHAIN_SERVICE, &account)
        .map_err(ClaudeProviderError::Keyring)?;
    match entry.get_password() {
        Ok(secret) => Ok(ClaudeCredentials::parse(secret.as_bytes())
            .map_err(ClaudeProviderError::KeyringDecode)?
            .claude_ai_oauth),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(ClaudeProviderError::Keyring(error)),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeUsageResponse {
    #[serde(default)]
    five_hour: Option<ClaudeUsageWindow>,
    #[serde(default)]
    seven_day: Option<ClaudeUsageWindow>,
    #[serde(default)]
    seven_day_opus: Option<ClaudeUsageWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<ClaudeUsageWindow>,
    /// Newer shape: a flat list of limit entries, each optionally naming the
    /// model it is scoped to. Supersedes the flat `seven_day_*` fields for
    /// per-model weekly windows.
    #[serde(default)]
    limits: Vec<ClaudeLimitEntry>,
    #[serde(default)]
    extra_usage: Option<ClaudeExtraUsage>,
}

impl ClaudeUsageResponse {
    fn usage_snapshot(&self) -> UsageSnapshot {
        let mut windows = Vec::new();

        if let Some(window) = self
            .five_hour
            .as_ref()
            .and_then(|window| window.rate_window("five-hour", "5-hour limit", FIVE_HOURS))
        {
            windows.push(window);
        }
        if let Some(window) = self
            .seven_day
            .as_ref()
            .and_then(|window| window.rate_window("seven-day", "Weekly limit", SEVEN_DAYS))
        {
            windows.push(window);
        }

        // Model-scoped weekly windows, preferring the `limits` list.
        let mut scoped = Vec::new();
        for entry in &self.limits {
            if let Some(window) = entry.scoped_rate_window() {
                scoped.push(window.0);
                windows.push(window.1);
            }
        }
        for (window, model) in [
            (self.seven_day_opus.as_ref(), "Opus"),
            (self.seven_day_sonnet.as_ref(), "Sonnet"),
        ] {
            if scoped.iter().any(|seen| seen.eq_ignore_ascii_case(model)) {
                continue;
            }
            if let Some(window) = window.and_then(|window| {
                window.rate_window(
                    &format!("seven-day-{}", slug(model)),
                    &format!("Weekly limit ({model})"),
                    SEVEN_DAYS,
                )
            }) {
                scoped.push(model.to_owned());
                windows.push(window);
            }
        }

        UsageSnapshot {
            windows,
            balances: self
                .extra_usage
                .as_ref()
                .and_then(ClaudeExtraUsage::balance)
                .into_iter()
                .collect(),
            reset_credits: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeUsageWindow {
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
}

impl ClaudeUsageWindow {
    fn rate_window(&self, id: &str, label: &str, duration: Duration) -> Option<RateWindow> {
        Some(RateWindow {
            id: id.to_owned(),
            label: label.to_owned(),
            used_percent: self.utilization?.clamp(0.0, 100.0),
            duration: Some(duration),
            resets_at: parse_timestamp(self.resets_at.as_deref()),
        })
    }
}

/// One entry from the `limits` array. `kind`/`group` classify the limit
/// (`kind: "weekly_scoped"`, `group: "weekly"`), and `scope.model.display_name`
/// names the model when the limit applies to one model rather than the account.
#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeLimitEntry {
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    scope: Option<ClaudeLimitScope>,
}

impl ClaudeLimitEntry {
    /// Returns the scoped model name alongside its window, or `None` when the
    /// entry is account-wide (already covered by `five_hour` / `seven_day`) or
    /// is an inapplicable promotional row — zero usage and no reset time.
    fn scoped_rate_window(&self) -> Option<(String, RateWindow)> {
        let model = self
            .scope
            .as_ref()
            .and_then(|scope| scope.model.as_ref())
            .and_then(|model| model.display_name.as_deref())
            .map(str::trim)
            .filter(|model| !model.is_empty())?;
        let percent = self.percent.unwrap_or(0.0);
        if percent <= 0.0 && self.resets_at.is_none() {
            return None;
        }
        let duration = self
            .group
            .as_deref()
            .filter(|group| group.eq_ignore_ascii_case("weekly"))
            .map(|_| SEVEN_DAYS);
        Some((
            model.to_owned(),
            RateWindow {
                id: format!("seven-day-{}", slug(model)),
                label: format!("Weekly limit ({model})"),
                used_percent: percent.clamp(0.0, 100.0),
                duration,
                resets_at: parse_timestamp(self.resets_at.as_deref()),
            },
        ))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeLimitScope {
    #[serde(default)]
    model: Option<ClaudeLimitScopeModel>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeLimitScopeModel {
    #[serde(default)]
    display_name: Option<String>,
}

/// Pay-as-you-go credits that extend a subscription past its plan limits.
#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeExtraUsage {
    #[serde(default)]
    is_enabled: Option<bool>,
    #[serde(default)]
    monthly_limit: Option<f64>,
    #[serde(default)]
    used_credits: Option<f64>,
    #[serde(default)]
    currency: Option<String>,
}

impl ClaudeExtraUsage {
    fn balance(&self) -> Option<BalanceSnapshot> {
        if !self.is_enabled.unwrap_or(false) {
            return None;
        }
        let limit = self.monthly_limit?;
        let used = self.used_credits.unwrap_or(0.0);
        Some(BalanceSnapshot {
            id: "extra-usage".to_owned(),
            label: "Extra usage credits".to_owned(),
            remaining: (limit - used).max(0.0),
            unit: self
                .currency
                .as_deref()
                .filter(|currency| !currency.is_empty())
                .unwrap_or("USD")
                .to_owned(),
        })
    }
}

#[derive(Debug, Error)]
pub enum ClaudeProviderError {
    #[error("Claude Code credentials were not found at {path}; run `claude` to sign in first")]
    MissingCredentials { path: PathBuf },
    #[error("Claude Code credentials at {path} contain no access token")]
    MissingAccessToken { path: PathBuf },
    #[error(
        "the Claude Code access token at {path} has expired; run `claude` to refresh it, \
         or set {CLAUDE_CODE_OAUTH_TOKEN_ENV}"
    )]
    ExpiredAccessToken { path: PathBuf },
    #[error("Claude Code credentials at {path} contain no refresh token")]
    MissingRefreshToken { path: PathBuf },
    #[error("the Claude Code refresh token at {path} has expired; run `claude` to sign in again")]
    ExpiredRefreshToken { path: PathBuf },
    #[error("could not read Claude Code credentials at {path}: {source}")]
    ReadCredentials {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write Claude Code credentials at {path}: {source}")]
    WriteCredentials {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not encode Claude Code credentials: {0}")]
    Encode(serde_json::Error),
    #[error("could not lock Claude Code credentials at {path}: {source}")]
    LockCredentials {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("timed out waiting for the Claude Code credential lock at {path}")]
    LockCredentialsTimeout { path: PathBuf },
    #[error("Claude rejected the token refresh with HTTP {status}: {body}")]
    RefreshRejected { status: u16, body: String },
    #[error("Claude Code token refresh returned HTTP {status}: {body}")]
    RefreshStatus { status: u16, body: String },
    #[error("could not parse Claude Code credentials at {path}: {source}")]
    ParseCredentials {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("could not read Claude Code credentials from the system keyring: {0}")]
    Keyring(keyring::Error),
    #[cfg(target_os = "macos")]
    #[error("could not run the macOS keychain reader: {0}")]
    KeychainCommand(std::io::Error),
    #[cfg(target_os = "macos")]
    #[error("the macOS keychain reader failed with {status}")]
    KeychainCommandStatus { status: std::process::ExitStatus },
    #[error("could not parse Claude Code credentials from the system keyring: {0}")]
    KeyringDecode(serde_json::Error),
    #[error("system keyring lookup task failed")]
    KeyringJoin,
    #[error("Claude Code usage request failed: {0}")]
    Http(reqwest::Error),
    #[error("Claude Code usage response could not be decoded: {0}")]
    Decode(serde_json::Error),
    #[error(
        "Claude rejected the usage request with HTTP {status}; run `claude` to re-authenticate: \
         {body}"
    )]
    Unauthorized { status: u16, body: String },
    #[error("Claude rate limited the usage endpoint; wait a few minutes and refresh: {body}")]
    RateLimited { body: String },
    #[error("Claude Code usage request returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },
    #[error("could not construct HTTP header {0}")]
    InvalidHeader(String),
}

impl From<ClaudeProviderError> for ProviderError {
    fn from(error: ClaudeProviderError) -> Self {
        match error {
            ClaudeProviderError::MissingCredentials { .. }
            | ClaudeProviderError::MissingAccessToken { .. }
            | ClaudeProviderError::ReadCredentials { .. }
            | ClaudeProviderError::ParseCredentials { .. }
            | ClaudeProviderError::KeyringDecode(_) => {
                ProviderError::NotConfigured(error.to_string())
            }
            ClaudeProviderError::ExpiredAccessToken { .. }
            | ClaudeProviderError::MissingRefreshToken { .. }
            | ClaudeProviderError::ExpiredRefreshToken { .. }
            | ClaudeProviderError::RefreshRejected { .. }
            | ClaudeProviderError::Unauthorized { .. } => {
                ProviderError::Authentication(error.to_string())
            }
            ClaudeProviderError::Decode(_) => ProviderError::Parse(error.to_string()),
            ClaudeProviderError::Keyring(_)
            | ClaudeProviderError::KeyringJoin
            | ClaudeProviderError::Http(_)
            | ClaudeProviderError::RateLimited { .. }
            | ClaudeProviderError::ApiStatus { .. }
            | ClaudeProviderError::RefreshStatus { .. }
            | ClaudeProviderError::WriteCredentials { .. }
            | ClaudeProviderError::LockCredentials { .. }
            | ClaudeProviderError::LockCredentialsTimeout { .. }
            | ClaudeProviderError::Encode(_)
            | ClaudeProviderError::InvalidHeader(_) => ProviderError::Network(error.to_string()),
            #[cfg(target_os = "macos")]
            ClaudeProviderError::KeychainCommand(_)
            | ClaudeProviderError::KeychainCommandStatus { .. } => {
                ProviderError::Network(error.to_string())
            }
        }
    }
}

fn plan_label(subscription_type: Option<&str>) -> String {
    match subscription_type {
        Some(subscription) => {
            let mut characters = subscription.chars();
            let titled = match characters.next() {
                Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
                None => return "Claude Code".to_owned(),
            };
            format!("Claude {titled}")
        }
        None => "Claude Code".to_owned(),
    }
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn parse_timestamp(value: Option<&str>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value?.trim(), &Rfc3339).ok()
}

fn bearer_value(token: &str) -> Result<HeaderValue, ClaudeProviderError> {
    HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| ClaudeProviderError::InvalidHeader(AUTHORIZATION.as_str().to_owned()))
}

fn now_unix_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1_000.0
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
    #[cfg(target_os = "macos")]
    use std::os::unix::process::ExitStatusExt;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config(usage_url: Url, credentials_path: PathBuf) -> ClaudeProviderConfig {
        ClaudeProviderConfig {
            oauth_token: None,
            token_url: Url::parse(CLAUDE_OAUTH_TOKEN_URL).expect("token URL"),
            usage_url,
            client_id: CLAUDE_OAUTH_CLIENT_ID.to_owned(),
            credentials_path: Some(credentials_path),
            config_dir: None,
            keyring_enabled: false,
            // Off by default in tests; the refresh tests opt in explicitly so
            // no other test can accidentally reach a token endpoint.
            refresh_enabled: false,
        }
    }

    fn write_credentials(dir: &tempfile::TempDir, expires_at: f64) -> PathBuf {
        let path = dir.path().join(CLAUDE_CREDENTIALS_FILE);
        fs::write(
            &path,
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "sk-ant-oat01-test",
                    "refreshToken": "sk-ant-ort01-test",
                    "expiresAt": expires_at,
                    "subscriptionType": "max",
                    "scopes": ["user:inference"],
                }
            })
            .to_string(),
        )
        .expect("write credentials");
        path
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_keychain_reader_decodes_security_output() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "access-token",
                    "refreshToken": "refresh-token",
                    "subscriptionType": "max"
                }
            })
            .to_string()
            .into_bytes(),
            stderr: Vec::new(),
        };

        let oauth = decode_macos_keychain_output(output)
            .expect("successful keychain read")
            .expect("OAuth credential");
        assert_eq!(oauth.access_token, "access-token");
        assert_eq!(oauth.refresh_token, "refresh-token");
        assert_eq!(oauth.subscription_type.as_deref(), Some("max"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_keychain_reader_maps_security_item_not_found() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(44 << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        assert!(
            decode_macos_keychain_output(output)
                .expect("missing item is not an error")
                .is_none()
        );
    }

    #[test]
    fn defaults_match_first_party_claude_code_endpoint() {
        let config = ClaudeProviderConfig::default();
        assert_eq!(
            config.usage_url.as_str(),
            "https://api.anthropic.com/api/oauth/usage"
        );
    }

    /// Live response shape captured from `GET /api/oauth/usage`.
    #[test]
    fn parses_first_party_usage_shape() {
        let response: ClaudeUsageResponse = serde_json::from_value(serde_json::json!({
            "five_hour": {
                "utilization": 3.0,
                "resets_at": "2026-07-25T06:29:59.669968+00:00",
                "limit_dollars": null,
            },
            "seven_day": {
                "utilization": 12.5,
                "resets_at": "2026-07-26T15:59:59.669992+00:00",
            },
            "seven_day_oauth_apps": null,
            "seven_day_opus": null,
            "seven_day_sonnet": null,
            "extra_usage": {
                "is_enabled": true,
                "monthly_limit": 50.0,
                "used_credits": 12.5,
                "currency": "USD",
            },
            "limits": [
                {"kind": "session", "group": "session", "percent": 3, "resets_at":
                 "2026-07-25T06:29:59.669968+00:00", "scope": null, "is_active": true},
                {"kind": "weekly_all", "group": "weekly", "percent": 0, "resets_at":
                 "2026-07-26T15:59:59.669992+00:00", "scope": null, "is_active": false},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 42,
                 "resets_at": "2026-07-26T15:59:59.669992+00:00",
                 "scope": {"model": {"id": null, "display_name": "Opus"}}, "is_active": true},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 0, "resets_at": null,
                 "scope": {"model": {"id": null, "display_name": "Fable"}}, "is_active": false},
            ],
            "member_dashboard_available": false,
        }))
        .expect("parse usage");

        let snapshot = response.usage_snapshot();
        // Account-wide windows come from the flat fields; the duplicate
        // `session`/`weekly_all` limit entries must not be added again.
        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(snapshot.windows[0].id, "five-hour");
        assert_eq!(snapshot.windows[0].used_percent, 3.0);
        assert_eq!(snapshot.windows[0].duration, Some(FIVE_HOURS));
        assert_eq!(
            snapshot.windows[0]
                .resets_at
                .expect("reset")
                .unix_timestamp(),
            1_784_960_999
        );
        assert_eq!(snapshot.windows[1].id, "seven-day");
        assert_eq!(snapshot.windows[1].used_percent, 12.5);
        // Scoped weekly window from `limits`; the inapplicable zero-usage
        // "Fable" promotional row is dropped.
        assert_eq!(snapshot.windows[2].id, "seven-day-opus");
        assert_eq!(snapshot.windows[2].label, "Weekly limit (Opus)");
        assert_eq!(snapshot.windows[2].used_percent, 42.0);
        assert_eq!(snapshot.windows[2].duration, Some(SEVEN_DAYS));

        assert_eq!(snapshot.balances.len(), 1);
        assert_eq!(snapshot.balances[0].remaining, 37.5);
        assert_eq!(snapshot.balances[0].unit, "USD");
    }

    #[test]
    fn falls_back_to_flat_scoped_weekly_fields() {
        let response: ClaudeUsageResponse = serde_json::from_value(serde_json::json!({
            "seven_day_opus": {"utilization": 20.0, "resets_at": null},
            "seven_day_sonnet": {"utilization": 5.0, "resets_at": null},
        }))
        .expect("parse usage");
        let snapshot = response.usage_snapshot();
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].id, "seven-day-opus");
        assert_eq!(snapshot.windows[1].id, "seven-day-sonnet");
    }

    #[test]
    fn prefers_scoped_limits_over_duplicate_flat_field() {
        let response: ClaudeUsageResponse = serde_json::from_value(serde_json::json!({
            "seven_day_opus": {"utilization": 20.0},
            "limits": [
                {"group": "weekly", "percent": 44,
                 "scope": {"model": {"display_name": "Opus"}}},
            ],
        }))
        .expect("parse usage");
        let snapshot = response.usage_snapshot();
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].used_percent, 44.0);
    }

    #[test]
    fn extra_usage_is_omitted_when_disabled() {
        let response: ClaudeUsageResponse = serde_json::from_value(serde_json::json!({
            "extra_usage": {"is_enabled": false, "monthly_limit": null, "used_credits": null},
        }))
        .expect("parse usage");
        assert!(response.usage_snapshot().balances.is_empty());
    }

    #[tokio::test]
    async fn provider_fetches_subscription_usage_with_oauth_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .and(header("authorization", "Bearer sk-ant-oat01-test"))
            .and(header("anthropic-beta", CLAUDE_OAUTH_BETA))
            .and(header("user-agent", "braindrain/0.1.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": {"utilization": 7.5, "resets_at": null},
            })))
            .mount(&server)
            .await;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = write_credentials(&tempdir, now_unix_millis() + 3_600_000.0);
        let usage_url =
            Url::parse(&format!("{}/api/oauth/usage", server.uri())).expect("usage URL");
        let provider = ClaudeProvider::new(test_config(usage_url, path));

        let snapshot = provider
            .refresh(RefreshContext {
                now: OffsetDateTime::from_unix_timestamp(1_784_419_200).expect("timestamp"),
            })
            .await
            .expect("refresh");

        assert_eq!(snapshot.provider, ProviderId::claude());
        assert_eq!(snapshot.source, ProviderSource::OAuth);
        assert_eq!(snapshot.usage.windows[0].used_percent, 7.5);
        assert_eq!(
            snapshot.identity.expect("identity").plan.as_deref(),
            Some("Claude Max")
        );
    }

    #[tokio::test]
    async fn expired_token_reports_authentication_error_without_a_request() {
        let server = MockServer::start().await;
        let tempdir = tempfile::tempdir().expect("tempdir");
        // Already expired: Claude Code refreshes on use, braindrain only reads.
        let path = write_credentials(&tempdir, now_unix_millis() - 1_000.0);
        let usage_url =
            Url::parse(&format!("{}/api/oauth/usage", server.uri())).expect("usage URL");
        let provider = ClaudeProvider::new(test_config(usage_url, path));

        let error = provider
            .refresh(RefreshContext::default())
            .await
            .expect_err("expired token");
        assert!(matches!(error, ProviderError::Authentication(_)));
        // No mocks were mounted, so any HTTP call would have failed the test.
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unauthorized_usage_response_maps_to_authentication_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(401).set_body_string("{\"error\":\"expired\"}"))
            .mount(&server)
            .await;
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = write_credentials(&tempdir, now_unix_millis() + 3_600_000.0);
        let usage_url =
            Url::parse(&format!("{}/api/oauth/usage", server.uri())).expect("usage URL");
        let provider = ClaudeProvider::new(test_config(usage_url, path));

        let error = provider
            .refresh(RefreshContext::default())
            .await
            .expect_err("unauthorized");
        assert!(matches!(error, ProviderError::Authentication(_)));
    }

    #[tokio::test]
    async fn missing_credentials_report_not_configured() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let provider = ClaudeProvider::new(test_config(
            Url::parse(CLAUDE_OAUTH_USAGE_URL).expect("usage URL"),
            tempdir.path().join(CLAUDE_CREDENTIALS_FILE),
        ));
        let error = provider
            .refresh(RefreshContext::default())
            .await
            .expect_err("missing credentials");
        assert!(matches!(error, ProviderError::NotConfigured(_)));
    }

    #[tokio::test]
    async fn expired_token_is_refreshed_and_rotation_written_back() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_string_contains("\"grant_type\":\"refresh_token\""))
            .and(body_string_contains(
                "\"refresh_token\":\"sk-ant-ort01-test\"",
            ))
            .and(body_string_contains(CLAUDE_OAUTH_CLIENT_ID))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "sk-ant-oat01-fresh",
                "refresh_token": "sk-ant-ort01-rotated",
                "expires_in": 28800,
                "refresh_token_expires_in": 2_592_000,
                "scope": "user:inference user:profile",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .and(header("authorization", "Bearer sk-ant-oat01-fresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": {"utilization": 11.0},
            })))
            .mount(&server)
            .await;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = write_credentials(&tempdir, now_unix_millis() - 1_000.0);
        let mut config = test_config(
            Url::parse(&format!("{}/api/oauth/usage", server.uri())).expect("usage URL"),
            path.clone(),
        );
        config.refresh_enabled = true;
        config.token_url =
            Url::parse(&format!("{}/v1/oauth/token", server.uri())).expect("token URL");

        let snapshot = ClaudeProvider::new(config)
            .refresh(RefreshContext::default())
            .await
            .expect("refresh after token exchange");
        assert_eq!(snapshot.usage.windows[0].used_percent, 11.0);

        let stored: Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse stored");
        let oauth = &stored["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], "sk-ant-oat01-fresh");
        assert_eq!(oauth["refreshToken"], "sk-ant-ort01-rotated");
        assert_eq!(
            oauth["scopes"],
            serde_json::json!(["user:inference", "user:profile"])
        );
        // Fields Claude Code owns but the exchange does not return must survive.
        assert_eq!(oauth["subscriptionType"], "max");
        // Timestamps stay integers, as the CLI writes them.
        assert!(oauth["expiresAt"].is_i64());
        assert!(oauth["expiresAt"].as_f64().expect("expiry") > now_unix_millis());
    }

    /// A response without `refresh_token` means "keep the existing one";
    /// Claude Code defaults it the same way.
    #[tokio::test]
    async fn refresh_without_rotation_keeps_existing_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "sk-ant-oat01-fresh",
                "expires_in": 28800,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = write_credentials(&tempdir, now_unix_millis() - 1_000.0);
        let mut config = test_config(
            Url::parse(&format!("{}/api/oauth/usage", server.uri())).expect("usage URL"),
            path.clone(),
        );
        config.refresh_enabled = true;
        config.token_url =
            Url::parse(&format!("{}/v1/oauth/token", server.uri())).expect("token URL");

        ClaudeProvider::new(config)
            .refresh(RefreshContext::default())
            .await
            .expect("refresh");

        let stored: Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse stored");
        assert_eq!(stored["claudeAiOauth"]["refreshToken"], "sk-ant-ort01-test");
    }

    #[tokio::test]
    async fn rejected_refresh_maps_to_authentication_error_and_leaves_file_intact() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string("{\"error\":\"invalid_grant\"}"),
            )
            .mount(&server)
            .await;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = write_credentials(&tempdir, now_unix_millis() - 1_000.0);
        let before = fs::read(&path).expect("read before");
        let mut config = test_config(
            Url::parse(&format!("{}/api/oauth/usage", server.uri())).expect("usage URL"),
            path.clone(),
        );
        config.refresh_enabled = true;
        config.token_url =
            Url::parse(&format!("{}/v1/oauth/token", server.uri())).expect("token URL");

        let error = ClaudeProvider::new(config)
            .refresh(RefreshContext::default())
            .await
            .expect_err("rejected refresh");
        assert!(matches!(error, ProviderError::Authentication(_)));
        // A failed exchange must never clobber the CLI's credentials.
        assert_eq!(fs::read(&path).expect("read after"), before);
    }

    #[tokio::test]
    async fn expired_refresh_token_is_not_exchanged() {
        let server = MockServer::start().await;
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join(CLAUDE_CREDENTIALS_FILE);
        let stale = now_unix_millis() - 1_000.0;
        fs::write(
            &path,
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "sk-ant-oat01-test",
                    "refreshToken": "sk-ant-ort01-test",
                    "expiresAt": stale,
                    "refreshTokenExpiresAt": stale,
                }
            })
            .to_string(),
        )
        .expect("write credentials");
        let mut config = test_config(
            Url::parse(&format!("{}/api/oauth/usage", server.uri())).expect("usage URL"),
            path,
        );
        config.refresh_enabled = true;
        config.token_url =
            Url::parse(&format!("{}/v1/oauth/token", server.uri())).expect("token URL");

        let error = ClaudeProvider::new(config)
            .refresh(RefreshContext::default())
            .await
            .expect_err("expired refresh token");
        assert!(matches!(error, ProviderError::Authentication(_)));
        // No mocks mounted: a token request would have surfaced here.
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
    }

    /// Rotation kills the old tokens, so a double exchange would strand one
    /// caller. The lock must collapse concurrent refreshes into one.
    #[tokio::test]
    async fn concurrent_refreshes_exchange_the_token_only_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "sk-ant-oat01-fresh",
                "refresh_token": "sk-ant-ort01-rotated",
                "expires_in": 28800,
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = write_credentials(&tempdir, now_unix_millis() - 1_000.0);
        let build = || {
            let mut config = test_config(
                Url::parse(&format!("{}/api/oauth/usage", server.uri())).expect("usage URL"),
                path.clone(),
            );
            config.refresh_enabled = true;
            config.token_url =
                Url::parse(&format!("{}/v1/oauth/token", server.uri())).expect("token URL");
            ClaudeProvider::new(config)
        };

        let (first_provider, second_provider) = (build(), build());
        let (first, second) = tokio::join!(
            first_provider.refresh(RefreshContext::default()),
            second_provider.refresh(RefreshContext::default())
        );
        first.expect("first refresh");
        second.expect("second refresh");
        // `expect(1)` is asserted when the server drops.
        drop(server);

        let stored: Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse stored");
        assert_eq!(
            stored["claudeAiOauth"]["refreshToken"],
            "sk-ant-ort01-rotated"
        );
    }

    #[test]
    fn apply_refresh_preserves_unknown_fields() {
        let mut raw: Value = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "old",
                "refreshToken": "old-refresh",
                "expiresAt": 1,
                "subscriptionType": "max",
                "rateLimitTier": "default_claude",
                "futureField": {"keep": true},
            },
            "otherTopLevel": "preserved",
        });
        let refreshed = ClaudeTokenRefreshResponse {
            access_token: "new".to_owned(),
            refresh_token: None,
            expires_in: Some(60.0),
            refresh_token_expires_in: None,
            scope: None,
        };
        apply_refresh(&mut raw, &refreshed, "old-refresh", 1_000.0);

        let oauth = &raw["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], "new");
        assert_eq!(oauth["refreshToken"], "old-refresh");
        assert_eq!(oauth["expiresAt"], 61_000);
        assert_eq!(oauth["rateLimitTier"], "default_claude");
        assert_eq!(oauth["futureField"]["keep"], true);
        assert_eq!(raw["otherTopLevel"], "preserved");
    }

    #[test]
    fn plan_label_titlecases_subscription_type() {
        assert_eq!(plan_label(Some("max")), "Claude Max");
        assert_eq!(plan_label(Some("pro")), "Claude Pro");
        assert_eq!(plan_label(None), "Claude Code");
    }

    #[test]
    fn body_preview_is_safe_at_multibyte_boundary() {
        let body = "界".repeat(300);
        let preview = body_preview(body.as_bytes());
        assert!(preview.ends_with("..."));
        assert!(preview.len() <= 518);
    }
}
