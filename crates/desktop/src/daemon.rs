//! Daemon systemd user unit + D-Bus activation lifecycle.
//!
//! The daemon is D-Bus auto-activatable: once `install()` has been run, any
//! client call on the bus name will cause systemd to start the unit on demand.
//! There is no need for explicit start/stop/restart controls — stopping is
//! futile (the next call re-activates), and starting is implicit.

use std::path::{Path, PathBuf};

use super::util::{
    DAEMON_SERVICE_NAME, daemon_service_path, dbus_service_contents, dbus_service_path,
    remove_file_if_exists, systemctl_user, systemd_service_contents, write_file,
};

/// Install the user systemd + D-Bus activation files.
///
/// `cli_exe` is the absolute path to a binary that knows how to run the daemon
/// (either the `braindrain` CLI, given `cli_args = "daemon run"`, or the GUI
/// binary, given `cli_args = "--daemon-run"`). `current_exe()` is the right
/// choice for both callers — the systemd unit's `ExecStart` invokes
/// `<cli_exe> <cli_args>`, so the binary that handles install is the same one
/// that gets activated later.
///
/// The daemon itself is not started or enabled here — it activates on demand
/// the first time any client calls the bus name. Returns the path of the
/// installed systemd user unit.
pub fn install(cli_exe: &Path, cli_args: &str) -> anyhow::Result<PathBuf> {
    let service_path = daemon_service_path()?;
    let dbus_path = dbus_service_path()?;

    write_file(
        &service_path,
        &systemd_service_contents(cli_exe, cli_args),
        "systemd user service",
    )?;
    write_file(
        &dbus_path,
        &dbus_service_contents(cli_exe, cli_args),
        "D-Bus service",
    )?;

    systemctl_user(["daemon-reload"])?;

    println!("installed {}", service_path.display());
    println!("installed {}", dbus_path.display());
    Ok(service_path)
}

/// Stop and remove the user systemd + D-Bus activation files.
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

/// Whether the systemd user unit file exists on disk.
///
/// This is the source of truth for "is the daemon configured for this user":
/// when true, D-Bus activation will start the daemon on demand.
pub fn is_installed() -> bool {
    daemon_service_path().map(|p| p.exists()).unwrap_or(false)
}
