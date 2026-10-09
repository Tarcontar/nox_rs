//! Grid cell for the media pool (`resources/ui/media-cell.ui`): thumbnail
//! (or icon), name and duration. Cells can be dragged onto the timeline.

use gtk::{CompositeTemplate, gdk, glib, prelude::*, subclass::prelude::*};

use crate::media_object::MediaObject;

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/nox/Nox/ui/media-cell.ui")]
    pub struct MediaCell {
        #[template_child]
        pub picture: TemplateChild<gtk::Picture>,
        #[template_child]
        pub icon: TemplateChild<gtk::Image>,
        #[template_child]
        pub name: TemplateChild<gtk::Label>,
        #[template_child]
        pub summary: TemplateChild<gtk::Label>,
        pub bindings: RefCell<Vec<glib::Binding>>,
        /// The bound item, for dragging it onto the timeline.
        pub media: RefCell<Option<MediaObject>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MediaCell {
        const NAME: &'static str = "NoxMediaCell";
        type Type = super::MediaCell;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            // Own node name so theme rules for boxes inside grid cells don't add padding.
            klass.set_css_name("media-cell");
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for MediaCell {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let drag = gtk::DragSource::new();
            drag.set_actions(gdk::DragAction::COPY);
            drag.connect_prepare(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                None,
                move |_, _, _| {
                    let media = obj.imp().media.borrow().clone()?;
                    Some(gdk::ContentProvider::for_value(&media.to_value()))
                }
            ));
            drag.connect_drag_begin(glib::clone!(
                #[weak(rename_to = picture)]
                self.picture.get(),
                move |source, _| {
                    if let Some(paintable) = picture.paintable() {
                        source.set_icon(Some(&paintable), 0, 0);
                    }
                }
            ));
            obj.add_controller(drag);
        }
    }

    impl WidgetImpl for MediaCell {}
    impl BoxImpl for MediaCell {}
}

glib::wrapper! {
    pub struct MediaCell(ObjectSubclass<imp::MediaCell>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl MediaCell {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn bind(&self, media: &MediaObject) {
        let imp = self.imp();
        imp.media.replace(Some(media.clone()));
        imp.name.set_label(&media.name());
        let bindings = vec![
            media
                .bind_property("thumbnail", &*imp.picture, "paintable")
                .sync_create()
                .build(),
            media
                .bind_property("thumbnail", &*imp.icon, "visible")
                .transform_to(|_, thumbnail: Option<gdk::Texture>| Some(thumbnail.is_none()))
                .sync_create()
                .build(),
            media
                .bind_property("icon-name", &*imp.icon, "icon-name")
                .sync_create()
                .build(),
            media
                .bind_property("summary", &*imp.summary, "label")
                .sync_create()
                .build(),
            media
                .bind_property("details", self, "tooltip-text")
                .sync_create()
                .build(),
        ];
        imp.bindings.replace(bindings);
    }

    pub fn unbind(&self) {
        self.imp().media.replace(None);
        for binding in self.imp().bindings.take() {
            binding.unbind();
        }
    }
}
