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
        drag: RefCell<Option<Drag>>,
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
            let _ = self
                .waveforms
                .set(crate::waveforms::Waveforms::new(move || {
                    if let Some(view) = view.upgrade() {
                        view.queue_draw();
                    }
                }));
            obj.set_focusable(true);
            obj.set_overflow(gtk::Overflow::Hidden);
            obj.update_property(&[
                gtk::accessible::Property::Label(&crate::i18n::gettext("Timeline")),
                gtk::accessible::Property::Description(&crate::i18n::gettext(
                    "Left and Right step one frame, Up and Down jump to edit points, \
                     Shift+V selects the clip at the playhead, Ctrl+A all clips and \
                     Escape none, Shift+F10 opens the clip menu (in a track header, the \
                     track menu), Delete removes the \
                     selected clips.",
                )),
            ]);

            let drag = gtk::GestureDrag::new();
            drag.connect_drag_begin(glib::clone!(
                #[weak]
                obj,
                move |gesture, x, y| {
                    let state = gesture.current_event_state();
                    obj.imp().drag_begin(
                        x,
                        y,
                        state.contains(gdk::ModifierType::CONTROL_MASK),
                        state.contains(gdk::ModifierType::SHIFT_MASK),
                        state.contains(gdk::ModifierType::ALT_MASK),
                    )
                }
            ));
            drag.connect_drag_update(glib::clone!(
                #[weak]
                obj,
                move |gesture, dx, dy| {
                    if let Some((x, y)) = gesture.start_point() {
                        obj.imp().drag_update(x, y, dx, dy);
                    }
                }
            ));
            drag.connect_drag_end(glib::clone!(
                #[weak]
                obj,
                move |_, _, _| obj.imp().drag_end()
            ));
            obj.add_controller(drag);

            let motion = gtk::EventControllerMotion::new();
            motion.connect_motion(glib::clone!(
                #[weak]
                obj,
                move |_, x, y| obj.imp().hover(x, y)
            ));
            obj.add_controller(motion);

            let scroll =
                gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
            scroll.connect_scroll(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |controller, dx, dy| {
                    let zoom = controller
                        .current_event_state()
                        .contains(gdk::ModifierType::CONTROL_MASK);
                    obj.imp().on_scroll(zoom, dx, dy);
                    glib::Propagation::Stop
                }
            ));
            obj.add_controller(scroll);

            let shortcuts = gtk::ShortcutController::new();
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("<Shift>Delete|<Shift>BackSpace"),
                Some(gtk::NamedAction::new("win.ripple-delete")),
            ));
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("Delete|BackSpace"),
                Some(gtk::CallbackAction::new(|widget, _| {
                    let view = widget
                        .downcast_ref::<super::TimelineView>()
                        .expect("timeline");
                    if view.selected_marker().is_some() {
                        view.delete_selected_marker();
                    } else {
                        view.delete_selection();
                    }
                    glib::Propagation::Stop
                })),
            ));
            // Through the window action, so the menu shows the state.
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("n"),
                Some(gtk::NamedAction::new("win.snapping")),
            ));
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("<Control>a"),
                Some(gtk::CallbackAction::new(|widget, _| {
                    let view = widget
                        .downcast_ref::<super::TimelineView>()
                        .expect("timeline");
                    view.select_all_clips();
                    glib::Propagation::Stop
                })),
            ));
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("Escape"),
                Some(gtk::CallbackAction::new(|widget, _| {
                    let view = widget
                        .downcast_ref::<super::TimelineView>()
                        .expect("timeline");
                    view.set_selection(None);
                    glib::Propagation::Stop
                })),
            ));
            // The clip menu without a mouse.
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("<Shift>F10|Menu"),
                Some(gtk::CallbackAction::new(|widget, _| {
                    let view = widget
                        .downcast_ref::<super::TimelineView>()
                        .expect("timeline");
                    view.imp().selection_menu();
                    glib::Propagation::Stop
                })),
            ));
            obj.add_controller(shortcuts);

            let drop = gtk::DropTarget::new(MediaObject::static_type(), gdk::DragAction::COPY);
            drop.connect_motion(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                gdk::DragAction::empty(),
                move |_, x, y| {
                    let imp = obj.imp();
                    imp.drop_hint
                        .set(Some((imp.track_at_y(y).unwrap_or(0), imp.drop_frame(x))));
                    obj.queue_draw();
                    gdk::DragAction::COPY
                }
            ));
            drop.connect_leave(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    obj.imp().drop_hint.set(None);
                    obj.queue_draw();
                }
            ));
            drop.connect_drop(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                false,
                move |target, value, x, y| {
                    let imp = obj.imp();
                    imp.drop_hint.set(None);
                    // Ctrl inserts (pushing what follows), as in Premiere.
                    let insert = target
                        .current_event_state()
                        .contains(gdk::ModifierType::CONTROL_MASK);
                    obj.queue_draw();
                    let Ok(media) = value.get::<MediaObject>() else {
                        return false;
                    };
                    let Some(info) = media.info() else {
                        tracing::info!("{} is still loading", media.name());
                        return false;
                    };
                    let frame = imp.drop_frame(x);
                    crate::framing::add_media(
                        &obj,
                        imp.project(),
                        info,
                        (imp.track_at_y(y), frame, insert),
                        None,
                        |_, _| {},
                    );
                    true
                }
            ));
            obj.add_controller(drop);

            let menu = gtk::PopoverMenu::from_model(Some(&clip_menu()));
            menu.set_parent(&*obj);
            menu.set_has_arrow(false);
            menu.set_halign(gtk::Align::Start);
            self.menu.set(menu).expect("menu set once");
            let ruler = gtk::PopoverMenu::from_model(Some(&ruler_menu()));
            ruler.set_parent(&*obj);
            ruler.set_has_arrow(false);
            ruler.set_halign(gtk::Align::Start);
            self.ruler_menu.set(ruler).expect("ruler menu set once");
            let context = gtk::GestureClick::builder().button(3).build();
            context.connect_pressed(glib::clone!(
                #[weak]
                obj,
                move |_, _, x, y| obj.imp().context_menu(x, y)
            ));
            obj.add_controller(context);

            let (video, audio) = track_menus();
            let track_menu = |model: &gio::MenuModel| {
                let menu = gtk::PopoverMenu::from_model(Some(model));
                menu.set_parent(&*obj);
                menu.set_has_arrow(false);
                menu.set_halign(gtk::Align::Start);
                menu
            };
            self.track_menus
                .set((track_menu(&video), track_menu(&audio)))
                .expect("menus set once");
            self.add_track_actions();

            obj.set_has_tooltip(true);
            obj.connect_query_tooltip(|obj, x, y, _, tooltip| {
                let imp = obj.imp();
                let hit = imp.hit(x.into(), y.into());
                if let Hit::Marker(index) = hit {
                    let project = imp.project();
                    let timeline = project.timeline();
                    let Some(marker) = timeline.markers().get(index) else {
                        return false;
                    };
                    let mut text = if marker.name.is_empty() {
                        crate::i18n::gettext("Marker")
                    } else {
                        marker.name.clone()
                    };
                    if !marker.comment.is_empty() {
                        text = format!("{text}\n{}", marker.comment); // no-i18n
                    }
                    tooltip.set_text(Some(&text));
                    return true;
                }
                false
            });
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

    fn row_y(row: usize) -> f32 {
        RULER_HEIGHT + row as f32 * TRACK_HEIGHT
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

        fn fps(&self) -> f64 {
            self.project().frame_rate().as_f64()
        }

        fn time_to_frames(&self, time: Time) -> f64 {
            time.as_seconds() * self.fps()
        }

        fn frame_time(&self, frame: i64) -> Time {
            Time::from_frames(frame, self.project().frame_rate())
        }

        fn draw_ruler(&self, snapshot: &gtk::Snapshot, width: f32) {
            let ppf = self.pixels_per_frame.get();
            let fps = self.project().frame_rate();
            let nominal = nox_project::nominal_fps(fps) as i64;
            let step = [1, 2, 5, 10]
                .into_iter()
                .chain([1, 2, 5, 10, 30, 60, 120, 300, 600, 1800, 3600].map(|s| s * nominal))
                .find(|&s| s as f64 * ppf >= MIN_TICK_SPACING)
                .unwrap_or(3600 * nominal);
            let minor = if step % 5 == 0 { step / 5 } else { step };

            let first = (self.scroll.get() / minor as f64).floor() as i64 * minor;
            let mut frame = first.max(0);
            loop {
                let x = self.frame_to_x(frame as f64) as f32;
                if x >= width {
                    break;
                }
                let major = frame % step == 0;
                let tick = if major { 10.0 } else { 4.0 };
                snapshot.append_color(
                    &TICK,
                    &graphene::Rect::new(x, RULER_HEIGHT - tick, 1.0, tick),
                );
                if major {
                    // Centered over its tick; the first one kept clear of
                    // the track headers.
                    let layout = self
                        .obj()
                        .create_pango_layout(Some(&self.project().timecode(frame)));
                    let width = layout.pixel_size().0 as f32;
                    let left = (x - width / 2.0).max(HEADER_WIDTH + 2.0);
                    snapshot.save();
                    snapshot.translate(&graphene::Point::new(left, 2.0));
                    snapshot.append_layout(&layout, &TEXT);
                    snapshot.restore();
                }
                frame += minor;
            }
        }

        /// Timeline markers as flags on the ruler (the selected one outlined;
        /// one being dragged where it goes).
        fn draw_markers(&self, snapshot: &gtk::Snapshot, timeline: &Timeline, drag: Option<&Drag>) {
            for (index, marker) in timeline.markers().iter().enumerate() {
                let time = match drag {
                    Some(Drag::Marker { index: i, to, .. }) if *i == index => *to,
                    _ => marker.marked_range.start,
                };
                let x = self.frame_to_x(self.time_to_frames(time)) as f32;
                let flag = graphene::Rect::new(x - 4.0, RULER_HEIGHT - 12.0, 9.0, 9.0);
                snapshot.append_color(&marker_color(&marker.color), &flag);
                snapshot.append_color(
                    &with_alpha(marker_color(&marker.color), 0.8),
                    &graphene::Rect::new(x - 0.5, RULER_HEIGHT - 3.0, 1.5, 3.0),
                );
                if self.marker.get() == Some(index) {
                    let outline = gsk::RoundedRect::from_rect(flag, 0.0);
                    snapshot.append_border(&outline, &[1.5; 4], &[gdk::RGBA::WHITE; 4]);
                }
            }
        }

        /// The In/Out marks: the range on the ruler, with brackets at the
        /// marks, and everything outside it shaded.
        fn draw_marks(
            &self,
            snapshot: &gtk::Snapshot,
            timeline: &Timeline,
            width: f32,
            height: f32,
        ) {
            let settings = &timeline.settings;
            if settings.mark_in.is_none() && settings.mark_out.is_none() {
                return;
            }
            let x_of = |time: Time| self.frame_to_x(self.time_to_frames(time)) as f32;
            let x0 = settings.mark_in.map_or(HEADER_WIDTH, x_of);
            let x1 = settings.mark_out.map_or(width, x_of);
            let tracks = height - RULER_HEIGHT;
            if x0 > HEADER_WIDTH {
                snapshot.append_color(
                    &OUTSIDE_MARKS,
                    &graphene::Rect::new(HEADER_WIDTH, RULER_HEIGHT, x0 - HEADER_WIDTH, tracks),
                );
            }
            if x1 < width {
                snapshot.append_color(
                    &OUTSIDE_MARKS,
                    &graphene::Rect::new(x1, RULER_HEIGHT, width - x1, tracks),
                );
            }
            snapshot.append_color(
                &with_alpha(MARK, 0.3),
                &graphene::Rect::new(x0, RULER_HEIGHT - 8.0, (x1 - x0).max(0.0), 8.0),
            );
            // Brackets: [ at In, ] at Out.
            let bracket = |x: f32, inward: f32| {
                snapshot.append_color(&MARK, &graphene::Rect::new(x - 1.0, 0.0, 2.0, height));
                let foot = if inward > 0.0 { x - 1.0 } else { x - 5.0 };
                snapshot.append_color(&MARK, &graphene::Rect::new(foot, 0.0, 6.0, 2.0));
                snapshot.append_color(
                    &MARK,
                    &graphene::Rect::new(foot, RULER_HEIGHT - 2.0, 6.0, 2.0),
                );
            };
            if settings.mark_in.is_some() {
                bracket(x0, 1.0);
            }
            if settings.mark_out.is_some() {
                bracket(x1, -1.0);
            }
        }

        fn draw_track(
            &self,
            snapshot: &gtk::Snapshot,
            timeline: &Timeline,
            index: usize,
            y: f32,
            drag: Option<&Drag>,
        ) {
            let track = timeline.track(index).expect("track");
            let moving: Vec<ItemRef> = match drag {
                Some(Drag::Move { linked, .. }) => linked.iter().map(|(r, _)| *r).collect(),
                Some(Drag::Trim { item, .. }) => vec![*item],
                _ => Vec::new(),
            };
            for (i, (range, child)) in track.child_ranges().enumerate() {
                let item = ItemRef {
                    track: index,
                    index: i,
                };
                let offline = match child {
                    Composable::Clip(clip) => clip
                        .media_reference()
                        .and_then(|r| timeline.media_path(r))
                        .is_some_and(|path| self.offline.borrow().contains(&path)),
                    _ => false,
                };
                let color = match child {
                    Composable::Gap(_) => continue,
                    // Drawn below, over both neighbors.
                    Composable::Transition(_) => continue,
                    _ if offline => OFFLINE_CLIP,
                    Composable::Clip(_) if track.kind == TrackKind::Video => VIDEO_CLIP,
                    Composable::Clip(_) => AUDIO_CLIP,
                    _ => NESTED_CLIP,
                };
                // Clips of muted or hidden tracks are dimmed like disabled clips.
                let enabled = child.item().is_none_or(|i| i.enabled) && track.item.enabled;
                let alpha = if moving.contains(&item) {
                    0.35
                } else if enabled {
                    1.0
                } else {
                    0.5
                };
                let selected = self.is_selected(item) && moving.is_empty();
                let mut label = clip_label(child);
                if offline {
                    label += &format!(" · {}", crate::i18n::gettext("Offline"));
                }
                self.draw_clip(
                    snapshot,
                    range,
                    y,
                    &label,
                    with_alpha(color, alpha),
                    selected,
                );
                if let Composable::Clip(clip) = child
                    && track.kind == TrackKind::Audio
                    && !offline
                {
                    self.draw_waveform(snapshot, timeline, clip, range, y, alpha);
                }
                if let Composable::Clip(clip) = child
                    && !moving.contains(&item)
                {
                    self.draw_fades(snapshot, item, clip, range, y);
                    self.draw_keyframes(snapshot, clip, range, y);
                }
                // Markers of the clip (from imported files), in its media's
                // time.
                if let Composable::Clip(clip) = child
                    && let Some(trimmed) = clip.trimmed_range()
                {
                    for marker in &clip.item.markers {
                        let offset = marker.marked_range.start - trimmed.start;
                        if offset.is_negative() || offset > trimmed.duration {
                            continue;
                        }
                        let x = self.frame_to_x(self.time_to_frames(range.start + offset)) as f32;
                        snapshot.append_color(
                            &marker_color(&marker.color),
                            &graphene::Rect::new(x - 2.0, y + 3.0, 5.0, 5.0),
                        );
                    }
                }
            }
            for (i, (range, child)) in track.child_ranges().enumerate() {
                if let Composable::Transition(_) = child {
                    let selected = self.is_selected(ItemRef {
                        track: index,
                        index: i,
                    });
                    self.draw_transition(snapshot, range, y, selected);
                }
            }
            if track.item.locked() {
                // Diagonal stripes over the whole row, as other editors do.
                let width = self.obj().width() as f32;
                let row =
                    graphene::Rect::new(HEADER_WIDTH, y, width - HEADER_WIDTH, TRACK_HEIGHT - 1.0);
                snapshot.push_repeat(&row, Some(&graphene::Rect::new(0.0, y, 12.0, TRACK_HEIGHT)));
                for step in 0..12 {
                    let x = step as f32;
                    let rect = graphene::Rect::new(
                        x,
                        y + x * TRACK_HEIGHT / 12.0,
                        2.0,
                        TRACK_HEIGHT / 12.0,
                    );
                    snapshot.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.3), &rect);
                }
                snapshot.pop();
            }
        }

        fn clip_rect(&self, range: TimeRange, y: f32) -> graphene::Rect {
            let x0 = self.frame_to_x(self.time_to_frames(range.start));
            let x1 = self.frame_to_x(self.time_to_frames(range.end()));
            // Keep tiny clips visible.
            let width = (x1 - x0).max(2.0);
            graphene::Rect::new(x0 as f32, y + 2.0, width as f32, TRACK_HEIGHT - 5.0)
        }

        fn draw_clip(
            &self,
            snapshot: &gtk::Snapshot,
            range: TimeRange,
            y: f32,
            name: &str,
            color: gdk::RGBA,
            selected: bool,
        ) {
            let rect = self.clip_rect(range, y);
            if rect.x() > self.obj().width() as f32 || rect.x() + rect.width() < HEADER_WIDTH {
                return;
            }
            let rounded = gsk::RoundedRect::from_rect(rect, 3.0);
            snapshot.push_rounded_clip(&rounded);
            snapshot.append_color(&color, &rect);
            if rect.width() > 16.0 {
                self.draw_text(
                    snapshot,
                    name,
                    rect.x().max(HEADER_WIDTH) + 5.0,
                    y + 5.0,
                    &with_alpha(CLIP_TEXT, color.alpha()),
                );
            }
            snapshot.pop();
            let (border, width) = if selected {
                (SELECTION, 2.0)
            } else {
                (CLIP_BORDER, 1.0)
            };
            snapshot.append_border(&rounded, &[width; 4], &[border; 4]);
        }

        /// The clip's waveform below its name: the peak level of each pixel
        /// column's stretch of media, scaled by the clip's volume.
        fn draw_waveform(
            &self,
            snapshot: &gtk::Snapshot,
            timeline: &Timeline,
            clip: &nox_project::Clip,
            range: TimeRange,
            y: f32,
            alpha: f32,
        ) {
            let rect = self.clip_rect(range, y);
            let (Some(path), Some(trimmed), Some(waveforms)) = (
                clip.media_reference().and_then(|r| timeline.media_path(r)),
                clip.trimmed_range(),
                self.waveforms.get(),
            ) else {
                return;
            };
            let left = rect.x().max(HEADER_WIDTH).floor();
            let right = (rect.x() + rect.width()).min(self.obj().width() as f32);
            if right - left < 2.0 {
                return;
            }
            let Some(waveform) = waveforms.get(&path) else {
                return;
            };
            // Seconds into the file at widget x.
            let file_start = clip
                .available_range()
                .map_or(trimmed.start, |available| trimmed.start - available.start);
            let fps = self.fps();
            let start_frame = self.time_to_frames(range.start);
            // Sped-up clips play their media faster; freeze frames none.
            let speed = clip.item.speed();
            let seconds_at = |x: f32| {
                (self.x_to_frame_f64(f64::from(x)) - start_frame) / fps * speed
                    + file_start.as_seconds()
            };
            let top = rect.y() + 16.0;
            let height = rect.y() + rect.height() - 2.0 - top;
            let middle = top + height / 2.0;
            let gain = clip.item.volume().min(1.0) as f32;
            let color = with_alpha(CLIP_TEXT, 0.45 * alpha);
            let mut x = left;
            while x < right {
                let peak = waveform.peak(seconds_at(x), seconds_at(x + 1.0)) * gain;
                let bar = (peak * height).max(if peak > 0.0 { 1.0 } else { 0.0 });
                snapshot.append_color(
                    &color,
                    &graphene::Rect::new(x, middle - bar / 2.0, 1.0, bar),
                );
                x += 1.0;
            }
        }

        fn draw_transition(
            &self,
            snapshot: &gtk::Snapshot,
            range: TimeRange,
            y: f32,
            selected: bool,
        ) {
            let rect = self.clip_rect(range, y);
            let rect = graphene::Rect::new(
                rect.x(),
                y + TRACK_HEIGHT / 2.0,
                rect.width(),
                TRACK_HEIGHT / 2.0 - 3.0,
            );
            snapshot.append_color(&with_alpha(CLIP_TEXT, 0.35), &rect);
            if selected {
                let rounded = gsk::RoundedRect::from_rect(rect, 2.0);
                snapshot.append_border(&rounded, &[2.0; 4], &[SELECTION; 4]);
            }
        }

        /// The moving or trimmed clips at their new position, and the snap
        /// line.
        fn draw_drag(
            &self,
            snapshot: &gtk::Snapshot,
            timeline: &Timeline,
            rows: &[usize],
            drag: Option<&Drag>,
            height: f32,
        ) {
            let row_of = |track: usize| rows.iter().position(|&i| i == track);
            let snapped = match drag {
                Some(Drag::Move {
                    item,
                    linked,
                    offset,
                    target_track,
                    snapped,
                    ..
                }) => {
                    for (r, range) in linked {
                        let track = if r == item { *target_track } else { r.track };
                        let (Some(row), Ok(child)) = (row_of(track), timeline.item(*r)) else {
                            continue;
                        };
                        let kind = &timeline.track(track).expect("track").kind;
                        let color = if *kind == TrackKind::Video {
                            VIDEO_CLIP
                        } else {
                            AUDIO_CLIP
                        };
                        let moved = TimeRange::new(range.start + *offset, range.duration);
                        self.draw_clip(snapshot, moved, row_y(row), child.name(), color, true);
                    }
                    *snapped
                }
                Some(Drag::Trim {
                    item,
                    edge,
                    ripple,
                    range,
                    to,
                    snapped,
                    ..
                }) => {
                    if let (Some(row), Ok(child)) = (row_of(item.track), timeline.item(*item)) {
                        let trimmed = match edge {
                            // A ripple-trimmed start stays in place.
                            Edge::Start if *ripple => {
                                TimeRange::new(range.start, range.end() - *to)
                            }
                            Edge::Start => TimeRange::from_start_end(*to, range.end()),
                            Edge::End => TimeRange::from_start_end(range.start, *to),
                        };
                        let kind = &timeline.track(item.track).expect("track").kind;
                        let color = if *kind == TrackKind::Video {
                            VIDEO_CLIP
                        } else {
                            AUDIO_CLIP
                        };
                        self.draw_clip(snapshot, trimmed, row_y(row), child.name(), color, true);
                    }
                    *snapped
                }
                Some(Drag::Roll {
                    item, to, snapped, ..
                }) => {
                    if let Some(row) = row_of(item.track) {
                        let x = self.frame_to_x(self.time_to_frames(*to)) as f32;
                        snapshot.append_color(
                            &SELECTION,
                            &graphene::Rect::new(x - 1.5, row_y(row), 3.0, TRACK_HEIGHT - 1.0),
                        );
                    }
                    *snapped
                }
                Some(Drag::Shift {
                    item,
                    slide,
                    range,
                    delta,
                    ..
                }) => {
                    if let (Some(row), Ok(child)) = (row_of(item.track), timeline.item(*item)) {
                        let fps = timeline.frame_rate();
                        let frames = delta.frame(fps);
                        let (shown, label) = if *slide {
                            (
                                TimeRange::new(range.start + *delta, range.duration),
                                crate::i18n::fill(
                                    &crate::i18n::gettext("Slide {frames}"),
                                    &[("frames", &format!("{frames:+}"))],
                                ),
                            )
                        } else {
                            (
                                *range,
                                crate::i18n::fill(
                                    &crate::i18n::gettext("Slip {frames}"),
                                    &[("frames", &format!("{frames:+}"))],
                                ),
                            )
                        };
                        let kind = &timeline.track(item.track).expect("track").kind;
                        let color = if *kind == TrackKind::Video {
                            VIDEO_CLIP
                        } else {
                            AUDIO_CLIP
                        };
                        let text = format!("{} · {label}", child.name()); // no-i18n
                        self.draw_clip(snapshot, shown, row_y(row), &text, color, true);
                    }
                    None
                }
                Some(Drag::Band { from, to, .. }) => {
                    let (x0, y0) = (from.0.min(to.0) as f32, from.1.min(to.1) as f32);
                    let (x1, y1) = (from.0.max(to.0) as f32, from.1.max(to.1) as f32);
                    let band = graphene::Rect::new(x0, y0, x1 - x0, y1 - y0);
                    snapshot.append_color(&with_alpha(SELECTION, 0.15), &band);
                    let outline = gsk::RoundedRect::from_rect(band, 0.0);
                    snapshot.append_border(&outline, &[1.0; 4], &[SELECTION; 4]);
                    None
                }
                _ => None,
            };
            if let Some(frame) = snapped {
                let x = self.frame_to_x(frame) as f32;
                snapshot.append_color(
                    &SNAP_LINE,
                    &graphene::Rect::new(x - 0.5, RULER_HEIGHT, 1.0, height),
                );
            }
        }

        fn draw_text(
            &self,
            snapshot: &gtk::Snapshot,
            text: &str,
            x: f32,
            y: f32,
            color: &gdk::RGBA,
        ) {
            let layout = self.obj().create_pango_layout(Some(text));
            snapshot.save();
            snapshot.translate(&graphene::Point::new(x, y));
            snapshot.append_layout(&layout, color);
            snapshot.restore();
        }

        /// Widget x coordinate of a (fractional) frame position.
        fn frame_to_x(&self, frame: f64) -> f64 {
            HEADER_WIDTH as f64 + (frame - self.scroll.get()) * self.pixels_per_frame.get()
        }

        fn x_to_frame_f64(&self, x: f64) -> f64 {
            (x - HEADER_WIDTH as f64) / self.pixels_per_frame.get() + self.scroll.get()
        }

        pub fn x_to_frame(&self, x: f64) -> i64 {
            self.x_to_frame_f64(x).round().max(0.0) as i64
        }

        /// Stack index of the track at widget y.
        fn track_at_y(&self, y: f64) -> Option<usize> {
            if y < RULER_HEIGHT as f64 {
                return None;
            }
            let row = ((y - RULER_HEIGHT as f64) / TRACK_HEIGHT as f64) as usize;
            rows(&self.project().timeline()).get(row).copied()
        }

        fn hit(&self, x: f64, y: f64) -> Hit {
            if y < RULER_HEIGHT as f64 {
                let project = self.project();
                let timeline = project.timeline();
                let near = timeline.markers().iter().position(|m| {
                    let mx = self.frame_to_x(self.time_to_frames(m.marked_range.start));
                    (mx - x).abs() <= 6.0
                });
                return near.map_or(Hit::Ruler, Hit::Marker);
            }
            let Some(track_index) = self.track_at_y(y) else {
                return Hit::Nothing;
            };
            let in_row = (y - RULER_HEIGHT as f64) % TRACK_HEIGHT as f64;
            if x < HEADER_WIDTH as f64 {
                return Hit::Header { track: track_index };
            }
            let project = self.project();
            let timeline = project.timeline();
            let track = timeline.track(track_index).expect("track");
            if in_row < FADE_GRAB_HEIGHT {
                for (index, (range, child)) in track.child_ranges().enumerate() {
                    let Composable::Clip(clip) = child else {
                        continue;
                    };
                    let item = ItemRef {
                        track: track_index,
                        index,
                    };
                    if !self.shows_fade_handles(item, range) {
                        continue;
                    }
                    for edge in [Edge::Start, Edge::End] {
                        let handle = self.fade_handle_x(range, clip, edge);
                        if (x - handle).abs() <= FADE_GRAB {
                            return Hit::Fade { item, edge };
                        }
                    }
                }
            }
            // Keyframe diamonds along the bottom of clips.
            if in_row >= TRACK_HEIGHT as f64 - 14.0 {
                for (index, (range, child)) in track.child_ranges().enumerate() {
                    let Composable::Clip(clip) = child else {
                        continue;
                    };
                    let near = clip.item.keyframe_times().into_iter().find(|&time| {
                        self.keyframe_x(clip, range, time)
                            .is_some_and(|kx| (kx - x).abs() <= 5.0)
                    });
                    if let Some(time) = near {
                        let item = ItemRef {
                            track: track_index,
                            index,
                        };
                        return Hit::Keyframe { item, time };
                    }
                }
            }
            // Transitions are drawn over the lower half of the cut.
            if in_row >= TRACK_HEIGHT as f64 / 2.0 {
                for (index, (range, child)) in track.child_ranges().enumerate() {
                    if !matches!(child, Composable::Transition(_)) {
                        continue;
                    }
                    let x0 = self.frame_to_x(self.time_to_frames(range.start));
                    let x1 = self.frame_to_x(self.time_to_frames(range.end()));
                    let item = ItemRef {
                        track: track_index,
                        index,
                    };
                    // Edges of transitions wide enough to grab in the middle.
                    if x1 - x0 > 3.0 * EDGE_GRAB {
                        if (x - x0).abs() <= EDGE_GRAB {
                            return Hit::TransitionEdge {
                                item,
                                edge: Edge::Start,
                            };
                        }
                        if (x - x1).abs() <= EDGE_GRAB {
                            return Hit::TransitionEdge {
                                item,
                                edge: Edge::End,
                            };
                        }
                    }
                    if x >= x0.min(x1 - 4.0) && x <= x1.max(x0 + 4.0) {
                        return Hit::Item {
                            item,
                            edge: None,
                            roll: false,
                        };
                    }
                }
            }
            for (index, (range, child)) in track.child_ranges().enumerate() {
                if child.is_gap() || matches!(child, Composable::Transition(_)) {
                    continue;
                }
                let x0 = self.frame_to_x(self.time_to_frames(range.start));
                let x1 = self.frame_to_x(self.time_to_frames(range.end()));
                if x < x0 - EDGE_GRAB || x > x1 + EDGE_GRAB {
                    continue;
                }
                let item = ItemRef {
                    track: track_index,
                    index,
                };
                // Edges only for clips, and only if the clip is wide enough
                // to still be grabbed in the middle.
                let trimmable = matches!(child, Composable::Clip(_)) && x1 - x0 > 3.0 * EDGE_GRAB;
                let edge = if trimmable && (x - x0).abs() <= EDGE_GRAB {
                    Edge::Start
                } else if trimmable && (x - x1).abs() <= EDGE_GRAB {
                    Edge::End
                } else if x < x0 || x > x1 {
                    continue;
                } else {
                    return Hit::Item {
                        item,
                        edge: None,
                        roll: false,
                    };
                };
                let cut = timeline.is_cut(item, edge);
                // Past the end of a clip at a cut: the next clip's start.
                if edge == Edge::End && x > x1 && cut {
                    continue;
                }
                let edge_x = if edge == Edge::Start { x0 } else { x1 };
                return Hit::Item {
                    item,
                    edge: Some(edge),
                    roll: cut && (x - edge_x).abs() <= ROLL_GRAB,
                };
            }
            Hit::Track
        }

        /// Whether clip `item` (at `range`) shows its fade handles: when
        /// selected or under the pointer, and wide enough.
        fn shows_fade_handles(&self, item: ItemRef, range: TimeRange) -> bool {
            let width = (self.time_to_frames(range.duration) * self.pixels_per_frame.get()).abs();
            width >= FADE_MIN_WIDTH && (self.is_selected(item) || self.hovered.get() == Some(item))
        }

        /// Widget x of the fade handle at `edge` of `clip` (at `range`):
        /// where its fade ends.
        fn fade_handle_x(&self, range: TimeRange, clip: &nox_project::Clip, edge: Edge) -> f64 {
            let pixels_per_second = self.fps() * self.pixels_per_frame.get();
            match edge {
                Edge::Start => {
                    self.frame_to_x(self.time_to_frames(range.start))
                        + clip.item.fade_in() * pixels_per_second
                }
                Edge::End => {
                    self.frame_to_x(self.time_to_frames(range.end()))
                        - clip.item.fade_out() * pixels_per_second
                }
            }
        }

        /// Sets the fade at `edge` of clip `item` to `seconds`, merging with
        /// earlier edits under `key`.
        pub fn set_fade(&self, item: ItemRef, edge: Edge, seconds: f64, key: &str) {
            let label = match edge {
                Edge::Start => crate::i18n::gettext("Fade In"),
                Edge::End => crate::i18n::gettext("Fade Out"),
            };
            let _ = self.project().edit_merging(&label, key, |t| {
                let clip = t
                    .track_mut(item.track)
                    .ok_or(EditError::NoTrack(item.track))?
                    .children
                    .get_mut(item.index)
                    .and_then(Composable::as_clip_mut)
                    .ok_or(EditError::NoItem {
                        track: item.track,
                        index: item.index,
                    })?;
                match edge {
                    Edge::Start => clip.item.set_fade_in(seconds),
                    Edge::End => clip.item.set_fade_out(seconds),
                }
                Ok::<_, EditError>(())
            });
        }

        /// Small diamonds along a clip's bottom edge where it has keyframes
        /// (of any property).
        fn draw_keyframes(
            &self,
            snapshot: &gtk::Snapshot,
            clip: &nox_project::Clip,
            range: TimeRange,
            y: f32,
        ) {
            let times = clip.item.keyframe_times();
            if times.is_empty() {
                return;
            }
            let rect = self.clip_rect(range, y);
            let bottom = rect.y() + rect.height() - 5.0;
            let path = gsk::PathBuilder::new();
            for time in times {
                let Some(x) = self.keyframe_x(clip, range, time) else {
                    continue;
                };
                let x = x as f32;
                path.move_to(x, bottom - 3.5);
                path.line_to(x + 3.5, bottom);
                path.line_to(x, bottom + 3.5);
                path.line_to(x - 3.5, bottom);
                path.close();
            }
            snapshot.append_fill(&path.to_path(), gsk::FillRule::Winding, &KEYFRAME);
        }

        /// Widget x of a keyframe at media time `time` of `clip` (at
        /// `range`), if it is within the clip. Sped-up clips pass their
        /// media faster; frozen ones show no keyframes.
        fn keyframe_x(&self, clip: &nox_project::Clip, range: TimeRange, time: f64) -> Option<f64> {
            let speed = clip.item.speed();
            let trimmed = clip.trimmed_range()?;
            if speed <= 0.0 {
                return None;
            }
            let local = (time - trimmed.start.as_seconds()) / speed;
            if local < -1e-9 || local > range.duration.as_seconds() + 1e-9 {
                return None;
            }
            Some(self.frame_to_x(self.time_to_frames(range.start) + local * self.fps()))
        }

        /// A clip's fade ramps (the faded part shaded above a line rising
        /// to full level), and its handles if it shows them.
        fn draw_fades(
            &self,
            snapshot: &gtk::Snapshot,
            item: ItemRef,
            clip: &nox_project::Clip,
            range: TimeRange,
            y: f32,
        ) {
            let rect = self.clip_rect(range, y);
            let (top, bottom) = (rect.y(), rect.y() + rect.height());
            let (x0, x1) = (rect.x(), rect.x() + rect.width());
            let line = with_alpha(CLIP_TEXT, 0.7);
            for edge in [Edge::Start, Edge::End] {
                let (corner, end) = match edge {
                    Edge::Start => (x0, self.fade_handle_x(range, clip, edge) as f32),
                    Edge::End => (x1, self.fade_handle_x(range, clip, edge) as f32),
                };
                if (end - corner).abs() < 0.5 {
                    continue;
                }
                let shade = gsk::PathBuilder::new();
                shade.move_to(corner, top);
                shade.line_to(end, top);
                shade.line_to(corner, bottom);
                shade.close();
                snapshot.append_fill(&shade.to_path(), gsk::FillRule::Winding, &FADE_SHADE);
                let ramp = gsk::PathBuilder::new();
                ramp.move_to(corner, bottom);
                ramp.line_to(end, top);
                snapshot.append_stroke(&ramp.to_path(), &gsk::Stroke::new(1.0), &line);
            }
            if self.shows_fade_handles(item, range) {
                for edge in [Edge::Start, Edge::End] {
                    let x = self.fade_handle_x(range, clip, edge) as f32;
                    let x = x.clamp(x0 + 3.0, x1 - 3.0);
                    let handle = graphene::Rect::new(x - 3.5, top, 7.0, 7.0);
                    snapshot.append_color(&CLIP_TEXT, &handle);
                    snapshot.append_border(
                        &gsk::RoundedRect::from_rect(handle, 0.0),
                        &[1.0; 4],
                        &[CLIP_BORDER; 4],
                    );
                }
            }
        }

        fn hover(&self, x: f64, y: f64) {
            self.pointer_x.set(x);
            if self.drag.borrow().is_some() {
                return;
            }
            let hit = self.hit(x, y);
            let hovered = match &hit {
                Hit::Item { item, .. } | Hit::Fade { item, .. } => Some(*item),
                _ => None,
            };
            if hovered != self.hovered.replace(hovered) {
                self.obj().queue_draw();
            }
            let cursor = match hit {
                Hit::Fade { .. } => Some("ew-resize"),
                Hit::TransitionEdge { .. } => Some("col-resize"),
                Hit::Keyframe { .. } => Some("grab"),
                Hit::Item {
                    edge: Some(_),
                    roll: true,
                    ..
                } => Some("ew-resize"),
                Hit::Item { edge: Some(_), .. } => Some("col-resize"),
                _ => None,
            };
            self.obj().set_cursor_from_name(cursor);
        }

        /// Snap targets in frames: item edges (of `timeline` with `exclude`
        /// removed) and the playhead.
        fn snap_points(&self, exclude: &[ItemRef]) -> Vec<f64> {
            if !self.snapping.get() {
                return Vec::new();
            }
            let project = self.project();
            let mut timeline = project.timeline().clone();
            // Lift from the back so earlier indices stay valid.
            let mut exclude = exclude.to_vec();
            exclude.sort_by_key(|r| std::cmp::Reverse(r.index));
            for item in exclude {
                if let (Ok(range), Some(track)) =
                    (timeline.item_range(item), timeline.track_mut(item.track))
                {
                    let _ = track.lift(range);
                }
            }
            let mut points: Vec<f64> = timeline
                .edit_points()
                .into_iter()
                .map(|t| self.time_to_frames(t))
                .collect();
            points.push(self.playhead.get() as f64);
            points
        }

        /// The snap point closest to any of `candidates` (in frames), within
        /// the snapping distance: `(candidate index, point)`.
        fn snap(&self, points: &[f64], candidates: &[f64]) -> Option<(usize, f64)> {
            let limit = SNAP_DISTANCE / self.pixels_per_frame.get();
            candidates
                .iter()
                .enumerate()
                .flat_map(|(i, &c)| points.iter().map(move |&p| (i, p, (p - c).abs())))
                .filter(|&(_, _, d)| d <= limit)
                .min_by(|a, b| a.2.total_cmp(&b.2))
                .map(|(i, p, _)| (i, p))
        }

        /// Frame for dropping media at widget x, snapped.
        fn drop_frame(&self, x: f64) -> i64 {
            let frame = self.x_to_frame_f64(x);
            let points = self.snap_points(&[]);
            match self.snap(&points, &[frame]) {
                Some((_, point)) => point.round() as i64,
                None => frame.round().max(0.0) as i64,
            }
        }

        /// Starts a drag at `x`, `y`. With Ctrl held (`ripple`), dragging
        /// an edge ripple-trims instead of trimming or rolling, and clicking
        /// a clip adds it to or removes it from the selection; with Shift,
        /// clicking a clip adds it. Dragging on empty track space draws a
        /// rubber band; dragging the ruler scrubs.
        fn drag_begin(&self, x: f64, y: f64, ripple: bool, extend: bool, alt: bool) {
            tracing::trace!("timeline press at {x:.0}, {y:.0}");
            let obj = self.obj();
            obj.grab_focus();
            let project = self.project().clone();
            let drag = match self.hit(x, y) {
                Hit::Keyframe { item, time } => {
                    let timeline = project.timeline();
                    let Ok(Composable::Clip(clip)) = timeline.item(item) else {
                        return;
                    };
                    let Some(trimmed) = clip.trimmed_range() else {
                        return;
                    };
                    // Between the neighboring keyframes, inside the clip.
                    let fps = timeline.frame_rate().as_f64();
                    let speed = clip.item.speed();
                    let frame = speed / fps;
                    let start = trimmed.start.as_seconds();
                    let end = start + clip.duration().as_seconds() * speed;
                    let times = clip.item.keyframe_times();
                    let before = times
                        .iter()
                        .copied()
                        .filter(|&t| t < time - 1e-4)
                        .fold(f64::NAN, f64::max);
                    let after = times
                        .iter()
                        .copied()
                        .filter(|&t| t > time + 1e-4)
                        .fold(f64::NAN, f64::min);
                    let min = if before.is_nan() {
                        start
                    } else {
                        before + frame
                    };
                    let max = if after.is_nan() {
                        end - frame
                    } else {
                        after - frame
                    };
                    drop(timeline);
                    self.set_selection(Some(item));
                    self.fade_drags.set(self.fade_drags.get() + 1);
                    Drag::Keyframe {
                        item,
                        origin: time,
                        at: time,
                        min,
                        max,
                        trimmed_start: start,
                        speed,
                        key: format!("keyframe-drag:{}", self.fade_drags.get()),
                    }
                }
                Hit::TransitionEdge { item, edge } => {
                    let start = match project.timeline().item(item) {
                        Ok(Composable::Transition(t)) => (t.in_offset + t.out_offset).as_seconds(),
                        _ => return,
                    };
                    self.set_selection(Some(item));
                    self.fade_drags.set(self.fade_drags.get() + 1);
                    Drag::TransitionLength {
                        item,
                        edge,
                        start,
                        key: format!("transition-drag:{}", self.fade_drags.get()),
                    }
                }
                Hit::Fade { item, edge } => {
                    let timeline = project.timeline();
                    let (Ok(Composable::Clip(clip)), Ok(range)) =
                        (timeline.item(item), timeline.item_range(item))
                    else {
                        return;
                    };
                    let (start, other) = match edge {
                        Edge::Start => (clip.item.fade_in(), clip.item.fade_out()),
                        Edge::End => (clip.item.fade_out(), clip.item.fade_in()),
                    };
                    let max = (range.duration.as_seconds() - other).max(0.0);
                    drop(timeline);
                    self.set_selection(Some(item));
                    self.fade_drags.set(self.fade_drags.get() + 1);
                    Drag::Fade {
                        item,
                        edge,
                        start,
                        max,
                        key: format!("fade-drag:{}", self.fade_drags.get()),
                    }
                }
                Hit::Header { .. } => return,
                Hit::Ruler => {
                    obj.scrub_to(x);
                    Drag::Scrub
                }
                Hit::Marker(index) => {
                    let Some(from) = project
                        .timeline()
                        .markers()
                        .get(index)
                        .map(|m| m.marked_range.start)
                    else {
                        return;
                    };
                    self.select_marker(Some(index));
                    obj.set_playhead(from.frame(project.frame_rate()));
                    Drag::Marker {
                        index,
                        from,
                        to: from,
                        snap_points: self.snap_points(&[]),
                    }
                }
                Hit::Track | Hit::Nothing => {
                    let base = if ripple || extend {
                        self.selected_items()
                    } else {
                        Vec::new()
                    };
                    self.select_items(base.clone());
                    Drag::Band {
                        from: (x, y),
                        to: (x, y),
                        base,
                    }
                }
                Hit::Item { item, edge, roll } => {
                    let body = edge.is_none() && !roll;
                    // Alt: slip, Alt+Shift: slide (one clip and its links).
                    if body && alt {
                        self.set_selection(Some(item));
                        let timeline = project.timeline();
                        let locked = timeline
                            .linked(item)
                            .iter()
                            .any(|r| timeline.track(r.track).is_none_or(|t| t.item.locked()));
                        let (Ok(range), false) = (timeline.item_range(item), locked) else {
                            return;
                        };
                        let limits = if extend {
                            timeline
                                .slide_limits(item)
                                .map(|(min, max)| (Some(min), max))
                        } else {
                            timeline.slip_limits(item)
                        };
                        let Ok((min, max)) = limits else {
                            return;
                        };
                        drop(timeline);
                        self.drag.replace(Some(Drag::Shift {
                            item,
                            slide: extend,
                            range,
                            min,
                            max,
                            delta: Time::zero(range.start.timebase),
                        }));
                        return;
                    }
                    if body && ripple {
                        self.toggle_selected(item);
                        return;
                    }
                    if body && extend {
                        if !self.is_selected(item) {
                            let mut items = self.selected_items();
                            items.insert(0, item);
                            self.select_items(items);
                        }
                        return;
                    }
                    if body && self.is_selected(item) {
                        // Keep the selection; this clip becomes primary.
                        let mut items = self.selected_items();
                        items.retain(|&r| r != item);
                        items.insert(0, item);
                        self.select_items(items);
                    } else {
                        self.set_selection(Some(item));
                    }
                    let timeline = project.timeline();
                    // Transitions are selected (to delete them), not dragged,
                    // and clips on locked tracks stay where they are.
                    if matches!(timeline.item(item), Ok(Composable::Transition(_)))
                        || timeline.track(item.track).is_none_or(|t| t.item.locked())
                    {
                        return;
                    }
                    let Ok(range) = timeline.item_range(item) else {
                        return;
                    };
                    let original = |edge: Edge| match edge {
                        Edge::Start => range.start,
                        Edge::End => range.end(),
                    };
                    match edge {
                        Some(edge) if roll && !ripple => {
                            let Ok((min, max)) = timeline.roll_limits(item, edge) else {
                                return;
                            };
                            drop(timeline);
                            Drag::Roll {
                                item,
                                edge,
                                cut: original(edge),
                                min,
                                max,
                                snap_points: self.snap_points(&[item]),
                                to: original(edge),
                                snapped: None,
                            }
                        }
                        Some(edge) => {
                            let limits = if ripple {
                                timeline.ripple_limits(item, edge)
                            } else {
                                timeline
                                    .track(item.track)
                                    .expect("track")
                                    .trim_limits(item.index, edge)
                            };
                            let Ok((min, max)) = limits else {
                                return;
                            };
                            drop(timeline);
                            Drag::Trim {
                                item,
                                edge,
                                ripple,
                                range,
                                min,
                                max,
                                snap_points: self.snap_points(&[item]),
                                to: original(edge),
                                snapped: None,
                            }
                        }
                        None => {
                            // Every selected clip moves; transitions and
                            // clips on locked tracks keep the rest in place.
                            let items = self.selected_items();
                            let movable = items.iter().all(|&r| {
                                matches!(timeline.item(r), Ok(Composable::Clip(_)))
                                    && timeline.track(r.track).is_some_and(|t| !t.item.locked())
                            });
                            if !movable {
                                return;
                            }
                            let mut linked: Vec<(ItemRef, TimeRange)> = Vec::new();
                            for &selected in &items {
                                for r in timeline.linked(selected) {
                                    if !linked.iter().any(|(other, _)| *other == r)
                                        && let Ok(range) = timeline.item_range(r)
                                    {
                                        linked.push((r, range));
                                    }
                                }
                            }
                            let single_track = items.iter().all(|r| r.track == item.track);
                            let kind = timeline.track(item.track).expect("track").kind.clone();
                            drop(timeline);
                            let refs: Vec<ItemRef> = linked.iter().map(|(r, _)| *r).collect();
                            Drag::Move {
                                item,
                                items,
                                single_track,
                                linked,
                                range,
                                kind,
                                snap_points: self.snap_points(&refs),
                                offset: Time::zero(range.start.timebase),
                                target_track: item.track,
                                snapped: None,
                            }
                        }
                    }
                }
            };
            self.drag.replace(Some(drag));
        }

        fn drag_update(&self, x: f64, y: f64, dx: f64, dy: f64) {
            let fade = match self.drag.borrow().as_ref() {
                Some(Drag::Fade {
                    item,
                    edge,
                    start,
                    max,
                    key,
                }) => Some((*item, *edge, *start, *max, key.clone())),
                _ => None,
            };
            let keyframe = match self.drag.borrow().as_ref() {
                Some(Drag::Keyframe {
                    item,
                    origin,
                    at,
                    min,
                    max,
                    trimmed_start,
                    speed,
                    key,
                }) => Some((
                    *item,
                    *origin,
                    *at,
                    *min,
                    *max,
                    *trimmed_start,
                    *speed,
                    key.clone(),
                )),
                _ => None,
            };
            if let Some((item, origin, at, min, max, trimmed_start, speed, key)) = keyframe {
                // Whole frames of the timeline, in media time.
                let fps = self.fps();
                let local =
                    (origin - trimmed_start) / speed + dx / self.pixels_per_frame.get() / fps;
                let to =
                    (trimmed_start + (local * fps).round() / fps * speed).clamp(min, max.max(min));
                if (to - at).abs() > 1e-9 {
                    let moved = self.project().edit_merging(
                        &crate::i18n::gettext("Move Keyframe"),
                        &key,
                        |t| {
                            let clip = t
                                .track_mut(item.track)
                                .and_then(|track| track.children.get_mut(item.index))
                                .and_then(Composable::as_clip_mut)
                                .ok_or(EditError::NoItem {
                                    track: item.track,
                                    index: item.index,
                                })?;
                            clip.item.move_keyframes(at, to);
                            Ok::<_, EditError>(())
                        },
                    );
                    if moved.is_ok()
                        && let Some(Drag::Keyframe { at, .. }) = self.drag.borrow_mut().as_mut()
                    {
                        *at = to;
                    }
                }
                return;
            }
            let transition = match self.drag.borrow().as_ref() {
                Some(Drag::TransitionLength {
                    item,
                    edge,
                    start,
                    key,
                }) => Some((*item, *edge, *start, key.clone())),
                _ => None,
            };
            if let Some((item, edge, start, key)) = transition {
                let fps = self.project().frame_rate();
                let moved = dx / self.pixels_per_frame.get() / fps.as_f64();
                let seconds = match edge {
                    Edge::End => start + 2.0 * moved,
                    Edge::Start => start - 2.0 * moved,
                };
                let frames = (seconds * fps.as_f64()).round().max(2.0) as i64;
                let length = Time::from_frames(frames, fps);
                let _ = self.project().edit_merging(
                    &crate::i18n::gettext("Change Transition Length"),
                    &key,
                    |t| t.set_transition_length(item, length),
                );
                return;
            }
            if let Some((item, edge, start, max, key)) = fade {
                // Inwards lengthens the fade; whole frames.
                let fps = self.fps();
                let seconds = dx / self.pixels_per_frame.get() / fps;
                let seconds = match edge {
                    Edge::Start => start + seconds,
                    Edge::End => start - seconds,
                };
                let seconds = ((seconds.clamp(0.0, max) * fps).round() / fps).min(max);
                self.set_fade(item, edge, seconds, &key);
                return;
            }
            let mut drag = self.drag.borrow_mut();
            let Some(drag) = drag.as_mut() else { return };
            let frames = dx / self.pixels_per_frame.get();
            match drag {
                Drag::Scrub => self.obj().scrub_to(x + dx),
                // Applied above, outside the borrow.
                Drag::Fade { .. } | Drag::TransitionLength { .. } | Drag::Keyframe { .. } => {}
                Drag::Shift {
                    slide,
                    min,
                    max,
                    delta,
                    ..
                } => {
                    // Slipping right shows earlier media, as with Premiere's
                    // slip tool; sliding right moves the clip right.
                    let moved = if *slide { frames } else { -frames };
                    let mut time = self.frame_time(moved.round() as i64);
                    if let Some(min) = min {
                        time = time.max(*min);
                    }
                    if let Some(max) = max {
                        time = time.min(*max);
                    }
                    *delta = time;
                }
                Drag::Marker {
                    from,
                    to,
                    snap_points,
                    ..
                } => {
                    let candidate = self.time_to_frames(*from) + frames;
                    let snapped = self.snap(snap_points, &[candidate]).map(|(_, p)| p);
                    let frame = snapped.unwrap_or(candidate).round().max(0.0) as i64;
                    *to = self.frame_time(frame);
                }
                Drag::Band { from, to, base } => {
                    *to = (x + dx, y + dy);
                    let found = self.clips_in_band(*from, *to);
                    let mut items = base.clone();
                    items.extend(found.into_iter().filter(|r| !base.contains(r)));
                    self.select_items(items);
                }
                Drag::Move {
                    linked,
                    single_track,
                    range,
                    kind,
                    snap_points,
                    offset,
                    target_track,
                    snapped,
                    ..
                } => {
                    let start = self.time_to_frames(range.start) + frames;
                    let length = self.time_to_frames(range.duration);
                    let (mut new_start, snap) =
                        match self.snap(snap_points, &[start, start + length]) {
                            Some((0, point)) => (point, Some(point)),
                            Some((_, point)) => (point - length, Some(point)),
                            None => (start.round(), None),
                        };
                    // Linked clips can't move before zero either.
                    let earliest = linked
                        .iter()
                        .map(|(_, r)| self.time_to_frames(r.start))
                        .fold(f64::INFINITY, f64::min);
                    let lead = self.time_to_frames(range.start) - earliest;
                    new_start = new_start.max(lead);
                    *offset = self.frame_time(new_start.round() as i64) - range.start;
                    if snap.is_some_and(|s| {
                        (s - new_start).abs() < 1e-6 || (s - new_start - length).abs() < 1e-6
                    }) {
                        *snapped = snap;
                    } else {
                        *snapped = None;
                    }
                    let project = self.project();
                    let timeline = project.timeline();
                    if let Some(track) = self.track_at_y(y + dy)
                        && *single_track
                        && timeline
                            .track(track)
                            .is_some_and(|t| t.kind == *kind && !t.item.locked())
                    {
                        *target_track = track;
                    }
                }
                Drag::Trim {
                    edge,
                    range,
                    min,
                    max,
                    snap_points,
                    to,
                    snapped,
                    ..
                } => {
                    let original = match edge {
                        Edge::Start => range.start,
                        Edge::End => range.end(),
                    };
                    let candidate = self.time_to_frames(original) + frames;
                    let snap = self.snap(snap_points, &[candidate]).map(|(_, p)| p);
                    let frame = snap.unwrap_or(candidate).round() as i64;
                    let one_frame = self.frame_time(1);
                    let mut time = self.frame_time(frame);
                    // Keep at least one frame and stay within the limits.
                    time = match edge {
                        Edge::Start => time.min(range.end() - one_frame).max(*min),
                        Edge::End => time.max(range.start + one_frame),
                    };
                    if let Some(max) = max {
                        time = time.min(*max);
                    }
                    *to = time;
                    *snapped = snap.filter(|&s| (s - self.time_to_frames(time)).abs() < 1e-6);
                }
                Drag::Roll {
                    cut,
                    min,
                    max,
                    snap_points,
                    to,
                    snapped,
                    ..
                } => {
                    let candidate = self.time_to_frames(*cut) + frames;
                    let snap = self.snap(snap_points, &[candidate]).map(|(_, p)| p);
                    let frame = snap.unwrap_or(candidate).round() as i64;
                    let time = self.frame_time(frame).max(*min).min(*max);
                    *to = time;
                    *snapped = snap.filter(|&s| (s - self.time_to_frames(time)).abs() < 1e-6);
                }
            }
            self.obj().queue_draw();
        }

        fn drag_end(&self) {
            let Some(drag) = self.drag.take() else { return };
            let project = self.project().clone();
            match drag {
                Drag::Scrub
                | Drag::Band { .. }
                | Drag::Fade { .. }
                | Drag::TransitionLength { .. }
                | Drag::Keyframe { .. } => {}
                Drag::Shift {
                    item, slide, delta, ..
                } => {
                    if !delta.is_zero() {
                        let result = if slide {
                            project.edit(&crate::i18n::gettext("Slide Clip"), |t| {
                                t.slide(item, delta)
                            })
                        } else {
                            project
                                .edit(&crate::i18n::gettext("Slip Clip"), |t| t.slip(item, delta))
                                .map(|()| item)
                        };
                        if let Ok(item) = result {
                            self.set_selection(Some(item));
                        }
                    }
                }
                Drag::Marker {
                    index, from, to, ..
                } => {
                    if to != from {
                        let moved = project.edit(&crate::i18n::gettext("Move Marker"), |t| {
                            t.move_marker(index, to)
                        });
                        if let Ok(index) = moved {
                            self.select_marker(Some(index));
                            self.obj().set_playhead(to.frame(project.frame_rate()));
                        }
                    }
                }
                Drag::Move {
                    item,
                    items,
                    offset,
                    target_track,
                    ..
                } => {
                    if !offset.is_zero() || target_track != item.track {
                        let label =
                            crate::i18n::ngettext("Move Clip", "Move Clips", items.len() as u32);
                        let moved = project
                            .edit(&label, |t| t.move_items(&items, item, target_track, offset));
                        if let Ok(moved) = moved {
                            self.select_items(moved);
                        }
                    }
                }
                Drag::Trim {
                    item,
                    edge,
                    ripple,
                    range,
                    to,
                    ..
                } => {
                    let original = match edge {
                        Edge::Start => range.start,
                        Edge::End => range.end(),
                    };
                    if to != original {
                        let start = if ripple {
                            let _ = project.edit(&crate::i18n::gettext("Ripple Trim"), |t| {
                                t.ripple_trim(item, edge, to)
                            });
                            range.start
                        } else {
                            let _ = project.edit(&crate::i18n::gettext("Trim Clip"), |t| {
                                t.trim_item(item, edge, to)
                            });
                            match edge {
                                Edge::Start => to,
                                Edge::End => range.start,
                            }
                        };
                        self.set_selection(project.timeline().item_at(item.track, start));
                    }
                }
                Drag::Roll {
                    item,
                    edge,
                    cut,
                    to,
                    ..
                } => {
                    if to != cut {
                        let _ = project.edit(&crate::i18n::gettext("Roll Edit"), |t| {
                            t.roll(item, edge, to)
                        });
                        let start = match edge {
                            Edge::Start => to,
                            Edge::End => {
                                project.timeline().item_range(item).map_or(to, |r| r.start)
                            }
                        };
                        self.set_selection(project.timeline().item_at(item.track, start));
                    }
                }
            }
            self.obj().queue_draw();
        }

        fn context_menu(&self, x: f64, y: f64) {
            let (menu, track) = match self.hit(x, y) {
                Hit::Ruler | Hit::Marker(_) => {
                    self.obj().scrub_to(x);
                    (self.ruler_menu.get().expect("ruler menu"), None)
                }
                Hit::Item { item, .. } => {
                    self.set_selection(Some(item));
                    (self.menu.get().expect("menu"), None)
                }
                Hit::Header { track, .. } => {
                    let (video, audio) = self.track_menus.get().expect("track menus");
                    let project = self.project();
                    let is_video = project
                        .timeline()
                        .track(track)
                        .is_some_and(|t| t.kind == TrackKind::Video);
                    (if is_video { video } else { audio }, Some(track))
                }
                _ => return,
            };
            if let Some(track) = track {
                self.menu_track.set(Some(track));
                self.update_track_actions();
            }
            menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            menu.popup();
        }

        /// The `track.*` actions of the track menus: `rename`, `delete`,
        /// and the toggles `muted` (for video: hidden) and `locked`.
        fn add_track_actions(&self) {
            let obj = self.obj();
            let group = &self.track_actions;
            let toggle = |name: &str, f: fn(&super::TimelineView, usize, bool)| {
                let action = gio::SimpleAction::new_stateful(name, None, &false.to_variant());
                action.connect_change_state(glib::clone!(
                    #[weak]
                    obj,
                    move |_, value| {
                        if let (Some(track), Some(on)) = (
                            obj.imp().menu_track.get(),
                            value.and_then(|v| v.get::<bool>()),
                        ) {
                            f(&obj, track, on);
                        }
                    }
                ));
                group.add_action(&action);
            };
            toggle("muted", |view, track, muted| {
                view.set_track_enabled(track, !muted)
            });
            toggle("locked", |view, track, locked| {
                view.set_track_locked(track, locked)
            });
            toggle("sync", |view, track, on| {
                view.set_track_sync_locked(track, on)
            });
            toggle("solo", |view, track, on| view.set_track_solo(track, on));
            let simple = |name: &str, f: fn(&super::TimelineView, usize)| {
                let action = gio::SimpleAction::new(name, None);
                action.connect_activate(glib::clone!(
                    #[weak]
                    obj,
                    move |_, _| {
                        if let Some(track) = obj.imp().menu_track.get() {
                            f(&obj, track);
                        }
                    }
                ));
                group.add_action(&action);
            };
            simple("rename", |view, track| view.show_rename_dialog(track));
            simple("delete", |view, track| view.delete_track(track));
            obj.insert_action_group("track", Some(group));
        }

        /// Shows the state of the menu's track in the toggles.
        fn update_track_actions(&self) {
            let project = self.project();
            let timeline = project.timeline();
            let Some(track) = self.menu_track.get().and_then(|t| timeline.track(t)) else {
                return;
            };
            let set = |name: &str, on: bool| {
                if let Some(action) = self
                    .track_actions
                    .lookup_action(name)
                    .and_downcast::<gio::SimpleAction>()
                {
                    action.set_state(&on.to_variant());
                }
            };
            set("muted", !track.item.enabled);
            set("locked", track.item.locked());
            set("sync", track.item.sync_locked());
            set("solo", track.item.solo());
            if let Some(delete) = self
                .track_actions
                .lookup_action("delete")
                .and_downcast::<gio::SimpleAction>()
            {
                delete.set_enabled(!track.item.locked());
            }
        }

        pub fn set_selection(&self, item: Option<ItemRef>) {
            self.select_items(item.into_iter().collect());
        }

        /// Selects timeline marker `index` (clearing the clip selection).
        pub fn select_marker(&self, index: Option<usize>) {
            if index.is_some() {
                self.select_items(Vec::new());
            }
            if self.marker.replace(index) != index {
                self.obj().emit_by_name::<()>("selection-changed", &[]);
            }
            self.obj().queue_draw();
        }

        /// Selects `items`; the first is the primary one.
        pub fn select_items(&self, mut items: Vec<ItemRef>) {
            if !items.is_empty() && self.marker.replace(None).is_some() {
                self.obj().emit_by_name::<()>("selection-changed", &[]);
            }
            let mut seen = Vec::new();
            items.retain(|r| {
                let new = !seen.contains(r);
                seen.push(*r);
                new
            });
            let primary = items.first().copied();
            let others: Vec<ItemRef> = items.iter().skip(1).copied().collect();
            let changed =
                self.selection.replace(primary) != primary || *self.others.borrow() != others;
            self.others.replace(others);
            if changed {
                self.obj().emit_by_name::<()>("selection-changed", &[]);
                match items.len() {
                    0 => {}
                    1 => self.announce_item(items[0]),
                    n => self.obj().announce(
                        &crate::i18n::fill(
                            &crate::i18n::ngettext(
                                "{n} clip selected",
                                "{n} clips selected",
                                n as u32,
                            ),
                            &[("n", &n)],
                        ),
                        gtk::AccessibleAnnouncementPriority::Medium,
                    ),
                }
            }
            self.obj().queue_draw();
        }

        /// The selected items, the primary one first.
        pub fn selected_items(&self) -> Vec<ItemRef> {
            self.selection
                .get()
                .into_iter()
                .chain(self.others.borrow().iter().copied())
                .collect()
        }

        fn is_selected(&self, item: ItemRef) -> bool {
            self.selection.get() == Some(item) || self.others.borrow().contains(&item)
        }

        fn toggle_selected(&self, item: ItemRef) {
            let mut items = self.selected_items();
            if items.contains(&item) {
                items.retain(|&r| r != item);
            } else {
                items.insert(0, item);
            }
            self.select_items(items);
        }

        /// The clips (not gaps or transitions) a rubber band touches.
        fn clips_in_band(&self, from: (f64, f64), to: (f64, f64)) -> Vec<ItemRef> {
            let (x0, x1) = (from.0.min(to.0), from.0.max(to.0));
            let (y0, y1) = (from.1.min(to.1), from.1.max(to.1));
            let (f0, f1) = (self.x_to_frame_f64(x0), self.x_to_frame_f64(x1));
            let project = self.project();
            let timeline = project.timeline();
            let mut found = Vec::new();
            for (row, track_index) in rows(&timeline).into_iter().enumerate() {
                let top = f64::from(row_y(row));
                if top + f64::from(TRACK_HEIGHT) < y0 || top > y1 {
                    continue;
                }
                let Some(track) = timeline.track(track_index) else {
                    continue;
                };
                for (index, (range, child)) in track.child_ranges().enumerate() {
                    if !matches!(child, Composable::Clip(_)) {
                        continue;
                    }
                    let start = self.time_to_frames(range.start);
                    let end = self.time_to_frames(range.end());
                    if end > f0 && start < f1 {
                        found.push(ItemRef {
                            track: track_index,
                            index,
                        });
                    }
                }
            }
            found
        }

        /// Tells screen readers what `item` is, since the selection is only
        /// drawn.
        fn announce_item(&self, item: ItemRef) {
            use crate::i18n::{fill, gettext};
            let project = self.project();
            let model = project.timeline();
            let (Ok(child), Ok(range)) = (model.item(item), model.item_range(item)) else {
                return;
            };
            let fps = model.frame_rate();
            let text = match child {
                Composable::Transition(t) => fill(
                    &gettext("Dissolve, {length}"),
                    &[(
                        "length",
                        &project.timecode((t.in_offset + t.out_offset).frame(fps)),
                    )],
                ),
                _ => fill(
                    &gettext("{name}, {start} to {end}"),
                    &[
                        ("name", &child.name()),
                        ("start", &project.timecode(range.start.frame(fps))),
                        ("end", &project.timecode(range.end().frame(fps))),
                    ],
                ),
            };
            drop(model);
            self.obj()
                .announce(&text, gtk::AccessibleAnnouncementPriority::Medium);
        }

        /// Opens the clip menu at the selected clip (Shift+F10, Menu), or
        /// the track menu when focus is in a track header.
        fn selection_menu(&self) {
            let focused = self.obj().focus_child();
            let header_row = self
                .headers
                .borrow()
                .iter()
                .position(|h| Some(h.upcast_ref::<gtk::Widget>()) == focused.as_ref());
            if let Some(row) = header_row {
                let y = row_y(row) + TRACK_HEIGHT / 2.0;
                self.context_menu(f64::from(HEADER_WIDTH / 2.0), f64::from(y));
                return;
            }
            let Some(item) = self.selection.get() else {
                return;
            };
            let project = self.project();
            let model = project.timeline();
            let Ok(range) = model.item_range(item) else {
                return;
            };
            let Some(row) = rows(&model).iter().position(|&t| t == item.track) else {
                return;
            };
            drop(model);
            let rect = self.clip_rect(range, row_y(row));
            let menu = self.menu.get().expect("menu");
            menu.set_pointing_to(Some(&gdk::Rectangle::new(
                rect.x() as i32,
                rect.y() as i32,
                rect.width().max(1.0) as i32,
                rect.height() as i32,
            )));
            menu.popup();
        }

        fn on_scroll(&self, zoom: bool, dx: f64, dy: f64) {
            if zoom {
                // Keep the frame under the pointer in place.
                let x = self.pointer_x.get().max(HEADER_WIDTH as f64);
                let anchor = self.x_to_frame_f64(x);
                let ppf = (self.pixels_per_frame.get() * 1.15f64.powf(-dy)).clamp(0.02, 40.0);
                self.pixels_per_frame.set(ppf);
                let scroll = anchor - (x - HEADER_WIDTH as f64) / ppf;
                self.scroll.set(scroll.max(0.0));
            } else {
                let delta = (dx + dy) * 40.0 / self.pixels_per_frame.get();
                self.scroll.set((self.scroll.get() + delta).max(0.0));
            }
            self.obj().queue_draw();
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
        let items = self.selected_items();
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

    /// Mutes (audio) or hides (video) track `index`, or turns it back on:
    /// OTIO's `enabled`, which playback and export respect.
    pub fn set_track_enabled(&self, index: usize, enabled: bool) {
        let project = self.imp().project().clone();
        let Some(kind) = project.timeline().track(index).map(|t| t.kind.clone()) else {
            return;
        };
        let _ = project.edit(&enable_label(&kind, enabled), |t| {
            let track = t.track_mut(index).ok_or(EditError::NoTrack(index))?;
            track.item.enabled = enabled;
            Ok::<_, EditError>(())
        });
    }

    /// Locks or unlocks track `index`; locked tracks reject edits.
    pub fn set_track_locked(&self, index: usize, locked: bool) {
        let project = self.imp().project().clone();
        let _ = project.edit(&lock_label(locked), |t| {
            let track = t.track_mut(index).ok_or(EditError::NoTrack(index))?;
            track.item.set_locked(locked);
            Ok::<_, EditError>(())
        });
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

    /// Applies a header toggle to track `index`.
    fn set_track_toggle(&self, index: usize, toggle: Toggle, on: bool) {
        match toggle {
            Toggle::Off => self.set_track_enabled(index, !on),
            Toggle::Solo => self.set_track_solo(index, on),
            Toggle::Lock => self.set_track_locked(index, on),
            Toggle::Sync => self.set_track_sync_locked(index, on),
        }
    }

    /// Solos audio track `index`, or not: while any track is soloed, only
    /// soloed tracks are heard.
    pub fn set_track_solo(&self, index: usize, on: bool) {
        let project = self.imp().project().clone();
        let label = if on {
            crate::i18n::gettext("Solo Track")
        } else {
            crate::i18n::gettext("Unsolo Track")
        };
        let _ = project.edit(&label, |t| {
            let track = t.track_mut(index).ok_or(EditError::NoTrack(index))?;
            track.item.set_solo(on);
            Ok::<_, EditError>(())
        });
    }

    /// Makes track `index` follow ripple edits on other tracks, or not.
    pub fn set_track_sync_locked(&self, index: usize, on: bool) {
        let project = self.imp().project().clone();
        let label = if on {
            crate::i18n::gettext("Sync Lock Track")
        } else {
            crate::i18n::gettext("Remove Sync Lock")
        };
        let _ = project.edit(&label, |t| {
            let track = t.track_mut(index).ok_or(EditError::NoTrack(index))?;
            track.item.set_sync_locked(on);
            Ok::<_, EditError>(())
        });
    }

    pub fn rename_track(&self, index: usize, name: &str) {
        let project = self.imp().project().clone();
        let _ = project.edit(&crate::i18n::gettext("Rename Track"), |t| {
            let track = t.track_mut(index).ok_or(EditError::NoTrack(index))?;
            track.item.name = name.to_owned();
            Ok::<_, EditError>(())
        });
    }

    /// Deletes track `index` with its clips (not if it is locked).
    pub fn delete_track(&self, index: usize) {
        let project = self.imp().project().clone();
        if project
            .edit(&crate::i18n::gettext("Delete Track"), |t| {
                t.remove_track(index)
            })
            .is_ok()
        {
            // Selections are by track index, which has changed.
            self.set_selection(None);
        }
    }

    /// Asks for a new name for track `index`.
    pub fn show_rename_dialog(&self, index: usize) {
        let project = self.imp().project().clone();
        let Some(name) = project.timeline().track(index).map(|t| t.item.name.clone()) else {
            return;
        };
        let builder = rename_dialog();
        let dialog: gtk::Window = builder.object("dialog").expect("dialog");
        let entry: gtk::Entry = builder.object("name").expect("name");
        let cancel: gtk::Button = builder.object("cancel").expect("cancel");
        let rename: gtk::Button = builder.object("rename").expect("rename");
        dialog.set_transient_for(self.root().and_downcast_ref::<gtk::Window>());
        entry.set_text(&name);
        cancel.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            move |_| dialog.close()
        ));
        rename.connect_clicked(glib::clone!(
            #[weak(rename_to = view)]
            self,
            #[weak]
            dialog,
            #[weak]
            entry,
            move |_| {
                view.rename_track(index, entry.text().trim());
                dialog.close();
            }
        ));
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::NamedAction::new("window.close")),
        ));
        dialog.add_controller(shortcuts);
        dialog.present();
    }

    fn scrub_to(&self, x: f64) {
        self.set_playhead(self.imp().x_to_frame(x));
    }
}

/// The rename dialog for tracks (`resources/ui/rename-track.ui`).
pub(crate) fn rename_dialog() -> gtk::Builder {
    let builder = gtk::Builder::new();
    builder.set_translation_domain(Some(crate::i18n::DOMAIN));
    builder
        .add_from_resource("/io/github/feliwir/Nox/ui/rename-track.ui")
        .expect("rename-track.ui is valid");
    builder
}
