use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gdk;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

use crate::media::thumbnail::ThumbnailGenerator;
use crate::media::MediaFile;
use crate::render::backend::{self, RenderBackend, RenderBackendType};

// ---------------------------------------------------------------------------
// Private implementation
// ---------------------------------------------------------------------------
mod imp {
    use super::*;

    #[derive(CompositeTemplate)]
    #[template(resource = "/com/github/nox/ui/media_library.ui")]
    pub struct NoxMediaLibrary {
        #[template_child]
        pub add_media_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub flow_box: TemplateChild<gtk::FlowBox>,
        #[template_child]
        pub drop_hint_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub drop_overlay: TemplateChild<gtk::Overlay>,

        /// Shared render backend for thumbnail uploads.
        pub render_backend: Rc<dyn RenderBackend>,
        /// Maps media_id → thumbnail texture (set asynchronously).
        pub thumbnails: RefCell<HashMap<u64, gdk::Texture>>,
        /// Callback invoked when a media item is selected.
        pub on_selected: Rc<RefCell<Option<Box<dyn Fn(u64) + 'static>>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NoxMediaLibrary {
        const NAME: &'static str = "NoxMediaLibrary";
        type Type = super::NoxMediaLibrary;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for NoxMediaLibrary {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_drop_target();
            obj.setup_flow_box();
        }
    }

    impl Default for NoxMediaLibrary {
        fn default() -> Self {
            Self {
                add_media_button: Default::default(),
                search_entry: Default::default(),
                flow_box: Default::default(),
                drop_hint_label: Default::default(),
                drop_overlay: Default::default(),
                render_backend: backend::create_backend(RenderBackendType::Software),
                thumbnails: Default::default(),
                on_selected: Default::default(),
            }
        }
    }

    impl WidgetImpl for NoxMediaLibrary {}
    impl BoxImpl for NoxMediaLibrary {}
}

// ---------------------------------------------------------------------------
// Public wrapper type
// ---------------------------------------------------------------------------
glib::wrapper! {
    pub struct NoxMediaLibrary(ObjectSubclass<imp::NoxMediaLibrary>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Buildable;
}

impl NoxMediaLibrary {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Register a callback for when the user selects a media item.
    pub fn connect_media_selected<F: Fn(u64) + 'static>(&self, f: F) {
        *self.imp().on_selected.as_ref().borrow_mut() = Some(Box::new(f));
    }

    /// Add a media file to the library and start thumbnail generation.
    pub fn add_media(&self, media: &MediaFile) {
        let media_id = media.id;
        let display_name = media.display_name.clone();
        let path = media.path.clone();

        // Build the thumbnail card widget
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .css_classes(["media-thumbnail"])
            .build();

        let picture = gtk::Picture::builder()
            .width_request(140)
            .height_request(80)
            .content_fit(gtk::ContentFit::Cover)
            .build();

        let label = gtk::Label::builder()
            .label(&display_name)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(18)
            .css_classes(["media-thumbnail-label"])
            .build();

        card.append(&picture);
        card.append(&label);

        // Store media_id on the FlowBoxChild for later lookup
        let flow_child = gtk::FlowBoxChild::builder()
            .child(&card)
            .name(&format!("media-{media_id}"))
            .build();
        self.imp().flow_box.append(&flow_child);

        // Set up DragSource on the card so clips can be dragged to the timeline
        let drag_source = gtk::DragSource::new();
        drag_source.set_actions(gdk::DragAction::COPY);
        let mid = media_id;
        drag_source.connect_prepare(move |_src, _x, _y| {
            let content = gdk::ContentProvider::for_value(&mid.to_value());
            Some(content)
        });
        card.add_controller(drag_source);

        // Start async thumbnail generation
        let result_handle = ThumbnailGenerator::generate_async(&path, media_id);

        let pic = picture.clone();
        let thumb_map = self.imp().thumbnails.clone();
        let renderer = Rc::clone(&self.imp().render_backend);
        let mid = media_id;
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            let mut lock = result_handle.lock().unwrap();
            if let Some(frame) = lock.take() {
                if let Ok(texture) = renderer.upload_frame(&frame) {
                    pic.set_paintable(Some(&texture));
                    thumb_map.borrow_mut().insert(mid, texture);
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    // -- Internal setup -----------------------------------------------------

    fn setup_flow_box(&self) {
        let imp = self.imp();
        let on_selected = Rc::clone(&imp.on_selected);

        imp.flow_box.connect_child_activated(move |_fb, child| {
            let name = child.widget_name();
            if let Some(id_str) = name.strip_prefix("media-") {
                if let Ok(id) = id_str.parse::<u64>() {
                    if let Some(ref cb) = *on_selected.borrow() {
                        cb(id);
                    }
                }
            }
        });
    }

    fn setup_drop_target(&self) {
        let drop_target = gtk::DropTarget::new(gio::File::static_type(), gdk::DragAction::COPY);

        let hint_label = self.imp().drop_hint_label.clone();
        drop_target.connect_enter(move |_dt, _x, _y| {
            hint_label.set_visible(true);
            gdk::DragAction::COPY
        });

        let hint_label = self.imp().drop_hint_label.clone();
        drop_target.connect_leave(move |_dt| {
            hint_label.set_visible(false);
        });

        // For drag-and-drop file import we would handle the `drop` signal here.
        // Omitted for brevity – the Import button is the primary import path.

        let hint_label = self.imp().drop_hint_label.clone();
        drop_target.connect_drop(move |_dt, value, _x, _y| {
            hint_label.set_visible(false);
            if let Ok(file) = value.get::<gio::File>() {
                if let Some(_path) = file.path() {
                    log::info!("File dropped onto media library (drop import TBD)");
                    // TODO: import the dropped file
                }
            }
            true
        });

        self.imp().drop_overlay.add_controller(drop_target);
    }
}
