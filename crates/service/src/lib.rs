use braindrain_core::{
    Provider, ProviderCredentialSchema, ProviderCredentials, ProviderError, ProviderId,
    ProviderSnapshot, RefreshContext,
};
use braindrain_providers_claude::{
    CLAUDE_CODE_OAUTH_TOKEN_ENV, CLAUDE_CONFIG_DIR_ENV, CLAUDE_KEYCHAIN_SERVICE,
    ClaudeAccessTokenSource, ClaudeProvider,
};
use braindrain_providers_cursor::{CURSOR_AUTH_TOKEN_ENV, CursorAccessTokenSource, CursorProvider};
use braindrain_providers_cursor::{CURSOR_KEYCHAIN_ACCOUNT, CURSOR_KEYCHAIN_SERVICE};
use braindrain_providers_google::{
    GOOGLE_AI_ACCESS_TOKEN_ENV, GOOGLE_KEYCHAIN_ACCOUNT, GOOGLE_KEYCHAIN_SERVICE,
    GoogleAccessTokenSource, GoogleProvider,
};
use braindrain_providers_kimi::{
    KIMI_API_KEY_ENV, KIMI_CODE_BASE_URL_ENV, KIMI_CODE_HOME_ENV, KIMI_SHARE_DIR_ENV,
    KimiAccessTokenSource, KimiProvider,
};
use braindrain_providers_openai::OpenAiProvider;
use braindrain_providers_opencode_go::{
    OPENCODE_AUTH_COOKIE_ENV, OPENCODE_KEYCHAIN_ACCOUNT, OPENCODE_KEYCHAIN_SERVICE,
    OPENCODE_WORKSPACE_ID_ENV, OpenCodeGoCredentialsSource, OpenCodeGoProvider,
};
use braindrain_providers_zai::{ZAI_API_KEY_ENV, ZaiApiKeySource, ZaiProvider};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("unsupported provider: {provider}")]
    UnsupportedProvider { provider: String },
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("credential error: {0}")]
    Credential(String),
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
        ProviderId::claude(),
        ProviderId::cursor(),
        ProviderId::kimi(),
        ProviderId::zai(),
        ProviderId::opencode_go(),
        ProviderId::google(),
    ]
}

pub fn normalize_provider_id(provider: &str) -> ProviderId {
    match provider {
        "codex" => ProviderId::openai(),
        "claude-code" | "anthropic" => ProviderId::claude(),
        "kimi-code" | "kimi-coding-plan" => ProviderId::kimi(),
        "z.ai" => ProviderId::zai(),
        "opencode" | "zen-go" | "opencode-zen" => ProviderId::opencode_go(),
        "google-ai" | "gemini" | "antigravity" | "agy" => ProviderId::google(),
        provider => ProviderId::new(provider),
    }
}

pub async fn info_provider(provider: &str) -> Result<ProviderInfo, ServiceError> {
    match normalize_provider_id(provider).as_str() {
        ProviderId::OPENAI => Ok(info_openai()),
        ProviderId::CLAUDE => Ok(info_claude().await),
        ProviderId::CURSOR => Ok(info_cursor()),
        ProviderId::KIMI => Ok(info_kimi()),
        ProviderId::ZAI => Ok(info_zai()),
        ProviderId::OPENCODE_GO => Ok(info_opencode_go().await),
        ProviderId::GOOGLE => Ok(info_google().await),
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
        ProviderId::CLAUDE => ClaudeProvider::default()
            .refresh(context)
            .await
            .map_err(ServiceError::from),
        ProviderId::CURSOR => CursorProvider::default()
            .refresh(context)
            .await
            .map_err(ServiceError::from),
        ProviderId::KIMI => KimiProvider::default()
            .refresh(context)
            .await
            .map_err(ServiceError::from),
        ProviderId::ZAI => ZaiProvider::default()
            .refresh(context)
            .await
            .map_err(ServiceError::from),
        ProviderId::OPENCODE_GO => OpenCodeGoProvider::default()
            .refresh(context)
            .await
            .map_err(ServiceError::from),
        ProviderId::GOOGLE => GoogleProvider::default()
            .refresh(context)
            .await
            .map_err(ServiceError::from),
        provider => Err(ServiceError::UnsupportedProvider {
            provider: provider.to_owned(),
        }),
    }
}

pub fn credential_schema(provider: &str) -> Option<ProviderCredentialSchema> {
    match normalize_provider_id(provider).as_str() {
        ProviderId::OPENCODE_GO => Some(OpenCodeGoProvider::credential_schema()),
        _ => None,
    }
}

pub async fn store_credentials(credentials: ProviderCredentials) -> Result<(), ServiceError> {
    match credentials.provider.as_str() {
        ProviderId::OPENCODE_GO => OpenCodeGoProvider::store_credentials(credentials)
            .await
            .map_err(|error| ServiceError::Credential(error.to_string())),
        provider => Err(ServiceError::UnsupportedProvider {
            provider: provider.to_owned(),
        }),
    }
}

pub async fn delete_credentials(provider: &str) -> Result<(), ServiceError> {
    match normalize_provider_id(provider).as_str() {
        ProviderId::OPENCODE_GO => OpenCodeGoProvider::delete_credentials()
            .await
            .map_err(|error| ServiceError::Credential(error.to_string())),
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

async fn info_claude() -> ProviderInfo {
    let provider = ClaudeProvider::default();
    let mut info = ProviderInfo::new(ProviderId::claude());
    info.push("usage_url", provider.usage_url().to_string());
    info.push(
        "credentials_path",
        provider.credentials_path().display().to_string(),
    );
    info.push("env_config_dir", CLAUDE_CONFIG_DIR_ENV);
    info.push("env_oauth_token", CLAUDE_CODE_OAUTH_TOKEN_ENV);
    info.push("keyring_service", CLAUDE_KEYCHAIN_SERVICE);

    match provider.access_token().await {
        Ok(token) => {
            info.push("auth_found", "true");
            info.push(
                "auth_source",
                match token.source {
                    ClaudeAccessTokenSource::Config => "config",
                    ClaudeAccessTokenSource::ClaudeCode => "claude_code",
                    ClaudeAccessTokenSource::Keyring => "keyring",
                    ClaudeAccessTokenSource::Environment(name) => name,
                },
            );
            info.push(
                "plan",
                token
                    .subscription_type
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
    if let Some(path) = provider.config().cursor_auth_path() {
        info.push("auth_path", path.display().to_string());
    }
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
                    CursorAccessTokenSource::ConfigFile => "config_file",
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

fn info_kimi() -> ProviderInfo {
    let provider = KimiProvider::default();
    let mut info = ProviderInfo::new(ProviderId::kimi());
    info.push("usage_url", provider.usage_url().to_string());
    info.push(
        "credentials_path",
        provider.credentials_path().display().to_string(),
    );
    info.push("env_code_home", KIMI_CODE_HOME_ENV);
    info.push("env_share_dir", KIMI_SHARE_DIR_ENV);
    info.push("env_api_key", KIMI_API_KEY_ENV);
    info.push("env_base_url", KIMI_CODE_BASE_URL_ENV);

    match provider.access_token() {
        Ok(token) => {
            info.push("auth_found", "true");
            info.push(
                "auth_source",
                match token.source {
                    KimiAccessTokenSource::Config => "config",
                    KimiAccessTokenSource::KimiCli => "kimi_cli",
                    KimiAccessTokenSource::Environment(name) => name,
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

async fn info_opencode_go() -> ProviderInfo {
    let provider = OpenCodeGoProvider::default();
    let mut info = ProviderInfo::new(ProviderId::opencode_go());
    info.push(
        "workspace_page",
        provider.config().base_url.as_str().to_owned(),
    );
    info.push("env_workspace_id", OPENCODE_WORKSPACE_ID_ENV);
    info.push("env_auth_cookie", OPENCODE_AUTH_COOKIE_ENV);
    info.push("keyring_service", OPENCODE_KEYCHAIN_SERVICE);
    info.push("keyring_account", OPENCODE_KEYCHAIN_ACCOUNT);

    match provider.credentials_async().await {
        Ok(credentials) => {
            info.push("auth_found", "true");
            info.push(
                "auth_source",
                match credentials.source {
                    OpenCodeGoCredentialsSource::Config => "config",
                    OpenCodeGoCredentialsSource::Environment => "environment",
                    OpenCodeGoCredentialsSource::Keyring => "keyring",
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

async fn info_google() -> ProviderInfo {
    let provider = GoogleProvider::default();
    let mut info = ProviderInfo::new(ProviderId::google());
    info.push("api_base_url", provider.config().api_base_url.to_string());
    info.push("token_url", provider.config().token_url.to_string());
    info.push("env_token", GOOGLE_AI_ACCESS_TOKEN_ENV);
    info.push("keyring_service", GOOGLE_KEYCHAIN_SERVICE);
    info.push("keyring_account", GOOGLE_KEYCHAIN_ACCOUNT);

    match provider.config().auth_token_async().await {
        Ok(token) => {
            info.push("auth_found", "true");
            info.push(
                "auth_source",
                match token.source {
                    GoogleAccessTokenSource::Config => "config",
                    GoogleAccessTokenSource::Environment(name) => name,
                    GoogleAccessTokenSource::Keyring => "keyring",
                },
            );
            info.push(
                "has_refresh_token",
                token.refresh_token.is_some().to_string(),
            );
            if let Some(expiry) = token.expiry {
                info.push(
                    "expiry",
                    expiry
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                );
            }
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
        assert_eq!(
            normalize_provider_id("claude-code").as_str(),
            ProviderId::CLAUDE
        );
        assert_eq!(
            normalize_provider_id("anthropic").as_str(),
            ProviderId::CLAUDE
        );
        assert_eq!(normalize_provider_id("claude").as_str(), ProviderId::CLAUDE);
        assert_eq!(normalize_provider_id("cursor").as_str(), ProviderId::CURSOR);
        assert_eq!(
            normalize_provider_id("kimi-code").as_str(),
            ProviderId::KIMI
        );
        assert_eq!(normalize_provider_id("kimi").as_str(), ProviderId::KIMI);
        assert_eq!(normalize_provider_id("z.ai").as_str(), ProviderId::ZAI);
        assert_eq!(normalize_provider_id("zai").as_str(), ProviderId::ZAI);
        assert_eq!(
            normalize_provider_id("opencode").as_str(),
            ProviderId::OPENCODE_GO
        );
        assert_eq!(
            normalize_provider_id("opencode-go").as_str(),
            ProviderId::OPENCODE_GO
        );
        assert_eq!(
            normalize_provider_id("google-ai").as_str(),
            ProviderId::GOOGLE
        );
        assert_eq!(normalize_provider_id("gemini").as_str(), ProviderId::GOOGLE);
        assert_eq!(
            normalize_provider_id("antigravity").as_str(),
            ProviderId::GOOGLE
        );
        assert_eq!(normalize_provider_id("agy").as_str(), ProviderId::GOOGLE);
        assert_eq!(normalize_provider_id("google").as_str(), ProviderId::GOOGLE);
    }

    #[test]
    fn provider_ids_are_canonical() {
        assert_eq!(
            provider_ids(),
            vec![
                ProviderId::openai(),
                ProviderId::claude(),
                ProviderId::cursor(),
                ProviderId::kimi(),
                ProviderId::zai(),
                ProviderId::opencode_go(),
                ProviderId::google(),
            ]
        );
    }
}
