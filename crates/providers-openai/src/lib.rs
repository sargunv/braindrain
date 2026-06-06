use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use braindrain_core::{
    AccountIdentity, Provider, ProviderError, ProviderFuture, ProviderId, ProviderSnapshot,
    ProviderSource, RefreshContext, UsageSnapshot,
};
use jsonwebtoken::dangerous::insecure_decode;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

pub const CHATGPT_WHAM_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

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
            let _client = &self.client;
            let auth = self
                .load_auth()
                .map_err(|error| ProviderError::NotConfigured(error.to_string()))?;
            let identity = auth.identity();
            Ok(ProviderSnapshot {
                provider: ProviderId::openai(),
                source: ProviderSource::OAuth,
                usage: UsageSnapshot::empty(),
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexAuth {
    #[serde(default)]
    pub tokens: CodexAuthTokens,
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

    pub fn identity(&self) -> Option<AccountIdentity> {
        let claims = self.tokens.id_token_claims().ok();
        let email = claims
            .as_ref()
            .and_then(|claims| claims.string("email"))
            .or_else(|| {
                claims
                    .as_ref()
                    .and_then(|claims| claims.string("https://api.openai.com/profile/email"))
            });
        let plan = claims
            .as_ref()
            .and_then(|claims| claims.string("chatgpt_plan_type"));

        if email.is_none() && plan.is_none() {
            return None;
        }
        Some(AccountIdentity { email, plan })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAuthTokens {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub expires_at: Option<OffsetDateTime>,
}

impl CodexAuthTokens {
    pub fn id_token_claims(&self) -> Result<JwtClaims, OpenAiProviderError> {
        JwtClaims::decode_unverified(&self.id_token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    #[error("id token is not a JWT payload")]
    InvalidJwt,
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

#[cfg(test)]
mod tests {
    use super::*;

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
                    eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20iLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBsdXMifQ.\
                    invalid-signature"
                    .to_owned(),
                ..CodexAuthTokens::default()
            },
        };

        let identity = auth.identity().expect("identity");
        assert_eq!(identity.email.as_deref(), Some("user@example.com"));
        assert_eq!(identity.plan.as_deref(), Some("plus"));
    }
}
