//! The inspector (`resources/ui/inspector.ui`): properties of the item
//! selected on the timeline, editable in place. Changes are undoable;
//! dragging a slider makes one undo step.

use gtk::{CompositeTemplate, glib, prelude::*, subclass::prelude::*};
use nox_project::keyframes::SAME_TIME;
use nox_project::{BlendMode, Composable, Ease, Param, Time, TrackKind, Transform, TransitionKind};
use std::rc::Rc;

use crate::i18n::{blend_mode_name, fill, gettext};
use crate::playback::Playback;
use crate::project::Project;
use crate::timeline::TimelineView;
use crate::ui_util::string_list;

mod effects;

mod imp {
    use super::*;
    use std::cell::{Cell, OnceCell};

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/feliwir/Nox/ui/inspector.ui")]
    pub struct Inspector {
        #[template_child]
        pub pages: TemplateChild<gtk::Stack>,
        #[template_child]
        pub name_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub details_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub decoder_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub composite_section: TemplateChild<gtk::Box>,
        #[template_child]
        pub effects_section: TemplateChild<gtk::Box>,
        #[template_child]
        pub effects_list: TemplateChild<gtk::Box>,
        #[template_child]
        pub add_effect: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub background_status: TemplateChild<gtk::Label>,
        #[template_child]
        pub audio_section: TemplateChild<gtk::Box>,
        #[template_child]
        pub opacity_scale: TemplateChild<gtk::Scale>,
        #[template_child]
        pub blend_dropdown: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub marker_title: TemplateChild<gtk::Label>,
        #[template_child]
        pub marker_name: TemplateChild<gtk::Entry>,
        #[template_child]
        pub marker_color: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub marker_note: TemplateChild<gtk::Entry>,
        #[template_child]
        pub volume_scale: TemplateChild<gtk::Scale>,
        #[template_child]
        pub fade_section: TemplateChild<gtk::Box>,
        #[template_child]
        pub transform_section: TemplateChild<gtk::Box>,
        #[template_child]
        pub position_x: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub position_y: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub zoom: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub rotation: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub crop_left: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub crop_right: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub crop_top: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub crop_bottom: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub fade_in: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub opacity_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub volume_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub position_x_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub position_y_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub zoom_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub rotation_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub crop_left_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub crop_right_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub crop_top_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub crop_bottom_key: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub keyframe_section: TemplateChild<gtk::Box>,
        #[template_child]
        pub transition_section: TemplateChild<gtk::Grid>,
        #[template_child]
        pub item_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub speed_section: TemplateChild<gtk::Box>,
        #[template_child]
        pub speed: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub speed_ripple: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub freeze: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub transition_type: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub transition_length: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub ease_dropdown: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub fade_out: TemplateChild<gtk::SpinButton>,
        pub timeline: OnceCell<TimelineView>,
        /// Tells how files decode (the playback engine's report).
        pub playback: OnceCell<std::rc::Weak<Playback>>,
        /// Set while the widgets are updated from the model, so their
        /// change handlers don't write the values back as edits.
        pub updating: Cell<bool>,
        /// The selected clip's effect rows (`effects.rs`).
        pub effect_rows: std::cell::RefCell<Vec<super::effects::EffectRow>>,
        /// Effects whose rows are open, by key.
        pub expanded_effects: std::cell::RefCell<std::collections::HashSet<String>>,
        /// Opens the next effect added.
        pub expand_added_effect: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Inspector {
        const NAME: &'static str = "NoxInspector";
        type Type = super::Inspector;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Inspector {
        fn constructed(&self) {
            self.parent_constructed();
            self.blend_dropdown.set_model(Some(&string_list(
                BlendMode::ALL.into_iter().map(blend_mode_name),
            )));
            self.updating.set(true);
            self.marker_color.set_model(Some(&string_list(
                nox_project::MARKER_COLORS
                    .iter()
                    .map(|c| marker_color_name(c)),
            )));
            self.updating.set(false);

            self.updating.set(true);
            self.transition_type.set_model(Some(&string_list(
                TransitionKind::ALL
                    .into_iter()
                    .map(crate::i18n::transition_name),
            )));
            self.updating.set(false);

            let obj = self.obj();
            // Spin buttons and sliders change their value on the wheel, so
            // scrolling down the inspector would edit whatever passes under
            // the pointer. Scroll the page instead, unless a field has focus.
            let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
            wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
            let scroller = self.item_scroller.get();
            wheel.connect_scroll(move |controller, _, dy| {
                let editing = scroller
                    .root()
                    .and_then(|root| root.focus())
                    .is_some_and(|focus| {
                        focus.is_ancestor(&scroller)
                            && (focus.is::<gtk::Text>() || focus.is::<gtk::Scale>())
                    });
                if editing {
                    return glib::Propagation::Proceed;
                }
                let adjustment = scroller.vadjustment();
                // Wheel clicks scroll as GTK does (page^(2/3)); touchpads
                // report pixels.
                let step = match controller.unit() {
                    gtk::gdk::ScrollUnit::Wheel => adjustment.page_size().powf(2.0 / 3.0),
                    _ => 1.0,
                };
                adjustment.set_value(adjustment.value() + dy * step);
                glib::Propagation::Stop
            });
            self.item_scroller.add_controller(wheel);
            obj.setup_effects();
            self.speed.adjustment().connect_value_changed(glib::clone!(
                #[weak]
                obj,
                move |adjustment| obj.set_speed(adjustment.value() / 100.0)
            ));
            self.transition_length
                .adjustment()
                .connect_value_changed(glib::clone!(
                    #[weak]
                    obj,
                    move |adjustment| obj.set_transition_length(adjustment.value())
                ));
            self.opacity_scale
                .adjustment()
                .connect_value_changed(glib::clone!(
                    #[weak]
                    obj,
                    move |adjustment| {
                        let opacity = adjustment.value() / 100.0;
                        let time = obj.media_time().unwrap_or_default();
                        obj.edit(&gettext("Change Opacity"), Some("opacity"), |item| {
                            item.set_value_at(Param::Opacity, time, opacity)
                        });
                    }
                ));
            self.blend_dropdown.connect_selected_notify(glib::clone!(
                #[weak]
                obj,
                move |dropdown| {
                    let Some(&mode) = BlendMode::ALL.get(dropdown.selected() as usize) else {
                        return;
                    };
                    obj.edit(&gettext("Change Blend Mode"), None, |item| {
                        item.set_blend_mode(mode)
                    });
                }
            ));
            for (field, spin) in self.transform_fields().into_iter().enumerate() {
                spin.adjustment().connect_value_changed(glib::clone!(
                    #[weak]
                    obj,
                    move |adjustment| {
                        let size = obj.sequence_size();
                        let mut transform = Transform::IDENTITY;
                        set_transform_field(&mut transform, field, adjustment.value(), size);
                        let param = FIELD_PARAMS[field];
                        let value = param.of(&transform);
                        let time = obj.media_time().unwrap_or_default();
                        obj.edit(
                            &gettext("Change Transform"),
                            Some(&format!("transform-{field}")),
                            |item| item.set_value_at(param, time, value),
                        );
                    }
                ));
            }
            for (spin, out) in [(&*self.fade_in, false), (&*self.fade_out, true)] {
                spin.adjustment().connect_value_changed(glib::clone!(
                    #[weak]
                    obj,
                    move |adjustment| {
                        let seconds = adjustment.value();
                        let (label, key) = if out {
                            (gettext("Fade Out"), "fade-out")
                        } else {
                            (gettext("Fade In"), "fade-in")
                        };
                        obj.edit(&label, Some(key), |item| {
                            if out {
                                item.set_fade_out(seconds)
                            } else {
                                item.set_fade_in(seconds)
                            }
                        });
                    }
                ));
            }
            self.volume_scale
                .adjustment()
                .connect_value_changed(glib::clone!(
                    #[weak]
                    obj,
                    move |adjustment| {
                        let volume = 10f64.powf(adjustment.value() / 20.0);
                        let time = obj.media_time().unwrap_or_default();
                        obj.edit(&gettext("Change Volume"), Some("volume"), |item| {
                            item.set_value_at(Param::Volume, time, volume)
                        });
                    }
                ));
        }
    }

    impl Inspector {
        /// The keyframe toggles and their properties.
        pub fn keyframe_toggles(&self) -> [(Param, &gtk::ToggleButton); 10] {
            [
                (Param::Opacity, &self.opacity_key),
                (Param::Volume, &self.volume_key),
                (FIELD_PARAMS[0], &self.position_x_key),
                (FIELD_PARAMS[1], &self.position_y_key),
                (FIELD_PARAMS[2], &self.zoom_key),
                (FIELD_PARAMS[3], &self.rotation_key),
                (FIELD_PARAMS[4], &self.crop_left_key),
                (FIELD_PARAMS[5], &self.crop_right_key),
                (FIELD_PARAMS[6], &self.crop_top_key),
                (FIELD_PARAMS[7], &self.crop_bottom_key),
            ]
        }

        /// The transform's spin buttons, in `set_transform_field` order.
        pub fn transform_fields(&self) -> [&gtk::SpinButton; 8] {
            [
                &self.position_x,
                &self.position_y,
                &self.zoom,
                &self.rotation,
                &self.crop_left,
                &self.crop_right,
                &self.crop_top,
                &self.crop_bottom,
            ]
        }
    }

    impl WidgetImpl for Inspector {}
    impl BoxImpl for Inspector {}
}

glib::wrapper! {
    pub struct Inspector(ObjectSubclass<imp::Inspector>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

#[gtk::template_callbacks]
impl Inspector {
    /// The marker's name, color or note was edited; typing makes one undo
    /// step per marker.
    #[template_callback]
    fn on_marker_changed(&self) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let Some(timeline) = imp.timeline.get() else {
            return;
        };
        let Some(index) = timeline.selected_marker() else {
            return;
        };
        let name = imp.marker_name.text();
        let note = imp.marker_note.text();
        let color = nox_project::MARKER_COLORS
            .get(imp.marker_color.selected() as usize)
            .copied()
            .unwrap_or(nox_project::MARKER_COLORS[0]);
        let _ = timeline.project().edit_merging(
            &gettext("Change Marker"),
            &format!("marker:{index}"),
            |t| t.set_marker(index, &name, color, &note),
        );
    }

    /// A keyframe toggle was clicked: adds or removes the keyframe of its
    /// property at the playhead.
    #[template_callback]
    fn on_keyframe_toggled(&self, button: &gtk::ToggleButton) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let Some((param, _)) = imp
            .keyframe_toggles()
            .into_iter()
            .find(|(_, b)| *b == button)
        else {
            return;
        };
        let Some(time) = self.media_time() else {
            return;
        };
        let label = if button.is_active() {
            gettext("Add Keyframe")
        } else {
            gettext("Remove Keyframe")
        };
        self.edit(&label, None, |item| item.toggle_keyframe(param, time));
    }

    /// Changes the selected clip's speed (one undo step while dragging).
    fn set_speed(&self, speed: f64) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let (Some(timeline), ripple) = (imp.timeline.get(), imp.speed_ripple.is_active()) else {
            return;
        };
        let Some(item) = timeline.selection() else {
            return;
        };
        let key = format!("speed:{}:{}", item.track, item.index);
        let changed = timeline
            .project()
            .edit_merging(&gettext("Change Speed"), &key, |t| {
                t.set_speed(item, speed, ripple)
            });
        if let Ok(item) = changed {
            timeline.set_selection(Some(item));
        }
    }

    #[template_callback]
    fn on_freeze_toggled(&self) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let Some(timeline) = imp.timeline.get() else {
            return;
        };
        let Some(item) = timeline.selection() else {
            return;
        };
        let fps = timeline.project().frame_rate();
        let frame = imp
            .freeze
            .is_active()
            .then(|| self.media_time())
            .flatten()
            .map(|seconds| Time::from_seconds(seconds, fps.inverse()));
        let label = if frame.is_some() {
            gettext("Freeze Frame")
        } else {
            gettext("Remove Freeze Frame")
        };
        let _ = timeline
            .project()
            .edit(&label, |t| t.set_freeze_frame(item, frame));
    }

    #[template_callback]
    fn on_transition_type_changed(&self) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let (Some(timeline), Some(&kind)) = (
            imp.timeline.get(),
            TransitionKind::ALL.get(imp.transition_type.selected() as usize),
        ) else {
            return;
        };
        let Some(item) = timeline.selection() else {
            return;
        };
        let _ = timeline.project().edit(&gettext("Change Transition"), |t| {
            let transition = t
                .track_mut(item.track)
                .and_then(|track| track.children.get_mut(item.index))
                .and_then(|child| match child {
                    Composable::Transition(transition) => Some(transition),
                    _ => None,
                })
                .ok_or("not a transition")?;
            transition.set_kind(kind);
            Ok::<_, &str>(())
        });
    }

    /// The selected transition's length changed: as long as the clips'
    /// media allows. Dragging makes one undo step.
    fn set_transition_length(&self, seconds: f64) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let Some(timeline) = imp.timeline.get() else {
            return;
        };
        let Some(item) = timeline.selection() else {
            return;
        };
        let project = timeline.project();
        let fps = project.frame_rate();
        let length = Time::from_frames((seconds * fps.as_f64()).round().max(1.0) as i64, fps);
        let key = format!("transition-length:{}:{}", item.track, item.index);
        let _ = project.edit_merging(&gettext("Change Transition Length"), &key, |t| {
            t.set_transition_length(item, length)
        });
    }

    #[template_callback]
    fn on_previous_keyframe(&self) {
        self.jump_to_keyframe(false);
    }

    #[template_callback]
    fn on_next_keyframe(&self) {
        self.jump_to_keyframe(true);
    }

    #[template_callback]
    fn on_ease_changed(&self) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let Some(time) = self.media_time() else {
            return;
        };
        let ease = if imp.ease_dropdown.selected() == 1 {
            Ease::Smooth
        } else {
            Ease::Linear
        };
        self.edit(&gettext("Change Easing"), None, |item| {
            item.set_ease_at(time, ease)
        });
    }

    /// The selected clip's media time (seconds) at the playhead.
    fn media_time(&self) -> Option<f64> {
        self.imp().timeline.get()?.selected_media_time()
    }

    /// Moves the playhead to the selected clip's next (or previous)
    /// keyframe of any property.
    fn jump_to_keyframe(&self, forward: bool) {
        if let Some(timeline) = self.imp().timeline.get() {
            timeline.jump_to_keyframe(forward);
        }
    }

    #[template_callback]
    fn on_reset_transform(&self) {
        self.edit(&gettext("Reset Transform"), None, |item| {
            item.set_transform(Transform::IDENTITY)
        });
    }

    /// The timeline's resolution, which positions are shown in.
    fn sequence_size(&self) -> (f64, f64) {
        self.imp()
            .timeline
            .get()
            .map_or((1920.0, 1080.0), |timeline| {
                let settings = timeline.project().timeline().settings.clone();
                (
                    f64::from(settings.width.max(1)),
                    f64::from(settings.height.max(1)),
                )
            })
    }

    /// Shows the selection of `timeline`. Called once.
    pub fn set_timeline(&self, timeline: &TimelineView) {
        let _ = self.imp().timeline.set(timeline.clone());
        timeline.connect_selection_changed(glib::clone!(
            #[weak(rename_to = inspector)]
            self,
            move |_| inspector.refresh()
        ));
        timeline.project().connect_changed(glib::clone!(
            #[weak(rename_to = inspector)]
            self,
            move |_| inspector.refresh()
        ));
        // Keyframed values change along the clip.
        timeline.connect_playhead_changed(glib::clone!(
            #[weak(rename_to = inspector)]
            self,
            move |_, _| inspector.refresh()
        ));
        self.refresh();
    }

    /// Shows how the selected clip's file decodes, as `playback` reports
    /// it. Called once.
    pub fn set_playback(&self, playback: &Rc<Playback>) {
        let _ = self.imp().playback.set(Rc::downgrade(playback));
        playback.connect_decoding_changed(glib::clone!(
            #[weak(rename_to = inspector)]
            self,
            move |_| inspector.show_decoder()
        ));
        self.show_decoder();
    }

    /// The decoder line: how the selected video clip's file decodes, once a
    /// frame of it has been shown. Hidden for everything else.
    fn show_decoder(&self) {
        let imp = self.imp();
        let label = &imp.decoder_label;
        let (Some(playback), Some(path)) = (
            imp.playback.get().and_then(std::rc::Weak::upgrade),
            self.selected_video_path(),
        ) else {
            label.set_visible(false);
            return;
        };
        let text = match playback.decoding(&path) {
            Some(decoding) if decoding.api.is_hardware() => fill(
                &gettext("Decoded with {api} ({decoder})"),
                &[
                    ("api", &decoding.api.name()),
                    ("decoder", &decoding.decoder),
                ],
            ),
            Some(decoding) => fill(
                &gettext("Decoded on the processor ({decoder})"),
                &[("decoder", &decoding.decoder)],
            ),
            None => gettext("Decoder known once a frame of the clip is shown"),
        };
        label.set_tooltip_text(Some(&text));
        label.set_text(&text);
        label.set_visible(true);
    }

    /// The file of the selected video clip, if one is selected.
    fn selected_video_path(&self) -> Option<std::path::PathBuf> {
        let timeline = self.imp().timeline.get()?;
        let project = timeline.project();
        let model = project.timeline();
        let item = timeline.selection()?;
        let video = model
            .track(item.track)
            .is_some_and(|t| t.kind == TrackKind::Video);
        match model.item(item).ok()? {
            Composable::Clip(clip) if video => model.media_path(clip.media_reference()?),
            _ => None,
        }
    }

    /// The models the registry prefers for removing the selected clip's
    /// background (one of each kind: a still's segmentation model and the
    /// matting model as its fallback) that aren't downloaded. `choice` is
    /// the effect's model choice: "auto" (for the clip), "video" or
    /// "still".
    fn missing_models(&self, choice: &str) -> Vec<&'static nox_ai::models::ModelInfo> {
        use nox_ai::models::{Conditions, Media, Task};
        let still = match choice {
            "video" => false,
            "still" => true,
            _ => self
                .selected_video_path()
                .and_then(|path| nox_media::probe(&path).ok())
                .is_some_and(|info| info.is_still_image()),
        };
        let conditions = Conditions {
            media: if still { Media::Still } else { Media::Video },
            f16: self
                .imp()
                .playback
                .get()
                .and_then(std::rc::Weak::upgrade)
                .is_some_and(|playback| playback.storage_16bit()),
        };
        let dir = nox_ai::models::default_dir();
        nox_ai::models::registry()
            .preferred(Task::BackgroundRemoval, &conditions)
            .into_iter()
            .filter(|m| !m.is_available(&dir))
            .collect()
    }

    fn edit(&self, label: &str, merge_key: Option<&str>, change: impl Fn(&mut nox_project::Item)) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        if let Some(timeline) = imp.timeline.get() {
            timeline.edit_selected_clip(label, merge_key, change);
        }
    }

    /// Updates the widgets from the selection.
    fn refresh(&self) {
        let imp = self.imp();
        let Some(timeline) = imp.timeline.get() else {
            return;
        };
        let project: Rc<Project> = timeline.project();
        if let Some(index) = timeline.selected_marker() {
            self.show_marker(&project, index);
            return;
        }
        let selection = timeline.selection();
        let model = project.timeline();
        let Some((item, composable)) =
            selection.and_then(|item| Some((item, model.item(item).ok()?)))
        else {
            imp.pages.set_visible_child_name("empty");
            return;
        };
        let fps = model.frame_rate();
        let length = |frames: i64| project.timecode(frames);
        imp.updating.set(true);
        match composable {
            Composable::Clip(clip) => {
                let video = model
                    .track(item.track)
                    .is_some_and(|t| t.kind == TrackKind::Video);
                imp.name_label.set_text(&clip.item.name);
                let kind = if video {
                    gettext("Video clip")
                } else {
                    gettext("Audio clip")
                };
                imp.details_label.set_text(&fill(
                    &gettext("{kind}, {length}"),
                    &[
                        ("kind", &kind),
                        ("length", &length(clip.duration().frame(fps))),
                    ],
                ));
                imp.composite_section.set_visible(video);
                imp.effects_section.set_visible(video);
                imp.audio_section.set_visible(!video);
                let time = self.media_time().unwrap_or_default();
                if video {
                    self.show_effects(&clip.item, time);
                }
                imp.opacity_scale
                    .adjustment()
                    .set_value(clip.item.value_at(Param::Opacity, time) * 100.0);
                let mode = clip.item.blend_mode();
                let index = BlendMode::ALL.iter().position(|&m| m == mode).unwrap_or(0);
                imp.blend_dropdown.set_selected(index as u32);
                imp.volume_scale
                    .adjustment()
                    .set_value(20.0 * clip.item.value_at(Param::Volume, time).max(1e-6).log10());
                // Each fade may take what the other leaves; steps of a frame.
                let seconds = clip.duration().as_seconds();
                let frame = 1.0 / fps.as_f64();
                let (fade_in, fade_out) = (clip.item.fade_in(), clip.item.fade_out());
                for (spin, value, other) in [
                    (&imp.fade_in, fade_in, fade_out),
                    (&imp.fade_out, fade_out, fade_in),
                ] {
                    let adjustment = spin.adjustment();
                    adjustment.set_upper((seconds - other).max(0.0));
                    adjustment.set_step_increment(frame);
                    adjustment.set_value(value);
                }
                imp.fade_section.set_visible(true);
                imp.transition_section.set_visible(false);
                imp.speed_section.set_visible(true);
                let frozen = clip.item.is_frozen();
                imp.speed.set_value(if frozen {
                    100.0
                } else {
                    clip.item.speed() * 100.0
                });
                imp.speed.set_sensitive(!frozen);
                imp.freeze.set_active(frozen);
                imp.freeze.set_visible(video);
                imp.transform_section.set_visible(video);
                let size = (
                    f64::from(model.settings.width.max(1)),
                    f64::from(model.settings.height.max(1)),
                );
                let transform = clip.item.transform_at(time);
                for (field, spin) in imp.transform_fields().into_iter().enumerate() {
                    spin.set_value(transform_field(&transform, field, size));
                }
                // Keyframe toggles: pressed on a keyframe, bright if the
                // property has any.
                for (param, button) in imp.keyframe_toggles() {
                    button.set_active(clip.item.has_keyframe(param, time));
                    if clip.item.is_animated(param) {
                        button.add_css_class("animated");
                    } else {
                        button.remove_css_class("animated");
                    }
                }
                imp.keyframe_section.set_visible(true);
                let ease = Param::ALL.into_iter().find_map(|param| {
                    clip.item
                        .keyframes(param)
                        .into_iter()
                        .find(|k| (k.time - time).abs() < SAME_TIME)
                        .map(|k| k.ease)
                });
                imp.ease_dropdown.set_sensitive(ease.is_some());
                imp.ease_dropdown
                    .set_selected(u32::from(ease == Some(Ease::Smooth)));
            }
            Composable::Transition(transition) => {
                let kind = transition.kind();
                imp.name_label.set_text(&crate::i18n::transition_name(kind));
                let index = TransitionKind::ALL
                    .iter()
                    .position(|&k| k == kind)
                    .unwrap_or(0);
                imp.transition_type.set_selected(index as u32);
                let adjustment = imp.transition_length.adjustment();
                adjustment.set_step_increment(1.0 / fps.as_f64());
                adjustment.set_value((transition.in_offset + transition.out_offset).as_seconds());
                imp.transition_section.set_visible(true);
                imp.speed_section.set_visible(false);
                let frames = (transition.in_offset + transition.out_offset).frame(fps);
                imp.details_label.set_text(&fill(
                    &gettext("Transition, {length}"),
                    &[("length", &length(frames))],
                ));
                imp.composite_section.set_visible(false);
                imp.effects_section.set_visible(false);
                imp.audio_section.set_visible(false);
                imp.fade_section.set_visible(false);
                imp.transform_section.set_visible(false);
                imp.keyframe_section.set_visible(false);
            }
            _ => {
                imp.updating.set(false);
                imp.pages.set_visible_child_name("empty");
                return;
            }
        }
        imp.updating.set(false);
        self.show_decoder();
        imp.pages.set_visible_child_name("item");
    }
}

/// What to do once models are downloaded (e.g. add the effect needing
/// them).
type AfterDownload = Rc<dyn Fn(&Inspector)>;

impl Inspector {
    /// Asks before downloading `models`, then downloads them, showing
    /// progress, and does `then`.
    fn offer_model_download(
        &self,
        models: Vec<&'static nox_ai::models::ModelInfo>,
        then: AfterDownload,
    ) {
        let dir = nox_ai::models::default_dir();
        let names = models
            .iter()
            .map(|m| format!("{} ({})", m.name, m.license))
            .collect::<Vec<_>>()
            .join(", ");
        let size: u64 = models.iter().map(|m| m.size).sum();
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(gettext("Download the background removal model?"))
            .detail(fill(
                &gettext(
                    "Removing backgrounds uses {models}, {size} MB downloaded once into {folder}.",
                ),
                &[
                    ("models", &names),
                    ("size", &(size / 1_000_000)),
                    ("folder", &dir.display()),
                ],
            ))
            .buttons([gettext("Cancel"), gettext("Download")])
            .cancel_button(0)
            .default_button(1)
            .build();
        let window = self.root().and_downcast::<gtk::Window>();
        dialog.choose(
            window.as_ref(),
            gtk::gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = inspector)]
                self,
                move |answer| {
                    if answer == Ok(1) {
                        inspector.download_models(models, then.clone());
                    }
                }
            ),
        );
    }

    fn download_models(
        &self,
        models: Vec<&'static nox_ai::models::ModelInfo>,
        then: AfterDownload,
    ) {
        let status = self.imp().background_status.get();
        status.set_text(&gettext("Downloading…"));
        status.set_visible(true);
        let (sender, receiver) = async_channel::unbounded::<(u64, u64)>();
        let download = gtk::gio::spawn_blocking(move || {
            // Progress over all of them.
            let total: u64 = models.iter().map(|m| m.size).sum();
            let mut before = 0;
            for model in models {
                model.download(
                    &nox_ai::models::default_dir(),
                    |done, _| {
                        let _ = sender.send_blocking((before + done, total));
                    },
                    &std::sync::atomic::AtomicBool::new(false),
                )?;
                before += model.size;
            }
            Ok::<_, nox_ai::Error>(())
        });
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = inspector)]
            self,
            async move {
                let status = inspector.imp().background_status.get();
                let mut shown = None;
                while let Ok((done, total)) = receiver.recv().await {
                    let percent = done * 100 / total.max(1);
                    if shown != Some(percent) {
                        shown = Some(percent);
                        status.set_text(&fill(
                            &gettext("Downloading… {percent} %"),
                            &[("percent", &percent)],
                        ));
                    }
                }
                match download.await {
                    Ok(Ok(_)) => {
                        status.set_visible(false);
                        if let Some(playback) = inspector
                            .imp()
                            .playback
                            .get()
                            .and_then(std::rc::Weak::upgrade)
                        {
                            playback.reload_effects();
                        }
                        then(&inspector);
                    }
                    Ok(Err(err)) => {
                        tracing::warn!("model download failed: {err}");
                        status.set_text(&fill(
                            &gettext("The download failed: {error}"),
                            &[("error", &err)],
                        ));
                    }
                    Err(_) => status.set_visible(false),
                }
            }
        ));
    }

    fn show_marker(&self, project: &Project, index: usize) {
        let imp = self.imp();
        let model = project.timeline();
        let Some(marker) = model.markers().get(index) else {
            imp.pages.set_visible_child_name("empty");
            return;
        };
        imp.updating.set(true);
        imp.marker_title.set_text(&fill(
            &gettext("Marker at {timecode}"),
            &[(
                "timecode",
                &project.timecode(marker.marked_range.start.frame(model.frame_rate())),
            )],
        ));
        // Only when different: rewriting the text would move the cursor.
        if imp.marker_name.text() != marker.name {
            imp.marker_name.set_text(&marker.name);
        }
        if imp.marker_note.text() != marker.comment {
            imp.marker_note.set_text(&marker.comment);
        }
        let color = nox_project::MARKER_COLORS
            .iter()
            .position(|c| *c == marker.color)
            .unwrap_or(0);
        imp.marker_color.set_selected(color as u32);
        imp.updating.set(false);
        imp.pages.set_visible_child_name("marker");
    }
}

/// The translated name of an OTIO marker color.
fn marker_color_name(color: &str) -> String {
    use crate::i18n::pgettext;
    match color {
        "RED" => pgettext("marker color", "Red"),
        "PINK" => pgettext("marker color", "Pink"),
        "ORANGE" => pgettext("marker color", "Orange"),
        "YELLOW" => pgettext("marker color", "Yellow"),
        "GREEN" => pgettext("marker color", "Green"),
        "CYAN" => pgettext("marker color", "Cyan"),
        "PURPLE" => pgettext("marker color", "Purple"),
        "MAGENTA" => pgettext("marker color", "Magenta"),
        "WHITE" => pgettext("marker color", "White"),
        "BLACK" => pgettext("marker color", "Black"),
        _ => pgettext("marker color", "Blue"),
    }
}

/// Transform field `field` as the inspector shows it: position in timeline
/// pixels (of `size`), zoom and crop in percent, rotation in degrees.
fn transform_field(t: &Transform, field: usize, (width, height): (f64, f64)) -> f64 {
    match field {
        0 => t.x * width,
        1 => t.y * height,
        2 => t.zoom * 100.0,
        3 => t.rotation,
        4..=7 => t.crop[CROP_ORDER[field - 4]] * 100.0,
        _ => unreachable!("transform field {field}"),
    }
}

fn set_transform_field(t: &mut Transform, field: usize, value: f64, (width, height): (f64, f64)) {
    match field {
        0 => t.x = value / width,
        1 => t.y = value / height,
        2 => t.zoom = value / 100.0,
        3 => t.rotation = value,
        4..=7 => t.crop[CROP_ORDER[field - 4]] = value / 100.0,
        _ => unreachable!("transform field {field}"),
    }
}

/// The properties of the transform fields, in field order.
const FIELD_PARAMS: [Param; 8] = [
    Param::X,
    Param::Y,
    Param::Zoom,
    Param::Rotation,
    Param::CropLeft,
    Param::CropRight,
    Param::CropTop,
    Param::CropBottom,
];

/// The crop fields (left, right, top, bottom) as indices of
/// `Transform::crop` (left, top, right, bottom).
const CROP_ORDER: [usize; 4] = [0, 2, 1, 3];
