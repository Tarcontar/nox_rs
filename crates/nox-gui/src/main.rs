// No console window for release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
mod clip_strip;
mod color_wheels;
mod export_dialog;
mod framing;
mod i18n;
mod inspector;
mod media_cell;
mod media_object;
mod media_pool;
mod playback;
mod preferences;
mod project;
mod scopes;
mod settings_dialog;
#[cfg(test)]
mod template_tests;
mod thumbnails;
mod timeline;
mod track_header;
mod transform_overlay;
mod viewer;
mod waveforms;
mod window;

use gtk::{gdk, gio, glib, prelude::*};

const APP_ID: &str = "io.github.feliwir.Nox";

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
    app.connect_startup(|app| {
        if let Some(settings) = gtk::Settings::default() {
            // The dark variant of the user's theme. Not GTK 4.20's
            // `gtk-interface-color-scheme`, although GTK calls this setting
            // deprecated: with a theme that has no separate dark theme
            // (Ubuntu's Yaru), that loads no stylesheet at all, and windows
            // come out unstyled and transparent.
            settings.set_gtk_application_prefer_dark_theme(true);
        }
        // The app's own icons (timeline tools, the app icon), also when run
        // uninstalled or with another app id.
        gtk::IconTheme::for_display(&gdk::Display::default().expect("no display"))
            .add_resource_path("/io/github/feliwir/Nox/icons");
        gtk::Window::set_default_icon_name(APP_ID);
        let provider = gtk::CssProvider::new();
        provider.load_from_resource("/io/github/feliwir/Nox/style.css");
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("no display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // Preferences are the application's, for every window.
        let preferences = gio::SimpleAction::new("preferences", None);
        preferences.connect_activate(glib::clone!(
            #[weak]
            app,
            move |_, _| preferences::show(app.active_window().as_ref())
        ));
        app.add_action(&preferences);
    });
    // Only shortcuts with Ctrl or Alt here: GTK runs application
    // accelerators before the focused widget, so a bare key would be taken
    // from it (Space from a focused button, arrows from a spin button).
    // Those keys are in a bubble-phase controller in window.ui.
    app.set_accels_for_action("win.new", &["<Control>n"]);
    app.set_accels_for_action("win.open", &["<Control>o"]);
    app.set_accels_for_action("win.save", &["<Control>s"]);
    app.set_accels_for_action("win.save-as", &["<Control><Shift>s"]);
    app.set_accels_for_action("win.undo", &["<Control>z"]);
    app.set_accels_for_action("win.split", &["<Control>b"]);
    app.set_accels_for_action("win.add-dissolve", &["<Control>t"]);
    app.set_accels_for_action("win.export", &["<Control>e"]);
    app.set_accels_for_action("app.preferences", &["<Control>comma"]);
    app.set_accels_for_action("win.redo", &["<Control><Shift>z", "<Control>y"]);
    // Clearing In/Out marks as in DaVinci Resolve.
    app.set_accels_for_action("win.clear-in", &["<Alt>i"]);
    app.set_accels_for_action("win.clear-out", &["<Alt>o"]);
    app.set_accels_for_action("win.clear-in-out", &["<Alt>x"]);
    app.connect_activate(|app| window::build(app, &[]).present());
    // `nox clip.mp4 …` imports the files into the media pool; `nox
    // edit.otio` opens a project.
    app.connect_open(|app, files, _| window::build(app, files).present());
    app.run()
}
