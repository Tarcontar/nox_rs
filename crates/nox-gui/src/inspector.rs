//! The inspector (`resources/ui/inspector.ui`): properties of the item
//! selected on the timeline, editable in place. Changes are undoable;
//! dragging a slider makes one undo step.

use gtk::{CompositeTemplate, glib, prelude::*, subclass::prelude::*};
use nox_project::{BlendMode, Composable, TrackKind};
use std::rc::Rc;

use crate::i18n::{blend_mode_name, fill, gettext};
use crate::project::Project;
use crate::timeline::TimelineView;

mod imp {
    use super::*;
    use std::cell::{Cell, OnceCell};

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/nox/Nox/ui/inspector.ui")]
    pub struct Inspector {
        #[template_child]
        pub pages: TemplateChild<gtk::Stack>,
        #[template_child]
        pub name_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub details_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub composite_section: TemplateChild<gtk::Box>,
        #[template_child]
        pub audio_section: TemplateChild<gtk::Box>,
        #[template_child]
        pub opacity_scale: TemplateChild<gtk::Scale>,
        #[template_child]
        pub blend_dropdown: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub volume_scale: TemplateChild<gtk::Scale>,
        pub timeline: OnceCell<TimelineView>,
        /// Set while the widgets are updated from the model, so their
        /// change handlers don't write the values back as edits.
        pub updating: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Inspector {
        const NAME: &'static str = "NoxInspector";
        type Type = super::Inspector;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Inspector {
        fn constructed(&self) {
            self.parent_constructed();
            let names: Vec<String> = BlendMode::ALL.into_iter().map(blend_mode_name).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            self.blend_dropdown
                .set_model(Some(&gtk::StringList::new(&names)));

            let obj = self.obj();
            self.opacity_scale
                .adjustment()
                .connect_value_changed(glib::clone!(
                    #[weak]
                    obj,
                    move |adjustment| {
                        let opacity = adjustment.value() / 100.0;
                        obj.edit(&gettext("Change Opacity"), Some("opacity"), |item| {
                            item.set_opacity(opacity)
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
            self.volume_scale
                .adjustment()
                .connect_value_changed(glib::clone!(
                    #[weak]
                    obj,
                    move |adjustment| {
                        let volume = 10f64.powf(adjustment.value() / 20.0);
                        obj.edit(&gettext("Change Volume"), Some("volume"), |item| {
                            item.set_volume(volume)
                        });
                    }
                ));
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

impl Inspector {
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
        self.refresh();
    }

    fn edit(
        &self,
        label: &str,
        merge_key: Option<&str>,
        change: impl FnOnce(&mut nox_project::Item),
    ) {
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
                imp.audio_section.set_visible(!video);
                imp.opacity_scale
                    .adjustment()
                    .set_value(clip.item.opacity() * 100.0);
                let mode = clip.item.blend_mode();
                let index = BlendMode::ALL.iter().position(|&m| m == mode).unwrap_or(0);
                imp.blend_dropdown.set_selected(index as u32);
                imp.volume_scale
                    .adjustment()
                    .set_value(20.0 * clip.item.volume().max(1e-6).log10());
            }
            Composable::Transition(transition) => {
                imp.name_label.set_text(&gettext("Dissolve"));
                let frames = (transition.in_offset + transition.out_offset).frame(fps);
                imp.details_label.set_text(&fill(
                    &gettext("Transition, {length}"),
                    &[("length", &length(frames))],
                ));
                imp.composite_section.set_visible(false);
                imp.audio_section.set_visible(false);
            }
            _ => {
                imp.updating.set(false);
                imp.pages.set_visible_child_name("empty");
                return;
            }
        }
        imp.updating.set(false);
        imp.pages.set_visible_child_name("item");
    }
}
