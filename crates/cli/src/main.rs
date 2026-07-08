use std::collections::HashMap;
use std::io::{self, Write};

use anyhow::{Context, bail};
use braindrain_core::ProviderCredentials;
use braindrain_daemon::DaemonClient;
use braindrain_service as service;
use clap::{Parser, Subcommand};

#[cfg(target_os = "linux")]
use braindrain_desktop as desktop;

const DAEMON_HINT: &str =
    "failed to reach BrainDrain daemon; start it with `braindrain daemon run`";

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
    /// Install or remove desktop integrations.
    Desktop {
        #[command(subcommand)]
        command: DesktopCommand,
    },
    /// Manage stored provider credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Run the D-Bus daemon in the foreground.
    Run,
    /// Install and activate the user systemd + D-Bus activation files.
    ///
    /// The daemon itself starts on demand the first time any client calls the
    /// bus name; explicit start/stop is unnecessary.
    Install,
    /// Remove the user systemd + D-Bus activation files.
    Uninstall,
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

#[derive(Debug, Subcommand)]
enum DesktopCommand {
    /// Install a desktop integration for the current user.
    Install {
        #[command(subcommand)]
        target: DesktopTarget,
    },
    /// Remove a desktop integration for the current user.
    Uninstall {
        #[command(subcommand)]
        target: DesktopTarget,
    },
}

#[derive(Debug, Subcommand)]
enum DesktopTarget {
    /// KDE Plasma widget.
    Plasma,
    /// Linux GUI app launcher (.desktop entry + icon).
    Linux,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Store credentials for a provider in the system keyring.
    Login {
        /// Provider id, for example opencode-go. The alias opencode maps to opencode-go.
        provider: String,
    },
    /// Remove stored credentials for a provider.
    Logout {
        /// Provider id, for example opencode-go. The alias opencode maps to opencode-go.
        provider: String,
    },
    /// Print the credential fields a provider accepts.
    Schema {
        /// Provider id, for example opencode-go. The alias opencode maps to opencode-go.
        provider: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Providers => list_providers(),
        Command::Info { provider } => info_provider(&provider).await,
        Command::Check { provider } => check_provider(&provider).await,
        Command::Daemon { command } => daemon_command(command).await,
        Command::Desktop { command } => desktop_command(command),
        Command::Auth { command } => auth_command(command).await,
    }
}

fn list_providers() -> anyhow::Result<()> {
    for provider in service::provider_ids() {
        println!("{}", provider.as_str());
    }
    Ok(())
}

async fn info_provider(provider: &str) -> anyhow::Result<()> {
    let info = service::info_provider(provider)
        .await
        .context("failed to inspect provider")?;
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
    match command {
        DaemonCommand::Run => braindrain_daemon::run_service().await,
        DaemonCommand::Install => {
            ensure_linux("daemon install")?;
            #[cfg(target_os = "linux")]
            {
                let exe = std::env::current_exe().context("failed to locate current executable")?;
                desktop::daemon::install(&exe)?;
            }
            Ok(())
        }
        DaemonCommand::Uninstall => {
            ensure_linux("daemon uninstall")?;
            #[cfg(target_os = "linux")]
            {
                desktop::daemon::uninstall()?;
            }
            Ok(())
        }
        command => daemon_client_command(command).await,
    }
}

async fn daemon_client_command(command: DaemonCommand) -> anyhow::Result<()> {
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
        DaemonCommand::Run | DaemonCommand::Install | DaemonCommand::Uninstall => {
            unreachable!("lifecycle commands are handled before connecting")
        }
    }
}

fn desktop_command(command: DesktopCommand) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match command {
            DesktopCommand::Install {
                target: DesktopTarget::Plasma,
            } => desktop::plasma::install(),
            DesktopCommand::Uninstall {
                target: DesktopTarget::Plasma,
            } => desktop::plasma::uninstall(),
            DesktopCommand::Install {
                target: DesktopTarget::Linux,
            } => desktop::linux::install(),
            DesktopCommand::Uninstall {
                target: DesktopTarget::Linux,
            } => desktop::linux::uninstall(),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
        bail!("desktop integration is only supported on Linux");
    }
}

async fn auth_command(command: AuthCommand) -> anyhow::Result<()> {
    match command {
        AuthCommand::Login { provider } => auth_login(&provider).await,
        AuthCommand::Logout { provider } => {
            let canonical = service::normalize_provider_id(&provider);
            service::delete_credentials(&provider)
                .await
                .context("failed to remove credentials")?;
            println!("removed credentials for {}", canonical.as_str());
            Ok(())
        }
        AuthCommand::Schema { provider } => auth_schema(&provider),
    }
}

async fn auth_login(provider: &str) -> anyhow::Result<()> {
    let schema = service::credential_schema(provider)
        .with_context(|| format!("provider '{provider}' does not support stored credentials"))?;

    let mut values: HashMap<String, String> = HashMap::new();
    for field in &schema.fields {
        let value = if field.secret {
            rpassword::prompt_password(format!("{}: ", field.label))?
        } else {
            print!("{}: ", field.label);
            io::stdout().flush()?;
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            line.trim().to_owned()
        };
        if value.is_empty() {
            bail!("{} is required", field.label);
        }
        values.insert(field.id.clone(), value);
    }

    let credentials = ProviderCredentials {
        provider: schema.provider.clone(),
        values,
    };
    service::store_credentials(credentials)
        .await
        .context("failed to store credentials")?;
    println!("stored credentials for {}", schema.provider.as_str());
    Ok(())
}

fn auth_schema(provider: &str) -> anyhow::Result<()> {
    let schema = service::credential_schema(provider)
        .with_context(|| format!("provider '{provider}' does not support stored credentials"))?;
    println!("provider={}", schema.provider.as_str());
    for field in schema.fields {
        println!("{}\t{}\t{}", field.id, field.label, field.secret);
    }
    Ok(())
}

fn print_json(data: &str) -> anyhow::Result<()> {
    match serde_json::from_str::<serde_json::Value>(data) {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value)?),
        Err(_) => println!("{data}"),
    }
    Ok(())
}

fn ensure_linux(action: &str) -> anyhow::Result<()> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        bail!("{action} is only supported on Linux")
    }
}
