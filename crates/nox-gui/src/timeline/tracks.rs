//! Track operations: enable, lock, solo, sync lock, rename and delete.

use super::*;

impl TimelineView {
    /// Mutes (audio) or hides (video) track `index`, or turns it back on:
    /// OTIO's `enabled`, which playback and export respect.
    pub fn set_track_enabled(&self, index: usize, enabled: bool) {
        let project = self.imp().project().clone();
        let Some(kind) = project.timeline().track(index).map(|t| t.kind.clone()) else {
            return;
        };
        project.edit_track(index, &enable_label(&kind, enabled), |track| {
            track.item.enabled = enabled;
        });
    }

    /// Locks or unlocks track `index`; locked tracks reject edits.
    pub fn set_track_locked(&self, index: usize, locked: bool) {
        let project = self.imp().project().clone();
        project.edit_track(index, &lock_label(locked), |track| {
            track.item.set_locked(locked);
        });
    }

    /// Applies a header toggle to track `index`.
    pub(super) fn set_track_toggle(&self, index: usize, toggle: Toggle, on: bool) {
        match toggle {
            Toggle::Off => self.set_track_enabled(index, !on),
            Toggle::Solo => self.set_track_solo(index, on),
            Toggle::Lock => self.set_track_locked(index, on),
            Toggle::Sync => self.set_track_sync_locked(index, on),
        }
    }

    /// Solos audio track `index`, or not: while any track is soloed, only
    /// soloed tracks are heard.
    pub fn set_track_solo(&self, index: usize, on: bool) {
        let project = self.imp().project().clone();
        let label = if on {
            crate::i18n::gettext("Solo Track")
        } else {
            crate::i18n::gettext("Unsolo Track")
        };
        project.edit_track(index, &label, |track| {
            track.item.set_solo(on);
        });
    }

    /// Makes track `index` follow ripple edits on other tracks, or not.
    pub fn set_track_sync_locked(&self, index: usize, on: bool) {
        let project = self.imp().project().clone();
        let label = if on {
            crate::i18n::gettext("Sync Lock Track")
        } else {
            crate::i18n::gettext("Remove Sync Lock")
        };
        project.edit_track(index, &label, |track| {
            track.item.set_sync_locked(on);
        });
    }

    pub fn rename_track(&self, index: usize, name: &str) {
        let project = self.imp().project().clone();
        project.edit_track(index, &crate::i18n::gettext("Rename Track"), |track| {
            track.item.name = name.to_owned();
        });
    }

    /// Deletes track `index` with its clips (not if it is locked).
    pub fn delete_track(&self, index: usize) {
        let project = self.imp().project().clone();
        if project
            .edit(&crate::i18n::gettext("Delete Track"), |t| {
                t.remove_track(index)
            })
            .is_ok()
        {
            // Selections are by track index, which has changed.
            self.set_selection(None);
        }
    }

    /// Asks for a new name for track `index`.
    pub fn show_rename_dialog(&self, index: usize) {
        let project = self.imp().project().clone();
        let Some(name) = project.timeline().track(index).map(|t| t.item.name.clone()) else {
            return;
        };
        let builder = rename_dialog();
        let dialog: gtk::Window = builder.object("dialog").expect("dialog");
        let entry: gtk::Entry = builder.object("name").expect("name");
        let cancel: gtk::Button = builder.object("cancel").expect("cancel");
        let rename: gtk::Button = builder.object("rename").expect("rename");
        dialog.set_transient_for(self.root().and_downcast_ref::<gtk::Window>());
        entry.set_text(&name);
        cancel.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            move |_| dialog.close()
        ));
        rename.connect_clicked(glib::clone!(
            #[weak(rename_to = view)]
            self,
            #[weak]
            dialog,
            #[weak]
            entry,
            move |_| {
                view.rename_track(index, entry.text().trim());
                dialog.close();
            }
        ));
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::NamedAction::new("window.close")),
        ));
        dialog.add_controller(shortcuts);
        dialog.present();
    }
}

/// The rename dialog for tracks (`resources/ui/rename-track.ui`).
pub(crate) fn rename_dialog() -> gtk::Builder {
    let builder = gtk::Builder::new();
    builder.set_translation_domain(Some(crate::i18n::DOMAIN));
    builder
        .add_from_resource("/io/github/feliwir/Nox/ui/rename-track.ui")
        .expect("rename-track.ui is valid");
    builder
}
