use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use crate::ui::NoxWindow;

// ---------------------------------------------------------------------------
// Private implementation
// ---------------------------------------------------------------------------
mod imp {
    use super::*;

    #[derive(Default)]
    pub struct NoxApplication;

    #[glib::object_subclass]
    impl ObjectSubclass for NoxApplication {
        const NAME: &'static str = "NoxApplication";
        type Type = super::NoxApplication;
        type ParentType = gtk::Application;
    }

    impl ObjectImpl for NoxApplication {}

    impl ApplicationImpl for NoxApplication {
        fn activate(&self) {
            let app = self.obj();
            app.load_css();

            let window = NoxWindow::new(app.upcast_ref::<gtk::Application>());
            window.present();
        }
    }

    impl GtkApplicationImpl for NoxApplication {}
}

// ---------------------------------------------------------------------------
// Public wrapper type
// ---------------------------------------------------------------------------
glib::wrapper! {
    pub struct NoxApplication(ObjectSubclass<imp::NoxApplication>)
        @extends gtk::Application, gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl NoxApplication {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", crate::APP_ID)
            .property("flags", gio::ApplicationFlags::default())
            .build()
    }

    fn load_css(&self) {
        let provider = gtk::CssProvider::new();
        provider.load_from_resource("/com/github/nox/style.css");

        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("Could not get default display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
