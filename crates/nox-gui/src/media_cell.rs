//! Grid cell for the media pool: thumbnail (or icon), name and duration.

use gtk::{gdk, glib, prelude::*, subclass::prelude::*};

use crate::media_object::{MediaObject, THUMBNAIL_HEIGHT, THUMBNAIL_WIDTH};

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct MediaCell {
        pub picture: gtk::Picture,
        pub icon: gtk::Image,
        pub name: gtk::Label,
        pub summary: gtk::Label,
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
            // Own node name so theme rules for boxes inside grid cells don't add padding.
            klass.set_css_name("media-cell");
        }
    }

    impl ObjectImpl for MediaCell {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Vertical);
            obj.set_spacing(2);
            obj.set_halign(gtk::Align::Center);

            self.icon.set_pixel_size(32);
            self.picture.set_content_fit(gtk::ContentFit::Contain);
            let frame = gtk::Overlay::builder()
                .child(&self.icon)
                .width_request(THUMBNAIL_WIDTH as i32)
                .height_request(THUMBNAIL_HEIGHT as i32)
                .css_classes(["thumbnail-frame"])
                .overflow(gtk::Overflow::Hidden)
                .build();
            frame.add_overlay(&self.picture);

            // The thumbnail frame sets the cell width; labels ellipsize within it.
            self.name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            self.name.set_max_width_chars(1);
            self.summary.add_css_class("media-summary");
            self.summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
            self.summary.set_max_width_chars(1);

            obj.append(&frame);
            obj.append(&self.name);
            obj.append(&self.summary);

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
                self.picture,
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
                .bind_property("thumbnail", &imp.picture, "paintable")
                .sync_create()
                .build(),
            media
                .bind_property("thumbnail", &imp.icon, "visible")
                .transform_to(|_, thumbnail: Option<gdk::Texture>| Some(thumbnail.is_none()))
                .sync_create()
                .build(),
            media
                .bind_property("icon-name", &imp.icon, "icon-name")
                .sync_create()
                .build(),
            media
                .bind_property("summary", &imp.summary, "label")
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
