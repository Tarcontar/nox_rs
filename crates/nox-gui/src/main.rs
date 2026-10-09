mod export_dialog;
mod i18n;
mod media_cell;
mod media_object;
mod media_pool;
mod playback;
mod project;
mod settings_dialog;
#[cfg(test)]
mod template_tests;
mod timeline;
mod viewer;
mod window;

use gtk::{gdk, gio, glib, prelude::*};

const APP_ID: &str = "io.github.nox.Nox";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NOX_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    i18n::init();
    gio::resources_register_include!("nox.gresource").expect("bundled resources");

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        // CAN_OVERRIDE_APP_ID allows `--gapplication-app-id=…` to run a
        // separate instance (e.g. for testing next to a running Nox).
        .flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::CAN_OVERRIDE_APP_ID)
        .build();
    app.connect_startup(|_| {
        if let Some(settings) = gtk::Settings::default() {
            settings.set_gtk_application_prefer_dark_theme(true);
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_resource("/io/github/nox/Nox/style.css");
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("no display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
    app.set_accels_for_action("win.previous-frame", &["Left"]);
    app.set_accels_for_action("win.next-frame", &["Right"]);
    app.set_accels_for_action("win.go-to-start", &["Home"]);
    app.set_accels_for_action("win.go-to-end", &["End"]);
    app.set_accels_for_action("win.new", &["<Control>n"]);
    app.set_accels_for_action("win.open", &["<Control>o"]);
    app.set_accels_for_action("win.save", &["<Control>s"]);
    app.set_accels_for_action("win.save-as", &["<Control><Shift>s"]);
    app.set_accels_for_action("win.undo", &["<Control>z"]);
    app.set_accels_for_action("win.split", &["<Control>b"]);
    app.set_accels_for_action("win.add-dissolve", &["<Control>t"]);
    app.set_accels_for_action("win.project-settings", &["<Shift>9"]);
    app.set_accels_for_action("win.export", &["<Control>e"]);
    app.set_accels_for_action("win.play-pause", &["space"]);
    app.set_accels_for_action("win.play", &["l"]);
    app.set_accels_for_action("win.pause", &["k"]);
    app.set_accels_for_action("win.redo", &["<Control><Shift>z", "<Control>y"]);
    app.connect_activate(|app| window::build(app, &[]).present());
    // `nox clip.mp4 …` imports the files into the media pool; `nox
    // edit.otio` opens a project.
    app.connect_open(|app, files, _| window::build(app, files).present());
    app.run()
}
