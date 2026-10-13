//! The export dialog (`resources/ui/export-dialog.ui`): where to, in which
//! format and quality, the whole timeline or the In to Out range, then a
//! progress bar while the timeline renders on its own thread.

use gtk::{CompositeTemplate, gio, glib, prelude::*, subclass::prelude::*};
use nox_engine::{EncodeQuality, Export, ExportEvent, ExportSettings, VideoCodec};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::i18n::{fill, gettext, n_, ngettext};
use crate::project::Project;

/// Formats offered: label and codec (the container follows the codec).
const FORMATS: &[(&str, VideoCodec)] = &[
    ("MP4 – H.264", VideoCodec::H264),
    ("MP4 – HEVC (H.265)", VideoCodec::Hevc),
    ("QuickTime – ProRes 422", VideoCodec::ProRes),
];
const QUALITIES: &[(&str, EncodeQuality)] = &[
    (n_("High"), EncodeQuality::High),
    (n_("Medium"), EncodeQuality::Medium),
    (n_("Low (small files)"), EncodeQuality::Low),
];

/// Presets, as other editors lead with (Resolve's Deliver page, Shotcut's
/// preset list): name, format index, quality index, hardware. "Custom" is
/// selected when the settings below are changed by hand.
const PRESETS: &[(&str, usize, usize, bool)] = &[
    (n_("H.264 Master"), 0, 0, true),
    (n_("YouTube / Web (H.264)"), 0, 1, true),
    (n_("HEVC (smaller files)"), 1, 0, true),
    (n_("ProRes 422 HQ (for editing)"), 2, 0, false),
    (n_("Review copy (small)"), 0, 2, true),
    (n_("Custom"), usize::MAX, usize::MAX, true),
];

fn string_list<'a>(names: impl Iterator<Item = &'a str>, translate: bool) -> gtk::StringList {
    let names: Vec<String> = names
        .map(|n| if translate { gettext(n) } else { n.to_owned() })
        .collect();
    gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>())
}

mod imp {
    use super::*;
    use std::cell::{Cell, OnceCell, RefCell};

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/nox/Nox/ui/export-dialog.ui")]
    pub struct ExportDialog {
        #[template_child]
        pub settings: TemplateChild<gtk::Grid>,
        #[template_child]
        pub preset: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub path: TemplateChild<gtk::Entry>,
        #[template_child]
        pub format: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub quality: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub hardware: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub audio: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub range: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub summary: TemplateChild<gtk::Label>,
        #[template_child]
        pub progress: TemplateChild<gtk::ProgressBar>,
        #[template_child]
        pub status: TemplateChild<gtk::Label>,
        #[template_child]
        pub open_folder: TemplateChild<gtk::Button>,
        #[template_child]
        pub cancel: TemplateChild<gtk::Button>,
        #[template_child]
        pub start: TemplateChild<gtk::Button>,
        /// A snapshot of the timeline taken when the dialog opened.
        pub timeline: OnceCell<Arc<nox_project::Timeline>>,
        /// The frames to export: the whole timeline, and In to Out if both
        /// marks are set.
        pub ranges: OnceCell<(Range<i64>, Option<Range<i64>>)>,
        /// The running export, if any; dropping it cancels.
        pub running: RefCell<Option<Export>>,
        /// Set while a preset fills in the settings.
        pub applying: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ExportDialog {
        const NAME: &'static str = "NoxExportDialog";
        type Type = super::ExportDialog;
        type ParentType = gtk::Window;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
            klass.add_binding_action(
                gtk::gdk::Key::Escape,
                gtk::gdk::ModifierType::empty(),
                "window.close",
            );
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ExportDialog {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup();
        }
    }

    impl WidgetImpl for ExportDialog {}

    impl WindowImpl for ExportDialog {
        fn close_request(&self) -> glib::Propagation {
            // Closing (or Escape) first cancels a running export and keeps
            // the dialog, so its outcome stays visible; closing again
            // closes it.
            if let Some(export) = self.running.borrow().as_ref() {
                export.cancel();
                return glib::Propagation::Stop;
            }
            self.parent_close_request()
        }
    }
}

glib::wrapper! {
    pub struct ExportDialog(ObjectSubclass<imp::ExportDialog>)
        @extends gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
            gtk::Root, gtk::ShortcutManager;
}

/// Opens the dialog for `project` over `parent`.
pub fn show(parent: &gtk::Window, project: &Rc<Project>) {
    let dialog: ExportDialog = glib::Object::builder()
        .property("transient-for", parent)
        .build();
    dialog.load(project);
    dialog.present();
}

#[gtk::template_callbacks]
impl ExportDialog {
    /// Fills the choices and wires presets ↔ settings.
    fn setup(&self) {
        let imp = self.imp();
        imp.preset
            .set_model(Some(&string_list(PRESETS.iter().map(|p| p.0), true)));
        // Format names are technical and stay as they are.
        imp.format
            .set_model(Some(&string_list(FORMATS.iter().map(|f| f.0), false)));
        imp.quality
            .set_model(Some(&string_list(QUALITIES.iter().map(|q| q.0), true)));

        // Keep the extension in step with the format.
        imp.format.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |format| {
                let codec = FORMATS[format.selected() as usize].1;
                let entry = &dialog.imp().path;
                let mut path = PathBuf::from(entry.text().as_str());
                path.set_extension(codec.extension());
                entry.set_text(&path.to_string_lossy());
                dialog.to_custom();
            }
        ));
        imp.quality.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.to_custom()
        ));
        imp.hardware.connect_toggled(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.to_custom()
        ));
        // A preset fills in the settings; changing them by hand makes it Custom.
        imp.preset.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |preset| {
                let (_, f, q, hw) = PRESETS[preset.selected() as usize];
                if f == usize::MAX {
                    return;
                }
                let imp = dialog.imp();
                imp.applying.set(true);
                imp.format.set_selected(f as u32);
                imp.quality.set_selected(q as u32);
                imp.hardware.set_active(hw);
                imp.applying.set(false);
            }
        ));
    }

    fn to_custom(&self) {
        let imp = self.imp();
        if !imp.applying.get() {
            imp.preset.set_selected((PRESETS.len() - 1) as u32);
        }
    }

    /// Takes the timeline and suggests a file next to the project. The
    /// range is In to Out when both marks are set (as on Resolve's Deliver
    /// page), else the whole timeline.
    pub(crate) fn load(&self, project: &Rc<Project>) {
        let imp = self.imp();
        let timeline = Arc::new(project.timeline().clone());
        let fps = timeline.frame_rate();
        let drop_frame = timeline.settings.drop_frame;
        let full = nox_engine::full_range(&timeline);
        let marked = nox_engine::marked_range(&timeline);
        let mut choices = vec![gettext("Entire timeline")];
        if let Some(marked) = &marked {
            let timecode = |frame| nox_project::format_timecode(frame, fps, drop_frame);
            choices.push(fill(
                &gettext("In to Out ({start} – {end})"),
                &[
                    ("start", &timecode(marked.start)),
                    ("end", &timecode(marked.end - 1)),
                ],
            ));
        }
        imp.range.set_model(Some(&string_list(
            choices.iter().map(String::as_str),
            false,
        )));
        imp.range.set_sensitive(marked.is_some());
        imp.range.set_selected(if marked.is_some() { 1 } else { 0 });
        imp.ranges.set((full, marked)).expect("loaded once");
        imp.range.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.update_summary()
        ));
        let dir = project
            .path()
            .and_then(|p| p.parent().map(PathBuf::from))
            .or_else(|| glib::user_special_dir(glib::UserDirectory::Videos))
            .unwrap_or_else(glib::home_dir);
        let file_name = project
            .path()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| timeline.name.clone());
        imp.path
            .set_text(&dir.join(format!("{file_name}.mp4")).to_string_lossy());
        imp.timeline.set(timeline).expect("loaded once");
        self.update_summary();
    }

    /// The frames the chosen range covers.
    fn range(&self) -> Range<i64> {
        let imp = self.imp();
        let (full, marked) = imp.ranges.get().expect("loaded");
        match marked {
            Some(marked) if imp.range.selected() == 1 => marked.clone(),
            _ => full.clone(),
        }
    }

    /// Describes what will be rendered: size, rate and length of the range.
    fn update_summary(&self) {
        let imp = self.imp();
        let Some(timeline) = imp.timeline.get() else {
            return;
        };
        let fps = timeline.frame_rate();
        let range = self.range();
        let frames = range.end - range.start;
        imp.summary.set_text(&fill(
            &ngettext(
                "{width} × {height}, {fps} fps, {frames} frame ({timecode})",
                "{width} × {height}, {fps} fps, {frames} frames ({timecode})",
                frames.clamp(0, u32::MAX.into()) as u32,
            ),
            &[
                ("width", &timeline.settings.width),
                ("height", &timeline.settings.height),
                ("fps", &format!("{:.3}", fps.as_f64())),
                ("frames", &frames),
                ("timecode", &nox_project::timecode(frames, fps)),
            ],
        ));
    }

    #[template_callback]
    fn on_browse_clicked(&self) {
        let current = PathBuf::from(self.imp().path.text().as_str());
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Export To"))
            .modal(true)
            .initial_name(
                current
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
            .build();
        if let Some(dir) = current.parent().filter(|d| d.is_dir()) {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let entry = self.imp().path.get();
        dialog.save(Some(self), gio::Cancellable::NONE, move |result| {
            if let Ok(Some(path)) = result.map(|f| f.path()) {
                entry.set_text(&path.to_string_lossy());
            }
        });
    }

    #[template_callback]
    fn on_open_folder_clicked(&self) {
        let file = gio::File::for_path(self.imp().path.text().as_str());
        gtk::FileLauncher::new(Some(&file)).open_containing_folder(
            Some(self),
            gio::Cancellable::NONE,
            |result| {
                if let Err(err) = result {
                    tracing::warn!("cannot open the folder: {err}");
                }
            },
        );
    }

    #[template_callback]
    fn on_cancel_clicked(&self) {
        match self.imp().running.borrow().as_ref() {
            Some(export) => export.cancel(),
            None => self.close(),
        }
    }

    /// Locks or unlocks the settings while an export runs.
    fn set_running(&self, running: bool) {
        let imp = self.imp();
        imp.settings.set_sensitive(!running);
        imp.start.set_sensitive(!running);
    }

    #[template_callback]
    fn on_export_clicked(&self) {
        let imp = self.imp();
        let Some(timeline) = imp.timeline.get().cloned() else {
            return;
        };
        imp.open_folder.set_visible(false);
        let path = PathBuf::from(imp.path.text().as_str());
        if path.file_name().is_none() {
            imp.status.set_text(&gettext("Choose a file to export to."));
            imp.status.set_visible(true);
            return;
        }
        let settings = ExportSettings {
            path,
            codec: FORMATS[imp.format.selected() as usize].1,
            quality: QUALITIES[imp.quality.selected() as usize].1,
            hardware: imp.hardware.is_active(),
            range: Some(self.range()),
            audio: imp.audio.is_active(),
        };
        self.set_running(true);
        imp.progress.set_visible(true);
        imp.progress.set_fraction(0.0);
        imp.progress.set_text(Some(&gettext("Starting…")));
        imp.status.set_visible(false);
        let (sender, receiver) = async_channel::unbounded();
        let export = Export::start(timeline, settings, move |event| {
            let _ = sender.send_blocking(event);
        });
        imp.running.replace(Some(export));
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            async move {
                while let Ok(event) = receiver.recv().await {
                    if dialog.on_event(event) {
                        break;
                    }
                }
            }
        ));
    }

    /// Shows an export event; true once it has finished.
    fn on_event(&self, event: ExportEvent) -> bool {
        let imp = self.imp();
        match event {
            ExportEvent::Progress { done, total } => {
                imp.progress.set_fraction(done as f64 / total as f64);
                imp.progress.set_text(Some(&fill(
                    &gettext("Frame {done} of {total}"),
                    &[("done", &done), ("total", &total)],
                )));
                false
            }
            ExportEvent::Finished(result) => {
                imp.running.replace(None);
                self.set_running(false);
                imp.status.set_visible(true);
                match result {
                    Ok(summary) => {
                        imp.progress.set_fraction(1.0);
                        imp.progress.set_text(Some(&gettext("Done")));
                        let encoder = if summary.zero_copy {
                            fill(
                                &gettext("{encoder}, zero copy"),
                                &[("encoder", &summary.encoder)],
                            )
                        } else {
                            summary.encoder.to_owned()
                        };
                        imp.status.set_text(&fill(
                            &ngettext(
                                "Exported {frames} frame to {path} in {seconds} s ({fps} fps, {encoder}).",
                                "Exported {frames} frames to {path} in {seconds} s ({fps} fps, {encoder}).",
                                summary.frames.clamp(0, u32::MAX.into()) as u32,
                            ),
                            &[
                                ("frames", &summary.frames),
                                ("path", &summary.path.display()),
                                ("seconds", &format!("{:.1}", summary.elapsed.as_secs_f64())),
                                ("fps", &format!("{:.0}", summary.fps())),
                                ("encoder", &encoder),
                            ],
                        ));
                        imp.cancel.set_label(&gettext("_Close"));
                        imp.open_folder.set_visible(true);
                        imp.open_folder.grab_focus();
                    }
                    Err(err) => {
                        imp.progress.set_visible(false);
                        imp.status.set_text(&fill(
                            &gettext("Export failed: {error}"),
                            &[("error", &err)],
                        ));
                        imp.cancel.grab_focus();
                    }
                }
                // The status label appears silently; tell screen readers.
                self.announce(
                    &imp.status.text(),
                    gtk::AccessibleAnnouncementPriority::High,
                );
                true
            }
        }
    }
}
