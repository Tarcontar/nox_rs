//! Builds every `.ui` template, so a renamed id, a missing callback or a
//! broken file fails here rather than when the user opens a dialog. Needs a
//! display; without one the test passes with a note.

use gtk::{glib, prelude::*};

use crate::export_dialog::ExportDialog;
use crate::media_cell::MediaCell;
use crate::media_pool::MediaPool;
use crate::project::Project;
use crate::settings_dialog::ProjectSettings;
use crate::timeline::TimelineView;
use crate::viewer::Viewer;
use crate::window::Window;

#[test]
fn templates_build() {
    if gtk::init().is_err() {
        eprintln!("skipping: no display");
        return;
    }
    crate::i18n::init();
    gtk::gio::resources_register_include!("nox.gresource").unwrap();
    // Templates report problems as GLib criticals; make them fail the test.
    glib::log_set_always_fatal(glib::LogLevels::LEVEL_CRITICAL | glib::LogLevels::LEVEL_WARNING);

    let window = glib::Object::new::<Window>();
    let project = Project::new();
    let timeline = window.imp_children().2;
    timeline.set_project(project.clone());
    assert!(window.titlebar().is_some(), "header bar");

    glib::Object::new::<MediaPool>();
    glib::Object::new::<MediaCell>();
    glib::Object::new::<Viewer>();
    glib::Object::new::<TimelineView>();
    assert!(crate::timeline::clip_menu().n_items() > 0, "clip menu");

    let settings = glib::Object::builder::<ProjectSettings>()
        .property("transient-for", &window)
        .build();
    settings.load(&project);
    let export = glib::Object::builder::<ExportDialog>()
        .property("transient-for", &window)
        .build();
    export.load(&project);

    let css = gtk::CssProvider::new();
    css.load_from_resource("/io/github/nox/Nox/style.css");

    settings.destroy();
    export.destroy();
    window.destroy();
}
