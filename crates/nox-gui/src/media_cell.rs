//! Grid cell for the media pool (`resources/ui/media-cell.ui`): thumbnail
//! (or icon), name and duration of its `media`, all bound in the template.
//! Cells can be dragged onto the timeline.

use gtk::{CompositeTemplate, gdk, glib, prelude::*, subclass::prelude::*};

use crate::media_object::MediaObject;

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default, CompositeTemplate, glib::Properties)]
    #[template(resource = "/io/github/feliwir/Nox/ui/media-cell.ui")]
    #[properties(wrapper_type = super::MediaCell)]
    pub struct MediaCell {
        #[template_child]
        pub picture: TemplateChild<gtk::Picture>,
        /// The item shown (set by the list item factory in
        /// `media-pool-item.ui`), also what a drag carries.
        #[property(get, set, nullable)]
        pub media: RefCell<Option<MediaObject>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MediaCell {
        const NAME: &'static str = "NoxMediaCell";
        type Type = super::MediaCell;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            MediaObject::ensure_type();
            klass.bind_template();
            klass.bind_template_instance_callbacks();
            // Own node name so theme rules for boxes inside grid cells don't add padding.
            klass.set_css_name("media-cell");
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    #[glib::derived_properties]
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
                    let media = obj.media()?;
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

#[gtk::template_callbacks(functions)]
impl MediaCell {
    /// For `<closure>` in the template: the icon shows until there is a
    /// thumbnail.
    #[template_callback]
    fn is_none(thumbnail: Option<gdk::Texture>) -> bool {
        thumbnail.is_none()
    }
}
