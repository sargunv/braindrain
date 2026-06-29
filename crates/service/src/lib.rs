use braindrain_core::{Provider, ProviderError, ProviderId, ProviderSnapshot, RefreshContext};
use braindrain_providers_cursor::{CURSOR_AUTH_TOKEN_ENV, CursorAccessTokenSource, CursorProvider};
use braindrain_providers_cursor::{CURSOR_KEYCHAIN_ACCOUNT, CURSOR_KEYCHAIN_SERVICE};
use braindrain_providers_openai::OpenAiProvider;
use braindrain_providers_zai::{ZAI_API_KEY_ENV, ZaiApiKeySource, ZaiProvider};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("unsupported provider: {provider}")]
    UnsupportedProvider { provider: String },
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub provider: ProviderId,
    pub fields: Vec<ProviderInfoField>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfoField {
    pub key: String,
    pub value: String,
}

pub fn provider_ids() -> Vec<ProviderId> {
    vec![
        ProviderId::openai(),
        ProviderId::cursor(),
        ProviderId::zai(),
    ]
}

pub fn normalize_provider_id(provider: &str) -> ProviderId {
    match provider {
        "codex" => ProviderId::openai(),
        "z.ai" => ProviderId::zai(),
        provider => ProviderId::new(provider),
    }
}

pub fn info_provider(provider: &str) -> Result<ProviderInfo, ServiceError> {
    match normalize_provider_id(provider).as_str() {
        ProviderId::OPENAI => Ok(info_openai()),
        ProviderId::CURSOR => Ok(info_cursor()),
        ProviderId::ZAI => Ok(info_zai()),
        provider => Err(ServiceError::UnsupportedProvider {
            provider: provider.to_owned(),
        }),
    }
}

pub async fn check_provider(provider: &str) -> Result<ProviderSnapshot, ServiceError> {
    let context = RefreshContext::default();
    match normalize_provider_id(provider).as_str() {
        ProviderId::OPENAI => OpenAiProvider::default()
            .refresh(context)
            .await
            .map_err(ServiceError::from),
        ProviderId::CURSOR => CursorProvider::default()
            .refresh(context)
            .await
            .map_err(ServiceError::from),
        ProviderId::ZAI => ZaiProvider::default()
            .refresh(context)
            .await
            .map_err(ServiceError::from),
        provider => Err(ServiceError::UnsupportedProvider {
            provider: provider.to_owned(),
        }),
    }
}

fn info_openai() -> ProviderInfo {
    let provider = OpenAiProvider::default();
    let mut info = ProviderInfo::new(ProviderId::openai());
    info.push("auth_path", provider.auth_path().display().to_string());

    match provider.load_auth() {
        Ok(auth) => {
            let identity = auth.identity();
            info.push("auth_found", "true");
            info.push(
                "has_access_token",
                (!auth.tokens.access_token.is_empty()).to_string(),
            );
            info.push(
                "has_refresh_token",
                (!auth.tokens.refresh_token.is_empty()).to_string(),
            );
            info.push(
                "account_id",
                auth.tokens
                    .account_id
                    .unwrap_or_else(|| "<unknown>".to_owned()),
            );
            info.push(
                "email",
                identity
                    .as_ref()
                    .and_then(|identity| identity.email.clone())
                    .unwrap_or_else(|| "<unknown>".to_owned()),
            );
            info.push(
                "plan",
                identity
                    .and_then(|identity| identity.plan)
                    .unwrap_or_else(|| "<unknown>".to_owned()),
            );
        }
        Err(error) => {
            info.push("auth_found", "false");
            info.push("auth_error", error.to_string());
        }
    }

    info
}

fn info_cursor() -> ProviderInfo {
    let provider = CursorProvider::default();
    let mut info = ProviderInfo::new(ProviderId::cursor());
    info.push("api_base_url", provider.config().api_base_url.to_string());
    info.push("env_token", CURSOR_AUTH_TOKEN_ENV);
    info.push("keyring_service", CURSOR_KEYCHAIN_SERVICE);
    info.push("keyring_account", CURSOR_KEYCHAIN_ACCOUNT);

    match provider.auth_token() {
        Ok(token) => {
            info.push("auth_found", "true");
            info.push(
                "auth_source",
                match token.source {
                    CursorAccessTokenSource::Environment(name) => name,
                    CursorAccessTokenSource::Keyring => "keyring",
                    CursorAccessTokenSource::Config => "config",
                },
            );
        }
        Err(error) => {
            info.push("auth_found", "false");
            info.push("auth_error", error.to_string());
        }
    }

    info
}

fn info_zai() -> ProviderInfo {
    let provider = ZaiProvider::default();
    let mut info = ProviderInfo::new(ProviderId::zai());
    info.push(
        "quota_url",
        provider.config().resolve_quota_url().to_string(),
    );
    info.push("env_api_key", ZAI_API_KEY_ENV);
    if let Some(path) = provider.config().opencode_auth_path() {
        info.push("opencode_auth_path", path.display().to_string());
    }

    match provider.api_key() {
        Ok(key) => {
            info.push("auth_found", "true");
            info.push(
                "auth_source",
                match key.source {
                    ZaiApiKeySource::Environment(name) => name,
                    ZaiApiKeySource::Opencode => "opencode",
                    ZaiApiKeySource::Config => "config",
                },
            );
        }
        Err(error) => {
            info.push("auth_found", "false");
            info.push("auth_error", error.to_string());
        }
    }

    info
}

impl ProviderInfo {
    fn new(provider: ProviderId) -> Self {
        Self {
            provider,
            fields: Vec::new(),
        }
    }

    fn push(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.fields.push(ProviderInfoField {
            key: key.into(),
            value: value.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_provider_aliases() {
        assert_eq!(normalize_provider_id("codex").as_str(), ProviderId::OPENAI);
        assert_eq!(normalize_provider_id("cursor").as_str(), ProviderId::CURSOR);
        assert_eq!(normalize_provider_id("z.ai").as_str(), ProviderId::ZAI);
        assert_eq!(normalize_provider_id("zai").as_str(), ProviderId::ZAI);
    }

    #[test]
    fn provider_ids_are_canonical() {
        assert_eq!(
            provider_ids(),
            vec![
                ProviderId::openai(),
                ProviderId::cursor(),
                ProviderId::zai()
            ]
        );
    }
}
