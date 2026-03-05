use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, CompositeTemplate};

use crate::media::decoder::{FfmpegDecoder, VideoDecoder};
use crate::render::backend::{self, RenderBackend, RenderBackendType};

// ---------------------------------------------------------------------------
// Private implementation
// ---------------------------------------------------------------------------
mod imp {
    use super::*;

    #[derive(CompositeTemplate)]
    #[template(resource = "/com/github/nox/ui/preview.ui")]
    pub struct NoxPreview {
        #[template_child]
        pub video_picture: TemplateChild<gtk::Picture>,
        #[template_child]
        pub timecode_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub scrub_bar: TemplateChild<gtk::Scale>,
        #[template_child]
        pub scrub_adjustment: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub play_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub skip_back_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub skip_forward_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub step_back_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub step_forward_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub position_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub duration_label: TemplateChild<gtk::Label>,

        /// Shared render backend (created once, reused for every frame).
        pub render_backend: Rc<dyn RenderBackend>,
        /// Active decoder for the currently loaded media.
        pub decoder: RefCell<Option<FfmpegDecoder>>,
        /// Whether playback is active.
        pub playing: RefCell<bool>,
        /// Playback timer source ID.
        pub playback_source: RefCell<Option<glib::SourceId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NoxPreview {
        const NAME: &'static str = "NoxPreview";
        type Type = super::NoxPreview;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for NoxPreview {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.connect_transport_controls();
        }
    }

    impl Default for NoxPreview {
        fn default() -> Self {
            Self {
                video_picture: Default::default(),
                timecode_label: Default::default(),
                scrub_bar: Default::default(),
                scrub_adjustment: Default::default(),
                play_button: Default::default(),
                skip_back_button: Default::default(),
                skip_forward_button: Default::default(),
                step_back_button: Default::default(),
                step_forward_button: Default::default(),
                position_label: Default::default(),
                duration_label: Default::default(),
                render_backend: backend::create_backend(RenderBackendType::Software),
                decoder: Default::default(),
                playing: Default::default(),
                playback_source: Default::default(),
            }
        }
    }

    impl WidgetImpl for NoxPreview {}
    impl BoxImpl for NoxPreview {}
}

// ---------------------------------------------------------------------------
// Public wrapper type
// ---------------------------------------------------------------------------
glib::wrapper! {
    pub struct NoxPreview(ObjectSubclass<imp::NoxPreview>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Buildable;
}

impl NoxPreview {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Load a media file and show its first frame.
    pub fn load_media(&self, path: &Path) {
        let imp = self.imp();

        match FfmpegDecoder::new(path) {
            Ok(mut decoder) => {
                let info = decoder.stream_info().clone();
                imp.duration_label
                    .set_label(&format_timecode(info.duration_secs));
                imp.scrub_adjustment.set_upper(info.duration_secs);
                imp.scrub_adjustment.set_value(0.0);

                // Decode and display the first frame
                if let Ok(Some(frame)) = decoder.decode_next_frame() {
                    if let Ok(texture) = imp.render_backend.upload_frame(&frame) {
                        imp.video_picture.set_paintable(Some(&texture));
                    }
                    imp.timecode_label
                        .set_label(&format_timecode(frame.timestamp_secs));
                    imp.position_label
                        .set_label(&format_timecode(frame.timestamp_secs));
                }

                *imp.decoder.borrow_mut() = Some(decoder);
            }
            Err(e) => {
                log::error!("Failed to open media: {e}");
            }
        }
    }

    /// Display a specific frame (e.g. scrubbing or playback tick).
    pub fn show_frame_at(&self, timestamp_secs: f64) {
        let imp = self.imp();
        let mut decoder = imp.decoder.borrow_mut();
        if let Some(ref mut dec) = *decoder {
            if let Ok(frame) = dec.decode_frame_at(timestamp_secs) {
                if let Ok(texture) = imp.render_backend.upload_frame(&frame) {
                    imp.video_picture.set_paintable(Some(&texture));
                }
                imp.timecode_label
                    .set_label(&format_timecode(frame.timestamp_secs));
                imp.position_label
                    .set_label(&format_timecode(frame.timestamp_secs));
            }
        }
    }

    // -- Transport controls -------------------------------------------------

    fn connect_transport_controls(&self) {
        let imp = self.imp();

        // Play / Pause
        let obj = self.clone();
        imp.play_button.connect_clicked(move |btn| {
            let imp = obj.imp();
            let is_playing = *imp.playing.borrow();
            if is_playing {
                obj.pause();
                btn.set_icon_name("media-playback-start-symbolic");
            } else {
                obj.play();
                btn.set_icon_name("media-playback-pause-symbolic");
            }
        });

        // Skip to start
        let obj = self.clone();
        imp.skip_back_button.connect_clicked(move |_| {
            let imp = obj.imp();
            imp.scrub_adjustment.set_value(0.0);
        });

        // Skip to end
        let obj = self.clone();
        imp.skip_forward_button.connect_clicked(move |_| {
            let imp = obj.imp();
            let end = imp.scrub_adjustment.upper();
            imp.scrub_adjustment.set_value(end);
        });

        // Step backward (~1 frame at 30fps)
        let obj = self.clone();
        imp.step_back_button.connect_clicked(move |_| {
            let imp = obj.imp();
            let v = imp.scrub_adjustment.value();
            imp.scrub_adjustment.set_value((v - 1.0 / 30.0).max(0.0));
        });

        // Step forward (~1 frame at 30fps)
        let obj = self.clone();
        imp.step_forward_button.connect_clicked(move |_| {
            let imp = obj.imp();
            let v = imp.scrub_adjustment.value();
            let max = imp.scrub_adjustment.upper();
            imp.scrub_adjustment.set_value((v + 1.0 / 30.0).min(max));
        });

        // Scrub bar value-changed: seek preview
        // (We don't do live scrub to avoid flooding the decoder; only on
        //  mouse-release in a real implementation.)
    }

    fn play(&self) {
        let imp = self.imp();
        *imp.playing.borrow_mut() = true;

        let obj = self.clone();
        let source = glib::timeout_add_local(
            std::time::Duration::from_millis(33), // ~30 fps
            move || {
                let imp = obj.imp();
                if !*imp.playing.borrow() {
                    return glib::ControlFlow::Break;
                }

                let mut decoder = imp.decoder.borrow_mut();
                if let Some(ref mut dec) = *decoder {
                    match dec.decode_next_frame() {
                        Ok(Some(frame)) => {
                            if let Ok(texture) = imp.render_backend.upload_frame(&frame) {
                                imp.video_picture.set_paintable(Some(&texture));
                            }
                            imp.timecode_label
                                .set_label(&format_timecode(frame.timestamp_secs));
                            imp.position_label
                                .set_label(&format_timecode(frame.timestamp_secs));
                            imp.scrub_adjustment.set_value(frame.timestamp_secs);
                        }
                        _ => {
                            *imp.playing.borrow_mut() = false;
                            imp.play_button
                                .set_icon_name("media-playback-start-symbolic");
                            return glib::ControlFlow::Break;
                        }
                    }
                }
                glib::ControlFlow::Continue
            },
        );

        *imp.playback_source.borrow_mut() = Some(source);
    }

    fn pause(&self) {
        let imp = self.imp();
        *imp.playing.borrow_mut() = false;
        if let Some(source) = imp.playback_source.borrow_mut().take() {
            source.remove();
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_timecode(secs: f64) -> String {
    let total = secs.max(0.0);
    let h = (total / 3600.0) as u32;
    let m = ((total % 3600.0) / 60.0) as u32;
    let s = (total % 60.0) as u32;
    let ms = ((total % 1.0) * 1000.0) as u32;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}
