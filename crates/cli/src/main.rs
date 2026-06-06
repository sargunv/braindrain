use anyhow::{Context, bail};
use braindrain_core::{Provider, ProviderId, RefreshContext};
use braindrain_providers_cursor::{CURSOR_AUTH_TOKEN_ENV, CursorAccessTokenSource, CursorProvider};
#[cfg(target_os = "macos")]
use braindrain_providers_cursor::{CURSOR_KEYCHAIN_ACCOUNT, CURSOR_KEYCHAIN_SERVICE};
use braindrain_providers_openai::OpenAiProvider;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "braindrain")]
#[command(about = "Manual probes for BrainDrain providers.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List providers known by the current core.
    Providers,
    /// Report local, non-network provider information.
    Info {
        /// Provider id, for example openai. The alias codex maps to openai.
        provider: String,
    },
    /// Refresh a provider and print the normalized snapshot.
    Check {
        /// Provider id, for example openai. The alias codex maps to openai.
        provider: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Providers => list_providers(),
        Command::Info { provider } => info_provider(&provider),
        Command::Check { provider } => check_provider(&provider).await,
    }
}

fn list_providers() -> anyhow::Result<()> {
    for provider in [ProviderId::openai(), ProviderId::cursor()] {
        println!("{}", provider.as_str());
    }
    Ok(())
}

fn info_provider(provider: &str) -> anyhow::Result<()> {
    match normalize_provider(provider).as_str() {
        ProviderId::OPENAI => info_openai(),
        ProviderId::CURSOR => info_cursor(),
        provider => bail!("provider info is not implemented for {provider}"),
    }
}

async fn check_provider(provider: &str) -> anyhow::Result<()> {
    match normalize_provider(provider).as_str() {
        ProviderId::OPENAI => check_openai().await,
        ProviderId::CURSOR => check_cursor().await,
        provider => bail!("provider check is not implemented for {provider}"),
    }
}

fn info_openai() -> anyhow::Result<()> {
    let provider = OpenAiProvider::default();
    println!("provider={}", ProviderId::OPENAI);
    println!("auth_path={}", provider.auth_path().display());

    match provider.load_auth() {
        Ok(auth) => {
            let identity = auth.identity();
            println!("auth_found=true");
            println!("has_access_token={}", !auth.tokens.access_token.is_empty());
            println!(
                "has_refresh_token={}",
                !auth.tokens.refresh_token.is_empty()
            );
            println!(
                "account_id={}",
                auth.tokens.account_id.as_deref().unwrap_or("<unknown>")
            );
            println!(
                "email={}",
                identity
                    .as_ref()
                    .and_then(|identity| identity.email.as_deref())
                    .unwrap_or("<unknown>")
            );
            println!(
                "plan={}",
                identity
                    .as_ref()
                    .and_then(|identity| identity.plan.as_deref())
                    .unwrap_or("<unknown>")
            );
        }
        Err(error) => {
            println!("auth_found=false");
            println!("auth_error={error}");
        }
    }

    Ok(())
}

fn info_cursor() -> anyhow::Result<()> {
    let provider = CursorProvider::default();
    println!("provider={}", ProviderId::CURSOR);
    println!("api_base_url={}", provider.config().api_base_url);
    println!("env_token={CURSOR_AUTH_TOKEN_ENV}");
    #[cfg(target_os = "macos")]
    {
        println!("keychain_service={CURSOR_KEYCHAIN_SERVICE}");
        println!("keychain_account={CURSOR_KEYCHAIN_ACCOUNT}");
    }

    match provider.auth_token() {
        Ok(token) => {
            println!("auth_found=true");
            println!(
                "auth_source={}",
                match token.source {
                    CursorAccessTokenSource::Environment(name) => name,
                    CursorAccessTokenSource::MacosKeychain => "macos-keychain",
                    CursorAccessTokenSource::Config => "config",
                }
            );
        }
        Err(error) => {
            println!("auth_found=false");
            println!("auth_error={error}");
        }
    }

    Ok(())
}

async fn check_openai() -> anyhow::Result<()> {
    let provider = OpenAiProvider::default();
    let snapshot = provider
        .refresh(RefreshContext::default())
        .await
        .context("failed to refresh OpenAI provider")?;
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
}

async fn check_cursor() -> anyhow::Result<()> {
    let provider = CursorProvider::default();
    let snapshot = provider
        .refresh(RefreshContext::default())
        .await
        .context("failed to refresh Cursor provider")?;
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
}

fn normalize_provider(provider: &str) -> ProviderId {
    match provider {
        "codex" => ProviderId::openai(),
        provider => ProviderId::new(provider),
    }
}
