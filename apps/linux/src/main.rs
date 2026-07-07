//! BrainDrain Linux GUI — a GTK/libadwaita app for monitoring AI provider
//! quota usage and managing the BrainDrain daemon.

mod app;
mod auth;
mod backend;
mod settings;

use relm4::RelmApp;

use app::{APP_BROKER, AppModel, AppMsg};

fn main() {
    env_logger::init();

    let app: RelmApp<AppMsg> = RelmApp::new("dev.sargunv.BrainDrain").with_broker(&APP_BROKER);
    app.run::<AppModel>(());
}
