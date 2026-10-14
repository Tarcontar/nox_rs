//! On-screen controls for the selected video clip's transform, over the
//! viewer (as Resolve's): its outline, drag inside to move it, a corner to
//! zoom, and the knob above it to rotate (Shift snaps to 15°). Edits are
//! undoable, one step per drag, and keyframed values are set at the
//! playhead, as in the inspector.

use gtk::{gdk, glib, graphene, gsk, prelude::*, subclass::prelude::*};
use nox_project::{Composable, EditError, ItemRef, Param, Transform};

use crate::media_pool::MediaPool;
use crate::timeline::TimelineView;

/// How far from a handle (pixels) it can be grabbed.
const GRAB: f64 = 10.0;
/// The rotation knob's distance above the picture's top edge.
const KNOB: f64 = 28.0;

/// Where things are, in widget pixels.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    item: ItemRef,
    /// The media time the transform is shown and edited at.
    time: f64,
    transform: Transform,
    corners: [(f64, f64); 4],
    center: (f64, f64),
    top: (f64, f64),
    knob: (f64, f64),
    /// Widget pixels per sequence pixel, and the sequence's size.
    scale: f64,
    sequence: (u32, u32),
}

#[derive(Debug, Clone)]
enum Drag {
    Move { start: (f64, f64) },
    Zoom { distance: f64 },
    Rotate { angle: f64 },
}

#[derive(Debug, Clone)]
struct Active {
    drag: Drag,
    origin: Geometry,
    key: String,
}

mod imp {
    use super::*;
    use std::cell::{Cell, OnceCell, RefCell};

    #[derive(Default)]
    pub struct TransformOverlay {
        pub timeline: OnceCell<TimelineView>,
        pub pool: OnceCell<MediaPool>,
        pub(super) active: RefCell<Option<Active>>,
        /// Numbers drags, so each is its own undo step.
        pub drags: Cell<u32>,
        /// Off on pages without the controls (the Color page).
        pub enabled: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TransformOverlay {
        const NAME: &'static str = "NoxTransformOverlay";
        type Type = super::TransformOverlay;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for TransformOverlay {
        fn constructed(&self) {
            self.parent_constructed();
            self.enabled.set(true);
            let obj = self.obj();
            // Handles of a picture moved off the frame stay in the viewer.
            obj.set_overflow(gtk::Overflow::Hidden);
            let drag = gtk::GestureDrag::new();
            drag.connect_drag_begin(glib::clone!(
                #[weak]
                obj,
                move |gesture, x, y| {
                    if !obj.begin(x, y) {
                        gesture.set_state(gtk::EventSequenceState::Denied);
                    }
                }
            ));
            drag.connect_drag_update(glib::clone!(
                #[weak]
                obj,
                move |gesture, dx, dy| {
                    let snap = gesture
                        .current_event_state()
                        .contains(gdk::ModifierType::SHIFT_MASK);
                    if let Some((x, y)) = gesture.start_point() {
                        obj.update(x + dx, y + dy, snap);
                    }
                }
            ));
            drag.connect_drag_end(glib::clone!(
                #[weak]
                obj,
                move |_, _, _| {
                    obj.imp().active.take();
                }
            ));
            obj.add_controller(drag);
            let motion = gtk::EventControllerMotion::new();
            motion.connect_motion(glib::clone!(
                #[weak]
                obj,
                move |_, x, y| obj.hover(x, y)
            ));
            obj.add_controller(motion);
        }
    }

    impl WidgetImpl for TransformOverlay {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(geometry) = self.obj().geometry() {
                super::draw(snapshot, &geometry);
            }
        }
    }
}

glib::wrapper! {
    pub struct TransformOverlay(ObjectSubclass<imp::TransformOverlay>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl TransformOverlay {
    /// Shows the controls for `timeline`'s selection; `pool` knows the
    /// pictures' sizes. Called once.
    pub fn set_up(&self, timeline: &TimelineView, pool: &MediaPool) {
        let imp = self.imp();
        let _ = imp.timeline.set(timeline.clone());
        let _ = imp.pool.set(pool.clone());
        let redraw = glib::clone!(
            #[weak(rename_to = overlay)]
            self,
            move || overlay.queue_draw()
        );
        let r = redraw.clone();
        timeline.connect_selection_changed(move |_| r());
        let r = redraw.clone();
        timeline.connect_playhead_changed(move |_, _| r());
        timeline.project().connect_changed(move |_| redraw());
    }

    /// Shows the controls (Edit page) or not (Color page).
    pub fn set_enabled(&self, enabled: bool) {
        self.imp().enabled.set(enabled);
        self.queue_draw();
    }

    /// The selected video clip's outline and handles, if there is one.
    fn geometry(&self) -> Option<Geometry> {
        let imp = self.imp();
        if !imp.enabled.get() {
            return None;
        }
        let (timeline, pool) = (imp.timeline.get()?, imp.pool.get()?);
        let item = timeline.selection()?;
        let time = timeline.selected_media_time()?;
        let project = timeline.project();
        let model = project.timeline();
        if model.track(item.track)?.kind != nox_project::TrackKind::Video {
            return None;
        }
        let Composable::Clip(clip) = model.item(item).ok()? else {
            return None;
        };
        let path = model.media_path(clip.media_reference()?)?;
        let video = pool.find(&path)?.info()?.video.clone()?;
        let settings = &model.settings;
        let sequence = (settings.width.max(1), settings.height.max(1));
        let placed = settings
            .scaling
            .place((video.width, video.height), sequence);
        let transform = clip.item.transform_at(time);
        // The picture shows the sequence fitted into the widget.
        let (w, h) = (f64::from(self.width()), f64::from(self.height()));
        let scale = (w / f64::from(sequence.0)).min(h / f64::from(sequence.1));
        let origin = (
            (w - f64::from(sequence.0) * scale) / 2.0,
            (h - f64::from(sequence.1) * scale) / 2.0,
        );
        let to_widget = |(x, y): (f64, f64)| (origin.0 + x * scale, origin.1 + y * scale);
        let corners = transform.corners(placed, sequence).map(to_widget);
        let center = to_widget(transform.center(placed, sequence));
        let top = midpoint(corners[0], corners[1]);
        let away = (top.0 - center.0, top.1 - center.1);
        let length = away.0.hypot(away.1).max(1e-6);
        let outside = (
            top.0 + away.0 / length * KNOB,
            top.1 + away.1 / length * KNOB,
        );
        // Inside the picture when there is no room above it.
        let fits = |(x, y): (f64, f64)| x >= 0.0 && y >= 0.0 && x <= w && y <= h;
        let knob = if fits(outside) {
            outside
        } else {
            (
                top.0 - away.0 / length * KNOB,
                top.1 - away.1 / length * KNOB,
            )
        };
        Some(Geometry {
            item,
            time,
            transform,
            corners,
            center,
            top,
            knob,
            scale,
            sequence,
        })
    }

    /// What is under `(x, y)`.
    fn hit(geometry: &Geometry, x: f64, y: f64) -> Option<Drag> {
        let near = |(px, py): (f64, f64)| (px - x).hypot(py - y) <= GRAB;
        if near(geometry.knob) {
            let angle = (y - geometry.center.1).atan2(x - geometry.center.0);
            return Some(Drag::Rotate { angle });
        }
        if geometry.corners.iter().any(|&c| near(c)) {
            let distance = (x - geometry.center.0).hypot(y - geometry.center.1);
            return Some(Drag::Zoom { distance });
        }
        inside(&geometry.corners, (x, y)).then_some(Drag::Move { start: (x, y) })
    }

    fn hover(&self, x: f64, y: f64) {
        let cursor = self
            .geometry()
            .and_then(|g| Self::hit(&g, x, y))
            .map(|drag| match drag {
                Drag::Move { .. } => "move",
                Drag::Zoom { .. } => "nwse-resize",
                Drag::Rotate { .. } => "grab",
            });
        self.set_cursor_from_name(cursor);
    }

    /// Starts a drag at `(x, y)`; false if nothing is there.
    fn begin(&self, x: f64, y: f64) -> bool {
        let Some(geometry) = self.geometry() else {
            return false;
        };
        let Some(drag) = Self::hit(&geometry, x, y) else {
            return false;
        };
        let imp = self.imp();
        imp.drags.set(imp.drags.get() + 1);
        imp.active.replace(Some(Active {
            drag,
            origin: geometry,
            key: format!("viewer-transform:{}", imp.drags.get()),
        }));
        true
    }

    fn update(&self, x: f64, y: f64, snap: bool) {
        let Some(active) = self.imp().active.borrow().clone() else {
            return;
        };
        let g = &active.origin;
        let t = g.transform;
        let changes: Vec<(Param, f64)> = match active.drag {
            Drag::Move { start } => {
                let dx = (x - start.0) / g.scale / f64::from(g.sequence.0);
                let dy = (y - start.1) / g.scale / f64::from(g.sequence.1);
                vec![(Param::X, t.x + dx), (Param::Y, t.y + dy)]
            }
            Drag::Zoom { distance } => {
                let now = (x - g.center.0).hypot(y - g.center.1);
                let zoom = (t.zoom * now / distance.max(1.0)).max(0.01);
                vec![(Param::Zoom, zoom)]
            }
            Drag::Rotate { angle } => {
                let now = (y - g.center.1).atan2(x - g.center.0);
                let mut rotation = t.rotation + (now - angle).to_degrees();
                // The same angle without jumps past ±180°.
                rotation = (rotation + 180.0).rem_euclid(360.0) - 180.0;
                if snap {
                    rotation = (rotation / 15.0).round() * 15.0;
                }
                vec![(Param::Rotation, rotation)]
            }
        };
        let label = match active.drag {
            Drag::Move { .. } => crate::i18n::gettext("Move Picture"),
            Drag::Zoom { .. } => crate::i18n::gettext("Zoom Picture"),
            Drag::Rotate { .. } => crate::i18n::gettext("Rotate Picture"),
        };
        let Some(timeline) = self.imp().timeline.get() else {
            return;
        };
        let item = g.item;
        let time = g.time;
        let _ = timeline
            .project()
            .edit_merging(&label, &active.key, |model| {
                let clip = model
                    .track_mut(item.track)
                    .ok_or(EditError::NoTrack(item.track))?
                    .children
                    .get_mut(item.index)
                    .and_then(Composable::as_clip_mut)
                    .ok_or(EditError::NoItem {
                        track: item.track,
                        index: item.index,
                    })?;
                for &(param, value) in &changes {
                    clip.item.set_value_at(param, time, value);
                }
                Ok::<_, EditError>(())
            });
    }
}

fn midpoint(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
}

/// Whether `point` is inside the convex quadrilateral `corners` (either
/// winding).
fn inside(corners: &[(f64, f64); 4], point: (f64, f64)) -> bool {
    let side = |a: (f64, f64), b: (f64, f64)| {
        (b.0 - a.0) * (point.1 - a.1) - (b.1 - a.1) * (point.0 - a.0)
    };
    let sides: Vec<f64> = (0..4)
        .map(|i| side(corners[i], corners[(i + 1) % 4]))
        .collect();
    sides.iter().all(|&s| s >= 0.0) || sides.iter().all(|&s| s <= 0.0)
}

fn draw(snapshot: &gtk::Snapshot, g: &Geometry) {
    let outline = gsk::PathBuilder::new();
    outline.move_to(g.corners[0].0 as f32, g.corners[0].1 as f32);
    for &(x, y) in &g.corners[1..] {
        outline.line_to(x as f32, y as f32);
    }
    outline.close();
    outline.move_to(g.top.0 as f32, g.top.1 as f32);
    outline.line_to(g.knob.0 as f32, g.knob.1 as f32);
    let outline = outline.to_path();
    // Dark under light, to show on any picture.
    let shadow = gdk::RGBA::new(0.0, 0.0, 0.0, 0.6);
    snapshot.append_stroke(&outline, &gsk::Stroke::new(3.0), &shadow);
    snapshot.append_stroke(&outline, &gsk::Stroke::new(1.0), &gdk::RGBA::WHITE);
    for &(x, y) in &g.corners {
        let square = graphene::Rect::new(x as f32 - 4.0, y as f32 - 4.0, 8.0, 8.0);
        snapshot.append_color(&gdk::RGBA::WHITE, &square);
        snapshot.append_border(
            &gsk::RoundedRect::from_rect(square, 0.0),
            &[1.0; 4],
            &[shadow; 4],
        );
    }
    let knob = gsk::PathBuilder::new();
    knob.add_circle(&graphene::Point::new(g.knob.0 as f32, g.knob.1 as f32), 5.0);
    let knob = knob.to_path();
    snapshot.append_fill(&knob, gsk::FillRule::Winding, &gdk::RGBA::WHITE);
    snapshot.append_stroke(&knob, &gsk::Stroke::new(1.0), &shadow);
}

#[cfg(test)]
mod tests {
    use super::inside;

    #[test]
    fn points_inside_quads() {
        let square = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert!(inside(&square, (5.0, 5.0)));
        assert!(!inside(&square, (11.0, 5.0)));
        // A diamond, wound the other way.
        let diamond = [(5.0, 0.0), (0.0, 5.0), (5.0, 10.0), (10.0, 5.0)];
        assert!(inside(&diamond, (5.0, 5.0)));
        assert!(!inside(&diamond, (1.0, 1.0)));
    }
}
