use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use braindrain_core::{
    AccountIdentity, Provider, ProviderError, ProviderFuture, ProviderId, ProviderSnapshot,
    ProviderSource, RateWindow, RefreshContext, UsageSnapshot,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

pub const ZAI_API_BASE_URL: &str = "https://api.z.ai";
pub const ZAI_BIGMODEL_BASE_URL: &str = "https://open.bigmodel.cn";
pub const ZAI_QUOTA_API_PATH: &str = "api/monitor/usage/quota/limit";
pub const ZAI_API_KEY_ENV: &str = "Z_AI_API_KEY";
pub const ZAI_QUOTA_URL_ENV: &str = "Z_AI_QUOTA_URL";
pub const ZAI_API_HOST_ENV: &str = "Z_AI_API_HOST";

pub const OPENCODE_AUTH_FILENAME: &str = "opencode/auth.json";
/// OpenCode provider ids that hold a z.ai API key, checked in priority order.
pub const OPENCODE_ZAI_PROVIDER_IDS: &[&str] = &["zai-coding-plan", "zai", "z.ai"];

#[derive(Debug, Clone)]
pub struct ZaiProvider {
    config: ZaiProviderConfig,
    client: reqwest::Client,
}

impl ZaiProvider {
    pub fn new(config: ZaiProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    pub fn config(&self) -> &ZaiProviderConfig {
        &self.config
    }

    pub fn api_key(&self) -> Result<ZaiApiKey, ZaiProviderError> {
        self.config.api_key()
    }

    pub fn quota_url(&self) -> Url {
        self.config.resolve_quota_url()
    }

    async fn fetch_quota(&self) -> Result<ZaiQuotaResponse, ZaiProviderError> {
        let key = self.config.api_key()?;
        let url = self.config.resolve_quota_url();

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", key.value))
                .map_err(|_| ZaiProviderError::InvalidHeader(AUTHORIZATION.as_str().to_owned()))?,
        );

        let response = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(ZaiProviderError::Http)?;
        let status = response.status();
        let body = response.bytes().await.map_err(ZaiProviderError::Http)?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ZaiProviderError::Unauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        if !status.is_success() {
            return Err(ZaiProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        if body.is_empty() {
            return Err(ZaiProviderError::EmptyBody);
        }

        let response: ZaiQuotaResponse =
            serde_json::from_slice(&body).map_err(ZaiProviderError::Decode)?;
        if !response.is_success() {
            return Err(ZaiProviderError::ApiFailed {
                code: response.code,
                msg: response.msg,
            });
        }
        Ok(response)
    }
}

impl Default for ZaiProvider {
    fn default() -> Self {
        Self::new(ZaiProviderConfig::default())
    }
}

impl Provider for ZaiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::zai()
    }

    fn refresh<'a>(
        &'a self,
        context: RefreshContext,
    ) -> ProviderFuture<'a, Result<ProviderSnapshot, ProviderError>> {
        Box::pin(async move {
            let quota = self.fetch_quota().await.map_err(ProviderError::from)?;
            Ok(ProviderSnapshot {
                provider: ProviderId::zai(),
                source: ProviderSource::Manual,
                usage: quota.usage_snapshot(),
                identity: quota.identity(),
                updated_at: context.now,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZaiApiKey {
    pub value: String,
    pub source: ZaiApiKeySource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZaiApiKeySource {
    Environment(&'static str),
    Opencode,
    Config,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZaiProviderConfig {
    pub api_key: Option<String>,
    pub api_base_url: Url,
    pub quota_url: Option<Url>,
    pub opencode_auth_path: Option<PathBuf>,
}

impl ZaiProviderConfig {
    pub fn api_key(&self) -> Result<ZaiApiKey, ZaiProviderError> {
        if let Some(key) = self.api_key.as_deref().filter(|key| !key.is_empty()) {
            return Ok(ZaiApiKey {
                value: key.to_owned(),
                source: ZaiApiKeySource::Config,
            });
        }

        let opencode_path = self
            .opencode_auth_path
            .clone()
            .or_else(opencode_auth_path_discovered);
        if let Some(key) = opencode_path.as_deref().and_then(opencode_api_key_in) {
            return Ok(ZaiApiKey {
                value: key,
                source: ZaiApiKeySource::Opencode,
            });
        }

        if let Some(key) = env::var_os(ZAI_API_KEY_ENV)
            .and_then(|key| key.into_string().ok())
            .filter(|key| !key.is_empty())
        {
            return Ok(ZaiApiKey {
                value: key,
                source: ZaiApiKeySource::Environment(ZAI_API_KEY_ENV),
            });
        }

        Err(ZaiProviderError::MissingApiKey)
    }

    pub fn opencode_auth_path(&self) -> Option<PathBuf> {
        self.opencode_auth_path
            .clone()
            .or_else(opencode_auth_path_discovered)
    }

    pub fn resolve_quota_url(&self) -> Url {
        if let Some(url) = self.quota_url.clone() {
            return url;
        }

        if let Some(url) = env_quota_url() {
            return url;
        }

        join_quota_path(self.api_base_url.as_str()).unwrap_or_else(|| {
            Url::parse(ZAI_API_BASE_URL)
                .expect("default z.ai base URL is valid")
                .join(ZAI_QUOTA_API_PATH)
                .expect("quota path is valid")
        })
    }

    pub fn bigmodel_cn() -> Self {
        Self {
            api_base_url: Url::parse(ZAI_BIGMODEL_BASE_URL).expect("valid BigModel base URL"),
            ..Self::default()
        }
    }
}

impl Default for ZaiProviderConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            api_base_url: Url::parse(ZAI_API_BASE_URL).expect("valid z.ai base URL"),
            quota_url: None,
            opencode_auth_path: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZaiLimitType {
    Tokens,
    Time,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZaiLimitUnit {
    Unknown,
    Minutes,
    Hours,
    Days,
    Weeks,
}

impl ZaiLimitUnit {
    fn from_code(code: i64) -> Self {
        match code {
            5 => Self::Minutes,
            3 => Self::Hours,
            1 => Self::Days,
            6 => Self::Weeks,
            _ => Self::Unknown,
        }
    }

    fn window_description(&self, number: i64) -> Option<String> {
        if number <= 0 {
            return None;
        }
        let unit = match self {
            Self::Minutes => "minute",
            Self::Hours => "hour",
            Self::Days => "day",
            Self::Weeks => "week",
            Self::Unknown => return None,
        };
        let plural = if number == 1 {
            unit.to_owned()
        } else {
            format!("{unit}s")
        };
        Some(format!("{number} {plural}"))
    }

    fn window_duration(&self, number: i64) -> Option<Duration> {
        if number <= 0 {
            return None;
        }
        let seconds = match self {
            Self::Minutes => number * 60,
            Self::Hours => number * 3_600,
            Self::Days => number * 86_400,
            Self::Weeks => number * 604_800,
            Self::Unknown => return None,
        };
        u64::try_from(seconds).ok().map(Duration::from_secs)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ZaiLimitEntry {
    limit_type: ZaiLimitType,
    unit: ZaiLimitUnit,
    number: i64,
    usage: Option<i64>,
    current_value: Option<i64>,
    remaining: Option<i64>,
    percentage: f64,
    next_reset_time: Option<OffsetDateTime>,
}

impl ZaiLimitEntry {
    fn is_monthly_marker(&self) -> bool {
        self.limit_type == ZaiLimitType::Time
            && self.unit == ZaiLimitUnit::Minutes
            && self.number == 1
    }

    fn used_percent(&self) -> f64 {
        if let Some(computed) = self.computed_used_percent() {
            return computed;
        }
        self.percentage
    }

    fn computed_used_percent(&self) -> Option<f64> {
        let limit = self.usage?; // z.ai sometimes omits quota fields; never invent zeros.
        if limit <= 0 {
            return None;
        }

        let used_from_remaining = self.remaining.map(|remaining| limit - remaining);
        let used_raw = match (used_from_remaining, self.current_value) {
            (Some(from_remaining), Some(current)) => from_remaining.max(current),
            (Some(from_remaining), None) => from_remaining,
            (None, Some(current)) => current,
            (None, None) => return None,
        };

        let used = used_raw.clamp(0, limit);
        let percent = (used as f64 / limit as f64) * 100.0;
        Some(percent.clamp(0.0, 100.0))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ZaiQuotaResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<ZaiQuotaData>,
}

impl ZaiQuotaResponse {
    fn is_success(&self) -> bool {
        self.success && self.code == 200
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ZaiQuotaData {
    #[serde(default)]
    limits: Vec<ZaiLimitRaw>,
    /// Plan tier reported by the coding-plan quota API (e.g. "pro").
    #[serde(default)]
    level: Option<String>,
}

impl ZaiQuotaData {
    fn plan_label(&self) -> Option<String> {
        let trimmed = self.level.as_ref()?.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ZaiLimitRaw {
    #[serde(rename = "type")]
    limit_type: String,
    #[serde(default)]
    unit: i64,
    #[serde(default)]
    number: i64,
    #[serde(default)]
    usage: Option<i64>,
    #[serde(default, rename = "currentValue")]
    current_value: Option<i64>,
    #[serde(default)]
    remaining: Option<i64>,
    #[serde(default)]
    percentage: Value,
    #[serde(default, rename = "nextResetTime")]
    next_reset_time: Option<i64>,
}

impl ZaiLimitRaw {
    fn to_entry(&self) -> Option<ZaiLimitEntry> {
        let limit_type = match self.limit_type.as_str() {
            "TOKENS_LIMIT" => ZaiLimitType::Tokens,
            "TIME_LIMIT" => ZaiLimitType::Time,
            _ => return None,
        };
        let next_reset_time = self.next_reset_time.and_then(epoch_millis_to_offset);
        Some(ZaiLimitEntry {
            limit_type,
            unit: ZaiLimitUnit::from_code(self.unit),
            number: self.number,
            usage: self.usage,
            current_value: self.current_value,
            remaining: self.remaining,
            percentage: number_to_f64(&self.percentage),
            next_reset_time,
        })
    }
}

impl ZaiQuotaResponse {
    fn usage_snapshot(&self) -> UsageSnapshot {
        let data = match &self.data {
            Some(data) if self.is_success() => data,
            _ => return UsageSnapshot::empty(),
        };

        let mut token_limits: Vec<&ZaiLimitEntry> = Vec::new();
        let mut time_limits: Vec<&ZaiLimitEntry> = Vec::new();
        let entries: Vec<ZaiLimitEntry> = data
            .limits
            .iter()
            .filter_map(ZaiLimitRaw::to_entry)
            .collect();
        for entry in &entries {
            match entry.limit_type {
                ZaiLimitType::Tokens => token_limits.push(entry),
                ZaiLimitType::Time => time_limits.push(entry),
            }
        }

        token_limits.sort_by_key(|entry| entry.unit.window_duration(entry.number));
        let primary_token = token_limits.last().cloned();
        let session_token = if token_limits.len() >= 2 {
            token_limits.first().cloned()
        } else {
            None
        };
        let time_limit = time_limits.into_iter().next();

        let mut windows = Vec::new();
        if let Some(entry) = primary_token {
            windows.push(token_window("tokens", "Tokens", entry));
        }
        if let Some(entry) = session_token {
            windows.push(token_window("session", "Session", entry));
        }
        if let Some(entry) = time_limit {
            windows.push(time_window(entry));
        }

        UsageSnapshot {
            windows,
            balances: Vec::new(),
            reset_credits: Vec::new(),
        }
    }

    fn identity(&self) -> Option<AccountIdentity> {
        let plan = self
            .data
            .as_ref()
            .filter(|_| self.is_success())
            .and_then(|data| data.plan_label())?;
        Some(AccountIdentity {
            email: None,
            plan: Some(plan),
        })
    }
}

fn token_window(id: &str, label_prefix: &str, entry: &ZaiLimitEntry) -> RateWindow {
    let label = entry
        .unit
        .window_description(entry.number)
        .map(|desc| format!("{label_prefix} ({desc})"))
        .unwrap_or_else(|| label_prefix.to_owned());
    RateWindow {
        id: id.to_owned(),
        label,
        used_percent: entry.used_percent(),
        duration: entry.unit.window_duration(entry.number),
        resets_at: entry.next_reset_time,
    }
}

fn time_window(entry: &ZaiLimitEntry) -> RateWindow {
    if entry.is_monthly_marker() {
        return RateWindow {
            id: "monthly".to_owned(),
            label: "Monthly".to_owned(),
            used_percent: entry.used_percent(),
            duration: None,
            resets_at: entry.next_reset_time,
        };
    }
    let label = entry
        .unit
        .window_description(entry.number)
        .map(|desc| format!("MCP ({desc})"))
        .unwrap_or_else(|| "MCP".to_owned());
    RateWindow {
        id: "mcp".to_owned(),
        label,
        used_percent: entry.used_percent(),
        duration: entry.unit.window_duration(entry.number),
        resets_at: entry.next_reset_time,
    }
}

#[derive(Debug, Error)]
pub enum ZaiProviderError {
    #[error("z.ai API key is not configured")]
    MissingApiKey,
    #[error("z.ai request failed: {0}")]
    Http(reqwest::Error),
    #[error("z.ai response could not be decoded: {0}")]
    Decode(serde_json::Error),
    #[error("z.ai API returned an empty response body")]
    EmptyBody,
    #[error("z.ai API reported failure (code {code}): {msg}")]
    ApiFailed { code: i64, msg: String },
    #[error("z.ai rejected request with HTTP {status}: {body}")]
    Unauthorized { status: u16, body: String },
    #[error("z.ai request returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },
    #[error("could not construct HTTP header {0}")]
    InvalidHeader(String),
}

impl From<ZaiProviderError> for ProviderError {
    fn from(error: ZaiProviderError) -> Self {
        match error {
            ZaiProviderError::MissingApiKey => ProviderError::NotConfigured(error.to_string()),
            ZaiProviderError::Decode(_) | ZaiProviderError::EmptyBody => {
                ProviderError::Parse(error.to_string())
            }
            ZaiProviderError::Unauthorized { .. } => {
                ProviderError::Authentication(error.to_string())
            }
            ZaiProviderError::Http(_)
            | ZaiProviderError::ApiStatus { .. }
            | ZaiProviderError::ApiFailed { .. }
            | ZaiProviderError::InvalidHeader(_) => ProviderError::Network(error.to_string()),
        }
    }
}

fn env_quota_url() -> Option<Url> {
    if let Ok(raw) = env::var(ZAI_QUOTA_URL_ENV)
        && let Ok(url) = Url::parse(raw.trim())
    {
        return Some(url);
    }
    if let Ok(host) = env::var(ZAI_API_HOST_ENV)
        && let Some(url) = join_quota_path(host.trim())
    {
        return Some(url);
    }
    None
}

fn join_quota_path(base: &str) -> Option<Url> {
    let cleaned = base.trim().trim_end_matches('/');
    if cleaned.is_empty() {
        return None;
    }
    let with_path = |url: Url| {
        if url.path() == "/" || url.path().is_empty() {
            url.join(ZAI_QUOTA_API_PATH).ok()
        } else {
            Some(url)
        }
    };
    if cleaned.contains("://") {
        with_path(Url::parse(cleaned).ok()?)
    } else {
        with_path(Url::parse(&format!("https://{cleaned}")).ok()?)
    }
}

fn opencode_auth_path_discovered() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        return Some(dir.join(OPENCODE_AUTH_FILENAME));
    }
    let home = env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join(OPENCODE_AUTH_FILENAME),
    )
}

#[derive(Debug, Clone, Deserialize)]
struct OpenCodeAuthEntry {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    key: Option<String>,
}

fn opencode_api_key_in(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    let auth: HashMap<String, OpenCodeAuthEntry> = serde_json::from_slice(&data).ok()?;
    for provider_id in OPENCODE_ZAI_PROVIDER_IDS {
        if let Some(entry) = auth.get(*provider_id)
            && entry.kind == "api"
            && let Some(key) = entry.key.as_deref().filter(|key| !key.is_empty())
        {
            return Some(key.to_owned());
        }
    }
    None
}

fn epoch_millis_to_offset(millis: i64) -> Option<OffsetDateTime> {
    let seconds = millis.div_euclid(1_000);
    let nanos = millis.rem_euclid(1_000) as u32 * 1_000_000;
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .replace_nanosecond(nanos)
        .ok()
}

fn number_to_f64(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().unwrap_or(0.0),
        Value::String(string) => string.parse::<f64>().unwrap_or(0.0),
        Value::Bool(true) => 100.0,
        _ => 0.0,
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

#[cfg(test)]
mod tests {
    use super::*;
    use braindrain_core::{Provider, RefreshContext};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn config_resolves_default_quota_url() {
        let config = ZaiProviderConfig::default();
        assert_eq!(
            config.resolve_quota_url().as_str(),
            "https://api.z.ai/api/monitor/usage/quota/limit"
        );
    }

    #[test]
    fn config_resolves_bigmodel_region() {
        let config = ZaiProviderConfig::bigmodel_cn();
        assert_eq!(
            config.resolve_quota_url().as_str(),
            "https://open.bigmodel.cn/api/monitor/usage/quota/limit"
        );
    }

    #[test]
    fn config_api_key_prefers_explicit_token_over_env() {
        let config = ZaiProviderConfig {
            api_key: Some("explicit".to_owned()),
            ..ZaiProviderConfig::default()
        };
        let key = config.api_key().expect("api key");
        assert_eq!(key.value, "explicit");
        assert_eq!(key.source, ZaiApiKeySource::Config);
    }

    #[test]
    fn api_key_detects_opencode_zai_coding_plan_entry() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::json!({
                "openai": { "type": "oauth", "refresh": "rt_x" },
                "zai-coding-plan": { "type": "api", "key": "opencode-key" }
            })
            .to_string(),
        )
        .expect("write auth");

        let config = ZaiProviderConfig {
            opencode_auth_path: Some(auth_path),
            ..ZaiProviderConfig::default()
        };
        let key = config.api_key().expect("api key");
        assert_eq!(key.value, "opencode-key");
        assert_eq!(key.source, ZaiApiKeySource::Opencode);
    }

    #[test]
    fn api_key_explicit_config_beats_opencode_entry() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::json!({ "zai-coding-plan": { "type": "api", "key": "opencode-key" } })
                .to_string(),
        )
        .expect("write auth");

        let config = ZaiProviderConfig {
            api_key: Some("explicit".to_owned()),
            opencode_auth_path: Some(auth_path),
            ..ZaiProviderConfig::default()
        };
        let key = config.api_key().expect("api key");
        assert_eq!(key.value, "explicit");
        assert_eq!(key.source, ZaiApiKeySource::Config);
    }

    #[test]
    fn api_key_skips_non_api_opencode_entries() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::json!({ "zai-coding-plan": { "type": "oauth", "refresh": "rt_x" } })
                .to_string(),
        )
        .expect("write auth");

        let config = ZaiProviderConfig {
            opencode_auth_path: Some(auth_path),
            ..ZaiProviderConfig::default()
        };
        assert!(matches!(
            config.api_key(),
            Err(ZaiProviderError::MissingApiKey)
        ));
    }

    #[test]
    fn opencode_api_key_in_matches_known_provider_ids() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::json!({ "zai": { "type": "api", "key": "alt-id-key" } }).to_string(),
        )
        .expect("write auth");

        assert_eq!(
            opencode_api_key_in(&auth_path),
            Some("alt-id-key".to_owned())
        );
    }

    #[test]
    fn limit_unit_window_descriptions_are_humanized() {
        assert_eq!(
            ZaiLimitUnit::Hours.window_description(5).as_deref(),
            Some("5 hours")
        );
        assert_eq!(
            ZaiLimitUnit::Days.window_description(1).as_deref(),
            Some("1 day")
        );
        assert_eq!(ZaiLimitUnit::Unknown.window_description(5), None);
    }

    #[test]
    fn quota_response_maps_windows_from_real_shape() {
        // Mirrors a captured z.ai coding-plan quota response: TOKENS_LIMIT
        // entries carry only `percentage` (no usage/remaining/currentValue), the
        // shorter session window is listed before the weekly one, and the plan
        // tier lives in `data.level`. Only the TIME_LIMIT (MCP) entry carries
        // quota fields.
        let response: ZaiQuotaResponse = serde_json::from_value(serde_json::json!({
            "code": 200,
            "msg": "Operation successful",
            "success": true,
            "data": {
                "level": "pro",
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "percentage": 100,
                        "nextResetTime": 1_782_038_385_385i64
                    },
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 6,
                        "number": 1,
                        "percentage": 20,
                        "nextResetTime": 1_782_624_845_994i64
                    },
                    {
                        "type": "TIME_LIMIT",
                        "unit": 5,
                        "number": 1,
                        "usage": 1000,
                        "currentValue": 0,
                        "remaining": 1000,
                        "percentage": 0,
                        "nextResetTime": 1_784_612_045_987i64,
                        "usageDetails": [
                            { "modelCode": "search-prime", "usage": 0 }
                        ]
                    }
                ]
            }
        }))
        .expect("parse quota");

        let snapshot = response.usage_snapshot();
        let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["tokens", "session", "monthly"]);

        let tokens = &snapshot.windows[0];
        assert_eq!(tokens.label, "Tokens (1 week)");
        assert_eq!(tokens.used_percent, 20.0);
        assert_eq!(tokens.duration, Some(Duration::from_secs(604_800)));
        assert_eq!(
            tokens.resets_at.expect("resets_at").unix_timestamp(),
            1_782_624_845
        );

        let session = &snapshot.windows[1];
        assert_eq!(session.label, "Session (5 hours)");
        assert_eq!(session.used_percent, 100.0);
        assert_eq!(session.duration, Some(Duration::from_secs(18_000)));

        let monthly = &snapshot.windows[2];
        assert_eq!(monthly.label, "Monthly");
        assert_eq!(monthly.used_percent, 0.0);
        assert!(monthly.duration.is_none());
        assert_eq!(
            monthly.resets_at.expect("resets_at").unix_timestamp(),
            1_784_612_045
        );

        let identity = response.identity().expect("identity");
        assert_eq!(identity.plan, Some("pro".to_owned()));
    }

    #[test]
    fn used_percent_falls_back_to_percentage_when_quota_omitted() {
        // Real TOKENS_LIMIT entries omit usage/remaining/currentValue, so the
        // server-provided `percentage` is the only source of truth.
        let entry = ZaiLimitEntry {
            limit_type: ZaiLimitType::Tokens,
            unit: ZaiLimitUnit::Days,
            number: 7,
            usage: None,
            current_value: None,
            remaining: None,
            percentage: 42.0,
            next_reset_time: None,
        };
        assert_eq!(entry.used_percent(), 42.0);
    }

    #[test]
    fn used_percent_computes_from_quota_for_time_limit() {
        // TIME_LIMIT (MCP) entries carry usage/remaining/currentValue, so usage
        // is derived from those rather than the `percentage` field.
        let entry = ZaiLimitEntry {
            limit_type: ZaiLimitType::Time,
            unit: ZaiLimitUnit::Minutes,
            number: 1,
            usage: Some(1_000),
            current_value: Some(250),
            remaining: Some(750),
            percentage: 0.0,
            next_reset_time: None,
        };
        assert_eq!(entry.used_percent(), 25.0);
    }

    #[test]
    fn used_percent_clamps_to_zero_when_remaining_exceeds_limit() {
        // Defensive: a malformed entry where remaining > usage must not go
        // negative or exceed 100%.
        let entry = ZaiLimitEntry {
            limit_type: ZaiLimitType::Time,
            unit: ZaiLimitUnit::Minutes,
            number: 1,
            usage: Some(100),
            current_value: None,
            remaining: Some(150),
            percentage: 0.0,
            next_reset_time: None,
        };
        assert_eq!(entry.used_percent(), 0.0);
    }

    #[tokio::test]
    async fn provider_fetches_quota_with_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/monitor/usage/quota/limit"))
            .and(header("authorization", "Bearer test-key"))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 200,
                "msg": "Operation successful",
                "success": true,
                "data": {
                    "level": "pro",
                    "limits": [
                        {
                            "type": "TOKENS_LIMIT",
                            "unit": 6,
                            "number": 1,
                            "percentage": 20,
                            "nextResetTime": 1_782_624_845_994i64
                        },
                        {
                            "type": "TIME_LIMIT",
                            "unit": 5,
                            "number": 1,
                            "usage": 1000,
                            "currentValue": 0,
                            "remaining": 1000,
                            "percentage": 0,
                            "nextResetTime": 1_784_612_045_987i64
                        }
                    ]
                }
            })))
            .mount(&server)
            .await;

        let provider = ZaiProvider::new(ZaiProviderConfig {
            api_key: Some("test-key".to_owned()),
            quota_url: Some(
                Url::parse(&format!("{}/api/monitor/usage/quota/limit", server.uri()))
                    .expect("quota url"),
            ),
            ..ZaiProviderConfig::default()
        });

        let snapshot = provider
            .refresh(RefreshContext {
                now: OffsetDateTime::from_unix_timestamp(1_780_704_000).expect("valid timestamp"),
            })
            .await
            .expect("refresh z.ai");

        assert_eq!(snapshot.provider, ProviderId::zai());
        assert_eq!(snapshot.source, ProviderSource::Manual);
        let ids: Vec<&str> = snapshot
            .usage
            .windows
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(ids, ["tokens", "monthly"]);
        assert_eq!(snapshot.usage.windows[0].id, "tokens");
        assert_eq!(snapshot.usage.windows[0].used_percent, 20.0);
        assert_eq!(snapshot.usage.windows[1].id, "monthly");
        assert_eq!(snapshot.usage.windows[1].used_percent, 0.0);
        let identity = snapshot.identity.expect("identity");
        assert_eq!(identity.plan, Some("pro".to_owned()));
    }
}
