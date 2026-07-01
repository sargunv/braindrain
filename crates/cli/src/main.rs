use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, bail};
use braindrain_core::ProviderCredentials;
use braindrain_daemon::{BUS_NAME, DaemonClient};
use braindrain_service as service;
use clap::{Parser, Subcommand};

const DAEMON_HINT: &str =
    "failed to reach BrainDrain daemon; start it with `braindrain daemon run`";
const DAEMON_SERVICE_NAME: &str = "braindrain-daemon.service";
const DBUS_SERVICE_FILE_NAME: &str = "dev.sargunv.BrainDrain1.service";
const PLASMOID_ID: &str = "dev.sargunv.braindrain";
const PLASMOID_FILES: &[(&str, &str)] = &[
    (
        "metadata.json",
        include_str!("../../../apps/plasma/package/metadata.json"),
    ),
    (
        "contents/ui/main.qml",
        include_str!("../../../apps/plasma/package/contents/ui/main.qml"),
    ),
];

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
    /// Install and start the user systemd and D-Bus activation files.
    Install,
    /// Remove the user systemd and D-Bus activation files.
    Uninstall,
    /// Start the installed user daemon service.
    Start,
    /// Stop the installed user daemon service.
    Stop,
    /// Restart the installed user daemon service.
    Restart,
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
        DaemonCommand::Install => daemon_install(),
        DaemonCommand::Uninstall => daemon_uninstall(),
        DaemonCommand::Start => systemctl_user(["start", DAEMON_SERVICE_NAME]),
        DaemonCommand::Stop => systemctl_user(["stop", DAEMON_SERVICE_NAME]),
        DaemonCommand::Restart => systemctl_user(["restart", DAEMON_SERVICE_NAME]),
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
        DaemonCommand::Run
        | DaemonCommand::Install
        | DaemonCommand::Uninstall
        | DaemonCommand::Start
        | DaemonCommand::Stop
        | DaemonCommand::Restart => {
            unreachable!("lifecycle commands are handled before connecting")
        }
    }
}

fn desktop_command(command: DesktopCommand) -> anyhow::Result<()> {
    match command {
        DesktopCommand::Install {
            target: DesktopTarget::Plasma,
        } => plasma_install(),
        DesktopCommand::Uninstall {
            target: DesktopTarget::Plasma,
        } => plasma_uninstall(),
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

fn daemon_install() -> anyhow::Result<()> {
    ensure_linux("daemon install")?;

    let exe = env::current_exe().context("failed to locate current braindrain executable")?;
    let service_path = daemon_service_path()?;
    let dbus_path = dbus_service_path()?;

    write_file(
        &service_path,
        &systemd_service_contents(&exe),
        "systemd user service",
    )?;
    write_file(&dbus_path, &dbus_service_contents(&exe), "D-Bus service")?;

    systemctl_user(["daemon-reload"])?;
    systemctl_user(["enable", "--now", DAEMON_SERVICE_NAME])?;

    println!("installed {}", service_path.display());
    println!("installed {}", dbus_path.display());
    Ok(())
}

fn daemon_uninstall() -> anyhow::Result<()> {
    ensure_linux("daemon uninstall")?;

    let service_path = daemon_service_path()?;
    let dbus_path = dbus_service_path()?;

    let _ = systemctl_user(["disable", "--now", DAEMON_SERVICE_NAME]);
    remove_file_if_exists(&service_path)?;
    remove_file_if_exists(&dbus_path)?;
    systemctl_user(["daemon-reload"])?;

    println!("removed {}", service_path.display());
    println!("removed {}", dbus_path.display());
    Ok(())
}

fn plasma_install() -> anyhow::Result<()> {
    ensure_linux("plasma install")?;

    let temp_dir = tempfile::tempdir().context("failed to create temporary Plasma package")?;
    let package_path = temp_dir.path().join("package");
    write_embedded_plasmoid(&package_path)?;

    let upgrade = run_process(
        "kpackagetool6",
        [
            OsStr::new("--type"),
            OsStr::new("Plasma/Applet"),
            OsStr::new("--upgrade"),
            package_path.as_os_str(),
        ],
    );

    match upgrade {
        Ok(()) => {
            println!("upgraded Plasma widget {PLASMOID_ID}");
            Ok(())
        }
        Err(_) => {
            run_process(
                "kpackagetool6",
                [
                    OsStr::new("--type"),
                    OsStr::new("Plasma/Applet"),
                    OsStr::new("--install"),
                    package_path.as_os_str(),
                ],
            )?;
            println!("installed Plasma widget {PLASMOID_ID}");
            Ok(())
        }
    }
}

fn plasma_uninstall() -> anyhow::Result<()> {
    ensure_linux("plasma uninstall")?;
    run_process(
        "kpackagetool6",
        [
            OsStr::new("--type"),
            OsStr::new("Plasma/Applet"),
            OsStr::new("--remove"),
            OsStr::new(PLASMOID_ID),
        ],
    )?;
    println!("removed Plasma widget {PLASMOID_ID}");
    Ok(())
}

fn write_embedded_plasmoid(package_path: &Path) -> anyhow::Result<()> {
    for (relative_path, contents) in PLASMOID_FILES {
        let path = package_path.join(relative_path);
        write_file(&path, contents, "embedded Plasma package file")?;
    }
    Ok(())
}

fn systemd_service_contents(exe: &Path) -> String {
    format!(
        "\
[Unit]
Description=BrainDrain D-Bus daemon

[Service]
Type=dbus
BusName={BUS_NAME}
ExecStart={} daemon run
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
",
        quoted_path(exe),
    )
}

fn dbus_service_contents(exe: &Path) -> String {
    format!(
        "\
[D-BUS Service]
Name={BUS_NAME}
Exec={} daemon run
SystemdService={DAEMON_SERVICE_NAME}
",
        quoted_path(exe),
    )
}

fn daemon_service_path() -> anyhow::Result<PathBuf> {
    Ok(xdg_config_home()?
        .join("systemd/user")
        .join(DAEMON_SERVICE_NAME))
}

fn dbus_service_path() -> anyhow::Result<PathBuf> {
    Ok(xdg_data_home()?
        .join("dbus-1/services")
        .join(DBUS_SERVICE_FILE_NAME))
}

fn xdg_config_home() -> anyhow::Result<PathBuf> {
    Ok(env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".config")))
}

fn xdg_data_home() -> anyhow::Result<PathBuf> {
    Ok(env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".local/share")))
}

fn home_dir() -> anyhow::Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn write_file(path: &Path, contents: &str, description: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("failed to find parent directory for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::write(path, contents)
        .with_context(|| format!("failed to write {description} at {}", path.display()))
}

fn remove_file_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn systemctl_user<I, S>(args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command_args = vec![OsString::from("--user")];
    command_args.extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
    run_process("systemctl", command_args)
}

fn run_process<I, S>(program: &str, args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = ProcessCommand::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "{program} exited with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout.trim(),
        stderr.trim(),
    )
}

fn quoted_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

fn ensure_linux(action: &str) -> anyhow::Result<()> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        bail!("{action} is only supported on Linux")
    }
}
