//! Daemon systemd user unit + D-Bus activation lifecycle.
//!
//! These functions are thin extractions of what used to live inline in the
//! `braindrain` CLI; behavior is identical.

use std::path::PathBuf;

use anyhow::Context;

use super::util::{
    DAEMON_SERVICE_NAME, current_exe, daemon_service_path, dbus_service_contents,
    dbus_service_path, remove_file_if_exists, systemctl_user, systemctl_user_status,
    systemd_service_contents, write_file,
};

/// Install and immediately start the user systemd + D-Bus activation files.
///
/// Returns the path of the installed systemd user unit.
pub fn install() -> anyhow::Result<PathBuf> {
    let exe = current_exe()?;
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
    Ok(service_path)
}

/// Disable and remove the user systemd + D-Bus activation files.
pub fn uninstall() -> anyhow::Result<()> {
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

/// Start the installed user daemon service.
pub fn start() -> anyhow::Result<()> {
    systemctl_user(["start", DAEMON_SERVICE_NAME])
}

/// Stop the installed user daemon service.
pub fn stop() -> anyhow::Result<()> {
    systemctl_user(["stop", DAEMON_SERVICE_NAME])
}

/// Restart the installed user daemon service.
pub fn restart() -> anyhow::Result<()> {
    systemctl_user(["restart", DAEMON_SERVICE_NAME])
}

/// Enable (autostart at login) the installed user daemon service.
pub fn enable() -> anyhow::Result<()> {
    systemctl_user(["enable", DAEMON_SERVICE_NAME])
}

/// Disable (no autostart at login) the installed user daemon service.
pub fn disable() -> anyhow::Result<()> {
    systemctl_user(["disable", DAEMON_SERVICE_NAME])
}

/// Whether the systemd user unit file exists on disk.
pub fn is_installed() -> bool {
    daemon_service_path().map(|p| p.exists()).unwrap_or(false)
}

/// Whether the daemon service is currently active (`systemctl --user is-active`).
pub fn is_running() -> bool {
    systemctl_user_status(["is-active", "--quiet", DAEMON_SERVICE_NAME]).unwrap_or(false)
}

/// Whether the daemon service is enabled for autostart at login.
pub fn is_enabled() -> bool {
    systemctl_user_status(["is-enabled", "--quiet", DAEMON_SERVICE_NAME]).unwrap_or(false)
}

/// Restart the service if it is installed; no-op (with context) otherwise.
/// Convenience for the GUI when the user clicks "restart".
pub fn ensure_started() -> anyhow::Result<()> {
    if is_installed() {
        start().context("failed to start daemon service")
    } else {
        install()?;
        Ok(())
    }
}
