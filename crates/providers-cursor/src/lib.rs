use std::env;

use braindrain_core::{
    AccountIdentity, Provider, ProviderError, ProviderFuture, ProviderId, ProviderSnapshot,
    ProviderSource, RateWindow, RefreshContext, UsageSnapshot,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize, de::Error as _};
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

pub const CURSOR_API_BASE_URL: &str = "https://api2.cursor.sh";
pub const CURSOR_CURRENT_PERIOD_USAGE_METHOD: &str =
    "aiserver.v1.DashboardService/GetCurrentPeriodUsage";
pub const CURSOR_PLAN_INFO_METHOD: &str = "aiserver.v1.DashboardService/GetPlanInfo";
pub const CURSOR_GET_ME_METHOD: &str = "aiserver.v1.DashboardService/GetMe";
pub const CURSOR_AUTH_TOKEN_ENV: &str = "CURSOR_AUTH_TOKEN";
pub const CURSOR_KEYCHAIN_SERVICE: &str = "cursor-access-token";
pub const CURSOR_KEYCHAIN_ACCOUNT: &str = "cursor-user";

#[derive(Debug, Clone)]
pub struct CursorProvider {
    config: CursorProviderConfig,
    client: reqwest::Client,
}

impl CursorProvider {
    pub fn new(config: CursorProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    pub fn config(&self) -> &CursorProviderConfig {
        &self.config
    }

    pub fn auth_token(&self) -> Result<CursorAccessToken, CursorProviderError> {
        self.config.auth_token()
    }

    async fn auth_token_for_refresh(&self) -> Result<CursorAccessToken, CursorProviderError> {
        self.config.auth_token_async().await
    }

    async fn fetch_current_period_usage(
        &self,
        token: &str,
    ) -> Result<CursorCurrentPeriodUsageResponse, CursorProviderError> {
        self.rpc(CURSOR_CURRENT_PERIOD_USAGE_METHOD, token).await
    }

    async fn fetch_plan_info(
        &self,
        token: &str,
    ) -> Result<CursorPlanInfoResponse, CursorProviderError> {
        self.rpc(CURSOR_PLAN_INFO_METHOD, token).await
    }

    async fn fetch_me(&self, token: &str) -> Result<CursorMeResponse, CursorProviderError> {
        self.rpc(CURSOR_GET_ME_METHOD, token).await
    }

    async fn rpc<T>(&self, method: &str, token: &str) -> Result<T, CursorProviderError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = self.config.method_url(method)?;
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("x-cursor-client-type", HeaderValue::from_static("cli"));
        headers.insert(
            "x-cursor-client-version",
            HeaderValue::from_static("cli-braindrain"),
        );
        headers.insert("x-ghost-mode", HeaderValue::from_static("true"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                CursorProviderError::InvalidHeader(AUTHORIZATION.as_str().to_owned())
            })?,
        );

        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(&EmptyRequest {})
            .send()
            .await
            .map_err(CursorProviderError::Http)?;
        let status = response.status();
        let body = response.bytes().await.map_err(CursorProviderError::Http)?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(CursorProviderError::Unauthorized {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }
        if !status.is_success() {
            return Err(CursorProviderError::ApiStatus {
                status: status.as_u16(),
                body: body_preview(&body),
            });
        }

        serde_json::from_slice(&body).map_err(CursorProviderError::Decode)
    }
}

impl Default for CursorProvider {
    fn default() -> Self {
        Self::new(CursorProviderConfig::default())
    }
}

impl Provider for CursorProvider {
    fn id(&self) -> ProviderId {
        ProviderId::cursor()
    }

    fn refresh<'a>(
        &'a self,
        context: RefreshContext,
    ) -> ProviderFuture<'a, Result<ProviderSnapshot, ProviderError>> {
        Box::pin(async move {
            let token = self
                .auth_token_for_refresh()
                .await
                .map_err(ProviderError::from)?;
            let (usage, plan_info, me) = tokio::try_join!(
                self.fetch_current_period_usage(&token.value),
                self.fetch_plan_info(&token.value),
                self.fetch_me(&token.value)
            )
            .map_err(ProviderError::from)?;

            Ok(ProviderSnapshot {
                provider: ProviderId::cursor(),
                source: ProviderSource::Cli,
                usage: usage.usage_snapshot(),
                identity: cursor_identity(&plan_info, &me),
                updated_at: context.now,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorAccessToken {
    pub value: String,
    pub source: CursorAccessTokenSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorAccessTokenSource {
    Environment(&'static str),
    Keyring,
    Config,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CursorProviderConfig {
    pub auth_token: Option<String>,
    pub api_base_url: Url,
}

impl CursorProviderConfig {
    pub fn method_url(&self, method: &str) -> Result<Url, CursorProviderError> {
        self.api_base_url
            .join(method)
            .map_err(CursorProviderError::Url)
    }

    pub fn auth_token(&self) -> Result<CursorAccessToken, CursorProviderError> {
        if let Some(token) = self.config_or_env_auth_token() {
            return Ok(token);
        }

        if let Some(token) = keyring_access_token_blocking()? {
            return Ok(CursorAccessToken {
                value: token,
                source: CursorAccessTokenSource::Keyring,
            });
        }

        Err(CursorProviderError::MissingAccessToken)
    }

    async fn auth_token_async(&self) -> Result<CursorAccessToken, CursorProviderError> {
        if let Some(token) = self.config_or_env_auth_token() {
            return Ok(token);
        }

        if let Some(token) = keyring_access_token_async().await? {
            return Ok(CursorAccessToken {
                value: token,
                source: CursorAccessTokenSource::Keyring,
            });
        }

        Err(CursorProviderError::MissingAccessToken)
    }

    fn config_or_env_auth_token(&self) -> Option<CursorAccessToken> {
        if let Some(token) = self.auth_token.as_deref().filter(|token| !token.is_empty()) {
            return Some(CursorAccessToken {
                value: token.to_owned(),
                source: CursorAccessTokenSource::Config,
            });
        }

        if let Some(token) = env::var_os(CURSOR_AUTH_TOKEN_ENV)
            .and_then(|token| token.into_string().ok())
            .filter(|token| !token.is_empty())
        {
            return Some(CursorAccessToken {
                value: token,
                source: CursorAccessTokenSource::Environment(CURSOR_AUTH_TOKEN_ENV),
            });
        }

        None
    }
}

impl Default for CursorProviderConfig {
    fn default() -> Self {
        Self {
            auth_token: None,
            api_base_url: Url::parse(CURSOR_API_BASE_URL).expect("valid Cursor API base URL"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorCurrentPeriodUsageResponse {
    #[serde(default, deserialize_with = "deserialize_epoch_millis_option")]
    pub billing_cycle_start: Option<OffsetDateTime>,
    #[serde(default, deserialize_with = "deserialize_epoch_millis_option")]
    pub billing_cycle_end: Option<OffsetDateTime>,
    #[serde(default)]
    pub plan_usage: Option<CursorPlanUsage>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl CursorCurrentPeriodUsageResponse {
    pub fn usage_snapshot(&self) -> UsageSnapshot {
        let Some(plan_usage) = &self.plan_usage else {
            return UsageSnapshot::empty();
        };

        let mut windows = Vec::new();
        if let Some(total_percent) = plan_usage.total_percent_used {
            windows.push(RateWindow {
                id: "total".to_owned(),
                label: "Total".to_owned(),
                used_percent: total_percent,
                duration: None,
                resets_at: self.billing_cycle_end,
            });
        }
        if let Some(auto_percent) = plan_usage.auto_percent_used {
            windows.push(RateWindow {
                id: "auto".to_owned(),
                label: "Auto + Composer".to_owned(),
                used_percent: auto_percent,
                duration: None,
                resets_at: self.billing_cycle_end,
            });
        }
        if let Some(api_percent) = plan_usage.api_percent_used {
            windows.push(RateWindow {
                id: "api".to_owned(),
                label: "API".to_owned(),
                used_percent: api_percent,
                duration: None,
                resets_at: self.billing_cycle_end,
            });
        }

        UsageSnapshot {
            windows,
            balances: Vec::new(),
            reset_credits: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorPlanUsage {
    #[serde(default)]
    pub total_spend: Option<i64>,
    #[serde(default)]
    pub included_spend: Option<i64>,
    #[serde(default)]
    pub bonus_spend: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub auto_percent_used: Option<f64>,
    #[serde(default)]
    pub api_percent_used: Option<f64>,
    #[serde(default)]
    pub total_percent_used: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorPlanInfoResponse {
    #[serde(default)]
    pub plan_info: Option<CursorPlanInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorPlanInfo {
    #[serde(default)]
    pub plan_name: Option<String>,
    #[serde(default)]
    pub included_amount_cents: Option<i64>,
    #[serde(default)]
    pub price: Option<String>,
    #[serde(default, deserialize_with = "deserialize_epoch_millis_option")]
    pub billing_cycle_end: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorMeResponse {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

fn cursor_identity(
    plan_info: &CursorPlanInfoResponse,
    me: &CursorMeResponse,
) -> Option<AccountIdentity> {
    let email = me.email.clone();
    let plan = plan_info
        .plan_info
        .as_ref()
        .and_then(|plan_info| plan_info.plan_name.clone());

    if email.is_none() && plan.is_none() {
        return None;
    }

    Some(AccountIdentity { email, plan })
}

#[derive(Debug, Error)]
pub enum CursorProviderError {
    #[error("Cursor auth token is not configured")]
    MissingAccessToken,
    #[error("could not read Cursor access token from system keyring: {0}")]
    Keychain(String),
    #[error("could not build Cursor API URL: {0}")]
    Url(url::ParseError),
    #[error("Cursor request failed: {0}")]
    Http(reqwest::Error),
    #[error("Cursor response could not be decoded: {0}")]
    Decode(serde_json::Error),
    #[error("Cursor rejected request with HTTP {status}: {body}")]
    Unauthorized { status: u16, body: String },
    #[error("Cursor request returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },
    #[error("could not construct HTTP header {0}")]
    InvalidHeader(String),
}

impl From<CursorProviderError> for ProviderError {
    fn from(error: CursorProviderError) -> Self {
        match error {
            CursorProviderError::MissingAccessToken => {
                ProviderError::NotConfigured(error.to_string())
            }
            CursorProviderError::Unauthorized { .. } => {
                ProviderError::Authentication(error.to_string())
            }
            CursorProviderError::Decode(_) => ProviderError::Parse(error.to_string()),
            CursorProviderError::Keychain(_)
            | CursorProviderError::Url(_)
            | CursorProviderError::Http(_)
            | CursorProviderError::ApiStatus { .. }
            | CursorProviderError::InvalidHeader(_) => ProviderError::Network(error.to_string()),
        }
    }
}

#[derive(Debug, Serialize)]
struct EmptyRequest {}

async fn keyring_access_token_async() -> Result<Option<String>, CursorProviderError> {
    tokio::task::spawn_blocking(keyring_access_token_blocking)
        .await
        .map_err(|error| CursorProviderError::Keychain(error.to_string()))?
}

fn keyring_access_token_blocking() -> Result<Option<String>, CursorProviderError> {
    let entry = keyring::Entry::new(CURSOR_KEYCHAIN_SERVICE, CURSOR_KEYCHAIN_ACCOUNT)
        .map_err(|error| CursorProviderError::Keychain(error.to_string()))?;

    match entry.get_password() {
        Ok(token) => Ok((!token.is_empty()).then_some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(CursorProviderError::Keychain(error.to_string())),
    }
}

fn deserialize_epoch_millis_option<'de, D>(
    deserializer: D,
) -> Result<Option<OffsetDateTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<EpochMillis>::deserialize(deserializer)?;
    value
        .map(|value| value.to_offset_date_time().map_err(D::Error::custom))
        .transpose()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum EpochMillis {
    String(String),
    Integer(i128),
}

impl EpochMillis {
    fn to_offset_date_time(&self) -> Result<OffsetDateTime, time::error::ComponentRange> {
        let millis = match self {
            Self::String(value) => value.parse::<i128>().unwrap_or_default(),
            Self::Integer(value) => *value,
        };
        let seconds = millis.div_euclid(1_000);
        let nanos = (millis.rem_euclid(1_000) * 1_000_000) as i64;
        OffsetDateTime::from_unix_timestamp(seconds as i64)?.replace_nanosecond(nanos as u32)
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
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn usage_snapshot_uses_cursor_percent_fields_with_reset() {
        let usage: CursorCurrentPeriodUsageResponse = serde_json::from_value(serde_json::json!({
            "billingCycleEnd": "1781980358000",
            "planUsage": {
                "totalPercentUsed": 45.08148148148148,
                "autoPercentUsed": 31.395555555555553,
                "apiPercentUsed": 100
            }
        }))
        .expect("decode usage");

        let snapshot = usage.usage_snapshot();

        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(snapshot.windows[0].id, "total");
        assert_eq!(snapshot.windows[0].used_percent, 45.08148148148148);
        assert_eq!(snapshot.windows[0].resets_at, usage.billing_cycle_end);
        assert_eq!(snapshot.windows[1].id, "auto");
        assert_eq!(snapshot.windows[2].id, "api");
    }

    #[tokio::test]
    async fn provider_fetches_usage_and_plan_with_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/{CURSOR_CURRENT_PERIOD_USAGE_METHOD}")))
            .and(header("authorization", "Bearer test-token"))
            .and(header("x-cursor-client-type", "cli"))
            .and(body_json(serde_json::json!({})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "billingCycleEnd": "1781980358000",
                "planUsage": {
                    "totalPercentUsed": 42,
                    "autoPercentUsed": 20,
                    "apiPercentUsed": 64
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/{CURSOR_PLAN_INFO_METHOD}")))
            .and(header("authorization", "Bearer test-token"))
            .and(body_json(serde_json::json!({})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "planInfo": {
                    "planName": "Pro",
                    "includedAmountCents": 2000,
                    "billingCycleEnd": "1781980358000"
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/{CURSOR_GET_ME_METHOD}")))
            .and(header("authorization", "Bearer test-token"))
            .and(body_json(serde_json::json!({})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "email": "sargun@example.com",
                "name": "Sargun"
            })))
            .mount(&server)
            .await;

        let provider = CursorProvider::new(CursorProviderConfig {
            auth_token: Some("test-token".to_owned()),
            api_base_url: Url::parse(&server.uri()).expect("mock URL"),
        });
        let snapshot = provider
            .refresh(RefreshContext {
                now: OffsetDateTime::from_unix_timestamp(1_780_704_000).expect("valid timestamp"),
            })
            .await
            .expect("refresh Cursor");

        assert_eq!(snapshot.provider, ProviderId::cursor());
        assert_eq!(snapshot.usage.windows.len(), 3);
        assert_eq!(snapshot.usage.windows[0].used_percent, 42.0);
        let identity = snapshot.identity.expect("Cursor identity");
        assert_eq!(identity.email, Some("sargun@example.com".to_owned()));
        assert_eq!(identity.plan, Some("Pro".to_owned()));
    }
}
