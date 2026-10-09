use gtk::{gdk, gio, glib, prelude::*};
use nox_engine::Preview;
use nox_project::{EditError, Rational, Time, TrackKind, supports_drop_frame};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::media_object::MediaObject;
use crate::project::Project;
use crate::{media_pool::MediaPool, timeline::TimelineView, viewer::Viewer};

/// Sequence frame rates offered in the menu.
const FRAME_RATES: &[(&str, i64, i64)] = &[
    ("23.976", 24000, 1001),
    ("24", 24, 1),
    ("25", 25, 1),
    ("29.97", 30000, 1001),
    ("30", 30, 1),
    ("50", 50, 1),
    ("59.94", 60000, 1001),
    ("60", 60, 1),
];

/// Builds the main window. `.otio` files open as the project, other files
/// are imported into the media pool.
pub fn build(app: &gtk::Application, files: &[gio::File]) -> gtk::ApplicationWindow {
    let project = Project::new();
    let pool = Rc::new(MediaPool::new());
    let viewer = Rc::new(Viewer::new());
    let timeline = TimelineView::new(project.clone());
    connect_preview(&project, &viewer, &timeline);

    let top = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&pool.widget)
        .end_child(&viewer.widget)
        .resize_start_child(false)
        .shrink_start_child(false)
        .position(340)
        .wide_handle(true)
        .build();

    let main = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(&top)
        .end_child(&timeline)
        .shrink_end_child(false)
        .position(500)
        // A thin handle's invisible grab area covers the top of the ruler.
        .wide_handle(true)
        .build();

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .default_width(1400)
        .default_height(860)
        .titlebar(&header_bar())
        .child(&main)
        .build();

    let ui = Rc::new(Ui {
        window: window.clone(),
        project: project.clone(),
        pool: pool.clone(),
        timeline: timeline.clone(),
    });
    // The window owns the controller; handlers only hold weak references.
    // GTK drops this handler on destroy, which breaks the window ↔ ui cycle.
    window.connect_destroy(glib::clone!(
        #[strong]
        ui,
        move |_| {
            let _ = &ui;
        }
    ));
    add_transport_actions(&window, &timeline, &project);
    add_project_actions(&ui);
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

fn header_bar() -> gtk::HeaderBar {
    let header = gtk::HeaderBar::new();
    let button = |icon: &str, tooltip: &str, action: &str| {
        gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .action_name(action)
            .build()
    };
    header.pack_start(&button(
        "document-open-symbolic",
        "Open… (Ctrl+O)",
        "win.open",
    ));
    header.pack_start(&button(
        "document-save-symbolic",
        "Save (Ctrl+S)",
        "win.save",
    ));
    header.pack_start(&button("edit-undo-symbolic", "Undo (Ctrl+Z)", "win.undo"));
    header.pack_start(&button(
        "edit-redo-symbolic",
        "Redo (Ctrl+Shift+Z)",
        "win.redo",
    ));

    let file_menu = gio::Menu::new();
    file_menu.append(Some("New Project"), Some("win.new"));
    file_menu.append(Some("Open…"), Some("win.open"));
    file_menu.append(Some("Save"), Some("win.save"));
    file_menu.append(Some("Save As…"), Some("win.save-as"));

    let rates = gio::Menu::new();
    for (label, num, den) in FRAME_RATES {
        let item = gio::MenuItem::new(Some(&format!("{label} fps")), None);
        item.set_action_and_target_value(
            Some("win.frame-rate"),
            Some(&format!("{num}/{den}").to_variant()),
        );
        rates.append_item(&item);
    }
    let sequence = gio::Menu::new();
    sequence.append_submenu(Some("Frame Rate"), &rates);
    sequence.append(Some("Drop-Frame Timecode"), Some("win.drop-frame"));
    sequence.append(Some("Split at Playhead"), Some("win.split"));
    sequence.append(Some("Add Video Track"), Some("win.add-video-track"));
    sequence.append(Some("Add Audio Track"), Some("win.add-audio-track"));
    sequence.append(Some("Snapping (N)"), Some("win.snapping"));

    let menu = gio::Menu::new();
    menu.append_section(None, &file_menu);
    menu.append_section(Some("Sequence"), &sequence);
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .menu_model(&menu)
            .primary(true)
            .build(),
    );
    header
}

struct Ui {
    window: gtk::ApplicationWindow,
    project: Rc<Project>,
    pool: Rc<MediaPool>,
    timeline: TimelineView,
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
            self.show_error(&format!("Cannot open {}", path.display()), &err.to_string());
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
                self.show_error(&format!("Cannot save {}", path.display()), &err.to_string());
                false
            }
        }
    }

    fn save_as(self: &Rc<Self>, then: Option<Box<dyn FnOnce()>>) {
        let name = format!("{}.otio", self.project.timeline().name);
        let dialog = gtk::FileDialog::builder()
            .title("Save Project")
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
            .title("Open Project")
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
            .message("Save changes to the project?")
            .detail("Unsaved changes will be lost otherwise.")
            .buttons(["Cancel", "Discard", "Save"])
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
        let _ = self.project.edit("Add Track", |t| {
            t.add_track(kind);
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
    otio.set_name(Some("OpenTimelineIO"));
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
        simple("redo", |ui| ui.project.redo()),
        simple("delete-clip", |ui| ui.timeline.delete_selection()),
        simple("split", |ui| {
            let time = Time::from_frames(ui.timeline.playhead(), ui.project.frame_rate());
            let _ = ui.project.edit("Split", |t| match t.split_at(time) {
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
        ui.window.add_action(&with_path("screenshot", |ui, path| {
            // A paintable of the toplevel itself renders nothing.
            let content = ui.window.child().expect("window content");
            if let Err(err) = screenshot(&content, path) {
                tracing::warn!("screenshot failed: {err}");
            }
        }));
    }

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
                let _ = ui.project.edit("Change Frame Rate", |t| {
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
            let _ = ui.project.edit("Change Timecode", |t| {
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
    window.add_action_entries([
        seek,
        step("previous-frame", -1),
        step("next-frame", 1),
        go_to_start,
        go_to_end,
    ]);
}

/// DRM modifiers GTK can import for RGBA8 frames, i.e. whether the engine
/// may hand over frames as DMA-BUF. `NOX_PRESENT=readback` disables it.
fn importable_modifiers() -> Vec<u64> {
    if std::env::var("NOX_PRESENT").is_ok_and(|v| v == "readback") {
        tracing::info!("NOX_PRESENT=readback: not using DMA-BUF");
        return Vec::new();
    }
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    let formats = display.dmabuf_formats();
    (0..formats.n_formats())
        .map(|i| formats.format(i))
        .filter(|&(fourcc, _)| fourcc == nox_engine::FOURCC_ABGR8888)
        .map(|(_, modifier)| modifier)
        .collect()
}

/// The viewer shows the topmost video clip at the playhead. Compositing of
/// all layers comes with the playback engine (M4).
fn connect_preview(project: &Rc<Project>, viewer: &Rc<Viewer>, timeline: &TimelineView) {
    let (sender, receiver) = async_channel::unbounded();
    let preview = Rc::new(Preview::new(importable_modifiers(), move |frame| {
        let _ = sender.send_blocking(frame);
    }));
    // The latest request (0: none, the viewer is cleared).
    let latest = Rc::new(Cell::new(0u64));
    // What the latest request was for, to skip repeating it.
    let requested: Rc<RefCell<Option<(PathBuf, Time)>>> = Rc::default();
    let offline: Rc<RefCell<HashSet<PathBuf>>> = Rc::default();

    let request = Rc::new(glib::clone!(
        #[strong]
        preview,
        #[strong]
        project,
        #[strong]
        viewer,
        #[strong]
        latest,
        #[strong]
        requested,
        #[weak]
        timeline,
        move || {
            let frame = timeline.playhead();
            viewer.timecode.set_text(&project.timecode(frame));
            let source = {
                let sequence = project.timeline();
                let time = Time::from_frames(frame, sequence.frame_rate());
                sequence.video_at(time).and_then(|resolved| {
                    let path = resolved.clip.media_reference()?.path()?;
                    // Source times count from the available range's start
                    // (e.g. a start timecode); the decoder from zero.
                    let start = resolved
                        .clip
                        .available_range()
                        .map_or(Time::zero(time.timebase), |r| r.start);
                    Some((path, resolved.source_time - start))
                })
            };
            match source {
                Some((path, _)) if !path.exists() => {
                    if offline.borrow_mut().insert(path.clone()) {
                        tracing::warn!("media offline: {}", path.display());
                    }
                    latest.set(0);
                    requested.replace(None);
                    viewer.clear();
                }
                Some(source) => {
                    if requested.borrow().as_ref() != Some(&source) {
                        latest.set(preview.request(&source.0, source.1));
                        requested.replace(Some(source));
                    }
                }
                None => {
                    latest.set(0);
                    requested.replace(None);
                    viewer.clear();
                }
            }
        }
    ));

    glib::spawn_future_local(glib::clone!(
        #[strong]
        viewer,
        #[strong]
        preview,
        #[strong]
        request,
        async move {
            while let Ok(result) = receiver.recv().await {
                match result {
                    // Drop frames of superseded requests.
                    Ok(frame) if frame.request == latest.get() => {
                        if let Err(err) = viewer.show_frame(frame.image) {
                            tracing::warn!(
                                "GTK cannot import DMA-BUF frames ({err}), falling back to readback"
                            );
                            preview.disable_dmabuf();
                            requested.replace(None);
                            request();
                        }
                    }
                    Ok(_) => {}
                    Err((id, err)) => {
                        tracing::warn!("preview: {err}");
                        if id == latest.get() {
                            viewer.clear();
                        }
                    }
                }
            }
        }
    ));

    timeline.connect_playhead_changed(glib::clone!(
        #[strong]
        request,
        move |_, _| request()
    ));
    project.connect_changed(move |_| request());
}
