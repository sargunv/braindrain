use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use braindrain_core::{
    AccountIdentity, BalanceSnapshot, Provider, ProviderCredentialField, ProviderCredentialSchema,
    ProviderCredentials, ProviderError, ProviderFuture, ProviderId, ProviderSnapshot,
    ProviderSource, RateWindow, RefreshContext, UsageSnapshot,
};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

pub const XIAOMI_CONSOLE_BASE_URL: &str = "https://platform.xiaomimimo.com";
pub const XIAOMI_USAGE_API_PATH: &str = "api/v1/tokenPlan/usage";
pub const XIAOMI_DETAIL_API_PATH: &str = "api/v1/tokenPlan/detail";
pub const XIAOMI_BALANCE_API_PATH: &str = "api/v1/balance";
pub const XIAOMI_AUTH_COOKIE_ENV: &str = "XIAOMI_AUTH_COOKIE";
pub const XIAOMI_API_KEY_ENV: &str = "XIAOMI_API_KEY";
pub const XIAOMI_CONSOLE_URL_ENV: &str = "XIAOMI_CONSOLE_URL";
pub const XIAOMI_KEYCHAIN_SERVICE: &str = "braindrain-xiaomi";
pub const XIAOMI_KEYCHAIN_ACCOUNT: &str = "xiaomi-token-plan";
pub const AUTH_COOKIE_FIELD: &str = "auth_cookie";

pub const MIMOCODE_AUTH_FILENAME: &str = "mimocode/auth.json";
pub const OPENCODE_AUTH_FILENAME: &str = "opencode/auth.json";
pub const MIMOCODE_XIAOMI_PROVIDER_IDS: &[&str] = &[
    "xiaomi",
    "xiaomi-token-plan",
    "xiaomi-token-plan-sgp",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-ams",
];
pub const OPENCODE_XIAOMI_PROVIDER_IDS: &[&str] = &[
    "xiaomi-token-plan-sgp",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan",
    "xiaomi",
];

#[derive(Debug, Clone)]
pub struct XiaomiProvider {
    config: XiaomiProviderConfig,
    client: reqwest::Client,
}

impl XiaomiProvider {
    pub fn new(config: XiaomiProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    pub fn config(&self) -> &XiaomiProviderConfig {
        &self.config
    }

    pub fn plan_key(&self) -> Result<XiaomiPlanKey, XiaomiProviderError> {
        self.config.plan_key()
    }

    pub fn auth_cookie(&self) -> Result<XiaomiAuthCookie, XiaomiProviderError> {
        self.config.auth_cookie()
    }

    pub async fn auth_cookie_async(&self) -> Result<XiaomiAuthCookie, XiaomiProviderError> {
        self.config.auth_cookie_async().await
    }

    pub fn usage_url(&self) -> Url {
        join_console_path(&self.config.resolve_console_url(), XIAOMI_USAGE_API_PATH)
            .expect("usage path is valid")
    }

    pub fn credential_schema() -> ProviderCredentialSchema {
        ProviderCredentialSchema {
            provider: ProviderId::xiaomi(),
            fields: vec![ProviderCredentialField {
                id: AUTH_COOKIE_FIELD.to_owned(),
                label: "Console cookie (Cookie request header from platform.xiaomimimo.com)"
                    .to_owned(),
                secret: true,
            }],
        }
    }

    pub async fn store_credentials(
        credentials: ProviderCredentials,
    ) -> Result<(), XiaomiProviderError> {
        let auth_cookie = credentials
            .values
            .get(AUTH_COOKIE_FIELD)
            .map(|value| normalize_cookie(value))
            .filter(|value| !value.is_empty())
            .ok_or(XiaomiProviderError::MissingField(AUTH_COOKIE_FIELD))?;

        let payload = serde_json::to_string(&StoredCredentials { auth_cookie })
            .map_err(XiaomiProviderError::Serialize)?;

        tokio::task::spawn_blocking(move || -> Result<(), XiaomiProviderError> {
            let entry = keyring::Entry::new(XIAOMI_KEYCHAIN_SERVICE, XIAOMI_KEYCHAIN_ACCOUNT)
                .map_err(keychain_error)?;
            entry.set_password(&payload).map_err(keychain_error)
        })
        .await
        .map_err(|error| XiaomiProviderError::Keychain(error.to_string()))?
    }

    pub async fn delete_credentials() -> Result<(), XiaomiProviderError> {
        tokio::task::spawn_blocking(|| -> Result<(), XiaomiProviderError> {
            let entry = keyring::Entry::new(XIAOMI_KEYCHAIN_SERVICE, XIAOMI_KEYCHAIN_ACCOUNT)
                .map_err(keychain_error)?;
            match entry.delete_credential() {
                Ok(()) => Ok(()),
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(keychain_error(error)),
            }
        })
        .await
        .map_err(|error| XiaomiProviderError::Keychain(error.to_string()))?
    }

    async fn fetch_data<T: DeserializeOwned>(
        &self,
        path: &str,
        auth_cookie: &str,
    ) -> Result<T, XiaomiProviderError> {
        let console_url = self.config.resolve_console_url();
        let url = join_console_path(&console_url, path)?;
        let origin = console_url.origin().ascii_serialization();

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            COOKIE,
            HeaderValue::from_str(auth_cookie)
                .map_err(|_| XiaomiProviderError::InvalidHeader(COOKIE.as_str().to_owned()))?,
        );
        headers.insert(
            ORIGIN,
            HeaderValue::from_str(&origin)
                .map_err(|_| XiaomiProviderError::InvalidHeader(ORIGIN.as_str().to_owned()))?,
        );
        headers.insert(
            REFERER,
            HeaderValue::from_str(console_url.as_str())
                .map_err(|_| XiaomiProviderError::InvalidHeader(REFERER.as_str().to_owned()))?,
        );

        let response = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(XiaomiProviderError::Http)?;
        let status = response.status();
        let body = response.bytes().await.map_err(XiaomiProviderError::Http)?;

        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(XiaomiProviderError::Unauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        if !status.is_success() {
            return Err(XiaomiProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        if body.is_empty() {
            return Err(XiaomiProviderError::EmptyBody);
        }

        let envelope: ConsoleEnvelope<T> =
            serde_json::from_slice(&body).map_err(XiaomiProviderError::Decode)?;
        if !envelope.is_success() {
            return Err(XiaomiProviderError::ApiFailed {
                code: envelope.code,
                msg: envelope.message,
            });
        }
        envelope
            .data
            .ok_or_else(|| XiaomiProviderError::Parse("response data is missing".to_owned()))
    }

    async fn fetch_snapshot(
        &self,
        now: OffsetDateTime,
    ) -> Result<ProviderSnapshot, XiaomiProviderError> {
        let auth_cookie = self
            .auth_cookie_async()
            .await
            .map_err(|error| match error {
                XiaomiProviderError::MissingCookie if self.plan_key().is_err() => {
                    XiaomiProviderError::MissingCredentials
                }
                other => other,
            })?;

        let usage: ConsoleUsageData = self
            .fetch_data(XIAOMI_USAGE_API_PATH, &auth_cookie.value)
            .await?;
        let detail: Option<ConsoleDetailData> = self
            .fetch_data(XIAOMI_DETAIL_API_PATH, &auth_cookie.value)
            .await
            .ok();
        let balance: Option<ConsoleBalanceData> = self
            .fetch_data(XIAOMI_BALANCE_API_PATH, &auth_cookie.value)
            .await
            .ok();

        Ok(ProviderSnapshot {
            provider: ProviderId::xiaomi(),
            source: ProviderSource::Web,
            usage: usage_snapshot(&usage, detail.as_ref(), balance.as_ref()),
            identity: identity(detail.as_ref()),
            updated_at: now,
        })
    }
}

impl Default for XiaomiProvider {
    fn default() -> Self {
        Self::new(XiaomiProviderConfig::default())
    }
}

impl Provider for XiaomiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::xiaomi()
    }

    fn refresh<'a>(
        &'a self,
        context: RefreshContext,
    ) -> ProviderFuture<'a, Result<ProviderSnapshot, ProviderError>> {
        Box::pin(async move {
            self.fetch_snapshot(context.now)
                .await
                .map_err(ProviderError::from)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XiaomiPlanKey {
    pub value: String,
    pub uid: Option<String>,
    pub base_url: Option<Url>,
    pub source: XiaomiPlanKeySource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiaomiPlanKeySource {
    Config,
    Mimocode,
    Opencode,
    Environment(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XiaomiAuthCookie {
    pub value: String,
    pub source: XiaomiAuthCookieSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiaomiAuthCookieSource {
    Config,
    Environment(&'static str),
    Keyring,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XiaomiProviderConfig {
    pub plan_key: Option<String>,
    pub auth_cookie: Option<String>,
    pub console_url: Option<Url>,
    pub mimocode_auth_path: Option<PathBuf>,
    pub opencode_auth_path: Option<PathBuf>,
    pub keyring_enabled: bool,
}

impl XiaomiProviderConfig {
    pub fn plan_key(&self) -> Result<XiaomiPlanKey, XiaomiProviderError> {
        if let Some(key) = self.plan_key.as_deref().filter(|key| !key.is_empty()) {
            return Ok(XiaomiPlanKey {
                value: key.to_owned(),
                uid: None,
                base_url: None,
                source: XiaomiPlanKeySource::Config,
            });
        }

        let mimocode_path = self
            .mimocode_auth_path
            .clone()
            .or_else(mimocode_auth_path_discovered);
        if let Some(key) = mimocode_path.as_deref().and_then(|path| {
            harness_plan_key_in(
                path,
                MIMOCODE_XIAOMI_PROVIDER_IDS,
                XiaomiPlanKeySource::Mimocode,
            )
        }) {
            return Ok(key);
        }

        let opencode_path = self
            .opencode_auth_path
            .clone()
            .or_else(opencode_auth_path_discovered);
        if let Some(key) = opencode_path.as_deref().and_then(|path| {
            harness_plan_key_in(
                path,
                OPENCODE_XIAOMI_PROVIDER_IDS,
                XiaomiPlanKeySource::Opencode,
            )
        }) {
            return Ok(key);
        }

        if let Some(key) = env::var_os(XIAOMI_API_KEY_ENV)
            .and_then(|key| key.into_string().ok())
            .filter(|key| !key.is_empty())
        {
            return Ok(XiaomiPlanKey {
                value: key,
                uid: None,
                base_url: None,
                source: XiaomiPlanKeySource::Environment(XIAOMI_API_KEY_ENV),
            });
        }

        Err(XiaomiProviderError::MissingPlanKey)
    }

    pub fn auth_cookie(&self) -> Result<XiaomiAuthCookie, XiaomiProviderError> {
        if let Some(cookie) = self.resolved_auth_cookie() {
            return Ok(cookie);
        }
        if self.keyring_enabled
            && let Some(cookie) = keyring_auth_cookie_blocking()?
        {
            return Ok(cookie);
        }
        Err(XiaomiProviderError::MissingCookie)
    }

    pub async fn auth_cookie_async(&self) -> Result<XiaomiAuthCookie, XiaomiProviderError> {
        if let Some(cookie) = self.resolved_auth_cookie() {
            return Ok(cookie);
        }
        if self.keyring_enabled {
            let stored = tokio::task::spawn_blocking(keyring_auth_cookie_blocking)
                .await
                .map_err(|error| XiaomiProviderError::Keychain(error.to_string()))??;
            if let Some(cookie) = stored {
                return Ok(cookie);
            }
        }
        Err(XiaomiProviderError::MissingCookie)
    }

    fn resolved_auth_cookie(&self) -> Option<XiaomiAuthCookie> {
        if let Some(cookie) = self
            .auth_cookie
            .as_deref()
            .map(normalize_cookie)
            .filter(|cookie| !cookie.is_empty())
        {
            return Some(XiaomiAuthCookie {
                value: cookie,
                source: XiaomiAuthCookieSource::Config,
            });
        }

        if let Some(cookie) = env::var_os(XIAOMI_AUTH_COOKIE_ENV)
            .and_then(|cookie| cookie.into_string().ok())
            .map(|cookie| normalize_cookie(&cookie))
            .filter(|cookie| !cookie.is_empty())
        {
            return Some(XiaomiAuthCookie {
                value: cookie,
                source: XiaomiAuthCookieSource::Environment(XIAOMI_AUTH_COOKIE_ENV),
            });
        }

        None
    }

    pub fn mimocode_auth_path(&self) -> Option<PathBuf> {
        self.mimocode_auth_path
            .clone()
            .or_else(mimocode_auth_path_discovered)
    }

    pub fn opencode_auth_path(&self) -> Option<PathBuf> {
        self.opencode_auth_path
            .clone()
            .or_else(opencode_auth_path_discovered)
    }

    pub fn resolve_console_url(&self) -> Url {
        if let Some(url) = self.console_url.clone() {
            return url;
        }

        if let Ok(raw) = env::var(XIAOMI_CONSOLE_URL_ENV)
            && let Ok(url) = Url::parse(raw.trim())
        {
            return url;
        }

        Url::parse(XIAOMI_CONSOLE_BASE_URL).expect("valid xiaomi console URL")
    }
}

impl Default for XiaomiProviderConfig {
    fn default() -> Self {
        Self {
            plan_key: None,
            auth_cookie: None,
            console_url: None,
            mimocode_auth_path: None,
            opencode_auth_path: None,
            keyring_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ConsoleEnvelope<T> {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<T>,
}

impl<T> ConsoleEnvelope<T> {
    fn is_success(&self) -> bool {
        self.code == 0
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ConsoleUsageData {
    #[serde(default)]
    usage: ConsoleUsageGroup,
    #[serde(rename = "monthUsage", default)]
    month_usage: ConsoleUsageGroup,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ConsoleUsageGroup {
    #[serde(default)]
    items: Vec<ConsoleUsageItem>,
}

impl ConsoleUsageGroup {
    fn item(&self, name: &str) -> Option<&ConsoleUsageItem> {
        self.items.iter().find(|item| item.name == name)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ConsoleUsageItem {
    name: String,
    #[serde(default)]
    used: f64,
    #[serde(default)]
    limit: f64,
    #[serde(default)]
    percent: f64,
}

impl ConsoleUsageItem {
    fn used_percent(&self) -> f64 {
        if self.limit > 0.0 {
            ((self.used / self.limit) * 100.0).clamp(0.0, 100.0)
        } else {
            (self.percent * 100.0).clamp(0.0, 100.0)
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ConsoleDetailData {
    #[serde(rename = "planName", default)]
    plan_name: Option<String>,
    #[serde(rename = "currentPeriodEnd", default)]
    current_period_end: Option<String>,
}

impl ConsoleDetailData {
    fn plan_label(&self) -> Option<String> {
        let trimmed = self.plan_name.as_ref()?.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    fn period_end(&self) -> Option<OffsetDateTime> {
        self.current_period_end
            .as_deref()
            .and_then(parse_console_timestamp)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ConsoleBalanceData {
    #[serde(default)]
    balance: Option<Value>,
    #[serde(default)]
    currency: Option<String>,
}

impl ConsoleBalanceData {
    fn snapshot(&self) -> Option<BalanceSnapshot> {
        let remaining = self.balance.as_ref().and_then(value_to_f64)?;
        Some(BalanceSnapshot {
            id: "balance".to_owned(),
            label: "Balance".to_owned(),
            remaining,
            unit: self
                .currency
                .as_deref()
                .map(str::trim)
                .filter(|unit| !unit.is_empty())
                .unwrap_or("credits")
                .to_owned(),
        })
    }
}

fn usage_snapshot(
    usage: &ConsoleUsageData,
    detail: Option<&ConsoleDetailData>,
    balance: Option<&ConsoleBalanceData>,
) -> UsageSnapshot {
    let mut windows = Vec::new();
    if let Some(item) = usage.usage.item("plan_total_token") {
        windows.push(RateWindow {
            id: "plan".to_owned(),
            label: "Token Plan".to_owned(),
            used_percent: item.used_percent(),
            duration: None,
            resets_at: detail.and_then(ConsoleDetailData::period_end),
        });
    }
    if let Some(item) = usage.month_usage.item("month_total_token") {
        windows.push(RateWindow {
            id: "monthly".to_owned(),
            label: "Monthly".to_owned(),
            used_percent: item.used_percent(),
            duration: None,
            resets_at: None,
        });
    }

    let mut balances = Vec::new();
    if let Some(item) = usage.usage.item("compensation_total_token")
        && item.limit > 0.0
    {
        balances.push(BalanceSnapshot {
            id: "compensation".to_owned(),
            label: "Compensation".to_owned(),
            remaining: (item.limit - item.used).max(0.0),
            unit: "tokens".to_owned(),
        });
    }
    if let Some(snapshot) = balance.and_then(ConsoleBalanceData::snapshot) {
        balances.push(snapshot);
    }

    UsageSnapshot {
        windows,
        balances,
        reset_credits: Vec::new(),
    }
}

fn identity(detail: Option<&ConsoleDetailData>) -> Option<AccountIdentity> {
    let plan = detail.and_then(ConsoleDetailData::plan_label)?;
    Some(AccountIdentity {
        email: None,
        plan: Some(plan),
    })
}

#[derive(Debug, Error)]
pub enum XiaomiProviderError {
    #[error(
        "xiaomi console cookie is not configured; run `braindrain auth login xiaomi` or set \
         XIAOMI_AUTH_COOKIE"
    )]
    MissingCookie,
    #[error("xiaomi provider is not configured: no console cookie or plan key found")]
    MissingCredentials,
    #[error("xiaomi plan key is not configured")]
    MissingPlanKey,
    #[error("a required field is missing: {0}")]
    MissingField(&'static str),
    #[error("could not read xiaomi credentials from system keyring: {0}")]
    Keychain(String),
    #[error("could not encode xiaomi credentials: {0}")]
    Serialize(serde_json::Error),
    #[error("could not build xiaomi URL: {0}")]
    Url(url::ParseError),
    #[error("xiaomi request failed: {0}")]
    Http(reqwest::Error),
    #[error("xiaomi response could not be decoded: {0}")]
    Decode(serde_json::Error),
    #[error("xiaomi console API returned an empty response body")]
    EmptyBody,
    #[error("xiaomi console API reported failure (code {code}): {msg}")]
    ApiFailed { code: i64, msg: String },
    #[error(
        "xiaomi console rejected the cookie (HTTP {status}); run `braindrain auth login xiaomi` \
         to refresh it: {body}"
    )]
    Unauthorized { status: u16, body: String },
    #[error("xiaomi console returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },
    #[error("could not construct HTTP header {0}")]
    InvalidHeader(String),
    #[error("could not parse xiaomi console data: {0}")]
    Parse(String),
}

impl From<XiaomiProviderError> for ProviderError {
    fn from(error: XiaomiProviderError) -> Self {
        match error {
            XiaomiProviderError::MissingCookie | XiaomiProviderError::Unauthorized { .. } => {
                ProviderError::Authentication(error.to_string())
            }
            XiaomiProviderError::MissingCredentials
            | XiaomiProviderError::MissingPlanKey
            | XiaomiProviderError::MissingField(_) => {
                ProviderError::NotConfigured(error.to_string())
            }
            XiaomiProviderError::Decode(_)
            | XiaomiProviderError::EmptyBody
            | XiaomiProviderError::Parse(_) => ProviderError::Parse(error.to_string()),
            XiaomiProviderError::Keychain(_)
            | XiaomiProviderError::Serialize(_)
            | XiaomiProviderError::Url(_)
            | XiaomiProviderError::Http(_)
            | XiaomiProviderError::ApiFailed { .. }
            | XiaomiProviderError::ApiStatus { .. }
            | XiaomiProviderError::InvalidHeader(_) => ProviderError::Network(error.to_string()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCredentials {
    auth_cookie: String,
}

#[derive(Debug, Clone, Deserialize)]
struct HarnessAuthEntry {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    metadata: Option<HarnessAuthMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
struct HarnessAuthMetadata {
    #[serde(default)]
    uid: Option<String>,
    #[serde(default, rename = "base_url")]
    base_url: Option<String>,
}

fn harness_plan_key_in(
    path: &Path,
    provider_ids: &[&str],
    source: XiaomiPlanKeySource,
) -> Option<XiaomiPlanKey> {
    let data = fs::read(path).ok()?;
    let auth: HashMap<String, HarnessAuthEntry> = serde_json::from_slice(&data).ok()?;
    for provider_id in provider_ids {
        if let Some(entry) = auth.get(*provider_id)
            && entry.kind == "api"
            && let Some(key) = entry.key.as_deref().filter(|key| !key.is_empty())
        {
            let metadata = entry.metadata.as_ref();
            let uid = metadata
                .and_then(|metadata| metadata.uid.as_deref())
                .map(str::trim)
                .filter(|uid| !uid.is_empty())
                .map(str::to_owned);
            let base_url = metadata
                .and_then(|metadata| metadata.base_url.as_deref())
                .and_then(|raw| Url::parse(raw.trim()).ok());
            return Some(XiaomiPlanKey {
                value: key.to_owned(),
                uid,
                base_url,
                source,
            });
        }
    }
    None
}

fn mimocode_auth_path_discovered() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        return Some(dir.join(MIMOCODE_AUTH_FILENAME));
    }
    let home = env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join(MIMOCODE_AUTH_FILENAME),
    )
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

fn keyring_auth_cookie_blocking() -> Result<Option<XiaomiAuthCookie>, XiaomiProviderError> {
    let entry = keyring::Entry::new(XIAOMI_KEYCHAIN_SERVICE, XIAOMI_KEYCHAIN_ACCOUNT)
        .map_err(keychain_error)?;
    let payload = match entry.get_password() {
        Ok(payload) => payload,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(error) => return Err(keychain_error(error)),
    };

    let stored: StoredCredentials =
        serde_json::from_str(&payload).map_err(XiaomiProviderError::Serialize)?;
    let value = normalize_cookie(&stored.auth_cookie);
    if value.is_empty() {
        return Ok(None);
    }

    Ok(Some(XiaomiAuthCookie {
        value,
        source: XiaomiAuthCookieSource::Keyring,
    }))
}

fn keychain_error(error: keyring::Error) -> XiaomiProviderError {
    XiaomiProviderError::Keychain(error.to_string())
}

fn normalize_cookie(value: &str) -> String {
    let trimmed = value.trim();
    let without_header = trimmed
        .strip_prefix("Cookie:")
        .or_else(|| trimmed.strip_prefix("cookie:"))
        .unwrap_or(trimmed);
    without_header.trim().to_owned()
}

fn join_console_path(base: &Url, path: &str) -> Result<Url, XiaomiProviderError> {
    let mut base = base.clone();
    if !base.path().ends_with('/') {
        let mut directory = base.path().to_owned();
        directory.push('/');
        base.set_path(&directory);
    }
    base.join(path).map_err(XiaomiProviderError::Url)
}

fn parse_console_timestamp(raw: &str) -> Option<OffsetDateTime> {
    let raw = raw.trim();
    if let Ok(parsed) = OffsetDateTime::parse(raw, &Rfc3339) {
        return Some(parsed);
    }
    for description in [
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second] UTC"),
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute] UTC"),
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]"),
    ] {
        if let Ok(parsed) = time::PrimitiveDateTime::parse(raw, description) {
            return Some(parsed.assume_utc());
        }
    }
    None
}

fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(string) => string.trim().parse::<f64>().ok(),
        _ => None,
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

    fn usage_body() -> serde_json::Value {
        serde_json::json!({
            "code": 0,
            "message": "",
            "data": {
                "monthUsage": {
                    "percent": 0.0513,
                    "items": [
                        { "name": "month_total_token", "used": 564164520, "limit": 11000000000_u64, "percent": 0.0513 }
                    ]
                },
                "usage": {
                    "percent": 0.05,
                    "items": [
                        { "name": "plan_total_token", "used": 564164520, "limit": 11000000000_u64, "percent": 0.05 },
                        { "name": "compensation_total_token", "used": 0, "limit": 0, "percent": 0 }
                    ]
                }
            }
        })
    }

    fn detail_body() -> serde_json::Value {
        serde_json::json!({
            "code": 0,
            "message": "",
            "data": {
                "planCode": "standard",
                "planName": "Standard",
                "currentPeriodEnd": "2026-10-24 23:59:59",
                "expired": false,
                "enableAutoRenew": true,
                "autoRenewDiscount": null,
                "hasAutoRenewSubscribed": true,
                "clawEnabled": false,
                "clawPeriodEnd": null,
                "clawPurchased": false
            }
        })
    }

    fn balance_body() -> serde_json::Value {
        serde_json::json!({
            "code": 0,
            "message": "",
            "data": {
                "balance": "0.00",
                "frozenBalance": "0.00",
                "currency": "USD",
                "overdraftLimit": "0.00",
                "remainingOverdraftLimit": "0.00",
                "giftBalance": "0.00",
                "cashBalance": "0.00"
            }
        })
    }

    fn hermetic_config() -> XiaomiProviderConfig {
        XiaomiProviderConfig {
            keyring_enabled: false,
            mimocode_auth_path: Some(PathBuf::from("/nonexistent/mimocode-auth.json")),
            opencode_auth_path: Some(PathBuf::from("/nonexistent/opencode-auth.json")),
            ..XiaomiProviderConfig::default()
        }
    }

    #[test]
    fn config_plan_key_prefers_explicit_token() {
        let config = XiaomiProviderConfig {
            plan_key: Some("explicit".to_owned()),
            ..hermetic_config()
        };
        let key = config.plan_key().expect("plan key");
        assert_eq!(key.value, "explicit");
        assert_eq!(key.source, XiaomiPlanKeySource::Config);
    }

    #[test]
    fn plan_key_reads_mimocode_entry_with_metadata() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::json!({
                "xiaomi": {
                    "type": "api",
                    "key": "mimocode-key",
                    "metadata": {
                        "uid": "6623873869",
                        "base_url": "https://token-plan-sgp.xiaomimimo.com/v1"
                    }
                }
            })
            .to_string(),
        )
        .expect("write auth");

        let config = XiaomiProviderConfig {
            mimocode_auth_path: Some(auth_path),
            ..hermetic_config()
        };
        let key = config.plan_key().expect("plan key");
        assert_eq!(key.value, "mimocode-key");
        assert_eq!(key.source, XiaomiPlanKeySource::Mimocode);
        assert_eq!(key.uid.as_deref(), Some("6623873869"));
        assert_eq!(
            key.base_url.as_ref().expect("base_url").as_str(),
            "https://token-plan-sgp.xiaomimimo.com/v1"
        );
    }

    #[test]
    fn plan_key_skips_non_api_mimocode_entries() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let auth_path = tempdir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::json!({ "xiaomi": { "type": "oauth", "refresh": "rt_x" } }).to_string(),
        )
        .expect("write auth");

        let config = XiaomiProviderConfig {
            mimocode_auth_path: Some(auth_path),
            ..hermetic_config()
        };
        assert!(matches!(
            config.plan_key(),
            Err(XiaomiProviderError::MissingPlanKey)
        ));
    }

    #[test]
    fn plan_key_falls_back_to_opencode_token_plan_entry() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mimocode_path = tempdir.path().join("mimocode-auth.json");
        let opencode_path = tempdir.path().join("opencode-auth.json");
        std::fs::write(
            &opencode_path,
            serde_json::json!({
                "openai": { "type": "oauth", "refresh": "rt_x" },
                "xiaomi-token-plan-sgp": { "type": "api", "key": "opencode-key" }
            })
            .to_string(),
        )
        .expect("write auth");

        let config = XiaomiProviderConfig {
            mimocode_auth_path: Some(mimocode_path),
            opencode_auth_path: Some(opencode_path),
            ..hermetic_config()
        };
        let key = config.plan_key().expect("plan key");
        assert_eq!(key.value, "opencode-key");
        assert_eq!(key.source, XiaomiPlanKeySource::Opencode);
    }

    #[test]
    fn plan_key_prefers_mimocode_over_opencode() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mimocode_path = tempdir.path().join("mimocode-auth.json");
        let opencode_path = tempdir.path().join("opencode-auth.json");
        std::fs::write(
            &mimocode_path,
            serde_json::json!({ "xiaomi": { "type": "api", "key": "mimocode-key" } }).to_string(),
        )
        .expect("write mimocode auth");
        std::fs::write(
            &opencode_path,
            serde_json::json!({ "xiaomi-token-plan-sgp": { "type": "api", "key": "opencode-key" } })
                .to_string(),
        )
        .expect("write opencode auth");

        let config = XiaomiProviderConfig {
            mimocode_auth_path: Some(mimocode_path),
            opencode_auth_path: Some(opencode_path),
            ..hermetic_config()
        };
        let key = config.plan_key().expect("plan key");
        assert_eq!(key.value, "mimocode-key");
        assert_eq!(key.source, XiaomiPlanKeySource::Mimocode);
    }

    #[test]
    fn normalize_cookie_strips_cookie_header_prefix() {
        assert_eq!(normalize_cookie("Cookie: a=1; b=2"), "a=1; b=2".to_owned());
        assert_eq!(normalize_cookie("  cookie: a=1  "), "a=1".to_owned());
        assert_eq!(normalize_cookie("a=1; b=2"), "a=1; b=2".to_owned());
        assert_eq!(normalize_cookie("   "), "".to_owned());
    }

    #[test]
    fn auth_cookie_prefers_explicit_config() {
        let config = XiaomiProviderConfig {
            auth_cookie: Some("Cookie: session=abc".to_owned()),
            ..hermetic_config()
        };
        let cookie = config.auth_cookie().expect("cookie");
        assert_eq!(cookie.value, "session=abc");
        assert_eq!(cookie.source, XiaomiAuthCookieSource::Config);
    }

    #[test]
    fn auth_cookie_reports_missing_cookie() {
        let config = hermetic_config();
        assert!(matches!(
            config.auth_cookie(),
            Err(XiaomiProviderError::MissingCookie)
        ));
    }

    #[test]
    fn usage_maps_plan_and_monthly_windows() {
        let usage: ConsoleUsageData =
            serde_json::from_value(usage_body()["data"].clone()).expect("parse usage");
        let detail: ConsoleDetailData =
            serde_json::from_value(detail_body()["data"].clone()).expect("parse detail");

        let snapshot = usage_snapshot(&usage, Some(&detail), None);
        let ids: Vec<&str> = snapshot.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["plan", "monthly"]);

        let plan = &snapshot.windows[0];
        assert_eq!(plan.label, "Token Plan");
        assert_eq!(plan.used_percent, 564164520.0 / 11000000000.0 * 100.0);
        assert!(plan.duration.is_none());
        assert_eq!(
            plan.resets_at.expect("resets_at"),
            OffsetDateTime::parse("2026-10-24T23:59:59Z", &Rfc3339).expect("timestamp")
        );

        let monthly = &snapshot.windows[1];
        assert_eq!(monthly.label, "Monthly");
        assert_eq!(monthly.used_percent, 564164520.0 / 11000000000.0 * 100.0);
        assert!(monthly.resets_at.is_none());

        assert!(snapshot.balances.is_empty());
    }

    #[test]
    fn used_percent_prefers_quota_over_percent_field() {
        let item = ConsoleUsageItem {
            name: "plan_total_token".to_owned(),
            used: 25.0,
            limit: 100.0,
            percent: 0.99,
        };
        assert_eq!(item.used_percent(), 25.0);
    }

    #[test]
    fn used_percent_treats_percent_field_as_ratio() {
        let item = ConsoleUsageItem {
            name: "plan_total_token".to_owned(),
            used: 0.0,
            limit: 0.0,
            percent: 0.25,
        };
        assert_eq!(item.used_percent(), 25.0);
    }

    #[test]
    fn used_percent_clamps_to_100() {
        let item = ConsoleUsageItem {
            name: "plan_total_token".to_owned(),
            used: 150.0,
            limit: 100.0,
            percent: 2.0,
        };
        assert_eq!(item.used_percent(), 100.0);
    }

    #[test]
    fn compensation_skipped_when_limit_zero() {
        let usage: ConsoleUsageData =
            serde_json::from_value(usage_body()["data"].clone()).expect("parse usage");
        let snapshot = usage_snapshot(&usage, None, None);
        assert!(snapshot.balances.is_empty());
    }

    #[test]
    fn compensation_maps_to_balance_when_limit_positive() {
        let usage: ConsoleUsageData = serde_json::from_value(serde_json::json!({
            "usage": {
                "items": [
                    { "name": "compensation_total_token", "used": 40, "limit": 100, "percent": 0.4 }
                ]
            }
        }))
        .expect("parse usage");

        let snapshot = usage_snapshot(&usage, None, None);
        assert_eq!(snapshot.balances.len(), 1);
        let compensation = &snapshot.balances[0];
        assert_eq!(compensation.id, "compensation");
        assert_eq!(compensation.remaining, 60.0);
        assert_eq!(compensation.unit, "tokens");
    }

    #[test]
    fn wallet_balance_parses_string_amounts() {
        let balance: ConsoleBalanceData =
            serde_json::from_value(balance_body()["data"].clone()).expect("parse balance");
        let snapshot = balance.snapshot().expect("balance snapshot");
        assert_eq!(snapshot.id, "balance");
        assert_eq!(snapshot.remaining, 0.0);
        assert_eq!(snapshot.unit, "USD");
    }

    #[test]
    fn parse_console_timestamp_accepts_console_and_rfc3339() {
        let expected = OffsetDateTime::parse("2026-10-24T23:59:59Z", &Rfc3339).expect("timestamp");
        assert_eq!(
            parse_console_timestamp("2026-10-24 23:59:59"),
            Some(expected)
        );
        assert_eq!(
            parse_console_timestamp("2026-10-24 23:59:59 UTC"),
            Some(expected)
        );
        assert_eq!(
            parse_console_timestamp("2026-10-24T23:59:59Z"),
            Some(expected)
        );
        assert_eq!(parse_console_timestamp("not a timestamp"), None);
    }

    #[tokio::test]
    async fn provider_fetches_usage_with_cookie_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/tokenPlan/usage"))
            .and(header("cookie", "session=abc; uid=1"))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(usage_body()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/tokenPlan/detail"))
            .and(header("cookie", "session=abc; uid=1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(detail_body()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/balance"))
            .and(header("cookie", "session=abc; uid=1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(balance_body()))
            .mount(&server)
            .await;

        let provider = XiaomiProvider::new(XiaomiProviderConfig {
            auth_cookie: Some("Cookie: session=abc; uid=1".to_owned()),
            console_url: Some(Url::parse(&server.uri()).expect("mock URL")),
            ..hermetic_config()
        });

        let snapshot = provider
            .refresh(RefreshContext {
                now: OffsetDateTime::from_unix_timestamp(1_780_704_000).expect("valid timestamp"),
            })
            .await
            .expect("refresh xiaomi");

        assert_eq!(snapshot.provider, ProviderId::xiaomi());
        assert_eq!(snapshot.source, ProviderSource::Web);
        let ids: Vec<&str> = snapshot
            .usage
            .windows
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(ids, ["plan", "monthly"]);
        assert_eq!(
            snapshot.usage.windows[0].resets_at.expect("resets_at"),
            OffsetDateTime::parse("2026-10-24T23:59:59Z", &Rfc3339).expect("timestamp")
        );
        assert_eq!(snapshot.usage.balances.len(), 1);
        assert_eq!(snapshot.usage.balances[0].id, "balance");
        let identity = snapshot.identity.expect("identity");
        assert_eq!(identity.plan.as_deref(), Some("Standard"));
    }

    #[tokio::test]
    async fn provider_tolerates_detail_and_balance_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/tokenPlan/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(usage_body()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/tokenPlan/detail"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/balance"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let provider = XiaomiProvider::new(XiaomiProviderConfig {
            auth_cookie: Some("session=abc".to_owned()),
            console_url: Some(Url::parse(&server.uri()).expect("mock URL")),
            ..hermetic_config()
        });

        let snapshot = provider
            .refresh(RefreshContext::default())
            .await
            .expect("refresh xiaomi");
        assert!(snapshot.usage.windows[0].resets_at.is_none());
        assert!(snapshot.usage.balances.is_empty());
        assert!(snapshot.identity.is_none());
    }

    #[tokio::test]
    async fn provider_maps_login_url_401_to_authentication() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/tokenPlan/usage"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "code": 401,
                "loginUrl": "https://account.xiaomi.com/pass/serviceLogin"
            })))
            .mount(&server)
            .await;

        let provider = XiaomiProvider::new(XiaomiProviderConfig {
            auth_cookie: Some("session=stale".to_owned()),
            console_url: Some(Url::parse(&server.uri()).expect("mock URL")),
            ..hermetic_config()
        });

        let error = provider
            .refresh(RefreshContext::default())
            .await
            .expect_err("stale cookie should fail");
        assert!(matches!(error, ProviderError::Authentication(_)));
        assert!(error.to_string().contains("braindrain auth login xiaomi"));
    }

    #[tokio::test]
    async fn provider_reports_auth_hint_without_cookie_but_with_plan_key() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mimocode_path = tempdir.path().join("auth.json");
        std::fs::write(
            &mimocode_path,
            serde_json::json!({
                "xiaomi": {
                    "type": "api",
                    "key": "mimocode-key",
                    "metadata": { "uid": "6623873869" }
                }
            })
            .to_string(),
        )
        .expect("write auth");

        let provider = XiaomiProvider::new(XiaomiProviderConfig {
            mimocode_auth_path: Some(mimocode_path),
            ..hermetic_config()
        });

        let error = provider
            .refresh(RefreshContext::default())
            .await
            .expect_err("missing cookie should fail");
        assert!(matches!(error, ProviderError::Authentication(_)));
        assert!(error.to_string().contains("braindrain auth login xiaomi"));
    }

    #[tokio::test]
    async fn provider_reports_not_configured_without_credentials() {
        let provider = XiaomiProvider::new(hermetic_config());
        let error = provider
            .refresh(RefreshContext::default())
            .await
            .expect_err("missing credentials should fail");
        assert!(matches!(error, ProviderError::NotConfigured(_)));
    }

    #[tokio::test]
    async fn provider_maps_nonzero_envelope_code_to_network_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/tokenPlan/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 402,
                "message": "quota purchase required",
                "data": null
            })))
            .mount(&server)
            .await;

        let provider = XiaomiProvider::new(XiaomiProviderConfig {
            auth_cookie: Some("session=abc".to_owned()),
            console_url: Some(Url::parse(&server.uri()).expect("mock URL")),
            ..hermetic_config()
        });

        let error = provider
            .refresh(RefreshContext::default())
            .await
            .expect_err("failure envelope should fail");
        assert!(matches!(error, ProviderError::Network(_)));
        assert!(error.to_string().contains("quota purchase required"));
    }
}
