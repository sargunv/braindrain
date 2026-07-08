//! KDE Plasma widget (plasmoid) install/uninstall.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::Context;
use tempfile::tempdir;

use super::util::{
    PLASMOID_FILES, PLASMOID_ID, is_plasma_session, plasma_widget_dir, run_process, write_file,
};

/// Install or upgrade the Plasma widget for the current user.
pub fn install() -> anyhow::Result<()> {
    let temp_dir = tempdir().context("failed to create temporary Plasma package")?;
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

/// Remove the Plasma widget for the current user.
pub fn uninstall() -> anyhow::Result<()> {
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

/// Whether the current session is a KDE Plasma desktop.
pub fn is_session_plasma() -> bool {
    is_plasma_session()
}

/// Whether the Plasma widget is installed for the current user.
pub fn is_installed() -> bool {
    plasma_widget_dir().map(|p| p.exists()).unwrap_or(false)
}
