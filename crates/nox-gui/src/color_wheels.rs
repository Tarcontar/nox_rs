//! The Color page's primary wheels (`resources/ui/color-wheels.ui`): lift,
//! gamma, gain and offset of the selected clip (`nox_color::grade`), each
//! a color balance wheel plus a master slider, as in DaVinci Resolve.
//!
//! A wheel's puck pushes the picture towards the hue under it: its position
//! is a Cb/Cr direction, turned into R, G, B changes that leave luma alone.
//! Lift and offset add to ACEScct values; gamma and gain multiply.

use gtk::{CompositeTemplate, gdk, glib, graphene, gsk, prelude::*, subclass::prelude::*};
use nox_project::{Grade, Wheel};
use std::cell::{Cell, OnceCell};

use crate::i18n::{fill, gettext};
use crate::timeline::TimelineView;

/// R, G, B change (luma-neutral, BT.709) of a puck at `(x, y)` in the unit
/// disc: `x` towards +Cb (blue), `y` towards +Cr (red).
fn rgb_delta(x: f32, y: f32) -> [f32; 3] {
    let (cb, cr) = (x * 0.5, y * 0.5);
    [1.5748 * cr, -0.1873 * cb - 0.4681 * cr, 1.8556 * cb]
}

/// The puck position whose `rgb_delta` is closest to `delta`.
fn puck_of(delta: [f32; 3]) -> (f32, f32) {
    let (x, y) = (delta[2] / 1.8556 / 0.5, delta[0] / 1.5748 / 0.5);
    let length = (x * x + y * y).sqrt();
    if length > 1.0 {
        (x / length, y / length)
    } else {
        (x, y)
    }
}

/// Which wheel, and how its controls map to grade values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[enum_type(name = "NoxWheelKind")]
pub enum WheelKind {
    #[default]
    Lift,
    Gamma,
    Gain,
    Offset,
}

impl WheelKind {
    const ALL: [Self; 4] = [Self::Lift, Self::Gamma, Self::Gain, Self::Offset];

    fn name(self) -> String {
        match self {
            Self::Lift => gettext("Lift"),
            Self::Gamma => gettext("Gamma"),
            Self::Gain => gettext("Gain"),
            Self::Offset => gettext("Offset"),
        }
    }

    fn additive(self) -> bool {
        matches!(self, Self::Lift | Self::Offset)
    }

    /// The wheel's values from the puck and the master slider (−1…1).
    fn wheel(self, puck: (f32, f32), master: f32) -> Wheel {
        let delta = rgb_delta(puck.0, puck.1);
        match self {
            Self::Lift => Wheel {
                rgb: delta.map(|d| d * 0.1),
                master: master * 0.1,
            },
            Self::Offset => Wheel {
                rgb: delta.map(|d| d * 0.1),
                master: master * 0.2,
            },
            Self::Gamma => Wheel {
                rgb: delta.map(|d| (d * 0.5).exp2()),
                master: (master * 0.6).exp2(),
            },
            Self::Gain => Wheel {
                rgb: delta.map(|d| (d * 0.5).exp2()),
                master: master.exp2(),
            },
        }
    }

    /// Puck and master slider showing `wheel` (the inverse of `wheel`).
    fn controls(self, wheel: Wheel) -> ((f32, f32), f32) {
        if self.additive() {
            let scale = if self == Self::Lift { 0.1 } else { 0.2 };
            (puck_of(wheel.rgb.map(|v| v / 0.1)), wheel.master / scale)
        } else {
            let scale = if self == Self::Gamma { 0.6 } else { 1.0 };
            (
                puck_of(wheel.rgb.map(|v| v.max(1e-6).log2() / 0.5)),
                wheel.master.max(1e-6).log2() / scale,
            )
        }
    }

    fn get(self, grade: &Grade) -> Wheel {
        match self {
            Self::Lift => grade.lift,
            Self::Gamma => grade.gamma,
            Self::Gain => grade.gain,
            Self::Offset => grade.offset,
        }
    }

    fn set(self, grade: &mut Grade, wheel: Wheel) {
        match self {
            Self::Lift => grade.lift = wheel,
            Self::Gamma => grade.gamma = wheel,
            Self::Gain => grade.gain = wheel,
            Self::Offset => grade.offset = wheel,
        }
    }
}

mod wheel {
    use super::*;
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct ColorWheel {
        /// Puck position in the unit disc (`x` right, `y` up).
        pub puck: Cell<(f32, f32)>,
        /// Puck when a drag started, in pixels from the centre.
        pub drag_start: Cell<(f64, f64)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ColorWheel {
        const NAME: &'static str = "NoxColorWheel";
        type Type = super::ColorWheel;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("color-wheel");
            klass.set_accessible_role(gtk::AccessibleRole::Slider);
        }
    }

    impl ObjectImpl for ColorWheel {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                // The user moved the puck.
                vec![glib::subclass::Signal::builder("moved").build()]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_focusable(true);
            obj.set_cursor_from_name(Some("crosshair"));

            let drag = gtk::GestureDrag::new();
            drag.connect_drag_begin(glib::clone!(
                #[weak]
                obj,
                move |_, x, y| {
                    obj.grab_focus();
                    let (cx, cy, _) = obj.imp().geometry();
                    obj.imp().drag_start.set((x - cx, y - cy));
                    obj.imp().move_to_pixels(x - cx, y - cy);
                }
            ));
            drag.connect_drag_update(glib::clone!(
                #[weak]
                obj,
                move |_, dx, dy| {
                    let (sx, sy) = obj.imp().drag_start.get();
                    obj.imp().move_to_pixels(sx + dx, sy + dy);
                }
            ));
            obj.add_controller(drag);

            let reset = gtk::GestureClick::new();
            reset.connect_pressed(glib::clone!(
                #[weak]
                obj,
                move |_, presses, _, _| {
                    if presses == 2 {
                        obj.imp().user_move((0.0, 0.0));
                    }
                }
            ));
            obj.add_controller(reset);

            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_, key, _, _| {
                    const STEP: f32 = 0.02;
                    let (x, y) = obj.imp().puck.get();
                    let puck = match key {
                        gdk::Key::Left => (x - STEP, y),
                        gdk::Key::Right => (x + STEP, y),
                        gdk::Key::Up => (x, y + STEP),
                        gdk::Key::Down => (x, y - STEP),
                        gdk::Key::Home | gdk::Key::Delete | gdk::Key::BackSpace => (0.0, 0.0),
                        _ => return glib::Propagation::Proceed,
                    };
                    obj.imp().user_move(puck);
                    glib::Propagation::Stop
                }
            ));
            obj.add_controller(keys);
        }
    }

    impl ColorWheel {
        /// Centre and radius of the disc, in widget pixels.
        pub fn geometry(&self) -> (f64, f64, f64) {
            let obj = self.obj();
            let (w, h) = (f64::from(obj.width()), f64::from(obj.height()));
            (w / 2.0, h / 2.0, (w.min(h) / 2.0 - 6.0).max(1.0))
        }

        fn move_to_pixels(&self, dx: f64, dy: f64) {
            let (_, _, radius) = self.geometry();
            self.user_move(((dx / radius) as f32, (-dy / radius) as f32));
        }

        fn user_move(&self, (x, y): (f32, f32)) {
            let length = (x * x + y * y).sqrt();
            let puck = if length > 1.0 {
                (x / length, y / length)
            } else {
                (x, y)
            };
            if puck != self.puck.get() {
                self.puck.set(puck);
                self.obj().queue_draw();
                self.obj().emit_by_name::<()>("moved", &[]);
            }
        }
    }

    impl WidgetImpl for ColorWheel {
        fn measure(&self, _: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            (110, 150, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let (cx, cy, radius) = self.geometry();
            let (cx, cy, radius) = (cx as f32, cy as f32, radius as f32);
            let bounds = graphene::Rect::new(cx - radius, cy - radius, 2.0 * radius, 2.0 * radius);
            let disc = gsk::RoundedRect::from_rect(bounds, radius);
            snapshot.push_rounded_clip(&disc);
            // Hues where the puck pushes towards them; 0° is up, clockwise.
            let stops: Vec<gsk::ColorStop> = (0..=24)
                .map(|i| {
                    let t = i as f32 / 24.0;
                    let angle = t * std::f32::consts::TAU;
                    let delta = rgb_delta(angle.sin(), angle.cos());
                    let rgb = delta.map(|d| (0.45 + d * 0.9).clamp(0.0, 1.0));
                    gsk::ColorStop::new(t, gdk::RGBA::new(rgb[0], rgb[1], rgb[2], 1.0))
                })
                .collect();
            snapshot.append_conic_gradient(&bounds, &graphene::Point::new(cx, cy), 0.0, &stops);
            // Desaturated towards the centre.
            snapshot.append_radial_gradient(
                &bounds,
                &graphene::Point::new(cx, cy),
                radius,
                radius,
                0.0,
                1.0,
                &[
                    gsk::ColorStop::new(0.0, gdk::RGBA::new(0.3, 0.3, 0.32, 1.0)),
                    gsk::ColorStop::new(1.0, gdk::RGBA::new(0.3, 0.3, 0.32, 0.0)),
                ],
            );
            snapshot.pop();
            let line = gdk::RGBA::new(1.0, 1.0, 1.0, 0.25);
            let ring = gsk::PathBuilder::new();
            ring.add_circle(&graphene::Point::new(cx, cy), radius);
            snapshot.append_stroke(&ring.to_path(), &gsk::Stroke::new(1.0), &line);
            let cross = gsk::PathBuilder::new();
            cross.move_to(cx - 5.0, cy);
            cross.line_to(cx + 5.0, cy);
            cross.move_to(cx, cy - 5.0);
            cross.line_to(cx, cy + 5.0);
            snapshot.append_stroke(&cross.to_path(), &gsk::Stroke::new(1.0), &line);

            let (x, y) = self.puck.get();
            let (px, py) = (cx + x * radius, cy - y * radius);
            let puck = gsk::PathBuilder::new();
            puck.add_circle(&graphene::Point::new(px, py), 5.0);
            let path = puck.to_path();
            snapshot.append_fill(
                &path,
                gsk::FillRule::Winding,
                &gdk::RGBA::new(0.1, 0.1, 0.1, 0.6),
            );
            snapshot.append_stroke(&path, &gsk::Stroke::new(2.0), &gdk::RGBA::WHITE);
            if self.obj().has_visible_focus() {
                let focus = gsk::PathBuilder::new();
                focus.add_circle(&graphene::Point::new(cx, cy), radius + 3.0);
                let accent = gdk::RGBA::new(0.21, 0.52, 0.89, 1.0);
                snapshot.append_stroke(&focus.to_path(), &gsk::Stroke::new(2.0), &accent);
            }
        }
    }
}

glib::wrapper! {
    /// A color balance wheel: drag (or use the arrow keys) to move the
    /// puck; double-click (or Home) resets it.
    pub struct ColorWheel(ObjectSubclass<wheel::ColorWheel>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ColorWheel {
    fn puck(&self) -> (f32, f32) {
        self.imp().puck.get()
    }

    /// Shows a position without emitting `moved`.
    fn set_puck(&self, puck: (f32, f32)) {
        self.imp().puck.set(puck);
        self.queue_draw();
    }

    fn connect_moved<F: Fn(&Self) + 'static>(&self, f: F) {
        self.connect_local("moved", false, move |values| {
            f(&values[0].get::<Self>().expect("ColorWheel"));
            None
        });
    }
}

mod panel {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/nox/Nox/ui/color-wheels.ui")]
    pub struct ColorWheels {
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub grade_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub lift: TemplateChild<ColorWheel>,
        #[template_child]
        pub gamma: TemplateChild<ColorWheel>,
        #[template_child]
        pub gain: TemplateChild<ColorWheel>,
        #[template_child]
        pub offset: TemplateChild<ColorWheel>,
        #[template_child]
        pub lift_master: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub gamma_master: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub gain_master: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub offset_master: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub lift_values: TemplateChild<gtk::Label>,
        #[template_child]
        pub gamma_values: TemplateChild<gtk::Label>,
        #[template_child]
        pub gain_values: TemplateChild<gtk::Label>,
        #[template_child]
        pub offset_values: TemplateChild<gtk::Label>,
        pub timeline: OnceCell<TimelineView>,
        /// Set while the controls follow the model.
        pub updating: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ColorWheels {
        const NAME: &'static str = "NoxColorWheels";
        type Type = super::ColorWheels;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            ColorWheel::ensure_type();
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ColorWheels {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            for kind in WheelKind::ALL {
                let (wheel, _, _) = obj.controls(kind);
                wheel.update_property(&[gtk::accessible::Property::Label(&kind.name())]);
                wheel.connect_moved(glib::clone!(
                    #[weak]
                    obj,
                    move |_| obj.write(kind)
                ));
            }
        }
    }

    impl WidgetImpl for ColorWheels {}
    impl BoxImpl for ColorWheels {}
}

glib::wrapper! {
    /// Lift, gamma, gain and offset of the selected clip.
    pub struct ColorWheels(ObjectSubclass<panel::ColorWheels>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

#[gtk::template_callbacks]
impl ColorWheels {
    /// Grades the selection of `timeline`. Called once.
    pub fn set_timeline(&self, timeline: &TimelineView) {
        let _ = self.imp().timeline.set(timeline.clone());
        timeline.connect_selection_changed(glib::clone!(
            #[weak(rename_to = wheels)]
            self,
            move |_| wheels.refresh()
        ));
        timeline.project().connect_changed(glib::clone!(
            #[weak(rename_to = wheels)]
            self,
            move |_| wheels.refresh()
        ));
        // A keyframed grade changes along the clip.
        timeline.connect_playhead_changed(glib::clone!(
            #[weak(rename_to = wheels)]
            self,
            move |_, _| wheels.refresh()
        ));
        self.refresh();
    }

    fn controls(&self, kind: WheelKind) -> (ColorWheel, gtk::Adjustment, gtk::Label) {
        let imp = self.imp();
        match kind {
            WheelKind::Lift => (imp.lift.get(), imp.lift_master.get(), imp.lift_values.get()),
            WheelKind::Gamma => (
                imp.gamma.get(),
                imp.gamma_master.get(),
                imp.gamma_values.get(),
            ),
            WheelKind::Gain => (imp.gain.get(), imp.gain_master.get(), imp.gain_values.get()),
            WheelKind::Offset => (
                imp.offset.get(),
                imp.offset_master.get(),
                imp.offset_values.get(),
            ),
        }
    }

    #[template_callback]
    fn on_lift_master(&self) {
        self.write(WheelKind::Lift);
    }

    #[template_callback]
    fn on_gamma_master(&self) {
        self.write(WheelKind::Gamma);
    }

    #[template_callback]
    fn on_gain_master(&self) {
        self.write(WheelKind::Gain);
    }

    #[template_callback]
    fn on_offset_master(&self) {
        self.write(WheelKind::Offset);
    }

    #[template_callback]
    fn on_reset_lift(&self) {
        self.reset(WheelKind::Lift);
    }

    #[template_callback]
    fn on_reset_gamma(&self) {
        self.reset(WheelKind::Gamma);
    }

    #[template_callback]
    fn on_reset_gain(&self) {
        self.reset(WheelKind::Gain);
    }

    #[template_callback]
    fn on_reset_offset(&self) {
        self.reset(WheelKind::Offset);
    }

    /// Adds or removes a grade keyframe at the playhead.
    #[template_callback]
    fn on_grade_key_toggled(&self, button: &gtk::ToggleButton) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let Some(timeline) = imp.timeline.get() else {
            return;
        };
        let Some(time) = timeline.selected_media_time() else {
            return;
        };
        let label = if button.is_active() {
            gettext("Add Keyframe")
        } else {
            gettext("Remove Keyframe")
        };
        timeline.edit_selected_clip(&label, None, |item| item.toggle_grade_keyframe(time));
    }

    #[template_callback]
    fn on_previous_keyframe(&self) {
        if let Some(timeline) = self.imp().timeline.get() {
            timeline.jump_to_keyframe(false);
        }
    }

    #[template_callback]
    fn on_next_keyframe(&self) {
        if let Some(timeline) = self.imp().timeline.get() {
            timeline.jump_to_keyframe(true);
        }
    }

    fn reset(&self, kind: WheelKind) {
        let (wheel, master, _) = self.controls(kind);
        self.imp().updating.set(true);
        wheel.set_puck((0.0, 0.0));
        master.set_value(0.0);
        self.imp().updating.set(false);
        self.write(kind);
    }

    /// Writes one wheel's controls into the selected clip's grade; a drag
    /// makes one undo step.
    fn write(&self, kind: WheelKind) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let Some(timeline) = imp.timeline.get() else {
            return;
        };
        let (wheel, master, _) = self.controls(kind);
        let values = kind.wheel(wheel.puck(), master.value() as f32);
        let label = fill(&gettext("Change {wheel}"), &[("wheel", &kind.name())]);
        let key = format!("grade-{kind:?}");
        let time = timeline.selected_media_time().unwrap_or_default();
        timeline.edit_selected_clip(&label, Some(&key), |item| {
            let mut grade = item.grade_at(time);
            kind.set(&mut grade, values);
            item.set_grade_at(time, grade);
        });
    }

    /// Shows the selected clip's grade.
    fn refresh(&self) {
        let imp = self.imp();
        let Some(timeline) = imp.timeline.get() else {
            return;
        };
        let project = timeline.project();
        let model = project.timeline();
        let time = timeline.selected_media_time().unwrap_or_default();
        let grade = timeline.selection().and_then(|item| {
            let video = model
                .track(item.track)
                .is_some_and(|t| t.kind == nox_project::TrackKind::Video);
            let clip = model.item(item).ok()?.as_clip()?;
            let item = &clip.item;
            video.then(|| {
                let keyed = !item.grade_keyframes().is_empty();
                (item.grade_at(time), item.has_grade_keyframe(time), keyed)
            })
        });
        drop(model);
        let Some((grade, at_keyframe, keyed)) = grade else {
            imp.stack.set_visible_child_name("empty");
            return;
        };
        imp.updating.set(true);
        imp.grade_key.set_active(at_keyframe);
        if keyed {
            imp.grade_key.add_css_class("animated");
        } else {
            imp.grade_key.remove_css_class("animated");
        }
        for kind in WheelKind::ALL {
            let (wheel, master, values) = self.controls(kind);
            let current = kind.get(&grade);
            let (puck, level) = kind.controls(current);
            wheel.set_puck(puck);
            master.set_value(f64::from(level));
            let [r, g, b] = current.rgb;
            let text = format!("{r:.2}  {g:.2}  {b:.2}  ·  {:.2}", current.master); // no-i18n
            values.set_text(&text);
            wheel.update_property(&[gtk::accessible::Property::ValueText(&fill(
                &gettext("Red {r}, green {g}, blue {b}, master {master}"),
                &[
                    ("r", &format!("{r:.2}")),
                    ("g", &format!("{g:.2}")),
                    ("b", &format!("{b:.2}")),
                    ("master", &format!("{:.2}", current.master)),
                ],
            ))]);
        }
        imp.updating.set(false);
        imp.stack.set_visible_child_name("wheels");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_round_trip() {
        for kind in WheelKind::ALL {
            for (puck, master) in [((0.0, 0.0), 0.0), ((0.3, -0.4), 0.5), ((-0.6, 0.2), -0.8)] {
                let wheel = kind.wheel(puck, master);
                let (back, level) = kind.controls(wheel);
                assert!(
                    (back.0 - puck.0).abs() < 1e-4 && (back.1 - puck.1).abs() < 1e-4,
                    "{kind:?} {puck:?} → {back:?}"
                );
                assert!((level - master).abs() < 1e-4, "{kind:?} {master} → {level}");
            }
            assert_eq!(kind.wheel((0.0, 0.0), 0.0), kind.get(&Grade::NEUTRAL));
        }
    }

    #[test]
    fn pucks_leave_luma_alone() {
        for (x, y) in [(1.0, 0.0), (0.0, 1.0), (-0.7, 0.7)] {
            let d = rgb_delta(x, y);
            let luma = 0.2126 * d[0] + 0.7152 * d[1] + 0.0722 * d[2];
            assert!(luma.abs() < 1e-4, "({x}, {y}): {luma}");
        }
        // Up is red, right is blue.
        assert!(rgb_delta(0.0, 1.0)[0] > 0.5);
        assert!(rgb_delta(1.0, 0.0)[2] > 0.5);
    }
}
