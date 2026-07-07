//! Linux GUI launcher (.desktop + icon) install/uninstall.

use super::util::{
    LINUX_DESKTOP_FILES, applications_dir, icon_dir, remove_file_if_exists, write_file,
};

/// Install the `.desktop` entry and scalable app icon for the current user.
pub fn install() -> anyhow::Result<()> {
    let apps = applications_dir()?;
    let icons = icon_dir()?;

    for (relative_path, contents) in LINUX_DESKTOP_FILES {
        let target = target_path(&apps, &icons, relative_path);
        write_file(&target, contents, "Linux desktop launcher file")?;
        println!("installed {}", target.display());
    }

    Ok(())
}

/// Remove the `.desktop` entry and scalable app icon for the current user.
pub fn uninstall() -> anyhow::Result<()> {
    let apps = applications_dir()?;
    let icons = icon_dir()?;

    for (relative_path, _) in LINUX_DESKTOP_FILES {
        let target = target_path(&apps, &icons, relative_path);
        remove_file_if_exists(&target)?;
        println!("removed {}", target.display());
    }

    Ok(())
}

pub fn is_installed() -> bool {
    applications_dir()
        .map(|d| d.join(super::util::LINUX_DESKTOP_FILE_NAME).exists())
        .unwrap_or(false)
}

fn target_path(
    apps: &std::path::Path,
    icons: &std::path::Path,
    relative_path: &str,
) -> std::path::PathBuf {
    if relative_path.ends_with(".desktop") {
        apps.join(relative_path)
    } else {
        icons.join(relative_path)
    }
}
