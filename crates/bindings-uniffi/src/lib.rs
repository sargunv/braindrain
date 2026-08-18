use std::sync::LazyLock;

use braindrain_core::{
    AccountIdentity, BalanceSnapshot, ProviderSnapshot, ProviderSource, RateWindow,
    ResetCreditSnapshot, UsageSnapshot,
};
use braindrain_service::{self as service, ServiceError};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::runtime::Runtime;

uniffi::setup_scaffolding!();

static TOKIO: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("braindrain")
        .build()
        .expect("create Tokio runtime")
});

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FfiProviderSnapshot {
    pub provider: String,
    pub source: String,
    pub usage: FfiUsageSnapshot,
    pub identity: Option<FfiAccountIdentity>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FfiUsageSnapshot {
    pub windows: Vec<FfiRateWindow>,
    pub balances: Vec<FfiBalanceSnapshot>,
    pub reset_credits: Vec<FfiResetCreditSnapshot>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FfiRateWindow {
    pub id: String,
    pub label: String,
    pub used_percent: f64,
    pub duration_seconds: Option<u64>,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FfiBalanceSnapshot {
    pub id: String,
    pub label: String,
    pub remaining: f64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FfiResetCreditSnapshot {
    pub id: String,
    pub granted_at: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiAccountIdentity {
    pub email: Option<String>,
    pub plan: Option<String>,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("unsupported provider: {provider}")]
    UnsupportedProvider { provider: String },
    #[error("service error: {message}")]
    Service { message: String },
    #[error("runtime error: {message}")]
    Runtime { message: String },
    #[error("time format error: {message}")]
    TimeFormat { message: String },
}

#[uniffi::export]
pub fn provider_ids() -> Vec<String> {
    service::provider_ids()
        .into_iter()
        .map(|provider| provider.as_str().to_owned())
        .collect()
}

#[uniffi::export]
pub async fn check_provider(provider: String) -> Result<FfiProviderSnapshot, FfiError> {
    let task = TOKIO.spawn(async move {
        let snapshot = service::check_provider(&provider)
            .await
            .map_err(FfiError::from)?;
        FfiProviderSnapshot::try_from(snapshot)
    });

    task.await.map_err(|error| FfiError::Runtime {
        message: error.to_string(),
    })?
}

impl TryFrom<ProviderSnapshot> for FfiProviderSnapshot {
    type Error = FfiError;

    fn try_from(snapshot: ProviderSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            provider: snapshot.provider.as_str().to_owned(),
            source: provider_source(snapshot.source).to_owned(),
            usage: snapshot.usage.try_into()?,
            identity: snapshot.identity.map(Into::into),
            updated_at: format_time(snapshot.updated_at)?,
        })
    }
}

impl TryFrom<UsageSnapshot> for FfiUsageSnapshot {
    type Error = FfiError;

    fn try_from(usage: UsageSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            windows: usage
                .windows
                .into_iter()
                .map(FfiRateWindow::try_from)
                .collect::<Result<_, _>>()?,
            balances: usage.balances.into_iter().map(Into::into).collect(),
            reset_credits: usage
                .reset_credits
                .into_iter()
                .map(FfiResetCreditSnapshot::try_from)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<RateWindow> for FfiRateWindow {
    type Error = FfiError;

    fn try_from(window: RateWindow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: window.id,
            label: window.label,
            used_percent: window.used_percent,
            duration_seconds: window.duration.map(|duration| duration.as_secs()),
            resets_at: window.resets_at.map(format_time).transpose()?,
        })
    }
}

impl From<BalanceSnapshot> for FfiBalanceSnapshot {
    fn from(balance: BalanceSnapshot) -> Self {
        Self {
            id: balance.id,
            label: balance.label,
            remaining: balance.remaining,
            unit: balance.unit,
        }
    }
}

impl TryFrom<ResetCreditSnapshot> for FfiResetCreditSnapshot {
    type Error = FfiError;

    fn try_from(credit: ResetCreditSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            id: credit.id,
            granted_at: credit.granted_at.map(format_time).transpose()?,
            expires_at: credit.expires_at.map(format_time).transpose()?,
        })
    }
}

impl From<AccountIdentity> for FfiAccountIdentity {
    fn from(identity: AccountIdentity) -> Self {
        Self {
            email: identity.email,
            plan: identity.plan,
        }
    }
}

impl From<ServiceError> for FfiError {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::UnsupportedProvider { provider } => {
                Self::UnsupportedProvider { provider }
            }
            error => Self::Service {
                message: error.to_string(),
            },
        }
    }
}

fn provider_source(source: ProviderSource) -> &'static str {
    match source {
        ProviderSource::OAuth => "oauth",
        ProviderSource::Cli => "cli",
        ProviderSource::Web => "web",
        ProviderSource::Local => "local",
        ProviderSource::Manual => "manual",
    }
}

fn format_time(time: OffsetDateTime) -> Result<String, FfiError> {
    time.format(&Rfc3339).map_err(|error| FfiError::TimeFormat {
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use braindrain_core::ProviderId;

    use super::*;

    #[test]
    fn ffi_snapshot_preserves_window_timing() {
        let updated_at =
            OffsetDateTime::from_unix_timestamp(1_780_704_000).expect("valid updated timestamp");
        let resets_at =
            OffsetDateTime::from_unix_timestamp(1_781_980_358).expect("valid reset timestamp");
        let snapshot = ProviderSnapshot {
            provider: ProviderId::openai(),
            source: ProviderSource::OAuth,
            usage: UsageSnapshot {
                windows: vec![RateWindow {
                    id: "5h".to_owned(),
                    label: "5-hour".to_owned(),
                    used_percent: 42.0,
                    duration: Some(Duration::from_secs(5 * 60 * 60)),
                    resets_at: Some(resets_at),
                }],
                balances: Vec::new(),
                reset_credits: vec![ResetCreditSnapshot {
                    id: "reset-credit-1".to_owned(),
                    granted_at: Some(updated_at),
                    expires_at: Some(resets_at),
                }],
            },
            identity: Some(AccountIdentity {
                email: Some("person@example.com".to_owned()),
                plan: Some("plus".to_owned()),
            }),
            updated_at,
        };

        let ffi_snapshot = FfiProviderSnapshot::try_from(snapshot).expect("convert snapshot");

        assert_eq!(ffi_snapshot.provider, "openai");
        assert_eq!(ffi_snapshot.source, "oauth");
        assert_eq!(ffi_snapshot.updated_at, "2026-06-06T00:00:00Z");
        assert_eq!(ffi_snapshot.usage.windows[0].duration_seconds, Some(18_000));
        assert_eq!(
            ffi_snapshot.usage.windows[0].resets_at,
            Some("2026-06-20T18:32:38Z".to_owned())
        );
        assert_eq!(
            ffi_snapshot.identity.and_then(|identity| identity.email),
            Some("person@example.com".to_owned())
        );
        assert_eq!(
            ffi_snapshot.usage.reset_credits[0].expires_at,
            Some("2026-06-20T18:32:38Z".to_owned())
        );
    }
}
