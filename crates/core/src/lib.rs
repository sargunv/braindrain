use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    pub const OPENAI: &'static str = "openai";
    pub const CURSOR: &'static str = "cursor";
    pub const ZAI: &'static str = "zai";
    pub const OPENCODE_GO: &'static str = "opencode-go";

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn openai() -> Self {
        Self::new(Self::OPENAI)
    }

    pub fn cursor() -> Self {
        Self::new(Self::CURSOR)
    }

    pub fn zai() -> Self {
        Self::new(Self::ZAI)
    }

    pub fn opencode_go() -> Self {
        Self::new(Self::OPENCODE_GO)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderSource {
    OAuth,
    Cli,
    Web,
    Local,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub provider: ProviderId,
    pub source: ProviderSource,
    pub usage: UsageSnapshot,
    pub identity: Option<AccountIdentity>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub windows: Vec<RateWindow>,
    pub balances: Vec<BalanceSnapshot>,
    pub reset_credits: Vec<ResetCreditSnapshot>,
}

impl UsageSnapshot {
    pub fn empty() -> Self {
        Self {
            windows: Vec::new(),
            balances: Vec::new(),
            reset_credits: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateWindow {
    pub id: String,
    pub label: String,
    pub used_percent: f64,
    #[serde(default, rename = "duration_seconds", with = "duration_seconds_option")]
    pub duration: Option<Duration>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    pub id: String,
    pub label: String,
    pub remaining: f64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetCreditSnapshot {
    pub id: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub granted_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountIdentity {
    pub email: Option<String>,
    pub plan: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredentialField {
    pub id: String,
    pub label: String,
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredentialSchema {
    pub provider: ProviderId,
    pub fields: Vec<ProviderCredentialField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredentials {
    pub provider: ProviderId,
    pub values: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct RefreshContext {
    pub now: OffsetDateTime,
}

impl Default for RefreshContext {
    fn default() -> Self {
        Self {
            now: OffsetDateTime::now_utc(),
        }
    }
}

pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn refresh<'a>(
        &'a self,
        context: RefreshContext,
    ) -> ProviderFuture<'a, Result<ProviderSnapshot, ProviderError>>;
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider is not configured: {0}")]
    NotConfigured(String),
    #[error("authentication failed: {0}")]
    Authentication(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Option<Self> {
        let dirs = ProjectDirs::from("dev", "sargunv", "braindrain")?;
        Some(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            cache_dir: dirs.cache_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
        })
    }
}

mod duration_seconds_option {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        duration
            .map(|duration| duration.as_secs())
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<u64>::deserialize(deserializer).map(|duration| duration.map(Duration::from_secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_window_serializes_duration_as_seconds() {
        let window = RateWindow {
            id: "5h".to_owned(),
            label: "5-hour".to_owned(),
            used_percent: 42.0,
            duration: Some(Duration::from_secs(5 * 60 * 60)),
            resets_at: None,
        };

        let json = serde_json::to_value(window).expect("serialize window");
        assert_eq!(json["duration_seconds"], 18_000);
    }

    #[test]
    fn snapshot_timestamps_serialize_as_rfc3339() {
        let updated_at =
            OffsetDateTime::from_unix_timestamp(1_780_704_000).expect("valid updated timestamp");
        let resets_at =
            OffsetDateTime::from_unix_timestamp(1_781_980_358).expect("valid reset timestamp");
        let snapshot = ProviderSnapshot {
            provider: ProviderId::cursor(),
            source: ProviderSource::Cli,
            usage: UsageSnapshot {
                windows: vec![RateWindow {
                    id: "total".to_owned(),
                    label: "Total".to_owned(),
                    used_percent: 45.0,
                    duration: None,
                    resets_at: Some(resets_at),
                }],
                balances: Vec::new(),
                reset_credits: Vec::new(),
            },
            identity: None,
            updated_at,
        };

        let json = serde_json::to_value(snapshot).expect("serialize snapshot");

        assert_eq!(json["updated_at"], "2026-06-06T00:00:00Z");
        assert_eq!(
            json["usage"]["windows"][0]["resets_at"],
            "2026-06-20T18:32:38Z"
        );
    }
}
