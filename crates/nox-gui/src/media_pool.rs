//! Media pool: every file imported into the project, as a thumbnail grid.
//! Files can be dropped onto it or picked with the import dialog; Delete
//! removes the selection.

use gtk::{gdk, gio, glib, prelude::*};

use crate::media_cell::MediaCell;
use crate::media_object::MediaObject;

pub struct MediaPool {
    pub widget: gtk::Box,
    store: gio::ListStore,
    selection: gtk::MultiSelection,
}

impl MediaPool {
    pub fn new() -> Self {
        let store = gio::ListStore::new::<MediaObject>();
        let selection = gtk::MultiSelection::new(Some(store.clone()));
        let widget = build(&store, &selection);
        Self {
            widget,
            store,
            selection,
        }
    }

    pub fn import(&self, file: &gio::File) {
        add_file(&self.store, file);
    }

    /// Selects the first item, if any.
    pub fn select_first(&self) {
        if self.store.n_items() > 0 {
            self.selection.select_item(0, true);
        }
    }

    /// Calls `f` with the first selected item whenever the selection changes.
    pub fn connect_selected<F: Fn(Option<MediaObject>) + 'static>(&self, f: F) {
        self.selection
            .connect_selection_changed(move |selection, _, _| {
                let selected = selection.selection();
                let first = (!selected.is_empty()).then(|| selected.nth(0));
                f(first
                    .and_then(|i| selection.item(i))
                    .and_downcast::<MediaObject>());
            });
    }
}

fn build(store: &gio::ListStore, selection: &gtk::MultiSelection) -> gtk::Box {
    let grid = gtk::GridView::builder()
        .model(selection)
        .factory(&item_factory())
        .max_columns(12)
        .enable_rubberband(true)
        .build();
    grid.add_controller(delete_shortcut(store, selection));

    let placeholder = gtk::Label::builder()
        .label("Drop media files here")
        .css_classes(["dim-label"])
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&placeholder, Some("empty"));
    stack.add_named(
        &gtk::ScrolledWindow::builder()
            .child(&grid)
            .vexpand(true)
            .build(),
        Some("grid"),
    );
    store.connect_items_changed(glib::clone!(
        #[weak]
        stack,
        move |store, _, _, _| {
            stack.set_visible_child_name(if store.n_items() == 0 {
                "empty"
            } else {
                "grid"
            });
        }
    ));

    let import = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Import Media…")
        .css_classes(["flat"])
        .build();
    import.connect_clicked(glib::clone!(
        #[weak]
        store,
        move |button| open_import_dialog(button, store)
    ));

    let title = gtk::Label::builder()
        .label("Media Pool")
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["pane-title"])
        .build();
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header.append(&title);
    header.append(&import);

    let pool = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["media-pool"])
        .width_request(220)
        .build();
    pool.append(&header);
    pool.append(&stack);

    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    drop.connect_drop(glib::clone!(
        #[weak]
        store,
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
    pool.add_controller(drop);

    pool
}

fn item_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        item.set_child(Some(&MediaCell::new()));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let media = item
            .item()
            .and_downcast::<MediaObject>()
            .expect("MediaObject");
        let cell = item.child().and_downcast::<MediaCell>().expect("MediaCell");
        cell.bind(&media);
    });
    factory.connect_unbind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        if let Some(cell) = item.child().and_downcast::<MediaCell>() {
            cell.unbind();
        }
    });
    factory
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

fn open_import_dialog(button: &gtk::Button, store: gio::ListStore) {
    let media = gtk::FileFilter::new();
    media.set_name(Some("Media Files"));
    for mime in ["video/*", "audio/*", "image/*"] {
        media.add_mime_type(mime);
    }
    let all = gtk::FileFilter::new();
    all.set_name(Some("All Files"));
    all.add_pattern("*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&media);
    filters.append(&all);

    let parent = button.root().and_downcast::<gtk::Window>();
    gtk::FileDialog::builder()
        .title("Import Media")
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
