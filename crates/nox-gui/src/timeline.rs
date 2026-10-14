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
use nox_project::{Composable, Edge, EditError, ItemRef, Time, TimeRange, Timeline, TrackKind};
use std::rc::Rc;

use crate::media_object::MediaObject;
use crate::project::Project;

const RULER_HEIGHT: f32 = 24.0;
const TRACK_HEIGHT: f32 = 44.0;
const HEADER_WIDTH: f32 = 72.0;
const MIN_TICK_SPACING: f64 = 90.0;
/// How close (in pixels) the pointer must be to a clip edge to trim.
const EDGE_GRAB: f64 = 6.0;
/// How close the pointer must be to a cut between two clips to roll it.
const ROLL_GRAB: f64 = 3.0;
/// Size and spacing of the toggles in track headers.
const HEADER_BUTTON: f32 = 16.0;
const HEADER_BUTTON_X: [f32; 2] = [8.0, 30.0];
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
const CLIP_BORDER: gdk::RGBA = rgb(0x12, 0x13, 0x15);
const SELECTION: gdk::RGBA = rgb(0xf0, 0xb0, 0x30);
const SNAP_LINE: gdk::RGBA = rgb(0xf0, 0xd0, 0x40);
const TICK: gdk::RGBA = rgb(0x70, 0x72, 0x78);
const TEXT: gdk::RGBA = rgb(0xb0, 0xb2, 0xb8);
const CLIP_TEXT: gdk::RGBA = rgb(0xe8, 0xea, 0xee);
const PLAYHEAD: gdk::RGBA = rgb(0xe5, 0x3b, 0x3b);
const MARK: gdk::RGBA = rgb(0x8c, 0xb4, 0xf0);
const OUTSIDE_MARKS: gdk::RGBA = gdk::RGBA::new(0.0, 0.0, 0.0, 0.4);
const HEADER_ICON: gdk::RGBA = rgb(0x80, 0x82, 0x88);
const HEADER_ICON_ACTIVE: gdk::RGBA = rgb(0xf0, 0x8a, 0x40);

const fn rgb(r: u8, g: u8, b: u8) -> gdk::RGBA {
    gdk::RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
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
fn clip_label(child: &Composable) -> String {
    let mut label = child.name().to_owned();
    if let Composable::Clip(clip) = child {
        let item = &clip.item;
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
    let builder = gtk::Builder::new();
    builder.set_translation_domain(Some(crate::i18n::DOMAIN));
    builder
        .add_from_resource("/io/github/nox/Nox/ui/menus.ui")
        .expect("menus.ui is valid");
    builder.object("clip_menu").expect("clip_menu in menus.ui")
}

/// The track header menus for video and audio tracks (`menus.ui`); their
/// `track.*` actions work on the track the menu was opened for.
pub(crate) fn track_menus() -> (gio::MenuModel, gio::MenuModel) {
    let builder = gtk::Builder::new();
    builder.set_translation_domain(Some(crate::i18n::DOMAIN));
    builder
        .add_from_resource("/io/github/nox/Nox/ui/menus.ui")
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

/// The toggles in a track header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderButton {
    /// Mute for audio, show/hide for video (the track's `enabled`).
    Enabled,
    Lock,
}

impl HeaderButton {
    const ALL: [HeaderButton; 2] = [Self::Enabled, Self::Lock];

    /// Where the toggle sits in a header row starting at `y`.
    fn rect(self, y: f32) -> graphene::Rect {
        let x = HEADER_BUTTON_X[self as usize];
        graphene::Rect::new(
            x,
            y + TRACK_HEIGHT - HEADER_BUTTON - 6.0,
            HEADER_BUTTON,
            HEADER_BUTTON,
        )
    }
}

/// An interaction in progress.
#[derive(Debug, Clone)]
enum Drag {
    Scrub,
    Move {
        item: ItemRef,
        /// Linked items moving along (including `item`).
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
    /// A clip or transition; near an edge of a clip, the edge, and in the
    /// middle of a cut between two clips, `roll`.
    Item {
        item: ItemRef,
        edge: Option<Edge>,
        roll: bool,
    },
    Track,
    /// A track header, and the toggle in it.
    Header {
        track: usize,
        button: Option<HeaderButton>,
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
        pub selection: Cell<Option<ItemRef>>,
        pub snapping: Cell<bool>,
        drag: RefCell<Option<Drag>>,
        /// Media being dragged over the timeline: stack index and frame.
        pub drop_hint: Cell<Option<(usize, i64)>>,
        pub pointer_x: Cell<f64>,
        /// Context menu for clips.
        pub menu: OnceCell<gtk::PopoverMenu>,
        /// Context menus for video and audio tracks.
        pub track_menus: OnceCell<(gtk::PopoverMenu, gtk::PopoverMenu)>,
        /// The track (stack index) the track menu was opened for.
        pub menu_track: Cell<Option<usize>>,
        /// The `track.*` actions of the track menus.
        pub track_actions: gio::SimpleActionGroup,
    }

    impl Default for TimelineView {
        fn default() -> Self {
            Self {
                project: OnceCell::new(),
                playhead: Cell::new(0),
                scroll: Cell::new(0.0),
                pixels_per_frame: Cell::new(4.0),
                selection: Cell::new(None),
                snapping: Cell::new(true),
                drag: RefCell::new(None),
                drop_hint: Cell::new(None),
                pointer_x: Cell::new(0.0),
                menu: OnceCell::new(),
                track_menus: OnceCell::new(),
                menu_track: Cell::new(None),
                track_actions: gio::SimpleActionGroup::new(),
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
            obj.set_focusable(true);
            obj.set_overflow(gtk::Overflow::Hidden);
            obj.update_property(&[
                gtk::accessible::Property::Label(&crate::i18n::gettext("Timeline")),
                gtk::accessible::Property::Description(&crate::i18n::gettext(
                    "Left and Right step one frame, Up and Down jump to edit points, \
                     Shift+V selects the clip at the playhead, Shift+F10 opens its menu, \
                     Delete removes it.",
                )),
            ]);

            let drag = gtk::GestureDrag::new();
            drag.connect_drag_begin(glib::clone!(
                #[weak]
                obj,
                move |gesture, x, y| {
                    let ripple = gesture
                        .current_event_state()
                        .contains(gdk::ModifierType::CONTROL_MASK);
                    obj.imp().drag_begin(x, y, ripple)
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
                    view.delete_selection();
                    glib::Propagation::Stop
                })),
            ));
            // Through the window action, so the menu shows the state.
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("n"),
                Some(gtk::NamedAction::new("win.snapping")),
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
                move |_, value, x, y| {
                    let imp = obj.imp();
                    imp.drop_hint.set(None);
                    obj.queue_draw();
                    let Ok(media) = value.get::<MediaObject>() else {
                        return false;
                    };
                    let Some(info) = media.info() else {
                        tracing::info!("{} is still loading", media.name());
                        return false;
                    };
                    let frame = imp.drop_frame(x);
                    imp.project()
                        .add_media(&info, imp.track_at_y(y), frame)
                        .is_ok()
                }
            ));
            obj.add_controller(drop);

            let menu = gtk::PopoverMenu::from_model(Some(&clip_menu()));
            menu.set_parent(&*obj);
            menu.set_has_arrow(false);
            menu.set_halign(gtk::Align::Start);
            self.menu.set(menu).expect("menu set once");
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
                let Hit::Header {
                    track,
                    button: Some(button),
                } = imp.hit(x.into(), y.into())
                else {
                    return false;
                };
                let project = imp.project();
                let timeline = project.timeline();
                let Some(track) = timeline.track(track) else {
                    return false;
                };
                // What clicking would do.
                tooltip.set_text(Some(&match button {
                    HeaderButton::Enabled => enable_label(&track.kind, !track.item.enabled),
                    HeaderButton::Lock => lock_label(!track.item.locked()),
                }));
                true
            });
        }

        fn dispose(&self) {
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
            for (row, &index) in rows.iter().enumerate() {
                self.draw_header(snapshot, &timeline, index, row);
            }
        }
    }

    fn row_y(row: usize) -> f32 {
        RULER_HEIGHT + row as f32 * TRACK_HEIGHT
    }

    impl TimelineView {
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
                    self.draw_text(
                        snapshot,
                        &self.project().timecode(frame),
                        x + 3.0,
                        2.0,
                        &TEXT,
                    );
                }
                frame += minor;
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

        /// A track header: name, mute (or show/hide) and lock toggles.
        fn draw_header(
            &self,
            snapshot: &gtk::Snapshot,
            timeline: &Timeline,
            index: usize,
            row: usize,
        ) {
            let track = timeline.track(index).expect("track");
            let y = row_y(row);
            let rect = graphene::Rect::new(0.0, y, HEADER_WIDTH - 1.0, TRACK_HEIGHT - 1.0);
            snapshot.append_color(&TRACK_HEADER, &rect);
            let name = if track.item.name.is_empty() {
                format!("{}{}", &track.kind.as_str()[..1], row + 1)
            } else {
                track.item.name.clone()
            };
            snapshot.push_clip(&rect);
            self.draw_text(snapshot, &name, 8.0, y + 4.0, &TEXT);
            snapshot.pop();
            for button in HeaderButton::ALL {
                let (icon, active) = match button {
                    HeaderButton::Enabled => {
                        let icon = match (&track.kind, track.item.enabled) {
                            (TrackKind::Audio, true) => "audio-volume-high-symbolic",
                            (TrackKind::Audio, false) => "audio-volume-muted-symbolic",
                            (_, true) => "view-reveal-symbolic",
                            (_, false) => "view-conceal-symbolic",
                        };
                        (icon, !track.item.enabled)
                    }
                    HeaderButton::Lock if track.item.locked() => ("changes-prevent-symbolic", true),
                    HeaderButton::Lock => ("changes-allow-symbolic", false),
                };
                let color = if active {
                    HEADER_ICON_ACTIVE
                } else {
                    HEADER_ICON
                };
                self.draw_icon(snapshot, icon, button.rect(y), &color);
            }
        }

        fn draw_icon(
            &self,
            snapshot: &gtk::Snapshot,
            name: &str,
            rect: graphene::Rect,
            color: &gdk::RGBA,
        ) {
            let obj = self.obj();
            let theme = gtk::IconTheme::for_display(&obj.display());
            let icon = theme.lookup_icon(
                name,
                &[],
                rect.width() as i32,
                obj.scale_factor(),
                gtk::TextDirection::None,
                gtk::IconLookupFlags::empty(),
            );
            snapshot.save();
            snapshot.translate(&graphene::Point::new(rect.x(), rect.y()));
            icon.snapshot_symbolic(
                snapshot,
                rect.width().into(),
                rect.height().into(),
                &[*color],
            );
            snapshot.restore();
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
                let color = match child {
                    Composable::Gap(_) => continue,
                    // Drawn below, over both neighbors.
                    Composable::Transition(_) => continue,
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
                let selected = self.selection.get() == Some(item) && moving.is_empty();
                self.draw_clip(
                    snapshot,
                    range,
                    y,
                    &clip_label(child),
                    with_alpha(color, alpha),
                    selected,
                );
            }
            for (i, (range, child)) in track.child_ranges().enumerate() {
                if let Composable::Transition(_) = child {
                    let selected = self.selection.get()
                        == Some(ItemRef {
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
                return Hit::Ruler;
            }
            let Some(track_index) = self.track_at_y(y) else {
                return Hit::Nothing;
            };
            let in_row = (y - RULER_HEIGHT as f64) % TRACK_HEIGHT as f64;
            if x < HEADER_WIDTH as f64 {
                let row_y = (y - in_row) as f32;
                let point = graphene::Point::new(x as f32, y as f32);
                let button = HeaderButton::ALL
                    .into_iter()
                    .find(|b| b.rect(row_y).contains_point(&point));
                return Hit::Header {
                    track: track_index,
                    button,
                };
            }
            let project = self.project();
            let timeline = project.timeline();
            let track = timeline.track(track_index).expect("track");
            // Transitions are drawn over the lower half of the cut.
            if in_row >= TRACK_HEIGHT as f64 / 2.0 {
                for (index, (range, child)) in track.child_ranges().enumerate() {
                    if !matches!(child, Composable::Transition(_)) {
                        continue;
                    }
                    let x0 = self.frame_to_x(self.time_to_frames(range.start));
                    let x1 = self.frame_to_x(self.time_to_frames(range.end()));
                    if x >= x0.min(x1 - 4.0) && x <= x1.max(x0 + 4.0) {
                        let item = ItemRef {
                            track: track_index,
                            index,
                        };
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

        fn hover(&self, x: f64, y: f64) {
            self.pointer_x.set(x);
            if self.drag.borrow().is_some() {
                return;
            }
            let cursor = match self.hit(x, y) {
                Hit::Item {
                    edge: Some(_),
                    roll: true,
                    ..
                } => Some("ew-resize"),
                Hit::Item { edge: Some(_), .. } => Some("col-resize"),
                Hit::Header {
                    button: Some(_), ..
                } => Some("pointer"),
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

        /// Starts a drag at `x`, `y`. With `ripple` (Ctrl held), dragging
        /// an edge ripple-trims instead of trimming or rolling.
        fn drag_begin(&self, x: f64, y: f64, ripple: bool) {
            tracing::trace!("timeline press at {x:.0}, {y:.0}");
            let obj = self.obj();
            obj.grab_focus();
            let project = self.project().clone();
            let drag = match self.hit(x, y) {
                Hit::Header { track, button } => {
                    match button {
                        Some(HeaderButton::Enabled) => {
                            let enabled = project
                                .timeline()
                                .track(track)
                                .is_some_and(|t| t.item.enabled);
                            obj.set_track_enabled(track, !enabled);
                        }
                        Some(HeaderButton::Lock) => {
                            let locked = project
                                .timeline()
                                .track(track)
                                .is_some_and(|t| t.item.locked());
                            obj.set_track_locked(track, !locked);
                        }
                        None => {}
                    }
                    return;
                }
                Hit::Ruler | Hit::Track | Hit::Nothing => {
                    if !matches!(self.hit(x, y), Hit::Ruler) {
                        self.set_selection(None);
                    }
                    obj.scrub_to(x);
                    Drag::Scrub
                }
                Hit::Item { item, edge, roll } => {
                    self.set_selection(Some(item));
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
                            let linked: Vec<(ItemRef, TimeRange)> = timeline
                                .linked(item)
                                .into_iter()
                                .filter_map(|r| Some((r, timeline.item_range(r).ok()?)))
                                .collect();
                            let kind = timeline.track(item.track).expect("track").kind.clone();
                            drop(timeline);
                            let refs: Vec<ItemRef> = linked.iter().map(|(r, _)| *r).collect();
                            Drag::Move {
                                item,
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
            let mut drag = self.drag.borrow_mut();
            let Some(drag) = drag.as_mut() else { return };
            let frames = dx / self.pixels_per_frame.get();
            match drag {
                Drag::Scrub => self.obj().scrub_to(x + dx),
                Drag::Move {
                    linked,
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
                Drag::Scrub => {}
                Drag::Move {
                    item,
                    offset,
                    target_track,
                    ..
                } => {
                    if !offset.is_zero() || target_track != item.track {
                        let moved = project.edit(&crate::i18n::gettext("Move Clip"), |t| {
                            t.move_item(item, target_track, offset)
                        });
                        if let Ok(moved) = moved {
                            self.set_selection(Some(moved));
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
            if let Some(delete) = self
                .track_actions
                .lookup_action("delete")
                .and_downcast::<gio::SimpleAction>()
            {
                delete.set_enabled(!track.item.locked());
            }
        }

        pub fn set_selection(&self, item: Option<ItemRef>) {
            if self.selection.replace(item) != item {
                self.obj().emit_by_name::<()>("selection-changed", &[]);
                if let Some(item) = item {
                    self.announce_item(item);
                }
            }
            self.obj().queue_draw();
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

        /// Opens the clip menu at the selected clip (Shift+F10, Menu).
        fn selection_menu(&self) {
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
                // Selections refer to positions, which edits may change.
                let imp = view.imp();
                let valid = imp
                    .selection
                    .get()
                    .is_some_and(|item| project.timeline().item(item).is_ok_and(|c| !c.is_gap()));
                if !valid {
                    imp.set_selection(None);
                }
                view.queue_resize();
                view.queue_draw();
            }
        ));
        if self.imp().project.set(project).is_err() {
            panic!("TimelineView::set_project called twice");
        }
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

    /// Changes the selected clip's properties as one undoable edit called
    /// `label`. With a `merge_key`, consecutive changes with the same key
    /// make one undo step (e.g. while a slider is dragged).
    pub fn edit_selected_clip(
        &self,
        label: &str,
        merge_key: Option<&str>,
        change: impl FnOnce(&mut nox_project::Item),
    ) {
        let Some(item) = self.selection() else {
            return;
        };
        let edit = |t: &mut Timeline| {
            let track = t
                .track_mut(item.track)
                .ok_or(EditError::NoTrack(item.track))?;
            let clip = track
                .children
                .get_mut(item.index)
                .and_then(Composable::as_clip_mut)
                .ok_or(EditError::NotTrimmable)?;
            change(&mut clip.item);
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

    /// Removes the selected clip (and the clips linked to it).
    pub fn delete_selection(&self) {
        let Some(item) = self.selection() else { return };
        let project = self.imp().project().clone();
        if project
            .edit(&crate::i18n::gettext("Delete Clip"), |t| {
                t.remove_item(item)
            })
            .is_ok()
        {
            self.set_selection(None);
        }
    }

    /// Removes the selected clip (and the clips linked to it) and closes
    /// the gaps on their tracks.
    pub fn ripple_delete_selection(&self) {
        let Some(item) = self.selection() else { return };
        let project = self.imp().project().clone();
        if project
            .edit(&crate::i18n::gettext("Ripple Delete"), |t| {
                t.ripple_remove_item(item)
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
        .add_from_resource("/io/github/nox/Nox/ui/rename-track.ui")
        .expect("rename-track.ui is valid");
    builder
}
