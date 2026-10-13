//! Media pool (`resources/ui/media-pool.ui`): every file imported into the
//! project, as a thumbnail grid. Files can be dropped onto it or picked
//! with the import dialog; Delete removes the selection.

use gtk::{CompositeTemplate, gdk, gio, glib, prelude::*, subclass::prelude::*};

use crate::i18n::gettext;
use crate::media_cell::MediaCell;
use crate::media_object::MediaObject;

mod imp {
    use super::*;

    #[derive(CompositeTemplate)]
    #[template(resource = "/io/github/nox/Nox/ui/media-pool.ui")]
    pub struct MediaPool {
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub grid: TemplateChild<gtk::GridView>,
        pub store: gio::ListStore,
        pub selection: gtk::MultiSelection,
    }

    impl Default for MediaPool {
        fn default() -> Self {
            let store = gio::ListStore::new::<MediaObject>();
            Self {
                stack: TemplateChild::default(),
                grid: TemplateChild::default(),
                selection: gtk::MultiSelection::new(Some(store.clone())),
                store,
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MediaPool {
        const NAME: &'static str = "NoxMediaPool";
        type Type = super::MediaPool;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            // Used by the grid's item template (media-pool-item.ui).
            MediaCell::ensure_type();
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for MediaPool {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            self.grid.set_model(Some(&self.selection));
            self.grid
                .add_controller(delete_shortcut(&self.store, &self.selection));
            self.store.connect_items_changed(glib::clone!(
                #[weak(rename_to = stack)]
                self.stack.get(),
                move |store, _, _, _| {
                    stack.set_visible_child_name(if store.n_items() == 0 {
                        "empty"
                    } else {
                        "grid"
                    });
                }
            ));
            let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
            drop.connect_drop(glib::clone!(
                #[weak(rename_to = store)]
                self.store,
                #[upgrade_or]
                false,
                move |_, value, _, _| {
                    let Ok(files) = value.get::<gdk::FileList>() else {
                        return false;
                    };
                    for file in files.files() {
                        add_file(&store, &file);
                    }
                    true
                }
            ));
            obj.add_controller(drop);
        }
    }

    impl WidgetImpl for MediaPool {}
    impl BoxImpl for MediaPool {}
}

glib::wrapper! {
    pub struct MediaPool(ObjectSubclass<imp::MediaPool>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

#[gtk::template_callbacks]
impl MediaPool {
    #[template_callback]
    fn on_import_clicked(&self) {
        let parent = self.root().and_downcast::<gtk::Window>();
        open_import_dialog(parent, self.imp().store.clone());
    }

    pub fn import(&self, file: &gio::File) {
        add_file(&self.imp().store, file);
    }

    /// The item with this path, if imported.
    pub fn find(&self, path: &std::path::Path) -> Option<MediaObject> {
        let path = path.to_string_lossy();
        self.imp()
            .store
            .iter::<MediaObject>()
            .flatten()
            .find(|media| media.path() == path)
    }

    /// Calls `f` when an item is activated (double-clicked or Enter).
    pub fn connect_activate<F: Fn(MediaObject) + 'static>(&self, f: F) {
        let store = self.imp().store.clone();
        self.imp().grid.connect_activate(move |_, position| {
            if let Some(media) = store.item(position).and_downcast::<MediaObject>() {
                f(media);
            }
        });
    }
}

fn delete_shortcut(
    store: &gio::ListStore,
    selection: &gtk::MultiSelection,
) -> gtk::ShortcutController {
    let action = gtk::CallbackAction::new(glib::clone!(
        #[weak]
        store,
        #[weak]
        selection,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, _| {
            let selected = selection.selection();
            // Remove from the back so earlier indices stay valid.
            for i in (0..store.n_items()).rev().filter(|&i| selected.contains(i)) {
                store.remove(i);
            }
            glib::Propagation::Stop
        }
    ));
    let controller = gtk::ShortcutController::new();
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Delete|BackSpace"),
        Some(action),
    ));
    controller
}

fn open_import_dialog(parent: Option<gtk::Window>, store: gio::ListStore) {
    let media = gtk::FileFilter::new();
    media.set_name(Some(&gettext("Media Files")));
    for mime in ["video/*", "audio/*", "image/*"] {
        media.add_mime_type(mime);
    }
    let all = gtk::FileFilter::new();
    all.set_name(Some(&gettext("All Files")));
    all.add_pattern("*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&media);
    filters.append(&all);

    gtk::FileDialog::builder()
        .title(gettext("Import Media"))
        .modal(true)
        .filters(&filters)
        .default_filter(&media)
        .build()
        .open_multiple(parent.as_ref(), gio::Cancellable::NONE, move |result| {
            let files = match result {
                Ok(files) => files,
                Err(err) => {
                    if !err.matches(gtk::DialogError::Dismissed) {
                        tracing::warn!("import dialog failed: {err}");
                    }
                    return;
                }
            };
            for file in files.iter::<gio::File>().flatten() {
                add_file(&store, &file);
            }
        });
}

fn add_file(store: &gio::ListStore, file: &gio::File) {
    let Some(path) = file.path() else {
        tracing::warn!("ignoring non-local file {}", file.uri());
        return;
    };
    if path.is_dir() {
        tracing::warn!("importing folders is not supported yet: {}", path.display());
        return;
    }
    let path_str = path.to_string_lossy();
    let already_imported = store
        .iter::<MediaObject>()
        .flatten()
        .any(|media| media.path() == path_str);
    if already_imported {
        tracing::info!("already imported: {}", path.display());
        return;
    }
    tracing::info!("importing {}", path.display());
    store.append(&MediaObject::import(&path));
}
