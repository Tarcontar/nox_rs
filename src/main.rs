mod app;
mod media;
mod render;
mod timeline;
mod ui;

use gtk::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "com.github.nox";

fn main() -> glib::ExitCode {
    env_logger::init();
    log::info!("Starting Nox Video Editor");

    gio::resources_register_include!("nox.gresource").expect("Failed to register resources.");

    let app = app::NoxApplication::new();
    app.run()
}
