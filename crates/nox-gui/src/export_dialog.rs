//! The export dialog (`resources/ui/export-dialog.ui`): where to, in which
//! format and quality, the encoder, the whole timeline or the In to Out
//! range, then a progress bar while the timeline renders on its own thread.
//!
//! The encoder list offers only encoders that work on this system for the
//! chosen format and dynamic range (`nox_engine::available_encoders`, probed
//! on a thread when the dialog first opens), each with what it's good at.

use gtk::{CompositeTemplate, gio, glib, prelude::*, subclass::prelude::*};
use nox_engine::{EncodeQuality, EncoderProbe, Export, ExportEvent, ExportSettings, VideoCodec};
use nox_media::OutputEncoding;
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

/// Dynamic ranges offered.
const RANGES: &[(&str, OutputEncoding)] = &[
    (n_("SDR (Rec. 709)"), OutputEncoding::Sdr),
    (n_("HDR10 (PQ, Rec. 2020)"), OutputEncoding::Pq),
    (n_("HLG (Rec. 2020)"), OutputEncoding::Hlg),
];

/// Presets, as other editors lead with (Resolve's Deliver page, Shotcut's
/// preset list): name, format index, quality index, dynamic range index.
/// Presets pick the encoder automatically. "Custom" is selected when the
/// settings below are changed by hand.
const PRESETS: &[(&str, usize, usize, usize)] = &[
    (n_("H.264 Master"), 0, 0, 0),
    (n_("YouTube / Web (H.264)"), 0, 1, 0),
    (n_("HEVC (smaller files)"), 1, 0, 0),
    (n_("HDR10 Master (HEVC)"), 1, 0, 1),
    (n_("HLG for broadcast and phones (HEVC)"), 1, 0, 2),
    (n_("ProRes 422 HQ (for editing)"), 2, 0, 0),
    (n_("Review copy (small)"), 0, 2, 0),
    (n_("Custom"), usize::MAX, usize::MAX, 0),
];

/// How the encoder list names an FFmpeg encoder, and what it's good at.
/// Product names stay as they are.
fn encoder_text(name: &str) -> (&'static str, &'static str) {
    match name {
        "h264_vulkan" | "hevc_vulkan" => (
            "Vulkan Video (GPU)",
            n_(
                "Encodes on the graphics card's video engine, and the frames never leave the card: the fastest choice, on AMD, Intel and NVIDIA cards. Files are somewhat larger than from a software encoder at the same quality.",
            ),
        ),
        "h264_nvenc" | "hevc_nvenc" => (
            "NVENC (NVIDIA GPU)",
            n_(
                "NVIDIA's video engine: very fast, with better quality for the size than most hardware encoders. NVIDIA cards only.",
            ),
        ),
        "libx264" => (
            "x264 (CPU)",
            n_(
                "Encodes on the processor: the best quality for the size and the most compatible files, but much slower than hardware, especially at 4K.",
            ),
        ),
        "libx265" => (
            "x265 (CPU)",
            n_(
                "Encodes on the processor: the smallest files at a given quality, but the slowest choice. Good when size matters more than time.",
            ),
        ),
        "prores_ks" => (
            "FFmpeg ProRes (CPU)",
            n_(
                "Encodes on the processor: large files that decode easily, made for editing and grading in other applications rather than for watching.",
            ),
        ),
        _ => ("", ""),
    }
}

/// Index of H.264 in `FORMATS` (SDR only) and of HEVC (HDR's default).
const H264: u32 = 0;
const HEVC: u32 = 1;

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
        pub dynamic_range: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub peak: TemplateChild<gtk::SpinButton>,
        #[template_child]
        pub peak_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub peak_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub quality: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub encoder: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub encoder_info: TemplateChild<gtk::Label>,
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
        /// Set while the encoder list is refilled.
        pub listing: Cell<bool>,
        /// The encoders listed after "Automatic"; `None` while they're
        /// being probed.
        pub encoders: RefCell<Option<Vec<EncoderProbe>>>,
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
        imp.dynamic_range
            .set_model(Some(&string_list(RANGES.iter().map(|r| r.0), true)));
        self.show_peak();
        // HDR needs HEVC or ProRes: choosing HDR leaves H.264, and choosing
        // H.264 goes back to SDR.
        imp.dynamic_range.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |range| {
                let imp = dialog.imp();
                if range.selected() != 0 && imp.format.selected() == H264 {
                    imp.format.set_selected(HEVC);
                }
                dialog.show_peak();
                dialog.list_encoders();
                dialog.update_summary();
                dialog.to_custom();
            }
        ));
        imp.peak.connect_value_changed(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.to_custom()
        ));

        // Keep the extension in step with the format.
        imp.format.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |format| {
                let codec = FORMATS[format.selected() as usize].1;
                let imp = dialog.imp();
                if format.selected() == H264 && imp.dynamic_range.selected() != 0 {
                    imp.dynamic_range.set_selected(0);
                }
                let entry = &imp.path;
                let mut path = PathBuf::from(entry.text().as_str());
                path.set_extension(codec.extension());
                entry.set_text(&path.to_string_lossy());
                dialog.list_encoders();
                dialog.to_custom();
            }
        ));
        imp.quality.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.to_custom()
        ));
        imp.encoder.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| {
                dialog.explain_encoder();
                // Following a format change isn't a choice.
                if !dialog.imp().listing.get() {
                    dialog.to_custom();
                }
            }
        ));
        self.list_encoders();
        // Probing opens every encoder once, which takes a moment the first
        // time.
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            async move {
                let _ = gio::spawn_blocking(|| {
                    nox_engine::available_encoders(VideoCodec::H264, OutputEncoding::Sdr)
                })
                .await;
                dialog.imp().encoders.replace(Some(Vec::new()));
                dialog.list_encoders();
            }
        ));
        // A preset fills in the settings; changing them by hand makes it Custom.
        imp.preset.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |preset| {
                let (_, f, q, r) = PRESETS[preset.selected() as usize];
                if f == usize::MAX {
                    return;
                }
                let imp = dialog.imp();
                imp.applying.set(true);
                // The range first: an HDR range moves the format off H.264.
                imp.dynamic_range.set_selected(r as u32);
                imp.format.set_selected(f as u32);
                imp.quality.set_selected(q as u32);
                imp.encoder.set_selected(0);
                imp.applying.set(false);
            }
        ));
    }

    fn codec(&self) -> VideoCodec {
        FORMATS[self.imp().format.selected() as usize].1
    }

    fn encoding(&self) -> OutputEncoding {
        RANGES[self.imp().dynamic_range.selected() as usize].1
    }

    /// Lists the encoders that work for the format and dynamic range,
    /// keeping the chosen one if it's still there.
    fn list_encoders(&self) {
        let imp = self.imp();
        let probed = imp.encoders.borrow().is_some();
        let previous = self.chosen_encoder().map(|e| e.name);
        let found = if probed {
            nox_engine::available_encoders(self.codec(), self.encoding())
        } else {
            Vec::new()
        };
        let mut names = vec![gettext("Automatic")];
        names.extend(found.iter().map(|e| encoder_text(e.name).0.to_owned()));
        let selected = previous
            .and_then(|name| found.iter().position(|e| e.name == name))
            .map_or(0, |i| i as u32 + 1);
        imp.listing.set(true);
        imp.encoders.replace(probed.then_some(found));
        imp.encoder
            .set_model(Some(&string_list(names.iter().map(String::as_str), false)));
        imp.encoder.set_selected(selected);
        imp.listing.set(false);
        self.explain_encoder();
    }

    /// The encoder chosen in the list; `None` for Automatic.
    fn chosen_encoder(&self) -> Option<EncoderProbe> {
        let imp = self.imp();
        let index = imp.encoder.selected().checked_sub(1)? as usize;
        imp.encoders.borrow().as_ref()?.get(index).cloned()
    }

    /// Says what the chosen encoder is good at.
    fn explain_encoder(&self) {
        let imp = self.imp();
        let text = match (imp.encoders.borrow().as_ref(), self.chosen_encoder()) {
            (None, _) => gettext("Checking which encoders work on this computer…"),
            (Some(found), None) => match found.first() {
                Some(best) => fill(
                    &gettext("Uses the fastest encoder that works, now {encoder}."),
                    &[("encoder", &encoder_text(best.name).0)],
                ),
                None => gettext("No encoder for this format works on this computer."),
            },
            (Some(_), Some(chosen)) => {
                let mut text = gettext(encoder_text(chosen.name).1);
                if chosen.hardware && chosen.gpu_frames.is_none() {
                    text += " ";
                    text +=
                        &gettext("Frames go through system memory on the way, which is slower.");
                }
                text
            }
        };
        imp.encoder_info.set_text(&text);
    }

    /// The mastering peak matters for HDR10 only.
    fn show_peak(&self) {
        let imp = self.imp();
        let pq = RANGES[imp.dynamic_range.selected() as usize].1 == OutputEncoding::Pq;
        imp.peak_label.set_visible(pq);
        imp.peak_box.set_visible(pq);
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
        // Nothing to hear: no audio track at all, rather than a silent one.
        let audible = nox_engine::has_audio(timeline, &range);
        imp.audio.set_sensitive(audible);
        imp.audio.set_tooltip_text(
            (!audible)
                .then(|| gettext("The timeline has no audio in this range"))
                .as_deref(),
        );
        let mut summary = fill(
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
        );
        let encoding = RANGES[imp.dynamic_range.selected() as usize].1;
        if encoding.is_hdr() {
            summary += &format!(
                ", {}",
                gettext(RANGES[imp.dynamic_range.selected() as usize].0)
            );
        }
        imp.summary.set_text(&summary);
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
            hardware: true,
            encoder: self.chosen_encoder().map(|e| e.name.to_owned()),
            range: Some(self.range()),
            audio: imp.audio.is_active(),
            encoding: RANGES[imp.dynamic_range.selected() as usize].1,
            peak_nits: imp.peak.value() as f32,
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
