//! Timeline: ruler, tracks and playhead, drawn directly with `snapshot()`.
//!
//! Drag anywhere to scrub, scroll to pan, Ctrl+scroll to zoom.

use gtk::{gdk, glib, graphene, prelude::*, subclass::prelude::*};
use nox_project::{Rational, timecode};

const RULER_HEIGHT: f32 = 24.0;
const TRACK_HEIGHT: f32 = 40.0;
const HEADER_WIDTH: f32 = 64.0;
const MIN_TICK_SPACING: f64 = 90.0;
const TRACKS: &[&str] = &["V2", "V1", "A1", "A2"];

const BACKGROUND: gdk::RGBA = rgb(0x1a, 0x1b, 0x1e);
const RULER: gdk::RGBA = rgb(0x26, 0x27, 0x2b);
const TRACK_HEADER: gdk::RGBA = rgb(0x2b, 0x2c, 0x31);
const VIDEO_TRACK: gdk::RGBA = rgb(0x22, 0x24, 0x2a);
const AUDIO_TRACK: gdk::RGBA = rgb(0x20, 0x26, 0x24);
const TICK: gdk::RGBA = rgb(0x70, 0x72, 0x78);
const TEXT: gdk::RGBA = rgb(0xb0, 0xb2, 0xb8);
const PLAYHEAD: gdk::RGBA = rgb(0xe5, 0x3b, 0x3b);

const fn rgb(r: u8, g: u8, b: u8) -> gdk::RGBA {
    gdk::RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}

mod imp {
    use super::*;
    use std::cell::Cell;
    use std::sync::OnceLock;

    pub struct TimelineView {
        /// Sequence frame rate; becomes a sequence setting in M3.
        pub fps: Cell<Rational>,
        pub playhead: Cell<i64>,
        /// Frame shown at the left edge of the track area.
        pub scroll: Cell<f64>,
        pub pixels_per_frame: Cell<f64>,
    }

    impl Default for TimelineView {
        fn default() -> Self {
            Self {
                fps: Cell::new(Rational::new(25, 1)),
                playhead: Cell::new(0),
                scroll: Cell::new(0.0),
                pixels_per_frame: Cell::new(4.0),
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
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_focusable(true);
            obj.set_overflow(gtk::Overflow::Hidden);

            let drag = gtk::GestureDrag::new();
            drag.connect_drag_begin(glib::clone!(
                #[weak]
                obj,
                move |_, x, _| obj.scrub_to(x)
            ));
            drag.connect_drag_update(glib::clone!(
                #[weak]
                obj,
                move |gesture, dx, _| {
                    if let Some((x, _)) = gesture.start_point() {
                        obj.scrub_to(x + dx);
                    }
                }
            ));
            obj.add_controller(drag);

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
        }
    }

    impl WidgetImpl for TimelineView {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            match orientation {
                gtk::Orientation::Horizontal => (200, 200, -1, -1),
                // Any height works since drawing is clipped; tracks will scroll vertically later.
                _ => (
                    0,
                    (RULER_HEIGHT + TRACK_HEIGHT * TRACKS.len() as f32) as i32,
                    -1,
                    -1,
                ),
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (width, height) = (obj.width() as f32, obj.height() as f32);

            snapshot.append_color(&BACKGROUND, &graphene::Rect::new(0.0, 0.0, width, height));
            snapshot.append_color(&RULER, &graphene::Rect::new(0.0, 0.0, width, RULER_HEIGHT));

            for (i, name) in TRACKS.iter().enumerate() {
                let y = RULER_HEIGHT + i as f32 * TRACK_HEIGHT;
                let color = if name.starts_with('V') {
                    VIDEO_TRACK
                } else {
                    AUDIO_TRACK
                };
                snapshot.append_color(
                    &color,
                    &graphene::Rect::new(HEADER_WIDTH, y, width - HEADER_WIDTH, TRACK_HEIGHT - 1.0),
                );
                snapshot.append_color(
                    &TRACK_HEADER,
                    &graphene::Rect::new(0.0, y, HEADER_WIDTH - 1.0, TRACK_HEIGHT - 1.0),
                );
                self.draw_text(snapshot, name, 8.0, y + 12.0);
            }

            snapshot.push_clip(&graphene::Rect::new(
                HEADER_WIDTH,
                0.0,
                width - HEADER_WIDTH,
                height,
            ));
            self.draw_ruler(snapshot, width);
            if let Some(x) = self.frame_to_x(self.playhead.get()) {
                snapshot.append_color(&PLAYHEAD, &graphene::Rect::new(x - 0.5, 0.0, 1.5, height));
                snapshot.append_color(&PLAYHEAD, &graphene::Rect::new(x - 5.0, 0.0, 11.0, 6.0));
            }
            snapshot.pop();
        }
    }

    impl TimelineView {
        fn draw_ruler(&self, snapshot: &gtk::Snapshot, width: f32) {
            let ppf = self.pixels_per_frame.get();
            let fps = self.fps.get();
            let nominal = ((fps.num + fps.den / 2) / fps.den).max(1);
            let step = [1, 2, 5, 10]
                .into_iter()
                .chain([1, 2, 5, 10, 30, 60, 120, 300, 600, 1800, 3600].map(|s| s * nominal))
                .find(|&s| s as f64 * ppf >= MIN_TICK_SPACING)
                .unwrap_or(3600 * nominal);
            let minor = if step % 5 == 0 { step / 5 } else { step };

            let first = (self.scroll.get() / minor as f64).floor() as i64 * minor;
            let mut frame = first.max(0);
            while let Some(x) = self.frame_to_x(frame).filter(|&x| x < width) {
                let major = frame % step == 0;
                let tick = if major { 10.0 } else { 4.0 };
                snapshot.append_color(
                    &TICK,
                    &graphene::Rect::new(x, RULER_HEIGHT - tick, 1.0, tick),
                );
                if major {
                    self.draw_text(snapshot, &timecode(frame, fps), x + 3.0, 2.0);
                }
                frame += minor;
            }
        }

        fn draw_text(&self, snapshot: &gtk::Snapshot, text: &str, x: f32, y: f32) {
            let layout = self.obj().create_pango_layout(Some(text));
            snapshot.save();
            snapshot.translate(&graphene::Point::new(x, y));
            snapshot.append_layout(&layout, &TEXT);
            snapshot.restore();
        }

        /// Widget x coordinate of `frame`, or `None` if it is scrolled off to the left.
        fn frame_to_x(&self, frame: i64) -> Option<f32> {
            let x = HEADER_WIDTH as f64
                + (frame as f64 - self.scroll.get()) * self.pixels_per_frame.get();
            (x >= HEADER_WIDTH as f64).then_some(x as f32)
        }

        pub fn x_to_frame(&self, x: f64) -> i64 {
            let frame = (x - HEADER_WIDTH as f64) / self.pixels_per_frame.get() + self.scroll.get();
            frame.round().max(0.0) as i64
        }

        fn on_scroll(&self, zoom: bool, dx: f64, dy: f64) {
            if zoom {
                let ppf = (self.pixels_per_frame.get() * 1.15f64.powf(-dy)).clamp(0.02, 40.0);
                self.pixels_per_frame.set(ppf);
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
    pub fn new() -> Self {
        glib::Object::builder()
            .property("hexpand", true)
            .property("vexpand", true)
            .build()
    }

    pub fn fps(&self) -> Rational {
        self.imp().fps.get()
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

    fn scrub_to(&self, x: f64) {
        self.grab_focus();
        self.set_playhead(self.imp().x_to_frame(x));
    }
}
