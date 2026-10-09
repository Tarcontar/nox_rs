mod media_cell;
mod media_object;
mod media_pool;
mod timeline;
mod viewer;
mod window;

use gtk::{gdk, gio, glib, prelude::*};

const APP_ID: &str = "io.github.nox.Nox";

const CSS: &str = "
.media-pool { background: #1e1f22; }
.media-pool:drop(active) { box-shadow: inset 0 0 0 2px #3584e4; }
.pane-title { font-weight: bold; padding: 6px 8px; }
.media-pool gridview > child { padding: 6px; }
.thumbnail-frame { background: #111214; border-radius: 4px; }
.media-summary { font-size: 8pt; opacity: 0.7; }
.viewer-canvas { background: black; }
.timecode { font-family: monospace; font-size: 13pt; padding: 0 8px; }
timeline { font-size: 8pt; }
";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NOX_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

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
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("no display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
    app.set_accels_for_action("win.previous-frame", &["Left"]);
    app.set_accels_for_action("win.next-frame", &["Right"]);
    app.set_accels_for_action("win.go-to-start", &["Home"]);
    app.connect_activate(|app| window::build(app, &[]).present());
    // `nox clip.mp4 …` imports the files into the media pool.
    app.connect_open(|app, files, _| window::build(app, files).present());
    app.run()
}
