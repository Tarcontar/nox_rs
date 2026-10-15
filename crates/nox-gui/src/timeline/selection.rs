//! Selecting clips and markers, and the context menus.

use super::*;

impl imp::TimelineView {
    pub(super) fn context_menu(&self, x: f64, y: f64) {
        let (menu, track) = match self.hit(x, y) {
            Hit::Ruler | Hit::Marker(_) => {
                self.obj().scrub_to(x);
                (self.ruler_menu.get().expect("ruler menu"), None)
            }
            Hit::Item { item, .. } => {
                self.set_selection(Some(item));
                (self.menu.get().expect("menu"), None)
            }
            Hit::Header { track, .. } => {
                let (video, audio) = self.track_menus.get().expect("track menus");
                let project = self.project();
                let is_video = project
                    .timeline()
                    .track(track)
                    .is_some_and(|t| t.kind == TrackKind::Video);
                (if is_video { video } else { audio }, Some(track))
            }
            _ => return,
        };
        if let Some(track) = track {
            self.menu_track.set(Some(track));
            self.update_track_actions();
        }
        menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        menu.popup();
    }

    /// The `track.*` actions of the track menus: `rename`, `delete`,
    /// and the toggles `muted` (for video: hidden) and `locked`.
    pub(super) fn add_track_actions(&self) {
        let obj = self.obj();
        let group = &self.track_actions;
        let toggle = |name: &str, f: fn(&super::TimelineView, usize, bool)| {
            let action = gio::SimpleAction::new_stateful(name, None, &false.to_variant());
            action.connect_change_state(glib::clone!(
                #[weak]
                obj,
                move |_, value| {
                    if let (Some(track), Some(on)) = (
                        obj.imp().menu_track.get(),
                        value.and_then(|v| v.get::<bool>()),
                    ) {
                        f(&obj, track, on);
                    }
                }
            ));
            group.add_action(&action);
        };
        toggle("muted", |view, track, muted| {
            view.set_track_enabled(track, !muted)
        });
        toggle("locked", |view, track, locked| {
            view.set_track_locked(track, locked)
        });
        toggle("sync", |view, track, on| {
            view.set_track_sync_locked(track, on)
        });
        toggle("solo", |view, track, on| view.set_track_solo(track, on));
        let simple = |name: &str, f: fn(&super::TimelineView, usize)| {
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(glib::clone!(
                #[weak]
                obj,
                move |_, _| {
                    if let Some(track) = obj.imp().menu_track.get() {
                        f(&obj, track);
                    }
                }
            ));
            group.add_action(&action);
        };
        simple("rename", |view, track| view.show_rename_dialog(track));
        simple("delete", |view, track| view.delete_track(track));
        obj.insert_action_group("track", Some(group));
    }

    /// Shows the state of the menu's track in the toggles.
    pub(super) fn update_track_actions(&self) {
        let project = self.project();
        let timeline = project.timeline();
        let Some(track) = self.menu_track.get().and_then(|t| timeline.track(t)) else {
            return;
        };
        let set = |name: &str, on: bool| {
            if let Some(action) = self
                .track_actions
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_state(&on.to_variant());
            }
        };
        set("muted", !track.item.enabled);
        set("locked", track.item.locked());
        set("sync", track.item.sync_locked());
        set("solo", track.item.solo());
        if let Some(delete) = self
            .track_actions
            .lookup_action("delete")
            .and_downcast::<gio::SimpleAction>()
        {
            delete.set_enabled(!track.item.locked());
        }
    }

    pub fn set_selection(&self, item: Option<ItemRef>) {
        self.select_items(item.into_iter().collect());
    }

    /// Selects timeline marker `index` (clearing the clip selection).
    pub fn select_marker(&self, index: Option<usize>) {
        if index.is_some() {
            self.select_items(Vec::new());
        }
        if self.marker.replace(index) != index {
            self.obj().emit_by_name::<()>("selection-changed", &[]);
        }
        self.obj().queue_draw();
    }

    /// Selects `items`; the first is the primary one.
    pub fn select_items(&self, mut items: Vec<ItemRef>) {
        if !items.is_empty() && self.marker.replace(None).is_some() {
            self.obj().emit_by_name::<()>("selection-changed", &[]);
        }
        let mut seen = Vec::new();
        items.retain(|r| {
            let new = !seen.contains(r);
            seen.push(*r);
            new
        });
        let primary = items.first().copied();
        let others: Vec<ItemRef> = items.iter().skip(1).copied().collect();
        let changed = self.selection.replace(primary) != primary || *self.others.borrow() != others;
        self.others.replace(others);
        if changed {
            self.obj().emit_by_name::<()>("selection-changed", &[]);
            match items.len() {
                0 => {}
                1 => self.announce_item(items[0]),
                n => self.obj().announce(
                    &crate::i18n::fill(
                        &crate::i18n::ngettext("{n} clip selected", "{n} clips selected", n as u32),
                        &[("n", &n)],
                    ),
                    gtk::AccessibleAnnouncementPriority::Medium,
                ),
            }
        }
        self.obj().queue_draw();
    }

    /// The selected items, the primary one first.
    pub fn selected_items(&self) -> Vec<ItemRef> {
        self.selection
            .get()
            .into_iter()
            .chain(self.others.borrow().iter().copied())
            .collect()
    }

    pub(super) fn is_selected(&self, item: ItemRef) -> bool {
        self.selection.get() == Some(item) || self.others.borrow().contains(&item)
    }

    pub(super) fn toggle_selected(&self, item: ItemRef) {
        let mut items = self.selected_items();
        if items.contains(&item) {
            items.retain(|&r| r != item);
        } else {
            items.insert(0, item);
        }
        self.select_items(items);
    }

    /// The clips (not gaps or transitions) a rubber band touches.
    pub(super) fn clips_in_band(&self, from: (f64, f64), to: (f64, f64)) -> Vec<ItemRef> {
        let (x0, x1) = (from.0.min(to.0), from.0.max(to.0));
        let (y0, y1) = (from.1.min(to.1), from.1.max(to.1));
        let (f0, f1) = (self.x_to_frame_f64(x0), self.x_to_frame_f64(x1));
        let project = self.project();
        let timeline = project.timeline();
        let mut found = Vec::new();
        for (row, track_index) in rows(&timeline).into_iter().enumerate() {
            let top = f64::from(row_y(row));
            if top + f64::from(TRACK_HEIGHT) < y0 || top > y1 {
                continue;
            }
            let Some(track) = timeline.track(track_index) else {
                continue;
            };
            for (index, (range, child)) in track.child_ranges().enumerate() {
                if !matches!(child, Composable::Clip(_)) {
                    continue;
                }
                let start = self.time_to_frames(range.start);
                let end = self.time_to_frames(range.end());
                if end > f0 && start < f1 {
                    found.push(ItemRef {
                        track: track_index,
                        index,
                    });
                }
            }
        }
        found
    }

    /// Tells screen readers what `item` is, since the selection is only
    /// drawn.
    pub(super) fn announce_item(&self, item: ItemRef) {
        use crate::i18n::{fill, gettext};
        let project = self.project();
        let model = project.timeline();
        let (Ok(child), Ok(range)) = (model.item(item), model.item_range(item)) else {
            return;
        };
        let fps = model.frame_rate();
        let text = match child {
            Composable::Transition(t) => fill(
                &gettext("Dissolve, {length}"),
                &[(
                    "length",
                    &project.timecode((t.in_offset + t.out_offset).frame(fps)),
                )],
            ),
            _ => fill(
                &gettext("{name}, {start} to {end}"),
                &[
                    ("name", &child.name()),
                    ("start", &project.timecode(range.start.frame(fps))),
                    ("end", &project.timecode(range.end().frame(fps))),
                ],
            ),
        };
        drop(model);
        self.obj()
            .announce(&text, gtk::AccessibleAnnouncementPriority::Medium);
    }

    /// Opens the clip menu at the selected clip (Shift+F10, Menu), or
    /// the track menu when focus is in a track header.
    pub(super) fn selection_menu(&self) {
        let focused = self.obj().focus_child();
        let header_row = self
            .headers
            .borrow()
            .iter()
            .position(|h| Some(h.upcast_ref::<gtk::Widget>()) == focused.as_ref());
        if let Some(row) = header_row {
            let y = row_y(row) + TRACK_HEIGHT / 2.0;
            self.context_menu(f64::from(HEADER_WIDTH / 2.0), f64::from(y));
            return;
        }
        let Some(item) = self.selection.get() else {
            return;
        };
        let project = self.project();
        let model = project.timeline();
        let Ok(range) = model.item_range(item) else {
            return;
        };
        let Some(row) = rows(&model).iter().position(|&t| t == item.track) else {
            return;
        };
        drop(model);
        let rect = self.clip_rect(range, row_y(row));
        let menu = self.menu.get().expect("menu");
        menu.set_pointing_to(Some(&gdk::Rectangle::new(
            rect.x() as i32,
            rect.y() as i32,
            rect.width().max(1.0) as i32,
            rect.height() as i32,
        )));
        menu.popup();
    }
}
