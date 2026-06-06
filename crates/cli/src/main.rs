use anyhow::Context;
use braindrain_service as service;
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
    for provider in service::provider_ids() {
        println!("{}", provider.as_str());
    }
    Ok(())
}

fn info_provider(provider: &str) -> anyhow::Result<()> {
    let info = service::info_provider(provider).context("failed to inspect provider")?;
    println!("provider={}", info.provider.as_str());
    for field in info.fields {
        println!("{}={}", field.key, field.value);
    }
    Ok(())
}

async fn check_provider(provider: &str) -> anyhow::Result<()> {
    let snapshot = service::check_provider(provider)
        .await
        .context("failed to check provider")?;
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
}
