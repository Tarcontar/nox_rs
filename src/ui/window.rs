use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib, CompositeTemplate};

use crate::media::MediaFile;
use crate::render::backend::{self, RenderBackendType};
use crate::timeline::Timeline;
use crate::ui::{NoxMediaLibrary, NoxPreview, NoxTimeline};

// ---------------------------------------------------------------------------
// Private implementation
// ---------------------------------------------------------------------------
mod imp {
    use super::*;

    #[derive(CompositeTemplate)]
    #[template(resource = "/com/github/nox/ui/window.ui")]
    pub struct NoxWindow {
        #[template_child]
        pub header_bar: TemplateChild<gtk::HeaderBar>,
        #[template_child]
        pub import_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub export_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub upper_paned: TemplateChild<gtk::Paned>,
        #[template_child]
        pub media_library: TemplateChild<NoxMediaLibrary>,
        #[template_child]
        pub preview: TemplateChild<NoxPreview>,
        #[template_child]
        pub timeline: TemplateChild<NoxTimeline>,

        /// Shared project timeline model.
        pub project_timeline: Rc<Timeline>,
        /// Shared media registry: media_id → MediaFile.
        pub media_files: Rc<RefCell<HashMap<u64, MediaFile>>>,
        /// Counter for media IDs.
        pub next_media_id: RefCell<u64>,
        /// Active render backend (shared via Rc so it is created once).
        pub render_backend: Rc<dyn crate::render::backend::RenderBackend>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NoxWindow {
        const NAME: &'static str = "NoxWindow";
        type Type = super::NoxWindow;
        type ParentType = gtk::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            // Ensure child widget types are registered before binding
            NoxMediaLibrary::ensure_type();
            NoxPreview::ensure_type();
            NoxTimeline::ensure_type();

            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for NoxWindow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            obj.setup_actions();
            obj.connect_signals();
        }
    }

    impl Default for NoxWindow {
        fn default() -> Self {
            Self {
                header_bar: Default::default(),
                import_button: Default::default(),
                export_button: Default::default(),
                upper_paned: Default::default(),
                media_library: Default::default(),
                preview: Default::default(),
                timeline: Default::default(),
                project_timeline: Default::default(),
                media_files: Default::default(),
                next_media_id: Default::default(),
                render_backend: backend::create_backend(RenderBackendType::Software),
            }
        }
    }

    impl WidgetImpl for NoxWindow {}
    impl WindowImpl for NoxWindow {}
    impl ApplicationWindowImpl for NoxWindow {}
}

// ---------------------------------------------------------------------------
// Public wrapper type
// ---------------------------------------------------------------------------
glib::wrapper! {
    pub struct NoxWindow(ObjectSubclass<imp::NoxWindow>)
        @extends gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl NoxWindow {
    pub fn new(app: &gtk::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    // -- Actions ------------------------------------------------------------

    fn setup_actions(&self) {
        let import_action = gio::SimpleAction::new("import-media", None);
        let win = self.clone();
        import_action.connect_activate(move |_, _| {
            win.on_import_media();
        });
        self.add_action(&import_action);
    }

    // -- Signal wiring ------------------------------------------------------

    fn connect_signals(&self) {
        let imp = self.imp();

        // Import button
        let win = self.clone();
        imp.import_button.connect_clicked(move |_| {
            win.on_import_media();
        });

        // Connect media library selection → preview
        let win = self.clone();
        imp.media_library.connect_media_selected(move |media_id| {
            win.on_media_selected(media_id);
        });

        // Connect timeline model to the timeline widget
        imp.timeline
            .set_timeline_model(imp.project_timeline.clone());
    }

    // -- Import flow --------------------------------------------------------

    fn on_import_media(&self) {
        let dialog = gtk::FileDialog::builder()
            .title("Import Media")
            .modal(true)
            .build();

        let video_filter = gtk::FileFilter::new();
        video_filter.set_name(Some("Video files"));
        video_filter.add_mime_type("video/*");

        let all_filter = gtk::FileFilter::new();
        all_filter.set_name(Some("All files"));
        all_filter.add_pattern("*");

        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&video_filter);
        filters.append(&all_filter);
        dialog.set_filters(Some(&filters));

        let win_parent = self.clone();
        let win_cb = self.clone();
        dialog.open_multiple(Some(&win_parent), gio::Cancellable::NONE, move |result| {
            if let Ok(files) = result {
                for i in 0..files.n_items() {
                    if let Some(file) = files.item(i).and_then(|o| o.downcast::<gio::File>().ok()) {
                        if let Some(path) = file.path() {
                            win_cb.import_file(&path);
                        }
                    }
                }
            }
        });
    }

    fn import_file(&self, path: &std::path::Path) {
        let imp = self.imp();
        let media_id = {
            let mut id = imp.next_media_id.borrow_mut();
            let current = *id;
            *id += 1;
            current
        };

        let media = MediaFile::from_path(media_id, path);
        imp.media_files.borrow_mut().insert(media_id, media.clone());

        // Add to the media library widget
        imp.media_library.add_media(&media);

        log::info!("Imported media: {} (id={})", media.display_name, media_id);
    }

    // -- Preview selection --------------------------------------------------

    fn on_media_selected(&self, media_id: u64) {
        let imp = self.imp();
        let path = {
            let files = imp.media_files.borrow();
            files.get(&media_id).map(|m| m.path.clone())
        };

        if let Some(path) = path {
            imp.preview.load_media(&path);
        }
    }
}
