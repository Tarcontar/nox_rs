//! The main window (`resources/ui/window.ui`): media pool and viewer on
//! top, the timeline below. The template holds the layout; `Ui` holds the
//! project and implements the window's actions.

use gtk::{CompositeTemplate, gdk, gio, glib, prelude::*, subclass::prelude::*};
use nox_project::{BlendMode, Edge, EditError, Rational, Time, TrackKind, supports_drop_frame};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::clip_strip::ClipStrip;
use crate::color_wheels::ColorWheels;
use crate::i18n::{fill, gettext};
use crate::inspector::Inspector;
use crate::media_object::MediaObject;
use crate::playback::Playback;
use crate::project::Project;
use crate::scopes::ScopesPanel;
use crate::{media_pool::MediaPool, timeline::TimelineView, viewer::Viewer};

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/nox/Nox/ui/window.ui")]
    pub struct Window {
        #[template_child]
        pub media_pool: TemplateChild<MediaPool>,
        #[template_child]
        pub viewer: TemplateChild<Viewer>,
        #[template_child]
        pub timeline: TemplateChild<TimelineView>,
        #[template_child]
        pub pages: TemplateChild<gtk::Stack>,
        #[template_child]
        pub edit_viewer_slot: TemplateChild<gtk::Box>,
        #[template_child]
        pub color_viewer_slot: TemplateChild<gtk::Box>,
        #[template_child]
        pub inspector: TemplateChild<Inspector>,
        #[template_child]
        pub clip_strip: TemplateChild<ClipStrip>,
        #[template_child]
        pub color_paned: TemplateChild<gtk::Paned>,
        #[template_child]
        pub scopes: TemplateChild<ScopesPanel>,
        #[template_child]
        pub color_wheels: TemplateChild<ColorWheels>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Window {
        const NAME: &'static str = "NoxWindow";
        type Type = super::Window;
        type ParentType = gtk::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            MediaPool::ensure_type();
            Viewer::ensure_type();
            TimelineView::ensure_type();
            Inspector::ensure_type();
            ClipStrip::ensure_type();
            ScopesPanel::ensure_type();
            ColorWheels::ensure_type();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Window {}
    impl WidgetImpl for Window {}
    impl WindowImpl for Window {}
    impl ApplicationWindowImpl for Window {}
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
            gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    /// The pool, viewer and timeline of the template.
    #[cfg(test)]
    pub(crate) fn imp_children(&self) -> (MediaPool, Viewer, TimelineView) {
        let imp = self.imp();
        (imp.media_pool.get(), imp.viewer.get(), imp.timeline.get())
    }
}

/// Builds the main window. `.otio` files open as the project, other files
/// are imported into the media pool.
pub fn build(app: &gtk::Application, files: &[gio::File]) -> Window {
    let window: Window = glib::Object::builder().property("application", app).build();
    let project = Project::new();
    let pool = window.imp().media_pool.get();
    let viewer = window.imp().viewer.get();
    let timeline = window.imp().timeline.get();
    timeline.set_project(project.clone());
    let playback = Playback::new(&project, &viewer, &timeline);

    let ui = Rc::new(Ui {
        window: window.clone().upcast(),
        project: project.clone(),
        pool: pool.clone(),
        timeline: timeline.clone(),
        playback,
    });
    // The window owns the controller; handlers only hold weak references.
    // The strong one is released when the window leaves the application
    // (closed, or destroyed after "discard changes"). Not on `destroy`:
    // GTK 4 emits that only once the last reference is gone, which the
    // window ↔ ui cycle prevents, so the controller and its render thread
    // outlived the window. The thread then kept decoding while the GPU
    // driver was unloaded at exit, and crashed in the driver.
    let owner = RefCell::new(Some(ui.clone()));
    app.connect_window_removed(glib::clone!(
        #[weak]
        window,
        move |_, removed| {
            if removed != window.upcast_ref::<gtk::Window>() {
                return;
            }
            if let Some(ui) = owner.take() {
                // Other closures may still hold the playback; stop it anyway.
                ui.playback.shutdown();
            }
        }
    ));
    window.imp().inspector.set_timeline(&timeline);
    window.imp().clip_strip.set_up(&timeline, &pool);
    window.imp().color_wheels.set_timeline(&timeline);
    add_page_action(&window, &ui.playback);
    add_transport_actions(window.upcast_ref(), &timeline, &project);
    add_project_actions(&ui);
    add_clip_actions(&ui);
    add_track_actions(&ui);
    pool.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |media| ui.append_media(&media)
    ));
    project.connect_changed(glib::clone!(
        #[weak]
        ui,
        move |_| ui.update()
    ));
    window.connect_close_request(glib::clone!(
        #[weak]
        ui,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |window| {
            if !ui.project.is_modified() {
                return glib::Propagation::Proceed;
            }
            ui.confirm_discard(glib::clone!(
                #[weak]
                window,
                move || window.destroy()
            ));
            glib::Propagation::Stop
        }
    ));

    for file in files {
        match file.path() {
            Some(path) if is_project_file(&path) => ui.open(&path),
            _ => pool.import(file),
        }
    }
    ui.update();
    window
}

fn is_project_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("otio"))
}

struct Ui {
    window: gtk::ApplicationWindow,
    project: Rc<Project>,
    pool: MediaPool,
    timeline: TimelineView,
    playback: Rc<Playback>,
}

impl Ui {
    fn action(&self, name: &str) -> gio::SimpleAction {
        self.window
            .lookup_action(name)
            .and_downcast::<gio::SimpleAction>()
            .expect("window action")
    }

    /// Syncs the title and action states with the project.
    fn update(&self) {
        let project = &self.project;
        self.window.set_title(Some(&project.title()));
        let undo = self.action("undo");
        undo.set_enabled(project.undo_label().is_some());
        let redo = self.action("redo");
        redo.set_enabled(project.redo_label().is_some());

        let fps = project.frame_rate();
        self.action("frame-rate")
            .set_state(&format!("{}/{}", fps.num, fps.den).to_variant());
        let drop_frame = self.action("drop-frame");
        drop_frame.set_enabled(supports_drop_frame(fps));
        drop_frame.set_state(&project.timeline().settings.drop_frame.to_variant());
        let settings = project.timeline().settings.clone();
        self.action("resolution")
            .set_state(&format!("{}x{}", settings.width, settings.height).to_variant());

        // Keep the playhead inside the sequence (plus one frame to see the end).
        let end = project.duration_frames();
        if self.timeline.playhead() > end {
            self.timeline.set_playhead(end);
        }
    }

    fn show_error(&self, message: &str, detail: &str) {
        tracing::warn!("{message}: {detail}");
        gtk::AlertDialog::builder()
            .message(message)
            .detail(detail)
            .modal(true)
            .build()
            .show(Some(&self.window));
    }

    fn open(&self, path: &Path) {
        if let Err(err) = self.project.open(path) {
            self.show_error(
                &fill(&gettext("Cannot open {path}"), &[("path", &path.display())]),
                &err.to_string(),
            );
            return;
        }
        // Import the media the timeline uses, so it can be added again.
        let paths: HashSet<PathBuf> = self
            .project
            .timeline()
            .clips()
            .filter_map(|c| c.media_reference()?.path())
            .collect();
        let mut missing = 0;
        for path in paths {
            if path.exists() {
                self.pool.import(&gio::File::for_path(&path));
            } else {
                tracing::warn!("media offline: {}", path.display());
                missing += 1;
            }
        }
        if missing > 0 {
            tracing::warn!("{missing} media files of {} are missing", path.display());
        }
        self.timeline.set_selection(None);
        self.timeline.set_playhead(0);
    }

    /// Saves to the project's file, or asks for one. Calls `then` after a
    /// successful save.
    fn save(self: &Rc<Self>, then: Option<Box<dyn FnOnce()>>) {
        match self.project.path() {
            Some(path) => {
                if self.save_to(&path)
                    && let Some(then) = then
                {
                    then();
                }
            }
            None => self.save_as(then),
        }
    }

    fn save_to(&self, path: &Path) -> bool {
        match self.project.save_as(path) {
            Ok(()) => true,
            Err(err) => {
                self.show_error(
                    &fill(&gettext("Cannot save {path}"), &[("path", &path.display())]),
                    &err.to_string(),
                );
                false
            }
        }
    }

    fn save_as(self: &Rc<Self>, then: Option<Box<dyn FnOnce()>>) {
        let name = format!("{}.otio", self.project.timeline().name);
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Save Project"))
            .modal(true)
            .initial_name(name)
            .filters(&otio_filters())
            .build();
        let ui = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let Some(mut path) = dialog_path(result) else {
                return;
            };
            if !is_project_file(&path) {
                path.set_extension("otio");
            }
            if ui.save_to(&path)
                && let Some(then) = then
            {
                then();
            }
        });
    }

    fn choose_and_open(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Open Project"))
            .modal(true)
            .filters(&otio_filters())
            .build();
        let ui = self.clone();
        dialog.open(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = dialog_path(result) {
                ui.open(&path);
            }
        });
    }

    /// Runs `proceed` unless the user cancels, offering to save first.
    fn confirm_discard(self: &Rc<Self>, proceed: impl FnOnce() + 'static) {
        if !self.project.is_modified() {
            proceed();
            return;
        }
        let dialog = gtk::AlertDialog::builder()
            .message(gettext("Save changes to the project?"))
            .detail(gettext("Unsaved changes will be lost otherwise."))
            .buttons([gettext("Cancel"), gettext("Discard"), gettext("Save")])
            .cancel_button(0)
            .default_button(2)
            .modal(true)
            .build();
        let ui = self.clone();
        dialog.choose(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |result| match result {
                Ok(1) => proceed(),
                Ok(2) => ui.save(Some(Box::new(proceed))),
                _ => {}
            },
        );
    }

    fn add_track(&self, kind: TrackKind) {
        let _ = self.project.edit(&gettext("Add Track"), |t| {
            t.add_track(kind);
            Ok::<_, EditError>(())
        });
    }

    /// Applies `change` to the selected clip as one undoable edit.
    fn edit_selected_clip(&self, label: &str, change: impl FnOnce(&mut nox_project::Item)) {
        self.timeline.edit_selected_clip(label, None, change);
    }

    /// A one-second dissolve at the selected clip's edge nearest the
    /// playhead.
    fn add_dissolve(&self) {
        let Some(item) = self.timeline.selection() else {
            tracing::info!("select a clip to add a dissolve to");
            return;
        };
        let fps = self.project.frame_rate();
        let playhead = Time::from_frames(self.timeline.playhead(), fps);
        let Ok(range) = self.project.timeline().item_range(item) else {
            return;
        };
        let edge = if (playhead - range.start).as_seconds().abs()
            <= (range.end() - playhead).as_seconds().abs()
        {
            Edge::Start
        } else {
            Edge::End
        };
        let one_second = Time::from_frames(nox_project::nominal_fps(fps) as i64, fps);
        let _ = self.project.edit(&gettext("Add Dissolve"), |t| {
            t.add_dissolve(item, edge, one_second)
        });
    }

    /// Changes the In/Out marks as an undoable edit; `change` gets the
    /// settings and the playhead's frame as a time.
    fn mark(&self, label: &str, change: impl FnOnce(&mut nox_project::Settings, Time)) {
        let fps = self.project.frame_rate();
        let playhead = Time::from_frames(self.timeline.playhead(), fps);
        let _ = self.project.edit(label, |t| {
            change(&mut t.settings, playhead);
            Ok::<_, EditError>(())
        });
    }

    /// Adds `media` at the end of the sequence.
    fn append_media(&self, media: &MediaObject) {
        let Some(info) = media.info() else {
            tracing::info!("{} is still loading", media.name());
            return;
        };
        let end = self.project.duration_frames();
        let _ = self.project.add_media(&info, None, end);
    }
}

fn otio_filters() -> gio::ListStore {
    let otio = gtk::FileFilter::new();
    otio.set_name(Some("OpenTimelineIO")); // no-i18n: a format name
    otio.add_suffix("otio");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&otio);
    filters
}

fn dialog_path(result: Result<gio::File, glib::Error>) -> Option<PathBuf> {
    match result {
        Ok(file) => file.path(),
        Err(err) => {
            if !err.matches(gtk::DialogError::Dismissed) {
                tracing::warn!("file dialog failed: {err}");
            }
            None
        }
    }
}

/// File, edit and sequence actions. The `*-path` variants take a path
/// instead of showing a dialog, for scripting over D-Bus.
/// `win.page` ("edit" or "color"; Shift+4 and Shift+6 as in Resolve)
/// switches pages. The one viewer moves to the visible page, so there is
/// one engine and nothing renders twice.
fn add_page_action(window: &Window, playback: &Rc<Playback>) {
    let imp = window.imp();
    let page = gio::SimpleAction::new_stateful(
        "page",
        Some(glib::VariantTy::STRING),
        &"edit".to_variant(),
    );
    page.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, name| {
            if let Some(name) = name.and_then(|n| n.get::<String>())
                && window.imp().pages.child_by_name(&name).is_some()
            {
                window.imp().pages.set_visible_child_name(&name);
            }
        }
    ));
    let playback = Rc::downgrade(playback);
    imp.pages.connect_visible_child_name_notify(glib::clone!(
        #[weak]
        window,
        #[weak]
        page,
        move |pages| {
            let imp = window.imp();
            let name = pages.visible_child_name().unwrap_or_default();
            page.set_state(&name.to_variant());
            let slot = if name == "color" {
                &imp.color_viewer_slot
            } else {
                &imp.edit_viewer_slot
            };
            // Scopes are measured only while they are visible.
            if let Some(playback) = playback.upgrade() {
                let panel = (name == "color").then(|| imp.scopes.get());
                playback.set_scopes(panel);
            }
            let viewer = imp.viewer.get();
            if viewer.parent().as_ref() != Some(slot.upcast_ref()) {
                // The slots are boxes; the viewer is their only child.
                if let Some(parent) = viewer.parent().and_downcast::<gtk::Box>() {
                    parent.remove(&viewer);
                }
                slot.append(&viewer);
            }
        }
    ));
    window.add_action(&page);

    // The Color page's splitter starts with room for the clip strip and the
    // wheels (380 px) below the viewer (at most half the page); a fixed position
    // hid them on small screens. Once set, the user's position is kept.
    let paned = imp.color_paned.get();
    paned.connect_max_position_notify(|paned| {
        let max = paned.max_position();
        if max > 0 && !paned.is_position_set() {
            paned.set_position((max - 380).max(max / 2));
        }
    });
}

fn add_project_actions(ui: &Rc<Ui>) {
    let simple = |name: &str, f: fn(&Rc<Ui>)| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(glib::clone!(
            #[weak]
            ui,
            move |_, _| f(&ui)
        ));
        action
    };
    let with_path = |name: &str, f: fn(&Rc<Ui>, &Path)| {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        action.connect_activate(glib::clone!(
            #[weak]
            ui,
            move |_, parameter| {
                if let Some(path) = parameter.and_then(|p| p.get::<String>()) {
                    f(&ui, Path::new(&path));
                }
            }
        ));
        action
    };
    let actions = [
        simple("new", |ui| {
            let target = ui.clone();
            ui.confirm_discard(move || {
                target.project.reset();
                target.timeline.set_playhead(0);
            });
        }),
        simple("open", |ui| {
            let target = ui.clone();
            ui.confirm_discard(move || target.choose_and_open());
        }),
        simple("save", |ui| ui.save(None)),
        simple("save-as", |ui| ui.save_as(None)),
        simple("undo", |ui| ui.project.undo()),
        simple("project-settings", |ui| {
            crate::settings_dialog::show(ui.window.upcast_ref(), &ui.project)
        }),
        simple("export", |ui| {
            crate::export_dialog::show(ui.window.upcast_ref(), &ui.project)
        }),
        simple("play-pause", |ui| ui.playback.toggle()),
        simple("play", |ui| ui.playback.play()),
        simple("pause", |ui| ui.playback.pause()),
        simple("redo", |ui| ui.project.redo()),
        simple("delete-clip", |ui| ui.timeline.delete_selection()),
        simple("ripple-delete", |ui| ui.timeline.ripple_delete_selection()),
        // Marks as in Resolve: In at the playhead's frame, Out after it, so
        // the frame under the playhead is included.
        simple("set-in", |ui| {
            ui.mark(&gettext("Mark In"), |s, playhead| {
                s.set_mark_in(Some(playhead))
            })
        }),
        simple("set-out", |ui| {
            let frame = Time::from_frames(1, ui.project.frame_rate());
            ui.mark(&gettext("Mark Out"), |s, playhead| {
                s.set_mark_out(Some(playhead + frame))
            })
        }),
        simple("clear-in", |ui| {
            ui.mark(&gettext("Clear In"), |s, _| s.set_mark_in(None))
        }),
        simple("clear-out", |ui| {
            ui.mark(&gettext("Clear Out"), |s, _| s.set_mark_out(None))
        }),
        simple("clear-in-out", |ui| {
            ui.mark(&gettext("Clear In and Out"), |s, _| {
                s.set_mark_in(None);
                s.set_mark_out(None);
            })
        }),
        simple("split", |ui| {
            let time = Time::from_frames(ui.timeline.playhead(), ui.project.frame_rate());
            let _ = ui
                .project
                .edit(&gettext("Split Clips"), |t| match t.split_at(time) {
                    0 => Err("no clip at the playhead"),
                    _ => Ok(()),
                });
        }),
        simple("add-video-track", |ui| ui.add_track(TrackKind::Video)),
        simple("add-audio-track", |ui| ui.add_track(TrackKind::Audio)),
        with_path("open-path", |ui, path| ui.open(path)),
        with_path("save-path", |ui, path| {
            ui.save_to(path);
        }),
        with_path("append-media", |ui, path| match ui.pool.find(path) {
            Some(media) => ui.append_media(&media),
            None => tracing::warn!("not in the media pool: {}", path.display()),
        }),
    ];
    for action in actions {
        ui.window.add_action(&action);
    }
    if std::env::var("NOX_TEST_ACTIONS").is_ok_and(|v| v == "1") {
        // The topmost dialog over the window (project settings, export).
        ui.window
            .add_action(&with_path("screenshot-dialog", |ui, path| {
                let dialog = gtk::Window::list_toplevels()
                    .into_iter()
                    .filter_map(|w| w.downcast::<gtk::Window>().ok())
                    .find(|w| {
                        w.is_visible() && w.transient_for().as_ref() == Some(ui.window.upcast_ref())
                    });
                match dialog.and_then(|d| d.child()) {
                    Some(content) => {
                        if let Err(err) = screenshot(&content, path) {
                            tracing::warn!("screenshot failed: {err}");
                        }
                    }
                    None => tracing::warn!("no dialog open"),
                }
            }));
        ui.window.add_action(&with_path("screenshot", |ui, path| {
            // A paintable of the toplevel itself renders nothing.
            let content = ui.window.child().expect("window content");
            if let Err(err) = screenshot(&content, path) {
                tracing::warn!("screenshot failed: {err}");
            }
        }));
        // Like the title bar's close button (close-request and all).
        let close = gio::SimpleAction::new("close", None);
        close.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            ui.window,
            move |_, _| window.close()
        ));
        ui.window.add_action(&close);
    }

    let resolution = gio::SimpleAction::new_stateful(
        "resolution",
        Some(glib::VariantTy::STRING),
        &"1920x1080".to_variant(),
    );
    resolution.connect_change_state(glib::clone!(
        #[weak]
        ui,
        move |_, value| {
            let Some(size) = value
                .and_then(|v| v.get::<String>())
                .and_then(|v| parse_size(&v))
            else {
                return;
            };
            let _ = ui.project.edit(&gettext("Change Resolution"), |t| {
                (t.settings.width, t.settings.height) = size;
                Ok::<_, EditError>(())
            });
        }
    ));
    ui.window.add_action(&resolution);

    let frame_rate = gio::SimpleAction::new_stateful(
        "frame-rate",
        Some(glib::VariantTy::STRING),
        &"25/1".to_variant(),
    );
    frame_rate.connect_change_state(glib::clone!(
        #[weak]
        ui,
        move |_, value| {
            let Some(rate) = value
                .and_then(|v| v.get::<String>())
                .and_then(|v| parse_rate(&v))
            else {
                return;
            };
            if rate != ui.project.frame_rate() {
                let _ = ui.project.edit(&gettext("Change Frame Rate"), |t| {
                    t.set_frame_rate(rate);
                    Ok::<_, EditError>(())
                });
            }
        }
    ));
    ui.window.add_action(&frame_rate);

    let drop_frame = gio::SimpleAction::new_stateful("drop-frame", None, &false.to_variant());
    drop_frame.connect_activate(|action, _| {
        let enabled = action
            .state()
            .and_then(|s| s.get::<bool>())
            .unwrap_or(false);
        action.change_state(&(!enabled).to_variant());
    });
    drop_frame.connect_change_state(glib::clone!(
        #[weak]
        ui,
        move |_, value| {
            let Some(enabled) = value.and_then(|v| v.get::<bool>()) else {
                return;
            };
            // The state itself follows the project in `Ui::update`.
            let _ = ui.project.edit(&gettext("Change Timecode"), |t| {
                t.settings.drop_frame = enabled;
                Ok::<_, EditError>(())
            });
        }
    ));
    ui.window.add_action(&drop_frame);

    let snapping = gio::SimpleAction::new_stateful("snapping", None, &true.to_variant());
    snapping.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |action, _| {
            let enabled = !ui.timeline.snapping();
            ui.timeline.set_snapping(enabled);
            action.set_state(&enabled.to_variant());
        }
    ));
    ui.window.add_action(&snapping);
}

/// Renders `widget` with GTK's renderer and saves it as PNG.
fn screenshot(widget: &gtk::Widget, path: &Path) -> Result<(), glib::BoolError> {
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    // The window background isn't part of its child; use the dark theme's.
    snapshot.append_color(
        &gdk::RGBA::new(0.14, 0.14, 0.14, 1.0),
        &gtk::graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32),
    );
    paintable.snapshot(&snapshot, widget.width().into(), widget.height().into());
    let node = snapshot
        .to_node()
        .ok_or_else(|| glib::bool_error!("nothing to render"))?;
    let renderer = widget
        .native()
        .ok_or_else(|| glib::bool_error!("not realized"))?
        .renderer()
        .ok_or_else(|| glib::bool_error!("no renderer"))?;
    renderer.render_texture(&node, None).save_to_png(path)?;
    tracing::info!("screenshot saved to {}", path.display());
    Ok(())
}

fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    let (width, height) = (width.parse().ok()?, height.parse().ok()?);
    (width >= 16 && height >= 16 && width <= 8192 && height <= 8192).then_some((width, height))
}

/// Actions on the selected clip, for its context menu (and D-Bus):
/// `clip-opacity(d)`, `clip-blend(s)`, `clip-volume(d)` and
/// `add-dissolve`, which adds a one-second dissolve at the selected
/// clip's edge nearest the playhead.
fn add_clip_actions(ui: &Rc<Ui>) {
    let with_param = |name: &str, ty: &glib::VariantTy, f: fn(&Ui, &glib::Variant)| {
        let action = gio::SimpleAction::new(name, Some(ty));
        action.connect_activate(glib::clone!(
            #[weak]
            ui,
            move |_, parameter| {
                if let Some(parameter) = parameter {
                    f(&ui, parameter);
                }
            }
        ));
        ui.window.add_action(&action);
    };
    with_param("clip-opacity", glib::VariantTy::DOUBLE, |ui, value| {
        if let Some(opacity) = value.get::<f64>() {
            ui.edit_selected_clip(&gettext("Change Opacity"), |item| item.set_opacity(opacity));
        }
    });
    with_param("clip-blend", glib::VariantTy::STRING, |ui, value| {
        if let Some(mode) = value.get::<String>().and_then(|m| BlendMode::parse(&m)) {
            ui.edit_selected_clip(&gettext("Change Blend Mode"), |item| {
                item.set_blend_mode(mode)
            });
        }
    });
    with_param("clip-volume", glib::VariantTy::DOUBLE, |ui, value| {
        if let Some(volume) = value.get::<f64>() {
            ui.edit_selected_clip(&gettext("Change Volume"), |item| item.set_volume(volume));
        }
    });
    // By stack and track index, for scripting; `(-1, -1)` clears.
    let select = gio::SimpleAction::new("select-clip", Some(glib::VariantTy::new("(xx)").unwrap()));
    select.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_, parameter| {
            let item = parameter
                .and_then(|p| p.get::<(i64, i64)>())
                .and_then(|(track, index)| {
                    Some(nox_project::ItemRef {
                        track: usize::try_from(track).ok()?,
                        index: usize::try_from(index).ok()?,
                    })
                })
                .filter(|&item| ui.project.timeline().item(item).is_ok());
            ui.timeline.set_selection(item);
        }
    ));
    ui.window.add_action(&select);
    let dissolve = gio::SimpleAction::new("add-dissolve", None);
    dissolve.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_, _| ui.add_dissolve()
    ));
    ui.window.add_action(&dissolve);
}

/// Track actions by stack index, for scripting (the track header menus use
/// the timeline's own `track.*` actions): `delete-track(x)`,
/// `set-track-enabled((xb))` (mute or hide), `set-track-locked((xb))` and
/// `rename-track((xs))`.
fn add_track_actions(ui: &Rc<Ui>) {
    let with_param = |name: &str, ty: &str, f: fn(&Ui, &glib::Variant)| {
        let ty = glib::VariantTy::new(ty).expect("variant type");
        let action = gio::SimpleAction::new(name, Some(ty));
        action.connect_activate(glib::clone!(
            #[weak]
            ui,
            move |_, parameter| {
                if let Some(parameter) = parameter {
                    f(&ui, parameter);
                }
            }
        ));
        ui.window.add_action(&action);
    };
    fn index(i: i64) -> Option<usize> {
        usize::try_from(i).ok()
    }
    with_param("delete-track", "x", |ui, value| {
        if let Some(track) = value.get::<i64>().and_then(index) {
            ui.timeline.delete_track(track);
        }
    });
    with_param("set-track-enabled", "(xb)", |ui, value| {
        if let Some((track, on)) = value.get::<(i64, bool)>()
            && let Some(track) = index(track)
        {
            ui.timeline.set_track_enabled(track, on);
        }
    });
    with_param("set-track-locked", "(xb)", |ui, value| {
        if let Some((track, on)) = value.get::<(i64, bool)>()
            && let Some(track) = index(track)
        {
            ui.timeline.set_track_locked(track, on);
        }
    });
    with_param("rename-track", "(xs)", |ui, value| {
        if let Some((track, name)) = value.get::<(i64, String)>()
            && let Some(track) = index(track)
        {
            ui.timeline.rename_track(track, &name);
        }
    });
}

fn parse_rate(text: &str) -> Option<Rational> {
    let (num, den) = text.split_once('/')?;
    let (num, den) = (num.parse().ok()?, den.parse().ok()?);
    (num > 0 && den > 0).then(|| Rational::new(num, den))
}

/// `win.seek(frame)`, `win.next-frame`, `win.previous-frame`,
/// `win.go-to-start` and `win.go-to-end`, used by the transport buttons and
/// shortcuts (and reachable over D-Bus via org.gtk.Actions, which tests use).
fn add_transport_actions(
    window: &gtk::ApplicationWindow,
    timeline: &TimelineView,
    project: &Rc<Project>,
) {
    let seek = gio::ActionEntry::builder("seek")
        .parameter_type(Some(&i64::static_variant_type()))
        .activate(glib::clone!(
            #[weak]
            timeline,
            move |_: &gtk::ApplicationWindow, _, parameter| {
                if let Some(frame) = parameter.and_then(|p| p.get::<i64>()) {
                    timeline.set_playhead(frame.max(0));
                }
            }
        ))
        .build();
    let step = |name: &str, delta: i64| {
        gio::ActionEntry::builder(name)
            .activate(glib::clone!(
                #[weak]
                timeline,
                move |_: &gtk::ApplicationWindow, _, _| {
                    timeline.set_playhead((timeline.playhead() + delta).max(0));
                }
            ))
            .build()
    };
    let go_to_start = gio::ActionEntry::builder("go-to-start")
        .activate(glib::clone!(
            #[weak]
            timeline,
            move |_: &gtk::ApplicationWindow, _, _| timeline.set_playhead(0)
        ))
        .build();
    let go_to_end = gio::ActionEntry::builder("go-to-end")
        .activate(glib::clone!(
            #[weak]
            timeline,
            #[strong]
            project,
            // The last frame of the sequence.
            move |_: &gtk::ApplicationWindow, _, _| {
                timeline.set_playhead((project.duration_frames() - 1).max(0))
            }
        ))
        .build();
    let timeline_entry = |name: &str, f: fn(&TimelineView)| {
        gio::ActionEntry::builder(name)
            .activate(glib::clone!(
                #[weak]
                timeline,
                move |_: &gtk::ApplicationWindow, _, _| f(&timeline)
            ))
            .build()
    };
    window.add_action_entries([
        seek,
        step("previous-frame", -1),
        step("next-frame", 1),
        go_to_start,
        go_to_end,
        timeline_entry("previous-edit", |t| t.go_to_edit_point(false)),
        timeline_entry("next-edit", |t| t.go_to_edit_point(true)),
        timeline_entry(
            "select-clip-at-playhead",
            TimelineView::select_clip_at_playhead,
        ),
    ]);
}
