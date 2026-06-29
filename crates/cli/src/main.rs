use anyhow::Context;
use braindrain_daemon::DaemonClient;
use braindrain_service as service;
use clap::{Parser, Subcommand};

const DAEMON_HINT: &str = "failed to reach BrainDrain daemon; start it with `mise run daemon`";

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
    /// Talk to a running BrainDrain daemon over D-Bus.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Print daemon status and cached provider states.
    Status,
    /// List providers exposed by the daemon.
    Providers,
    /// Print the cached state for one provider.
    Snapshot {
        /// Provider id, for example openai. The alias codex maps to openai.
        provider: String,
    },
    /// Refresh one provider through the daemon and print the resulting state.
    Refresh {
        /// Provider id, for example openai. The alias codex maps to openai.
        provider: String,
    },
    /// Refresh all providers through the daemon and print the resulting states.
    RefreshAll,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Providers => list_providers(),
        Command::Info { provider } => info_provider(&provider),
        Command::Check { provider } => check_provider(&provider).await,
        Command::Daemon { command } => daemon_command(command).await,
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

async fn daemon_command(command: DaemonCommand) -> anyhow::Result<()> {
    let client = DaemonClient::connect().await.context(DAEMON_HINT)?;

    match command {
        DaemonCommand::Status => print_json(&client.status().await.context(DAEMON_HINT)?),
        DaemonCommand::Providers => {
            for provider in client.list_providers().await.context(DAEMON_HINT)? {
                println!("{provider}");
            }
            Ok(())
        }
        DaemonCommand::Snapshot { provider } => {
            print_json(&client.get_snapshot(&provider).await.context(DAEMON_HINT)?)
        }
        DaemonCommand::Refresh { provider } => {
            print_json(&client.refresh(&provider).await.context(DAEMON_HINT)?)
        }
        DaemonCommand::RefreshAll => print_json(&client.refresh_all().await.context(DAEMON_HINT)?),
    }
}

fn print_json(data: &str) -> anyhow::Result<()> {
    match serde_json::from_str::<serde_json::Value>(data) {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value)?),
        Err(_) => println!("{data}"),
    }
    Ok(())
}
