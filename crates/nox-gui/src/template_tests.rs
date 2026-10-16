//! Builds every `.ui` template, so a renamed id, a missing callback or a
//! broken file fails here rather than when the user opens a dialog. Needs a
//! display; without one the test passes with a note.

use gtk::{glib, prelude::*};

use crate::clip_strip::ClipStrip;
use crate::color_wheels::ColorWheels;
use crate::export_dialog::ExportDialog;
use crate::inspector::Inspector;
use crate::media_cell::MediaCell;
use crate::media_pool::MediaPool;
use crate::preferences::PreferencesWindow;
use crate::project::Project;
use crate::scopes::ScopesPanel;
use crate::settings_dialog::ProjectSettings;
use crate::timeline::TimelineView;
use crate::viewer::Viewer;
use crate::window::Window;

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "GTK must be initialized on the main thread on macOS; tests run on others"
)]
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
    // Bare keys run after the focused widget (bubble phase), not as
    // application accelerators, which would take them from spin buttons
    // and focused buttons.
    let controllers = window.observe_controllers();
    let keys = (0..controllers.n_items())
        .filter_map(|i| {
            controllers
                .item(i)
                .and_downcast::<gtk::ShortcutController>()
        })
        .find(|c| c.propagation_phase() == gtk::PropagationPhase::Bubble)
        .expect("window shortcut controller");
    let bound: Vec<(String, String)> = (0..keys.n_items())
        .filter_map(|i| keys.item(i).and_downcast::<gtk::Shortcut>())
        .map(|shortcut| {
            let trigger = shortcut.trigger().map(|t| t.to_str().to_string());
            let action = shortcut.action().map(|a| a.to_str().to_string());
            (trigger.unwrap_or_default(), action.unwrap_or_default())
        })
        .collect();
    for (trigger, action) in [
        ("space", "action(win.play-pause)"),
        ("Left", "action(win.previous-frame)"),
        ("i", "action(win.set-in)"),
        ("Up", "action(win.previous-edit)"),
        ("<Shift>v", "action(win.select-clip-at-playhead)"),
    ] {
        assert!(
            bound.iter().any(|(t, a)| t == trigger && a == action),
            "{trigger} → {action} in {bound:?}"
        );
    }

    glib::Object::new::<MediaPool>();
    glib::Object::new::<MediaCell>();
    glib::Object::new::<Viewer>();
    glib::Object::new::<TimelineView>();
    glib::Object::new::<ScopesPanel>().show(None);
    glib::Object::new::<ColorWheels>().set_timeline(&timeline);
    let inspector = glib::Object::new::<Inspector>();
    inspector.set_timeline(&timeline);
    glib::Object::new::<crate::node_editor::NodeEditor>().set_up(&timeline, &inspector);
    let panel = glib::Object::new::<crate::node_panel::NodePanel>();
    panel.set_up(&timeline);
    panel.show(Some(nox_project::SOURCE.into()));
    glib::Object::new::<ClipStrip>().set_up(&timeline, &window.imp_children().0);
    assert!(crate::timeline::clip_menu().n_items() > 0, "clip menu");
    let (video, audio) = crate::timeline::track_menus();
    assert!(video.n_items() > 0 && audio.n_items() > 0, "track menus");
    let rename = crate::timeline::rename_dialog();
    let dialog: gtk::Window = rename.object("dialog").expect("rename dialog");
    assert!(rename.object::<gtk::Entry>("name").is_some(), "name entry");

    let settings = glib::Object::builder::<ProjectSettings>()
        .property("transient-for", &window)
        .build();
    settings.load(&project);
    let export = glib::Object::builder::<ExportDialog>()
        .property("transient-for", &window)
        .build();
    export.load(&project);
    let preferences = glib::Object::builder::<PreferencesWindow>()
        .property("transient-for", &window)
        .build();

    let css = gtk::CssProvider::new();
    css.load_from_resource("/io/github/feliwir/Nox/style.css");

    settings.destroy();
    export.destroy();
    preferences.destroy();
    dialog.destroy();
    window.destroy();
}
