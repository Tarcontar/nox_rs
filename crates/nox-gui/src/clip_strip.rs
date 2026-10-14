//! The Color page's clip strip (`resources/ui/clip-strip.ui`): the video
//! clips of the timeline in time order, as thumbnails in a horizontal list.
//! Selecting one selects it on the timeline and moves the playhead to its
//! start, so the viewer, scopes and color controls show that clip.

use gtk::{CompositeTemplate, gio, glib, prelude::*, subclass::prelude::*};
use nox_project::{Composable, ItemRef, TrackKind};

use crate::media_object::MediaObject;
use crate::media_pool::MediaPool;
use crate::timeline::TimelineView;

mod row {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::ClipRow)]
    pub struct ClipRow {
        #[property(get, set)]
        pub name: RefCell<String>,
        /// Start on the timeline.
        #[property(get, set)]
        pub timecode: RefCell<String>,
        /// The media pool item, for the thumbnail.
        #[property(get, set, nullable)]
        pub media: RefCell<Option<MediaObject>>,
        pub item: Cell<Option<ItemRef>>,
        pub start: Cell<i64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ClipRow {
        const NAME: &'static str = "NoxClipRow";
        type Type = super::ClipRow;
    }

    #[glib::derived_properties]
    impl ObjectImpl for ClipRow {}
}

glib::wrapper! {
    /// A row of the clip strip.
    pub struct ClipRow(ObjectSubclass<row::ClipRow>);
}

impl ClipRow {
    fn item(&self) -> Option<ItemRef> {
        self.imp().item.get()
    }
}

mod imp {
    use super::*;
    use std::cell::{Cell, OnceCell};

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/nox/Nox/ui/clip-strip.ui")]
    pub struct ClipStrip {
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub store: TemplateChild<gio::ListStore>,
        #[template_child]
        pub selection: TemplateChild<gtk::SingleSelection>,
        pub timeline: OnceCell<TimelineView>,
        pub pool: OnceCell<MediaPool>,
        /// Set while the list follows the timeline, so the change isn't
        /// sent back to it.
        pub updating: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ClipStrip {
        const NAME: &'static str = "NoxClipStrip";
        type Type = super::ClipStrip;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            ClipRow::ensure_type();
            MediaObject::ensure_type();
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ClipStrip {}
    impl WidgetImpl for ClipStrip {}
    impl BoxImpl for ClipStrip {}
}

glib::wrapper! {
    pub struct ClipStrip(ObjectSubclass<imp::ClipStrip>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

#[gtk::template_callbacks]
impl ClipStrip {
    /// Shows the clips of `timeline`'s project, with thumbnails from `pool`.
    /// Called once.
    pub fn set_up(&self, timeline: &TimelineView, pool: &MediaPool) {
        let imp = self.imp();
        let _ = imp.timeline.set(timeline.clone());
        let _ = imp.pool.set(pool.clone());
        timeline.project().connect_changed(glib::clone!(
            #[weak(rename_to = strip)]
            self,
            move |_| strip.rebuild()
        ));
        timeline.connect_selection_changed(glib::clone!(
            #[weak(rename_to = strip)]
            self,
            move |_| strip.show_selection()
        ));
        // Opening a project imports its media after the timeline is shown;
        // rows need the pool's items for their thumbnails.
        pool.connect_items_changed(glib::clone!(
            #[weak(rename_to = strip)]
            self,
            move || strip.rebuild()
        ));
        self.rebuild();
    }

    /// A row was selected in the list.
    #[template_callback]
    fn on_selected(&self) {
        let imp = self.imp();
        if imp.updating.get() {
            return;
        }
        let (Some(timeline), Some(row)) = (
            imp.timeline.get(),
            imp.selection.selected_item().and_downcast::<ClipRow>(),
        ) else {
            return;
        };
        timeline.set_selection(row.item());
        timeline.set_playhead(row.imp().start.get());
    }

    /// Refills the list from the timeline.
    fn rebuild(&self) {
        let imp = self.imp();
        let (Some(timeline), Some(pool)) = (imp.timeline.get(), imp.pool.get()) else {
            return;
        };
        let project = timeline.project();
        let model = project.timeline();
        let fps = model.frame_rate();
        let mut rows: Vec<(i64, ItemRef, ClipRow)> = Vec::new();
        for (track_index, track) in model.tracks() {
            if track.kind != TrackKind::Video {
                continue;
            }
            for (index, child) in track.children.iter().enumerate() {
                let Composable::Clip(clip) = child else {
                    continue;
                };
                let item = ItemRef {
                    track: track_index,
                    index,
                };
                let Ok(range) = model.item_range(item) else {
                    continue;
                };
                let start = range.start.frame(fps);
                let media = clip
                    .media_reference()
                    .and_then(|r| r.path())
                    .and_then(|path| pool.find(&path));
                let row: ClipRow = glib::Object::builder()
                    .property("name", &clip.item.name)
                    .property("timecode", project.timecode(start))
                    .property("media", media)
                    .build();
                row.imp().item.set(Some(item));
                row.imp().start.set(start);
                rows.push((start, item, row));
            }
        }
        drop(model);
        rows.sort_by_key(|&(start, item, _)| (start, item.track));
        let rows: Vec<ClipRow> = rows.into_iter().map(|(_, _, row)| row).collect();
        imp.updating.set(true);
        imp.store.splice(0, imp.store.n_items(), &rows);
        imp.updating.set(false);
        imp.stack
            .set_visible_child_name(if rows.is_empty() { "empty" } else { "clips" });
        self.show_selection();
    }

    /// Selects the timeline's selected clip in the list.
    fn show_selection(&self) {
        let imp = self.imp();
        let Some(timeline) = imp.timeline.get() else {
            return;
        };
        let selected = timeline.selection();
        let position = imp
            .store
            .iter::<ClipRow>()
            .flatten()
            .position(|row| selected.is_some() && row.item() == selected)
            .map_or(gtk::INVALID_LIST_POSITION, |p| p as u32);
        imp.updating.set(true);
        imp.selection.set_selected(position);
        imp.updating.set(false);
    }
}
