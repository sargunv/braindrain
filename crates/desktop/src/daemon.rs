//! Daemon systemd user unit + D-Bus activation lifecycle.
//!
//! The daemon is D-Bus auto-activatable: once `install()` has been run, any
//! client call on the bus name will cause systemd to start the unit on demand.
//! There is no need for explicit start/stop/restart controls — stopping is
//! futile (the next call re-activates), and starting is implicit.

use std::env;
use std::path::{Path, PathBuf};

use super::util::{
    DAEMON_SERVICE_NAME, daemon_service_path, dbus_service_contents, dbus_service_path,
    remove_file_if_exists, systemctl_user, systemd_service_contents, write_file,
};

/// Error returned when the `braindrain` CLI binary cannot be located on `PATH`.
/// Carries enough context for the GUI to surface a useful install-prompt error.
#[derive(Debug, thiserror::Error)]
#[error("the `braindrain` CLI binary was not found on PATH; install it first")]
pub struct DaemonCliNotFound;

/// Install the user systemd + D-Bus activation files.
///
/// `cli_exe` is the absolute path to the `braindrain` CLI binary, which the
/// unit's `ExecStart` invokes as `<cli_exe> daemon run`. The daemon itself is
/// not started or enabled here — it activates on demand the first time any
/// client calls the bus name. Returns the path of the installed systemd user
/// unit.
pub fn install(cli_exe: &Path) -> anyhow::Result<PathBuf> {
    let service_path = daemon_service_path()?;
    let dbus_path = dbus_service_path()?;

    write_file(
        &service_path,
        &systemd_service_contents(cli_exe),
        "systemd user service",
    )?;
    write_file(&dbus_path, &dbus_service_contents(cli_exe), "D-Bus service")?;

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

/// Locate the `braindrain` CLI binary on `PATH`.
///
/// Used by callers that aren't the CLI itself (e.g. the GUI app) to resolve
/// the executable to advertise in `ExecStart`. The CLI binary can pass its
/// own `current_exe()` directly to [`install`].
pub fn find_cli_on_path() -> Result<PathBuf, DaemonCliNotFound> {
    const CLI_BASENAME: &str = "braindrain";

    let path = env::var_os("PATH").ok_or(DaemonCliNotFound)?;
    env::split_paths(&path)
        .map(|dir| dir.join(CLI_BASENAME))
        .find(|candidate| candidate.is_file())
        .ok_or(DaemonCliNotFound)
}
