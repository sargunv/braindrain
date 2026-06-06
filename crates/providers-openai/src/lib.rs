use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use braindrain_core::{
    AccountIdentity, BalanceSnapshot, Provider, ProviderError, ProviderFuture, ProviderId,
    ProviderSnapshot, ProviderSource, RateWindow, RefreshContext, UsageSnapshot,
};
use jsonwebtoken::dangerous::insecure_decode;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

pub const CHATGPT_WHAM_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

const CHATGPT_ACCOUNT_ID_HEADER: &str = "ChatGPT-Account-Id";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_OAUTH_SCOPE: &str = "openid profile email";
const TOKEN_REFRESH_MAX_AGE: Duration = Duration::from_secs(8 * 24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    pub fn config(&self) -> &OpenAiProviderConfig {
        &self.config
    }

    pub fn auth_path(&self) -> PathBuf {
        self.config.auth_path()
    }

    pub fn load_auth(&self) -> Result<CodexAuth, OpenAiProviderError> {
        CodexAuth::load_from_path(self.auth_path())
    }

    pub async fn refresh_auth(&self, auth: &mut CodexAuth) -> Result<bool, OpenAiProviderError> {
        if !auth.needs_refresh(OffsetDateTime::now_utc()) {
            return Ok(false);
        }
        if auth.tokens.refresh_token.is_empty() {
            return Ok(false);
        }

        self.refresh_tokens(auth).await?;
        auth.save_to_path(self.auth_path())?;
        Ok(true)
    }

    async fn refresh_tokens(&self, auth: &mut CodexAuth) -> Result<(), OpenAiProviderError> {
        let body = TokenRefreshRequest {
            client_id: CODEX_OAUTH_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: &auth.tokens.refresh_token,
            scope: CODEX_OAUTH_SCOPE,
        };
        let response = self
            .client
            .post(self.config.refresh_url.clone())
            .header(USER_AGENT, "braindrain")
            .header(ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(OpenAiProviderError::Http)?;

        let status = response.status();
        let body = response.bytes().await.map_err(OpenAiProviderError::Http)?;
        if !status.is_success() {
            return Err(OpenAiProviderError::TokenRefreshFailed {
                status: status.as_u16(),
                kind: refresh_error_kind(status.as_u16(), &body),
                body: body_preview(&body),
            });
        }

        let refreshed: TokenRefreshResponse =
            serde_json::from_slice(&body).map_err(OpenAiProviderError::Decode)?;
        if let Some(access_token) = refreshed.access_token {
            auth.tokens.access_token = access_token;
        }
        if let Some(refresh_token) = refreshed.refresh_token {
            auth.tokens.refresh_token = refresh_token;
        }
        if let Some(id_token) = refreshed.id_token {
            auth.tokens.id_token = id_token;
        }
        auth.last_refresh = Some(OffsetDateTime::now_utc());
        Ok(())
    }

    async fn fetch_usage(
        &self,
        auth: &CodexAuth,
    ) -> Result<CodexUsageResponse, OpenAiProviderError> {
        if auth.tokens.access_token.is_empty() {
            return Err(OpenAiProviderError::MissingAccessToken);
        }

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("braindrain"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", auth.tokens.access_token)).map_err(
                |_| OpenAiProviderError::InvalidHeader(AUTHORIZATION.as_str().to_owned()),
            )?,
        );
        if let Some(account_id) = auth.tokens.account_id.as_deref() {
            headers.insert(
                CHATGPT_ACCOUNT_ID_HEADER,
                HeaderValue::from_str(account_id).map_err(|_| {
                    OpenAiProviderError::InvalidHeader(CHATGPT_ACCOUNT_ID_HEADER.to_owned())
                })?,
            );
        }

        let response = self
            .client
            .get(self.config.usage_url.clone())
            .headers(headers)
            .send()
            .await
            .map_err(OpenAiProviderError::Http)?;
        let status = response.status();
        let body = response.bytes().await.map_err(OpenAiProviderError::Http)?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(OpenAiProviderError::Unauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        if !status.is_success() {
            return Err(OpenAiProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        serde_json::from_slice(&body).map_err(OpenAiProviderError::Decode)
    }
}

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new(OpenAiProviderConfig::default())
    }
}

impl Provider for OpenAiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::openai()
    }

    fn refresh<'a>(
        &'a self,
        context: RefreshContext,
    ) -> ProviderFuture<'a, Result<ProviderSnapshot, ProviderError>> {
        Box::pin(async move {
            let mut auth = self.load_auth().map_err(ProviderError::from)?;
            self.refresh_auth(&mut auth)
                .await
                .map_err(ProviderError::from)?;
            let usage_response = self.fetch_usage(&auth).await.map_err(ProviderError::from)?;
            let identity = usage_response.identity(&auth);
            let usage = usage_response.usage_snapshot();
            Ok(ProviderSnapshot {
                provider: ProviderId::openai(),
                source: ProviderSource::OAuth,
                usage,
                identity,
                updated_at: context.now,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiProviderConfig {
    pub codex_home: Option<PathBuf>,
    pub env_codex_home: Option<PathBuf>,
    pub usage_url: Url,
    pub refresh_url: Url,
}

impl OpenAiProviderConfig {
    pub fn auth_path(&self) -> PathBuf {
        let home = self
            .codex_home
            .clone()
            .or_else(|| self.env_codex_home.clone())
            .or_else(env_codex_home)
            .unwrap_or_else(default_codex_home);
        home.join("auth.json")
    }
}

impl Default for OpenAiProviderConfig {
    fn default() -> Self {
        Self {
            codex_home: None,
            env_codex_home: None,
            usage_url: Url::parse(CHATGPT_WHAM_USAGE_URL).expect("valid ChatGPT usage URL"),
            refresh_url: Url::parse(OPENAI_OAUTH_TOKEN_URL).expect("valid OpenAI OAuth token URL"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexAuth {
    #[serde(default)]
    pub tokens: CodexAuthTokens,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_refresh: Option<OffsetDateTime>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CodexAuth {
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, OpenAiProviderError> {
        let path = path.as_ref();
        let data = fs::read(path).map_err(|source| OpenAiProviderError::ReadAuth {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_slice(&data).map_err(|source| OpenAiProviderError::ParseAuth {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), OpenAiProviderError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| OpenAiProviderError::WriteAuth {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let data = serde_json::to_vec_pretty(self).map_err(OpenAiProviderError::Encode)?;
        fs::write(path, data).map_err(|source| OpenAiProviderError::WriteAuth {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn needs_refresh(&self, now: OffsetDateTime) -> bool {
        let Some(last_refresh) = self.last_refresh else {
            return true;
        };
        (now - last_refresh) >= TOKEN_REFRESH_MAX_AGE
    }

    pub fn identity(&self) -> Option<AccountIdentity> {
        let claims = self.tokens.id_token_claims().ok();
        let email = claims
            .as_ref()
            .and_then(|claims| claims.string("email"))
            .or_else(|| {
                claims
                    .as_ref()
                    .and_then(|claims| claims.string("https://api.openai.com/profile/email"))
            })
            .or_else(|| {
                claims.as_ref().and_then(|claims| {
                    claims
                        .object("https://api.openai.com/profile")?
                        .string("email")
                })
            });
        let plan = claims
            .as_ref()
            .and_then(|claims| claims.string("chatgpt_plan_type"))
            .or_else(|| {
                claims.as_ref().and_then(|claims| {
                    claims
                        .object("https://api.openai.com/auth")?
                        .string("chatgpt_plan_type")
                })
            })
            .or_else(|| {
                claims.as_ref().and_then(|claims| {
                    claims.string("https://api.openai.com/auth.chatgpt_plan_type")
                })
            });

        if email.is_none() && plan.is_none() {
            return None;
        }
        Some(AccountIdentity { email, plan })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodexAuthTokens {
    #[serde(default, alias = "accessToken")]
    pub access_token: String,
    #[serde(default, alias = "refreshToken")]
    pub refresh_token: String,
    #[serde(default, alias = "idToken")]
    pub id_token: String,
    #[serde(default, alias = "accountId")]
    pub account_id: Option<String>,
    #[serde(default, alias = "expiresAt", with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CodexAuthTokens {
    pub fn id_token_claims(&self) -> Result<JwtClaims, OpenAiProviderError> {
        JwtClaims::decode_unverified(&self.id_token)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JwtClaims {
    claims: serde_json::Map<String, serde_json::Value>,
}

impl JwtClaims {
    pub fn decode_unverified(token: &str) -> Result<Self, OpenAiProviderError> {
        let data = insecure_decode::<serde_json::Value>(token)
            .map_err(|_| OpenAiProviderError::InvalidJwt)?;
        let serde_json::Value::Object(claims) = data.claims else {
            return Err(OpenAiProviderError::InvalidJwt);
        };
        Ok(Self { claims })
    }

    pub fn string(&self, key: &str) -> Option<String> {
        self.claims.get(key)?.as_str().map(ToOwned::to_owned)
    }

    pub fn object(&self, key: &str) -> Option<JwtObject<'_>> {
        let claims = self.claims.get(key)?.as_object()?;
        Some(JwtObject { claims })
    }
}

pub struct JwtObject<'a> {
    claims: &'a Map<String, Value>,
}

impl JwtObject<'_> {
    pub fn string(&self, key: &str) -> Option<String> {
        self.claims.get(key)?.as_str().map(ToOwned::to_owned)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexUsageResponse {
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<CodexRateLimit>,
    #[serde(default)]
    pub credits: Option<CodexCredits>,
    #[serde(default)]
    pub additional_rate_limits: Vec<CodexAdditionalRateLimit>,
}

impl CodexUsageResponse {
    pub fn identity(&self, auth: &CodexAuth) -> Option<AccountIdentity> {
        let mut identity = auth.identity().unwrap_or(AccountIdentity {
            email: None,
            plan: None,
        });
        if let Some(plan_type) = self.plan_type.clone() {
            identity.plan = Some(plan_type);
        }
        if identity.email.is_none() && identity.plan.is_none() {
            None
        } else {
            Some(identity)
        }
    }

    pub fn usage_snapshot(&self) -> UsageSnapshot {
        let mut windows = Vec::new();
        if let Some(rate_limit) = &self.rate_limit {
            windows.extend(standard_windows(rate_limit));
        }
        for additional in &self.additional_rate_limits {
            windows.extend(additional.windows());
        }

        let mut balances = Vec::new();
        if let Some(balance) = self
            .credits
            .as_ref()
            .and_then(CodexCredits::balance_snapshot)
        {
            balances.push(balance);
        }

        UsageSnapshot { windows, balances }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexRateLimit {
    #[serde(default)]
    pub primary_window: Option<CodexRateWindow>,
    #[serde(default)]
    pub secondary_window: Option<CodexRateWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexRateWindow {
    pub used_percent: f64,
    #[serde(default, with = "unix_timestamp_option")]
    pub reset_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub limit_window_seconds: Option<u64>,
}

impl CodexRateWindow {
    fn duration(&self) -> Option<Duration> {
        self.limit_window_seconds.map(Duration::from_secs)
    }

    fn role(&self) -> WindowRole {
        match self.limit_window_seconds {
            Some(18_000) => WindowRole::Session,
            Some(604_800) => WindowRole::Weekly,
            _ => WindowRole::Unknown,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexCredits {
    #[serde(default)]
    pub has_credits: bool,
    #[serde(default)]
    pub unlimited: bool,
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    pub balance: Option<f64>,
}

impl CodexCredits {
    fn balance_snapshot(&self) -> Option<BalanceSnapshot> {
        if self.unlimited || !self.has_credits {
            return None;
        }
        Some(BalanceSnapshot {
            id: "credits".to_owned(),
            label: "Credits".to_owned(),
            remaining: self.balance?,
            unit: "credits".to_owned(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexAdditionalRateLimit {
    #[serde(default)]
    pub limit_name: Option<String>,
    #[serde(default)]
    pub metered_feature: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<CodexRateLimit>,
}

impl CodexAdditionalRateLimit {
    fn windows(&self) -> Vec<RateWindow> {
        let Some(rate_limit) = &self.rate_limit else {
            return Vec::new();
        };
        if self.is_spark_limit() {
            return spark_windows(rate_limit);
        }

        let title = self
            .limit_name
            .as_deref()
            .or(self.metered_feature.as_deref())
            .unwrap_or("Additional limit");
        let id = format!("codex-{}", slug(title));
        additional_windows(rate_limit, &id, title)
    }

    fn is_spark_limit(&self) -> bool {
        self.limit_name
            .as_deref()
            .or(self.metered_feature.as_deref())
            .map(|value| value.to_ascii_lowercase().contains("spark"))
            .unwrap_or(false)
    }
}

#[derive(Debug, Error)]
pub enum OpenAiProviderError {
    #[error("could not read Codex auth file at {path}: {source}")]
    ReadAuth {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse Codex auth file at {path}: {source}")]
    ParseAuth {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("could not write Codex auth file at {path}: {source}")]
    WriteAuth {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not serialize Codex auth file: {0}")]
    Encode(serde_json::Error),
    #[error("Codex auth file does not contain an access token")]
    MissingAccessToken,
    #[error("OpenAI request failed: {0}")]
    Http(reqwest::Error),
    #[error("OpenAI response could not be decoded: {0}")]
    Decode(serde_json::Error),
    #[error("OpenAI rejected usage request with HTTP {status}: {body}")]
    Unauthorized { status: u16, body: String },
    #[error("OpenAI usage request returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },
    #[error("OpenAI OAuth refresh failed with HTTP {status}: {kind}: {body}")]
    TokenRefreshFailed {
        status: u16,
        kind: RefreshFailureKind,
        body: String,
    },
    #[error("could not construct HTTP header {0}")]
    InvalidHeader(String),
    #[error("id token is not a JWT payload")]
    InvalidJwt,
}

impl From<OpenAiProviderError> for ProviderError {
    fn from(error: OpenAiProviderError) -> Self {
        match error {
            OpenAiProviderError::ReadAuth { .. }
            | OpenAiProviderError::ParseAuth { .. }
            | OpenAiProviderError::MissingAccessToken => {
                ProviderError::NotConfigured(error.to_string())
            }
            OpenAiProviderError::InvalidJwt
            | OpenAiProviderError::Decode(_)
            | OpenAiProviderError::Encode(_) => ProviderError::Parse(error.to_string()),
            OpenAiProviderError::Http(_) | OpenAiProviderError::ApiStatus { .. } => {
                ProviderError::Network(error.to_string())
            }
            OpenAiProviderError::Unauthorized { .. }
            | OpenAiProviderError::TokenRefreshFailed {
                kind:
                    RefreshFailureKind::Expired
                    | RefreshFailureKind::Invalidated
                    | RefreshFailureKind::Reused,
                ..
            } => ProviderError::Authentication(error.to_string()),
            OpenAiProviderError::WriteAuth { .. }
            | OpenAiProviderError::InvalidHeader(_)
            | OpenAiProviderError::TokenRefreshFailed { .. } => {
                ProviderError::Network(error.to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailureKind {
    Expired,
    Invalidated,
    Reused,
    Unknown,
}

impl std::fmt::Display for RefreshFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => f.write_str("expired refresh token"),
            Self::Invalidated => f.write_str("invalidated refresh token"),
            Self::Reused => f.write_str("reused refresh token"),
            Self::Unknown => f.write_str("unknown refresh error"),
        }
    }
}

#[derive(Debug, Serialize)]
struct TokenRefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
    scope: &'a str,
}

#[derive(Debug, Deserialize)]
struct TokenRefreshResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowRole {
    Session,
    Weekly,
    Unknown,
}

fn standard_windows(rate_limit: &CodexRateLimit) -> Vec<RateWindow> {
    let mut windows = Vec::new();
    let primary = rate_limit.primary_window.as_ref();
    let secondary = rate_limit.secondary_window.as_ref();

    match (primary, secondary) {
        (Some(primary), Some(secondary))
            if primary.role() == WindowRole::Weekly && secondary.role() == WindowRole::Session =>
        {
            windows.push(rate_window("5h", "5-hour", secondary));
            windows.push(rate_window("weekly", "Weekly", primary));
        }
        (Some(primary), Some(secondary)) => {
            windows.push(described_standard_window(primary, "primary", "Primary"));
            windows.push(described_standard_window(
                secondary,
                "secondary",
                "Secondary",
            ));
        }
        (Some(primary), None) => {
            windows.push(described_standard_window(primary, "primary", "Primary"));
        }
        (None, Some(secondary)) => {
            windows.push(described_standard_window(
                secondary,
                "secondary",
                "Secondary",
            ));
        }
        (None, None) => {}
    }

    windows
}

fn described_standard_window(
    window: &CodexRateWindow,
    fallback_id: &str,
    fallback_label: &str,
) -> RateWindow {
    match window.role() {
        WindowRole::Session => rate_window("5h", "5-hour", window),
        WindowRole::Weekly => rate_window("weekly", "Weekly", window),
        WindowRole::Unknown => rate_window(fallback_id, fallback_label, window),
    }
}

fn spark_windows(rate_limit: &CodexRateLimit) -> Vec<RateWindow> {
    let mut windows = Vec::new();

    let primary = rate_limit.primary_window.as_ref();
    let secondary = rate_limit.secondary_window.as_ref();
    match (primary, secondary) {
        (Some(primary), Some(secondary))
            if primary.role() == WindowRole::Weekly && secondary.role() == WindowRole::Session =>
        {
            windows.push(described_spark_window(
                secondary,
                "codex-spark",
                "Codex Spark",
            ));
            windows.push(described_spark_window(
                primary,
                "codex-spark-weekly",
                "Codex Spark Secondary",
            ));
        }
        (Some(primary), Some(secondary)) => {
            windows.push(described_spark_window(
                primary,
                "codex-spark",
                "Codex Spark",
            ));
            windows.push(described_spark_window(
                secondary,
                "codex-spark-weekly",
                "Codex Spark Secondary",
            ));
        }
        (Some(primary), None) => {
            windows.push(described_spark_window(
                primary,
                "codex-spark",
                "Codex Spark",
            ));
        }
        (None, Some(secondary)) => {
            windows.push(described_spark_window(
                secondary,
                "codex-spark-weekly",
                "Codex Spark Secondary",
            ));
        }
        (None, None) => {}
    }

    windows
}

fn described_spark_window(
    window: &CodexRateWindow,
    fallback_id: &str,
    fallback_label: &str,
) -> RateWindow {
    match window.role() {
        WindowRole::Session => rate_window("codex-spark", "Codex Spark 5-hour", window),
        WindowRole::Weekly => rate_window("codex-spark-weekly", "Codex Spark Weekly", window),
        WindowRole::Unknown => rate_window(fallback_id, fallback_label, window),
    }
}

fn additional_windows(rate_limit: &CodexRateLimit, id: &str, title: &str) -> Vec<RateWindow> {
    let mut windows = Vec::new();
    if let Some(primary) = &rate_limit.primary_window {
        windows.push(rate_window(id, title, primary));
    }
    if let Some(secondary) = &rate_limit.secondary_window {
        windows.push(rate_window(
            &format!("{id}-secondary"),
            &format!("{title} Secondary"),
            secondary,
        ));
    }
    windows
}

fn rate_window(id: &str, label: &str, window: &CodexRateWindow) -> RateWindow {
    RateWindow {
        id: id.to_owned(),
        label: label.to_owned(),
        used_percent: window.used_percent,
        duration: window.duration(),
        resets_at: window.reset_at,
    }
}

fn slug(value: &str) -> String {
    let mut slug = String::new();
    let mut needs_dash = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if needs_dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character);
            needs_dash = false;
        } else {
            needs_dash = true;
        }
    }
    if slug.is_empty() {
        "additional-limit".to_owned()
    } else {
        slug
    }
}

fn refresh_error_kind(status: u16, body: &[u8]) -> RefreshFailureKind {
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(body) else {
        return if status == 401 {
            RefreshFailureKind::Expired
        } else {
            RefreshFailureKind::Unknown
        };
    };
    let error = body
        .get("error")
        .or_else(|| body.get("error_code"))
        .or_else(|| body.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    match error {
        "refresh_token_expired" => RefreshFailureKind::Expired,
        "refresh_token_reused" => RefreshFailureKind::Reused,
        "invalid_grant" | "refresh_token_invalidated" => RefreshFailureKind::Invalidated,
        _ if status == 401 => RefreshFailureKind::Expired,
        _ => RefreshFailureKind::Unknown,
    }
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

fn default_codex_home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn env_codex_home() -> Option<PathBuf> {
    env::var_os("CODEX_HOME").map(PathBuf::from)
}

fn deserialize_optional_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("number cannot be represented as f64"))
            .map(Some),
        Some(Value::String(string)) => string
            .parse::<f64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected number or string, got {other}"
        ))),
    }
}

mod unix_timestamp_option {
    use serde::{Deserialize, Deserializer};
    use time::OffsetDateTime;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Some(timestamp) = Option::<i64>::deserialize(deserializer)? else {
            return Ok(None);
        };
        OffsetDateTime::from_unix_timestamp(timestamp)
            .map(Some)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn config_auth_path_precedence_is_explicit_then_env_then_default() {
        let explicit = OpenAiProviderConfig {
            codex_home: Some(PathBuf::from("/tmp/explicit-codex-home")),
            env_codex_home: Some(PathBuf::from("/tmp/env-codex-home")),
            ..OpenAiProviderConfig::default()
        };
        assert_eq!(
            explicit.auth_path(),
            PathBuf::from("/tmp/explicit-codex-home/auth.json")
        );

        let env = OpenAiProviderConfig {
            env_codex_home: Some(PathBuf::from("/tmp/env-codex-home")),
            ..OpenAiProviderConfig::default()
        };
        assert_eq!(
            env.auth_path(),
            PathBuf::from("/tmp/env-codex-home/auth.json")
        );

        let fallback = OpenAiProviderConfig::default();
        assert!(fallback.auth_path().ends_with(".codex/auth.json"));
    }

    #[test]
    fn auth_extracts_identity_from_id_token() {
        let auth = CodexAuth {
            tokens: CodexAuthTokens {
                id_token: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                    eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9wbGFuX3R5cGUiOiJwbHVzIn19.\
                    invalid-signature"
                    .to_owned(),
                ..CodexAuthTokens::default()
            },
            last_refresh: None,
            extra: Map::new(),
        };

        let identity = auth.identity().expect("identity");
        assert_eq!(identity.email.as_deref(), Some("user@example.com"));
        assert_eq!(identity.plan.as_deref(), Some("plus"));
    }

    #[test]
    fn auth_parses_codex_snake_case_tokens_and_preserves_extra_fields() {
        let auth: CodexAuth = serde_json::from_value(serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "access",
                "refresh_token": "refresh",
                "id_token": "id",
                "account_id": "account",
                "unknown_token_field": true
            },
            "last_refresh": "2026-06-01T00:00:00Z"
        }))
        .expect("parse auth");

        assert_eq!(auth.tokens.access_token, "access");
        assert_eq!(auth.tokens.refresh_token, "refresh");
        assert_eq!(auth.tokens.id_token, "id");
        assert_eq!(auth.tokens.account_id.as_deref(), Some("account"));
        assert_eq!(auth.extra["auth_mode"], "chatgpt");
        assert_eq!(auth.tokens.extra["unknown_token_field"], true);
        assert!(
            !auth.needs_refresh(
                OffsetDateTime::from_unix_timestamp(1_780_358_400).expect("timestamp")
            )
        );
    }

    #[test]
    fn usage_response_maps_standard_windows_credits_and_spark_limits() {
        let usage: CodexUsageResponse = serde_json::from_value(serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 42,
                    "reset_at": 1_700_000_000,
                    "limit_window_seconds": 18_000
                },
                "secondary_window": {
                    "used_percent": 84,
                    "reset_at": 1_700_604_800,
                    "limit_window_seconds": 604_800
                }
            },
            "credits": {
                "has_credits": true,
                "unlimited": false,
                "balance": "12.5"
            },
            "additional_rate_limits": [
                {
                    "limit_name": "Codex Spark",
                    "rate_limit": {
                        "primary_window": {
                            "used_percent": 1,
                            "limit_window_seconds": 18_000
                        },
                        "secondary_window": {
                            "used_percent": 2,
                            "limit_window_seconds": 604_800
                        }
                    }
                }
            ]
        }))
        .expect("parse usage");

        let snapshot = usage.usage_snapshot();
        let ids: Vec<_> = snapshot
            .windows
            .iter()
            .map(|window| window.id.as_str())
            .collect();
        assert_eq!(ids, ["5h", "weekly", "codex-spark", "codex-spark-weekly"]);
        assert_eq!(
            snapshot.windows[0].duration,
            Some(Duration::from_secs(18_000))
        );
        assert_eq!(
            snapshot.windows[1].duration,
            Some(Duration::from_secs(604_800))
        );
        assert_eq!(snapshot.balances.len(), 1);
        assert_eq!(snapshot.balances[0].remaining, 12.5);
    }

    #[tokio::test]
    async fn refresh_uses_codex_auth_to_fetch_usage() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        fs::write(
            tempdir.path().join("auth.json"),
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "access_token": "access",
                    "refresh_token": "refresh",
                    "id_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                        eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20ifQ.\
                        invalid-signature",
                    "account_id": "account"
                },
                "last_refresh": "2026-06-01T00:00:00Z"
            }))
            .expect("auth json"),
        )
        .expect("write auth");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usage"))
            .and(header("authorization", "Bearer access"))
            .and(header(CHATGPT_ACCOUNT_ID_HEADER, "account"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 25,
                        "limit_window_seconds": 18_000
                    }
                }
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            codex_home: Some(tempdir.path().to_path_buf()),
            usage_url: Url::parse(&format!("{}/usage", server.uri())).expect("usage url"),
            refresh_url: Url::parse(&format!("{}/token", server.uri())).expect("refresh url"),
            ..OpenAiProviderConfig::default()
        });

        let snapshot = provider
            .refresh(RefreshContext {
                now: OffsetDateTime::from_unix_timestamp(1_780_358_400).expect("timestamp"),
            })
            .await
            .expect("refresh");

        assert_eq!(
            snapshot.identity.and_then(|identity| identity.plan),
            Some("plus".to_owned())
        );
        assert_eq!(snapshot.usage.windows.len(), 1);
        assert_eq!(snapshot.usage.windows[0].id, "5h");
        assert_eq!(snapshot.usage.windows[0].used_percent, 25.0);
    }
}
