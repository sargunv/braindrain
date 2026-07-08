//! Internal helpers shared across desktop integrations: XDG path resolution,
//! systemctl --user wrappers, file writing, and external process running.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, bail};
use braindrain_daemon::BUS_NAME;

pub const DAEMON_SERVICE_NAME: &str = "braindrain-daemon.service";
pub const DBUS_SERVICE_FILE_NAME: &str = "dev.sargunv.BrainDrain1.service";
pub const PLASMOID_ID: &str = "dev.sargunv.braindrain";
pub const LINUX_DESKTOP_FILE_NAME: &str = "dev.sargunv.BrainDrain.desktop";

/// Embedded Plasma widget files; reused when installing the widget.
pub const PLASMOID_FILES: &[(&str, &str)] = &[
    (
        "metadata.json",
        include_str!("../../../apps/plasma/package/metadata.json"),
    ),
    (
        "contents/ui/main.qml",
        include_str!("../../../apps/plasma/package/contents/ui/main.qml"),
    ),
];

/// Embedded launcher files for the Linux GUI app.
pub const LINUX_DESKTOP_FILES: &[(&str, &str)] = &[
    (
        LINUX_DESKTOP_FILE_NAME,
        include_str!("../../../apps/linux/data/dev.sargunv.BrainDrain.desktop"),
    ),
    (
        "dev.sargunv.BrainDrain.svg",
        include_str!("../../../apps/linux/data/dev.sargunv.BrainDrain.svg"),
    ),
];

pub fn systemd_service_contents(exe: &Path, args: &str) -> String {
    format!(
        "\
[Unit]
Description=BrainDrain D-Bus daemon

[Service]
Type=dbus
BusName={BUS_NAME}
ExecStart={} {args}
Restart=on-failure
RestartSec=5
",
        quoted_path(exe),
    )
}

pub fn dbus_service_contents(exe: &Path, args: &str) -> String {
    format!(
        "\
[D-BUS Service]
Name={BUS_NAME}
Exec={} {args}
SystemdService={DAEMON_SERVICE_NAME}
",
        quoted_path(exe),
    )
}

pub fn daemon_service_path() -> anyhow::Result<PathBuf> {
    Ok(xdg_config_home()?
        .join("systemd/user")
        .join(DAEMON_SERVICE_NAME))
}

pub fn dbus_service_path() -> anyhow::Result<PathBuf> {
    Ok(xdg_data_home()?
        .join("dbus-1/services")
        .join(DBUS_SERVICE_FILE_NAME))
}

/// `$XDG_DATA_HOME/applications` — where launcher `.desktop` files live.
pub fn applications_dir() -> anyhow::Result<PathBuf> {
    Ok(xdg_data_home()?.join("applications"))
}

/// `$XDG_DATA_HOME/icons/hicolor/scalable/apps` — where SVG app icons live.
pub fn icon_dir() -> anyhow::Result<PathBuf> {
    Ok(xdg_data_home()?
        .join("icons")
        .join("hicolor")
        .join("scalable")
        .join("apps"))
}

pub fn xdg_config_home() -> anyhow::Result<PathBuf> {
    Ok(env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".config")))
}

pub fn xdg_data_home() -> anyhow::Result<PathBuf> {
    Ok(env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".local/share")))
}

pub fn home_dir() -> anyhow::Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn write_file(path: &Path, contents: &str, description: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("failed to find parent directory for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::write(path, contents)
        .with_context(|| format!("failed to write {description} at {}", path.display()))
}

pub fn remove_file_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub fn systemctl_user<I, S>(args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command_args = vec![OsString::from("--user")];
    command_args.extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
    run_process("systemctl", command_args)
}

pub fn run_process<I, S>(program: &str, args: I) -> anyhow::Result<()>
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

pub fn quoted_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}
