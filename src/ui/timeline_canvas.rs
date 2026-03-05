//! Custom timeline canvas widget that renders using GSK snapshots.
//!
//! This widget overrides `WidgetImpl::snapshot()` to draw the timeline ruler,
//! track lanes, clips, and playhead using GSK render nodes — no Cairo.

use std::cell::RefCell;
use std::rc::Rc;

use gdk;
use glib;
use graphene;
use gsk;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use pango;

use crate::timeline::Timeline;

// ---------------------------------------------------------------------------
// Layout constants (exported so the parent NoxTimeline can do hit-testing)
// ---------------------------------------------------------------------------

pub const TRACK_HEIGHT: f32 = 48.0;
pub const RULER_HEIGHT: f32 = 24.0;

const TRACK_COLORS: &[(f32, f32, f32)] = &[
    (0.30, 0.55, 0.88), // blue
    (0.24, 0.70, 0.44), // green
    (0.80, 0.40, 0.30), // red-orange
    (0.60, 0.40, 0.75), // purple
];

// ---------------------------------------------------------------------------
// Private implementation
// ---------------------------------------------------------------------------
mod imp {
    use super::*;

    #[derive(Default)]
    pub struct NoxTimelineCanvas {
        pub timeline_model: RefCell<Option<Rc<Timeline>>>,
        pub selected_clip: RefCell<Option<u64>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NoxTimelineCanvas {
        const NAME: &'static str = "NoxTimelineCanvas";
        type Type = super::NoxTimelineCanvas;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for NoxTimelineCanvas {}

    impl WidgetImpl for NoxTimelineCanvas {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let natural = if let Some(ref model) = *self.timeline_model.borrow() {
                    let zoom = *model.zoom.borrow() as f32;
                    let dur = model.duration_secs() as f32;
                    ((dur * zoom + 200.0).max(2000.0)) as i32
                } else {
                    2000
                };
                (200, natural, -1, -1)
            } else {
                let n_tracks = if let Some(ref model) = *self.timeline_model.borrow() {
                    model.tracks.borrow().len()
                } else {
                    2
                };
                let h = (RULER_HEIGHT + n_tracks as f32 * TRACK_HEIGHT) as i32;
                (h.max(100), h.max(100), -1, -1)
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let width = widget.width() as f32;
            let height = widget.height() as f32;

            if width <= 0.0 || height <= 0.0 {
                return;
            }

            // Background
            snapshot.append_color(
                &gdk::RGBA::new(0.12, 0.12, 0.14, 1.0),
                &graphene::Rect::new(0.0, 0.0, width, height),
            );

            let model_ref = self.timeline_model.borrow();
            let Some(model) = model_ref.as_ref() else {
                return;
            };

            let zoom = *model.zoom.borrow() as f32;
            let playhead = *model.playhead_secs.borrow() as f32;
            let sel = *self.selected_clip.borrow();

            // Ruler
            self.draw_ruler(snapshot, &widget, width, zoom);

            // Tracks & clips
            let tracks = model.tracks.borrow();
            for (ti, track) in tracks.iter().enumerate() {
                let y = RULER_HEIGHT + ti as f32 * TRACK_HEIGHT;
                let (cr, cg, cb) = TRACK_COLORS[ti % TRACK_COLORS.len()];

                // Track lane background
                snapshot.append_color(
                    &gdk::RGBA::new(cr, cg, cb, 0.08),
                    &graphene::Rect::new(0.0, y, width, TRACK_HEIGHT),
                );

                // Clips
                for clip in &track.clips {
                    let x = clip.position_secs as f32 * zoom;
                    let w = (clip.duration_secs() as f32 * zoom).max(4.0);
                    let is_sel = sel == Some(clip.id);

                    let cy = y + 2.0;
                    let ch = TRACK_HEIGHT - 4.0;

                    // Rounded clip body
                    let clip_bounds = graphene::Rect::new(x, cy, w, ch);
                    let rounded = gsk::RoundedRect::from_rect(clip_bounds, 4.0);

                    snapshot.push_rounded_clip(&rounded);
                    snapshot.append_color(
                        &gdk::RGBA::new(cr, cg, cb, if is_sel { 0.9 } else { 0.6 }),
                        &graphene::Rect::new(x, cy, w, ch),
                    );
                    snapshot.pop();

                    // Selection border
                    if is_sel {
                        let white = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
                        snapshot.append_border(&rounded, &[1.5; 4], &[white; 4]);
                    }
                }

                // Track separator line
                snapshot.append_color(
                    &gdk::RGBA::new(1.0, 1.0, 1.0, 0.06),
                    &graphene::Rect::new(0.0, y + TRACK_HEIGHT - 1.0, width, 1.0),
                );
            }

            // Playhead line
            let px = playhead * zoom;
            snapshot.append_color(
                &gdk::RGBA::new(1.0, 0.3, 0.3, 1.0),
                &graphene::Rect::new(px - 0.75, 0.0, 1.5, height),
            );

            // Playhead indicator (rectangle at top)
            snapshot.append_color(
                &gdk::RGBA::new(1.0, 0.3, 0.3, 1.0),
                &graphene::Rect::new(px - 5.0, 0.0, 10.0, 5.0),
            );
        }
    }

    impl NoxTimelineCanvas {
        fn draw_ruler(
            &self,
            snapshot: &gtk::Snapshot,
            widget: &super::NoxTimelineCanvas,
            width: f32,
            zoom: f32,
        ) {
            // Ruler background
            snapshot.append_color(
                &gdk::RGBA::new(0.20, 0.20, 0.22, 1.0),
                &graphene::Rect::new(0.0, 0.0, width, RULER_HEIGHT),
            );

            let secs_per_tick = (60.0_f32 / zoom).max(0.5).min(10.0);
            let pango_ctx = widget.pango_context();

            let mut t: f32 = 0.0;
            while t * zoom < width {
                let x = t * zoom;
                let is_major = (t / secs_per_tick).round() as i32 % 5 == 0;
                let tick_h = if is_major {
                    RULER_HEIGHT * 0.7
                } else {
                    RULER_HEIGHT * 0.35
                };

                // Tick mark
                snapshot.append_color(
                    &gdk::RGBA::new(1.0, 1.0, 1.0, 0.5),
                    &graphene::Rect::new(x, RULER_HEIGHT - tick_h, 1.0, tick_h),
                );

                // Label for major ticks
                if is_major {
                    let m = (t / 60.0) as u32;
                    let s = (t % 60.0) as u32;
                    let text = format!("{m:02}:{s:02}");

                    let layout = pango::Layout::new(&pango_ctx);
                    let mut font = pango::FontDescription::new();
                    font.set_family("Monospace");
                    font.set_size(8 * pango::SCALE);
                    layout.set_font_description(Some(&font));
                    layout.set_text(&text);

                    snapshot.save();
                    snapshot.translate(&graphene::Point::new(x + 3.0, 2.0));
                    snapshot.append_layout(&layout, &gdk::RGBA::new(1.0, 1.0, 1.0, 0.5));
                    snapshot.restore();
                }

                t += secs_per_tick;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public wrapper type
// ---------------------------------------------------------------------------
glib::wrapper! {
    pub struct NoxTimelineCanvas(ObjectSubclass<imp::NoxTimelineCanvas>)
        @extends gtk::Widget,
        @implements gtk::Buildable;
}

impl NoxTimelineCanvas {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Assign the shared timeline model and trigger a redraw.
    pub fn set_timeline_model(&self, model: Rc<Timeline>) {
        *self.imp().timeline_model.borrow_mut() = Some(model);
        self.queue_resize();
    }

    /// Set the currently selected clip and redraw.
    pub fn set_selected_clip(&self, clip_id: Option<u64>) {
        *self.imp().selected_clip.borrow_mut() = clip_id;
        self.queue_draw();
    }

    /// Return the currently selected clip ID.
    pub fn selected_clip(&self) -> Option<u64> {
        *self.imp().selected_clip.borrow()
    }

    /// Convert an x pixel coordinate to timeline seconds.
    pub fn x_to_secs(&self, x: f64) -> f64 {
        let model = self.imp().timeline_model.borrow();
        if let Some(m) = model.as_ref() {
            let zoom = *m.zoom.borrow();
            x / zoom
        } else {
            0.0
        }
    }

    /// Find which clip (if any) lies at the given pixel position.
    pub fn clip_at(&self, x: f64, y: f64) -> Option<u64> {
        let model = self.imp().timeline_model.borrow();
        let model = model.as_ref()?;
        let zoom = *model.zoom.borrow();
        let tracks = model.tracks.borrow();

        for (ti, track) in tracks.iter().enumerate() {
            let ty = RULER_HEIGHT as f64 + ti as f64 * TRACK_HEIGHT as f64;
            if y >= ty && y < ty + TRACK_HEIGHT as f64 {
                for clip in &track.clips {
                    let cx = clip.position_secs * zoom;
                    let cw = clip.duration_secs() * zoom;
                    if x >= cx && x < cx + cw {
                        return Some(clip.id);
                    }
                }
            }
        }
        None
    }
}
