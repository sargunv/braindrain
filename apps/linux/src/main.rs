//! BrainDrain Linux GUI — a GTK/libadwaita app for monitoring AI provider
//! quota usage and managing the BrainDrain daemon.
//!
//! When invoked with the hidden `--daemon-run` flag, this same binary runs the
//! D-Bus daemon in the foreground (the same path as `braindrain daemon run`).
//! The installed systemd unit's `ExecStart` points back at this binary, so the
//! GUI app is self-contained: it can install itself as the daemon's runner
//! without needing a separate `braindrain` CLI on `PATH`.

mod app;
mod auth;
mod backend;
mod settings;

use braindrain_daemon as daemon;
use relm4::RelmApp;

use app::{APP_BROKER, AppModel, AppMsg};

/// Hidden argv flag that selects the daemon-runner mode of this binary.
const DAEMON_RUN_FLAG: &str = "--daemon-run";

fn main() {
    env_logger::init();

    if std::env::args().any(|a| a == DAEMON_RUN_FLAG) {
        run_daemon();
        return;
    }

    let app: RelmApp<AppMsg> = RelmApp::new("dev.sargunv.BrainDrain").with_broker(&APP_BROKER);
    app.run::<AppModel>(());
}

fn run_daemon() {
    let runtime = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
    if let Err(error) = runtime.block_on(daemon::run_service()) {
        log::error!("daemon exited with error: {error:?}");
        std::process::exit(1);
    }
}
