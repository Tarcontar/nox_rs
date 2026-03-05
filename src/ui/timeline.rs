use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, CompositeTemplate};

use super::timeline_canvas::TRACK_HEIGHT;
use crate::timeline::Timeline;
use crate::ui::NoxTimelineCanvas;

// ---------------------------------------------------------------------------
// Private implementation
// ---------------------------------------------------------------------------
mod imp {
    use super::*;

    #[derive(CompositeTemplate, Default)]
    #[template(resource = "/com/github/nox/ui/timeline.ui")]
    pub struct NoxTimeline {
        #[template_child]
        pub snap_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub split_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub delete_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub zoom_scale: TemplateChild<gtk::Scale>,
        #[template_child]
        pub zoom_adjustment: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub zoom_in_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub zoom_out_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub track_headers: TemplateChild<gtk::Box>,
        #[template_child]
        pub timeline_canvas: TemplateChild<NoxTimelineCanvas>,
        #[template_child]
        pub timeline_scroll: TemplateChild<gtk::ScrolledWindow>,

        /// Reference to the project timeline model.
        ///
        /// Wrapped in `Rc` so closures set up in `constructed()` share the
        /// same cell and observe later `set_timeline_model()` writes.
        pub timeline_model: Rc<RefCell<Option<Rc<Timeline>>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NoxTimeline {
        const NAME: &'static str = "NoxTimeline";
        type Type = super::NoxTimeline;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            NoxTimelineCanvas::ensure_type();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for NoxTimeline {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_controls();
            obj.setup_drop_target();
            obj.setup_track_headers();
        }
    }

    impl WidgetImpl for NoxTimeline {}
    impl BoxImpl for NoxTimeline {}
}

// ---------------------------------------------------------------------------
// Public wrapper type
// ---------------------------------------------------------------------------
glib::wrapper! {
    pub struct NoxTimeline(ObjectSubclass<imp::NoxTimeline>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Buildable;
}

impl NoxTimeline {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Connect the UI to a timeline data model.
    pub fn set_timeline_model(&self, model: Rc<Timeline>) {
        let imp = self.imp();
        imp.timeline_canvas.set_timeline_model(model.clone());
        *imp.timeline_model.as_ref().borrow_mut() = Some(model);
        self.setup_track_headers();
    }

    // -- Track header labels ------------------------------------------------

    fn setup_track_headers(&self) {
        let imp = self.imp();
        // Clear existing
        while let Some(child) = imp.track_headers.first_child() {
            imp.track_headers.remove(&child);
        }

        if let Some(ref model) = *imp.timeline_model.as_ref().borrow() {
            for track in model.tracks.borrow().iter() {
                let header = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(4)
                    .css_classes(["track-header"])
                    .height_request(TRACK_HEIGHT as i32)
                    .build();

                let label = gtk::Label::builder()
                    .label(&track.name)
                    .hexpand(true)
                    .xalign(0.0)
                    .build();

                header.append(&label);
                imp.track_headers.append(&header);
            }
        }
    }

    // -- Controls -----------------------------------------------------------

    fn setup_controls(&self) {
        let imp = self.imp();

        // Zoom in / out
        let adj = imp.zoom_adjustment.clone();
        imp.zoom_in_button.connect_clicked(move |_| {
            adj.set_value((adj.value() + 20.0).min(adj.upper()));
        });
        let adj = imp.zoom_adjustment.clone();
        imp.zoom_out_button.connect_clicked(move |_| {
            adj.set_value((adj.value() - 20.0).max(adj.lower()));
        });

        // Zoom slider → update model + resize canvas
        let model = imp.timeline_model.clone();
        let canvas = imp.timeline_canvas.clone();
        imp.zoom_adjustment.connect_value_changed(move |adj| {
            if let Some(ref m) = *model.borrow() {
                *m.zoom.borrow_mut() = adj.value();
                canvas.queue_resize();
            }
        });

        // Split clip at playhead
        let model = imp.timeline_model.clone();
        let canvas = imp.timeline_canvas.clone();
        imp.split_button.connect_clicked(move |_| {
            if let Some(ref m) = *model.borrow() {
                if let Some(clip_id) = canvas.selected_clip() {
                    let ph = *m.playhead_secs.borrow();
                    m.split_clip_at(clip_id, ph);
                    canvas.queue_draw();
                }
            }
        });

        // Delete selected clip
        let model = imp.timeline_model.clone();
        let canvas = imp.timeline_canvas.clone();
        imp.delete_button.connect_clicked(move |_| {
            if let Some(ref m) = *model.borrow() {
                if let Some(clip_id) = canvas.selected_clip() {
                    m.remove_clip(clip_id);
                    canvas.set_selected_clip(None);
                }
            }
        });

        // Click on canvas → select clip / move playhead
        let model = imp.timeline_model.clone();
        let canvas = imp.timeline_canvas.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_gesture, _n, x, y| {
            let found = canvas.clip_at(x, y);
            canvas.set_selected_clip(found);

            let secs = canvas.x_to_secs(x).max(0.0);
            if let Some(ref m) = *model.borrow() {
                *m.playhead_secs.borrow_mut() = secs;
            }
            canvas.queue_draw();
        });
        imp.timeline_canvas.add_controller(click);
    }

    // -- Drop target (receive media from library) ---------------------------

    fn setup_drop_target(&self) {
        let imp = self.imp();
        let drop_target = gtk::DropTarget::new(u64::static_type(), gdk::DragAction::COPY);

        let model = imp.timeline_model.clone();
        let canvas = imp.timeline_canvas.clone();

        drop_target.connect_drop(move |_dt, value, x, _y| {
            let media_id = match value.get::<u64>() {
                Ok(id) => id,
                Err(_) => return false,
            };

            if let Some(ref m) = *model.borrow() {
                let position_secs = canvas.x_to_secs(x).max(0.0);
                // Default source duration (overridden once we query the media)
                let source_duration = 10.0;
                m.add_clip(0, media_id, source_duration, position_secs);
                canvas.queue_draw();
                log::info!(
                    "Dropped media {} at {:.2}s on track 0",
                    media_id,
                    position_secs
                );
            }
            true
        });

        imp.timeline_canvas.add_controller(drop_target);
    }
}
