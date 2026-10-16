//! Timeline: ruler, tracks with their clips, and the playhead, drawn
//! directly with `snapshot()`.
//!
//! Drag on the ruler or empty space to scrub, drag a clip to move it (also
//! to another track of the same kind), drag a clip's edge to trim it. With
//! Ctrl held, a trim ripples: what follows on the track moves along. The
//! middle of a cut between two clips rolls the cut. Drop media from the
//! media pool onto a track to add it. Scroll to pan, Ctrl+scroll to zoom,
//! N toggles snapping, Delete removes the selected clip and Shift+Delete
//! ripple-deletes it. The In/Out marks show on the ruler, with everything
//! outside them shaded.
//!
//! Track headers have toggles for mute (video: hide) and lock; their
//! right-click menu also renames and deletes the track. Locked tracks
//! can't be edited. Edits go through the project, so they can be undone.

use gtk::{gdk, gio, glib, graphene, gsk, prelude::*, subclass::prelude::*};
use nox_project::{
    Composable, Edge, EditError, ItemRef, Time, TimeRange, Timeline, Track, TrackKind,
};
use std::rc::Rc;

use crate::media_object::MediaObject;
use crate::project::Project;
use crate::track_header::{Toggle, TrackHeader};

mod drag;
mod draw;
mod geometry;
mod input;
mod selection;
mod tracks;

#[cfg(test)]
pub(crate) use tracks::rename_dialog;

const RULER_HEIGHT: f32 = 24.0;
const TRACK_HEIGHT: f32 = 44.0;
const HEADER_WIDTH: f32 = 112.0;
const MIN_TICK_SPACING: f64 = 90.0;
/// How close (in pixels) the pointer must be to a clip edge to trim.
const EDGE_GRAB: f64 = 6.0;
/// How close the pointer must be to a cut between two clips to roll it.
const ROLL_GRAB: f64 = 3.0;
/// Fade handles: grab distance, and how far down from a clip's top.
const FADE_GRAB: f64 = 6.0;
const FADE_GRAB_HEIGHT: f64 = 12.0;
/// Clips narrower than this (pixels) show no fade handles.
const FADE_MIN_WIDTH: f64 = 30.0;
const FADE_SHADE: gdk::RGBA = gdk::RGBA::new(0.0, 0.0, 0.0, 0.35);
const KEYFRAME: gdk::RGBA = rgb(0xf0, 0x8a, 0x40);
/// Size and spacing of the toggles in track headers.
/// Snapping distance in pixels.
const SNAP_DISTANCE: f64 = 8.0;

const BACKGROUND: gdk::RGBA = rgb(0x1a, 0x1b, 0x1e);
const RULER: gdk::RGBA = rgb(0x26, 0x27, 0x2b);
const TRACK_HEADER: gdk::RGBA = rgb(0x2b, 0x2c, 0x31);
const VIDEO_TRACK: gdk::RGBA = rgb(0x22, 0x24, 0x2a);
const AUDIO_TRACK: gdk::RGBA = rgb(0x20, 0x26, 0x24);
const VIDEO_CLIP: gdk::RGBA = rgb(0x3a, 0x5a, 0x8c);
const AUDIO_CLIP: gdk::RGBA = rgb(0x35, 0x7a, 0x5a);
const NESTED_CLIP: gdk::RGBA = rgb(0x6a, 0x4a, 0x8c);
/// Clips whose media file is missing.
const OFFLINE_CLIP: gdk::RGBA = rgb(0x8c, 0x2e, 0x2e);
const CLIP_BORDER: gdk::RGBA = rgb(0x12, 0x13, 0x15);
const SELECTION: gdk::RGBA = rgb(0xf0, 0xb0, 0x30);
const SNAP_LINE: gdk::RGBA = rgb(0xf0, 0xd0, 0x40);
const TICK: gdk::RGBA = rgb(0x70, 0x72, 0x78);
const TEXT: gdk::RGBA = rgb(0xb0, 0xb2, 0xb8);
const CLIP_TEXT: gdk::RGBA = rgb(0xe8, 0xea, 0xee);
const PLAYHEAD: gdk::RGBA = rgb(0xe5, 0x3b, 0x3b);
const MARK: gdk::RGBA = rgb(0x8c, 0xb4, 0xf0);
const OUTSIDE_MARKS: gdk::RGBA = gdk::RGBA::new(0.0, 0.0, 0.0, 0.4);

const fn rgb(r: u8, g: u8, b: u8) -> gdk::RGBA {
    gdk::RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}

/// The color of an OTIO marker color name.
fn marker_color(name: &str) -> gdk::RGBA {
    match name {
        "RED" => rgb(0xE0, 0x3E, 0x3E),
        "PINK" => rgb(0xF0, 0x8A, 0xC0),
        "ORANGE" => rgb(0xF0, 0x8C, 0x2E),
        "YELLOW" => rgb(0xF2, 0xD0, 0x3A),
        "GREEN" => rgb(0x4C, 0xC0, 0x5A),
        "CYAN" => rgb(0x3E, 0xC8, 0xD8),
        "PURPLE" => rgb(0x96, 0x6C, 0xE0),
        "MAGENTA" => rgb(0xD8, 0x4C, 0xD8),
        "WHITE" => rgb(0xF0, 0xF0, 0xF0),
        "BLACK" => rgb(0x30, 0x30, 0x30),
        _ => rgb(0x3E, 0x82, 0xF0), // BLUE and unknown names
    }
}

fn with_alpha(color: gdk::RGBA, alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(color.red(), color.green(), color.blue(), alpha)
}

/// Stack indices of the top-level tracks in display order: video tracks
/// with the topmost layer first, then audio and other tracks.
fn rows(timeline: &Timeline) -> Vec<usize> {
    let mut rows: Vec<usize> = timeline
        .tracks()
        .rev()
        .filter(|(_, t)| t.kind == TrackKind::Video)
        .map(|(i, _)| i)
        .collect();
    rows.extend(
        timeline
            .tracks()
            .filter(|(_, t)| t.kind != TrackKind::Video)
            .map(|(i, _)| i),
    );
    rows
}

/// A clip's name plus whatever differs from the defaults.
/// A track's name, or a default like V1 or A2 from its kind and row.
fn track_name(track: &Track, row: usize) -> String {
    if track.item.name.is_empty() {
        format!("{}{}", &track.kind.as_str()[..1], row + 1)
    } else {
        track.item.name.clone()
    }
}

fn clip_label(child: &Composable) -> String {
    let mut label = child.name().to_owned();
    if let Composable::Clip(clip) = child {
        let item = &clip.item;
        if item.is_frozen() {
            label += &format!(" · {}", crate::i18n::gettext("Freeze"));
        } else if item.speed() != 1.0 {
            // As a multiplier, not to be confused with the opacity.
            let speed = format!("{:.2}", item.speed());
            let speed = speed.trim_end_matches('0').trim_end_matches('.');
            label += &format!(" · {speed}×"); // no-i18n: a number
        }
        if item.opacity() < 1.0 {
            label += &format!(" · {:.0}%", item.opacity() * 100.0);
        }
        if item.blend_mode() != nox_project::BlendMode::Normal {
            label += &format!(" · {}", crate::i18n::blend_mode_name(item.blend_mode()));
        }
        if item.volume() != 1.0 {
            label += &if item.volume() == 0.0 {
                format!(" · {}", crate::i18n::gettext("muted"))
            } else {
                format!(" · {:+.0} dB", 20.0 * item.volume().log10())
            };
        }
    }
    label
}

/// The clip context menu (`resources/ui/menus.ui`); its actions work on
/// the selection.
pub(crate) fn clip_menu() -> gtk::gio::MenuModel {
    menu_model("clip_menu")
}

pub(crate) fn ruler_menu() -> gtk::gio::MenuModel {
    menu_model("ruler_menu")
}

fn menu_model(id: &str) -> gtk::gio::MenuModel {
    let builder = gtk::Builder::new();
    builder.set_translation_domain(Some(crate::i18n::DOMAIN));
    builder
        .add_from_resource("/io/github/feliwir/Nox/ui/menus.ui")
        .expect("menus.ui is valid");
    builder
        .object(id)
        .unwrap_or_else(|| panic!("{id} in menus.ui"))
}

/// The track header menus for video and audio tracks (`menus.ui`); their
/// `track.*` actions work on the track the menu was opened for.
pub(crate) fn track_menus() -> (gio::MenuModel, gio::MenuModel) {
    let builder = gtk::Builder::new();
    builder.set_translation_domain(Some(crate::i18n::DOMAIN));
    builder
        .add_from_resource("/io/github/feliwir/Nox/ui/menus.ui")
        .expect("menus.ui is valid");
    (
        builder
            .object("video_track_menu")
            .expect("video_track_menu"),
        builder
            .object("audio_track_menu")
            .expect("audio_track_menu"),
    )
}

/// Undo labels (and menu texts) for toggling a track.
fn enable_label(kind: &TrackKind, enabled: bool) -> String {
    use crate::i18n::gettext;
    match (kind, enabled) {
        (TrackKind::Audio, true) => gettext("Unmute Track"),
        (TrackKind::Audio, false) => gettext("Mute Track"),
        (_, true) => gettext("Enable Track"),
        (_, false) => gettext("Disable Track"),
    }
}

fn lock_label(locked: bool) -> String {
    if locked {
        crate::i18n::gettext("Lock Track")
    } else {
        crate::i18n::gettext("Unlock Track")
    }
}

fn row_y(row: usize) -> f32 {
    RULER_HEIGHT + row as f32 * TRACK_HEIGHT
}

/// An interaction in progress.
#[derive(Debug, Clone)]
enum Drag {
    Scrub,
    /// Slipping (`slide: false`) or sliding clip `item` by `delta`, within
    /// `min..=max` (`None`: no limit).
    Shift {
        item: ItemRef,
        slide: bool,
        range: TimeRange,
        min: Option<Time>,
        max: Option<Time>,
        delta: Time,
    },
    /// Dragging the fade handle at `edge` of clip `item`: the fade was
    /// `start` seconds, may grow to `max`; edits merge under `key`.
    Fade {
        item: ItemRef,
        edge: Edge,
        start: f64,
        max: f64,
        key: String,
    },
    /// Dragging the `edge` of transition `item`, which was `start` seconds
    /// long; it stays centered on the cut, so it changes by twice the
    /// movement. Edits merge under `key`.
    TransitionLength {
        item: ItemRef,
        edge: Edge,
        start: f64,
        key: String,
    },
    /// Moving the keyframes of clip `item` now at media time `at` (they
    /// started at `origin`), within `min..=max` (the neighboring keyframes
    /// and the clip). `trimmed_start` and `speed` map media time to the
    /// timeline. Edits merge under `key`.
    Keyframe {
        item: ItemRef,
        origin: f64,
        at: f64,
        min: f64,
        max: f64,
        trimmed_start: f64,
        speed: f64,
        key: String,
    },
    /// Moving timeline marker `index` from `from` to `to`.
    Marker {
        index: usize,
        from: Time,
        to: Time,
        snap_points: Vec<f64>,
    },
    /// A rubber band from `from` to `to` (widget pixels), selecting the
    /// clips it touches in addition to `base`.
    Band {
        from: (f64, f64),
        to: (f64, f64),
        base: Vec<ItemRef>,
    },
    Move {
        /// The clip the drag started on.
        item: ItemRef,
        /// The selected clips moving.
        items: Vec<ItemRef>,
        /// Whether they can change tracks (all on `item`'s track).
        single_track: bool,
        /// Items moving along (selected and linked, including `item`).
        linked: Vec<(ItemRef, TimeRange)>,
        range: TimeRange,
        kind: TrackKind,
        /// Snap targets in frames, without the moving items' own edges.
        snap_points: Vec<f64>,
        offset: Time,
        target_track: usize,
        snapped: Option<f64>,
    },
    Trim {
        item: ItemRef,
        edge: Edge,
        /// Ripple trim: what follows moves along.
        ripple: bool,
        range: TimeRange,
        min: Time,
        max: Option<Time>,
        snap_points: Vec<f64>,
        to: Time,
        snapped: Option<f64>,
    },
    Roll {
        item: ItemRef,
        edge: Edge,
        /// Where the cut was.
        cut: Time,
        min: Time,
        max: Time,
        snap_points: Vec<f64>,
        to: Time,
        snapped: Option<f64>,
    },
}

/// What is under the pointer.
enum Hit {
    Ruler,
    /// The fade handle at `edge` of a clip.
    Fade {
        item: ItemRef,
        edge: Edge,
    },
    /// An edge of a transition, which changes its length.
    TransitionEdge {
        item: ItemRef,
        edge: Edge,
    },
    /// The keyframes of clip `item` at media time `time`.
    Keyframe {
        item: ItemRef,
        time: f64,
    },
    /// A timeline marker on the ruler.
    Marker(usize),
    /// A clip or transition; near an edge of a clip, the edge, and in the
    /// middle of a cut between two clips, `roll`.
    Item {
        item: ItemRef,
        edge: Option<Edge>,
        roll: bool,
    },
    Track,
    /// A track header (outside its toggles, which are widgets).
    Header {
        track: usize,
    },
    Nothing,
}

mod imp {
    use super::*;
    use std::cell::{Cell, OnceCell, RefCell};
    use std::sync::OnceLock;

    pub struct TimelineView {
        pub project: OnceCell<Rc<Project>>,
        pub playhead: Cell<i64>,
        /// Frame shown at the left edge of the track area.
        pub scroll: Cell<f64>,
        pub pixels_per_frame: Cell<f64>,
        /// The primary selected item: what the inspector and the color
        /// wheels show.
        pub selection: Cell<Option<ItemRef>>,
        /// Further selected items (multi-selection), without `selection`.
        pub others: RefCell<Vec<ItemRef>>,
        /// The selected timeline marker; exclusive with clip selection.
        pub marker: Cell<Option<usize>>,
        pub snapping: Cell<bool>,
        pub(super) drag: RefCell<Option<Drag>>,
        /// Media being dragged over the timeline: stack index and frame.
        pub drop_hint: Cell<Option<(usize, i64)>>,
        pub pointer_x: Cell<f64>,
        /// Context menu for clips.
        pub menu: OnceCell<gtk::PopoverMenu>,
        /// Context menu for the ruler.
        pub ruler_menu: OnceCell<gtk::PopoverMenu>,
        /// Context menus for video and audio tracks.
        pub track_menus: OnceCell<(gtk::PopoverMenu, gtk::PopoverMenu)>,
        /// The track (stack index) the track menu was opened for.
        pub menu_track: Cell<Option<usize>>,
        /// The `track.*` actions of the track menus.
        pub track_actions: gio::SimpleActionGroup,
        /// Media files of the timeline that don't exist, checked after
        /// every change.
        pub offline: RefCell<std::collections::BTreeSet<std::path::PathBuf>>,
        /// Waveforms of audio clips' files.
        pub waveforms: OnceCell<Rc<crate::waveforms::Waveforms>>,
        /// Filmstrips of video clips' files.
        pub thumbnails: OnceCell<Rc<crate::thumbnails::Thumbnails>>,
        /// One header per track row, in row order.
        pub headers: RefCell<Vec<TrackHeader>>,
        /// The clip under the pointer, which shows its fade handles.
        pub hovered: Cell<Option<ItemRef>>,
        /// Numbers fade drags, so each is its own undo step.
        pub fade_drags: Cell<u32>,
    }

    impl Default for TimelineView {
        fn default() -> Self {
            Self {
                project: OnceCell::new(),
                playhead: Cell::new(0),
                scroll: Cell::new(0.0),
                pixels_per_frame: Cell::new(4.0),
                selection: Cell::new(None),
                others: RefCell::default(),
                marker: Cell::new(None),
                snapping: Cell::new(true),
                drag: RefCell::new(None),
                drop_hint: Cell::new(None),
                pointer_x: Cell::new(0.0),
                menu: OnceCell::new(),
                ruler_menu: OnceCell::new(),
                track_menus: OnceCell::new(),
                menu_track: Cell::new(None),
                track_actions: gio::SimpleActionGroup::new(),
                offline: RefCell::default(),
                waveforms: OnceCell::new(),
                thumbnails: OnceCell::new(),
                headers: RefCell::default(),
                hovered: Cell::new(None),
                fade_drags: Cell::new(0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TimelineView {
        const NAME: &'static str = "NoxTimelineView";
        type Type = super::TimelineView;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("timeline");
            // Drawn in code: one accessible element with a name and the
            // keys to use; the selection is announced (`set_selection`).
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }
    }

    impl ObjectImpl for TimelineView {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    glib::subclass::Signal::builder("playhead-changed")
                        .param_types([i64::static_type()])
                        .build(),
                    // The selected item changed (or went away).
                    glib::subclass::Signal::builder("selection-changed").build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let view = obj.downgrade();
            let redraw = move || {
                if let Some(view) = view.upgrade() {
                    view.queue_draw();
                }
            };
            let _ = self
                .waveforms
                .set(crate::waveforms::Waveforms::new(redraw.clone()));
            let _ = self
                .thumbnails
                .set(crate::thumbnails::Thumbnails::new(redraw));
            // Thumbnails and waveforms come and go with the preferences.
            let preferences = crate::preferences::Preferences::get();
            for property in ["timeline-thumbnails", "timeline-waveforms"] {
                preferences.connect_notify_local(
                    Some(property),
                    glib::clone!(
                        #[weak]
                        obj,
                        move |_, _| obj.queue_draw()
                    ),
                );
            }
            self.set_up_input();
        }

        fn dispose(&self) {
            for header in self.headers.take() {
                header.unparent();
            }
            if let Some(menu) = self.ruler_menu.get() {
                menu.unparent();
            }
            if let Some(menu) = self.menu.get() {
                menu.unparent();
            }
            if let Some((video, audio)) = self.track_menus.get() {
                video.unparent();
                audio.unparent();
            }
        }
    }

    impl WidgetImpl for TimelineView {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            match orientation {
                gtk::Orientation::Horizontal => (200, 200, -1, -1),
                _ => {
                    let rows = self.project.get().map_or(2, |p| rows(&p.timeline()).len());
                    // Any height works since drawing is clipped.
                    let natural = RULER_HEIGHT + TRACK_HEIGHT * rows as f32;
                    (0, natural as i32, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            for (row, header) in self.headers.borrow().iter().enumerate() {
                header.size_allocate(
                    &gdk::Rectangle::new(
                        0,
                        row_y(row) as i32,
                        HEADER_WIDTH as i32 - 1,
                        TRACK_HEIGHT as i32 - 1,
                    ),
                    -1,
                );
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (width, height) = (obj.width() as f32, obj.height() as f32);
            snapshot.append_color(&BACKGROUND, &graphene::Rect::new(0.0, 0.0, width, height));
            snapshot.append_color(&RULER, &graphene::Rect::new(0.0, 0.0, width, RULER_HEIGHT));

            let project = self.project();
            let timeline = project.timeline();
            let rows = rows(&timeline);
            for (row, &index) in rows.iter().enumerate() {
                let track = timeline.track(index).expect("track");
                let y = row_y(row);
                let color = if track.kind == TrackKind::Video {
                    VIDEO_TRACK
                } else {
                    AUDIO_TRACK
                };
                snapshot.append_color(
                    &color,
                    &graphene::Rect::new(HEADER_WIDTH, y, width - HEADER_WIDTH, TRACK_HEIGHT - 1.0),
                );
            }

            snapshot.push_clip(&graphene::Rect::new(
                HEADER_WIDTH,
                0.0,
                width - HEADER_WIDTH,
                height,
            ));
            self.draw_ruler(snapshot, width);
            let drag = self.drag.borrow().clone();
            for (row, &index) in rows.iter().enumerate() {
                self.draw_track(snapshot, &timeline, index, row_y(row), drag.as_ref());
            }
            self.draw_drag(snapshot, &timeline, &rows, drag.as_ref(), height);
            self.draw_marks(snapshot, &timeline, width, height);
            self.draw_markers(snapshot, &timeline, drag.as_ref());
            if let Some((index, frame)) = self.drop_hint.get()
                && let Some(row) = rows.iter().position(|&i| i == index)
            {
                let x = self.frame_to_x(frame as f64);
                let y = row_y(row);
                snapshot.append_color(
                    &SELECTION,
                    &graphene::Rect::new(x as f32 - 1.0, y, 2.0, TRACK_HEIGHT - 1.0),
                );
            }
            let x = self.frame_to_x(self.playhead.get() as f64) as f32;
            snapshot.append_color(&PLAYHEAD, &graphene::Rect::new(x - 0.5, 0.0, 1.5, height));
            snapshot.append_color(&PLAYHEAD, &graphene::Rect::new(x - 5.0, 0.0, 11.0, 6.0));
            snapshot.pop();

            // Track headers on top of everything scrolled.
            snapshot.append_color(
                &TRACK_HEADER,
                &graphene::Rect::new(0.0, RULER_HEIGHT, HEADER_WIDTH - 1.0, height),
            );
            for header in self.headers.borrow().iter() {
                obj.snapshot_child(header, snapshot);
            }
        }
    }

    impl TimelineView {
        /// Makes the headers match the track rows: one per row, showing its
        /// track.
        pub fn sync_headers(&self) {
            let obj = self.obj();
            let project = self.project();
            let timeline = project.timeline();
            let rows = rows(&timeline);
            let mut headers = self.headers.borrow_mut();
            while headers.len() > rows.len() {
                headers.pop().expect("header").unparent();
            }
            while headers.len() < rows.len() {
                let view = obj.downgrade();
                let header = TrackHeader::new(move |track, toggle, on| {
                    if let Some(view) = view.upgrade() {
                        view.set_track_toggle(track, toggle, on);
                    }
                });
                header.set_parent(&*obj);
                headers.push(header);
            }
            for (row, (&index, header)) in rows.iter().zip(headers.iter()).enumerate() {
                let track = timeline.track(index).expect("track");
                header.show(index, track, &track_name(track, row));
            }
        }

        pub fn project(&self) -> &Rc<Project> {
            self.project.get().expect("project set at construction")
        }

        pub(super) fn fps(&self) -> f64 {
            self.project().frame_rate().as_f64()
        }

        pub(super) fn time_to_frames(&self, time: Time) -> f64 {
            time.as_seconds() * self.fps()
        }

        pub(super) fn frame_time(&self, frame: i64) -> Time {
            Time::from_frames(frame, self.project().frame_rate())
        }
    }
}

glib::wrapper! {
    pub struct TimelineView(ObjectSubclass<imp::TimelineView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl TimelineView {
    /// Shows and edits `project`. Called once, before the widget is shown.
    pub fn set_project(&self, project: Rc<Project>) {
        project.connect_changed(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |project| {
                let imp = view.imp();
                imp.offline.replace(project.timeline().offline_files());
                imp.sync_headers();
                // Selections refer to positions, which edits may change.
                let items = imp.selected_items();
                let valid: Vec<ItemRef> = items
                    .iter()
                    .copied()
                    .filter(|&item| project.timeline().item(item).is_ok_and(|c| !c.is_gap()))
                    .collect();
                if valid != items {
                    imp.select_items(valid);
                }
                if imp
                    .marker
                    .get()
                    .is_some_and(|i| i >= project.timeline().markers().len())
                {
                    imp.select_marker(None);
                }
                view.queue_resize();
                view.queue_draw();
            }
        ));
        self.imp()
            .offline
            .replace(project.timeline().offline_files());
        if self.imp().project.set(project).is_err() {
            panic!("TimelineView::set_project called twice");
        }
        self.imp().sync_headers();
        self.queue_resize();
    }

    pub fn playhead(&self) -> i64 {
        self.imp().playhead.get()
    }

    pub fn set_playhead(&self, frame: i64) {
        let imp = self.imp();
        if imp.playhead.replace(frame) != frame {
            self.emit_by_name::<()>("playhead-changed", &[&frame]);
            self.queue_draw();
        }
    }

    pub fn connect_playhead_changed<F: Fn(&Self, i64) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("playhead-changed", false, move |values| {
            let obj = values[0].get::<Self>().expect("TimelineView");
            let frame = values[1].get::<i64>().expect("frame");
            f(&obj, frame);
            None
        })
    }

    pub fn project(&self) -> Rc<Project> {
        self.imp().project().clone()
    }

    /// Changes the selected clips' properties as one undoable edit called
    /// `label` (every selected clip gets the same change). With a
    /// `merge_key`, consecutive changes with the same key make one undo step
    /// (e.g. while a slider is dragged).
    pub fn edit_selected_clip(
        &self,
        label: &str,
        merge_key: Option<&str>,
        change: impl Fn(&mut nox_project::Item),
    ) {
        self.edit_clips(self.selected_items(), label, merge_key, change);
    }

    /// Changes the clip shown (clicked last), not the rest of the
    /// selection: for edits by effect key (node editing), which other
    /// clips' effects don't share.
    pub fn edit_shown_clip(
        &self,
        label: &str,
        merge_key: Option<&str>,
        change: impl Fn(&mut nox_project::Item),
    ) {
        self.edit_clips(
            self.selection().into_iter().collect(),
            label,
            merge_key,
            change,
        );
    }

    fn edit_clips(
        &self,
        items: Vec<ItemRef>,
        label: &str,
        merge_key: Option<&str>,
        change: impl Fn(&mut nox_project::Item),
    ) {
        let Some(&item) = items.first() else {
            return;
        };
        let edit = |t: &mut Timeline| {
            for &r in &items {
                let clip = t
                    .track_mut(r.track)
                    .ok_or(EditError::NoTrack(r.track))?
                    .children
                    .get_mut(r.index)
                    .and_then(Composable::as_clip_mut);
                // Transitions in the selection have no clip properties.
                if let Some(clip) = clip {
                    change(&mut clip.item);
                }
            }
            Ok::<_, EditError>(())
        };
        let project = self.project();
        let _ = match merge_key {
            // Per clip, so editing another clip starts a new step.
            Some(key) => {
                let key = format!("{key}:{}:{}", item.track, item.index);
                project.edit_merging(label, &key, edit)
            }
            None => project.edit(label, edit),
        };
    }

    /// Selects the clip under the playhead, topmost track first (as
    /// shown); with none there, clears the selection.
    pub fn select_clip_at_playhead(&self) {
        let project = self.project();
        let model = project.timeline();
        let fps = model.frame_rate();
        let frame = self.playhead();
        let found = rows(&model).into_iter().find_map(|track_index| {
            let track = model.track(track_index)?;
            (0..track.children.len()).find_map(|index| {
                let item = ItemRef {
                    track: track_index,
                    index,
                };
                let child = &track.children[index];
                let range = model.item_range(item).ok()?;
                let covers = range.start.frame(fps) <= frame && frame < range.end().frame(fps);
                (covers && matches!(child, Composable::Clip(_))).then_some(item)
            })
        });
        drop(model);
        self.set_selection(found);
    }

    /// Moves the playhead to the next (or previous) clip boundary.
    pub fn go_to_edit_point(&self, forward: bool) {
        let project = self.project();
        let fps = project.frame_rate();
        let frame = self.playhead();
        let points: Vec<i64> = project
            .timeline()
            .edit_points()
            .into_iter()
            .map(|t| t.frame(fps))
            .collect();
        let target = if forward {
            points.into_iter().find(|&p| p > frame)
        } else {
            points.into_iter().rev().find(|&p| p < frame)
        };
        if let Some(target) = target {
            self.set_playhead(target);
        }
    }

    pub fn connect_selection_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("selection-changed", false, move |values| {
            f(&values[0].get::<Self>().expect("TimelineView"));
            None
        })
    }

    pub fn snapping(&self) -> bool {
        self.imp().snapping.get()
    }

    pub fn set_snapping(&self, enabled: bool) {
        self.imp().snapping.set(enabled);
    }

    pub fn selection(&self) -> Option<ItemRef> {
        self.imp().selection.get()
    }

    pub fn set_selection(&self, item: Option<ItemRef>) {
        self.imp().set_selection(item);
    }

    /// The selected items, the primary one (`selection`) first.
    pub fn selected_items(&self) -> Vec<ItemRef> {
        self.imp().selected_items()
    }

    /// Selects `items`; the first is the primary one.
    pub fn select_items(&self, items: Vec<ItemRef>) {
        self.imp().select_items(items);
    }

    /// The selected timeline marker.
    pub fn selected_marker(&self) -> Option<usize> {
        self.imp().marker.get()
    }

    pub fn select_marker(&self, index: Option<usize>) {
        self.imp().select_marker(index);
    }

    /// Adds a timeline marker at the playhead (M) and selects it.
    pub fn add_marker_at_playhead(&self) {
        let project = self.project();
        let at = Time::from_frames(self.playhead(), project.frame_rate());
        let added = project.edit(&crate::i18n::gettext("Add Marker"), |t| {
            Ok::<_, EditError>(t.add_marker(at, nox_project::MARKER_COLORS[0]))
        });
        if let Ok(index) = added {
            self.select_marker(Some(index));
        }
    }

    /// Moves the playhead to the next (or previous) timeline marker.
    pub fn go_to_marker(&self, forward: bool) {
        let project = self.project();
        let fps = project.frame_rate();
        let frame = self.playhead();
        let frames: Vec<i64> = project
            .timeline()
            .markers()
            .iter()
            .map(|m| m.marked_range.start.frame(fps))
            .collect();
        let target = if forward {
            frames.into_iter().find(|&f| f > frame)
        } else {
            frames.into_iter().rev().find(|&f| f < frame)
        };
        if let Some(target) = target {
            self.set_playhead(target);
        }
    }

    pub fn delete_selected_marker(&self) {
        let Some(index) = self.selected_marker() else {
            return;
        };
        let project = self.project();
        if project
            .edit(&crate::i18n::gettext("Delete Marker"), |t| {
                t.remove_marker(index)
            })
            .is_ok()
        {
            self.select_marker(None);
        }
    }

    /// Selects every clip on unlocked tracks.
    pub fn select_all_clips(&self) {
        let project = self.project();
        let timeline = project.timeline();
        let items: Vec<ItemRef> = rows(&timeline)
            .into_iter()
            .filter_map(|t| Some((t, timeline.track(t)?)))
            .filter(|(_, track)| !track.item.locked())
            .flat_map(|(t, track)| {
                track
                    .children
                    .iter()
                    .enumerate()
                    .filter(|(_, child)| matches!(child, Composable::Clip(_)))
                    .map(move |(index, _)| ItemRef { track: t, index })
                    .collect::<Vec<_>>()
            })
            .collect();
        drop(timeline);
        self.select_items(items);
    }

    /// Removes the selected clips (and the clips linked to them).
    pub fn delete_selection(&self) {
        let items = self.selected_items();
        if items.is_empty() {
            return;
        }
        let project = self.imp().project().clone();
        let label = crate::i18n::ngettext("Delete Clip", "Delete Clips", items.len() as u32);
        if project.edit(&label, |t| t.remove_items(&items)).is_ok() {
            self.set_selection(None);
        }
    }

    /// Removes the selected clips (and the clips linked to them) and closes
    /// the gaps on their tracks.
    pub fn ripple_delete_selection(&self) {
        let items = self.selected_items();
        if items.is_empty() {
            return;
        }
        let project = self.imp().project().clone();
        if project
            .edit(&crate::i18n::gettext("Ripple Delete"), |t| {
                t.ripple_remove_items(&items)
            })
            .is_ok()
        {
            self.set_selection(None);
        }
    }

    /// Moves the playhead to the selected clip's next (or previous)
    /// keyframe of any property.
    pub fn jump_to_keyframe(&self, forward: bool) {
        let Some(now) = self.selected_media_time() else {
            return;
        };
        let frame = {
            let project = self.project();
            let model = project.timeline();
            let Some(item) = self.selection() else {
                return;
            };
            let (Ok(Composable::Clip(clip)), Ok(range)) =
                (model.item(item), model.item_range(item))
            else {
                return;
            };
            let same = nox_project::keyframes::SAME_TIME;
            let times = clip.item.keyframe_times();
            let target = if forward {
                times.into_iter().find(|&t| t > now + same)
            } else {
                times.into_iter().rev().find(|&t| t < now - same)
            };
            let (Some(target), Some(trimmed)) = (target, clip.trimmed_range()) else {
                return;
            };
            // Media time back to the timeline, at the clip's speed.
            let speed = clip.item.speed().max(1e-6);
            let seconds = range.start.as_seconds() + (target - trimmed.start.as_seconds()) / speed;
            (seconds * model.frame_rate().as_f64()).round() as i64
        };
        self.set_playhead(frame.max(0));
    }

    /// The selected clip's media time (seconds) at the playhead, held
    /// inside the clip: where keyframes are set and values shown.
    pub fn selected_media_time(&self) -> Option<f64> {
        let item = self.selection()?;
        let project = self.project();
        let model = project.timeline();
        let Composable::Clip(clip) = model.item(item).ok()? else {
            return None;
        };
        let range = model.item_range(item).ok()?;
        let fps = model.frame_rate();
        let zero = Time::zero(fps.inverse());
        let playhead = Time::from_frames(self.playhead(), fps);
        let last = (range.duration - Time::from_frames(1, fps)).max(zero);
        let local = (playhead - range.start).max(zero).min(last);
        Some(clip.media_time(local)?.as_seconds())
    }

    fn scrub_to(&self, x: f64) {
        self.set_playhead(self.imp().x_to_frame(x));
    }
}
