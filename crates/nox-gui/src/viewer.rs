//! The viewer (`resources/ui/viewer.ui`): the sequence at the playhead,
//! with transport controls.

use gtk::{CompositeTemplate, gdk, glib, prelude::*, subclass::prelude::*};
use nox_engine::{FrameImage, RenderedFrame};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use crate::i18n::gettext;

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/io/github/nox/Nox/ui/viewer.ui")]
    pub struct Viewer {
        #[template_child]
        pub picture: TemplateChild<gtk::Picture>,
        #[template_child]
        pub timecode: TemplateChild<gtk::Label>,
        #[template_child]
        pub play_button: TemplateChild<gtk::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Viewer {
        const NAME: &'static str = "NoxViewer";
        type Type = super::Viewer;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Viewer {}
    impl WidgetImpl for Viewer {}
    impl BoxImpl for Viewer {}
}

glib::wrapper! {
    pub struct Viewer(ObjectSubclass<imp::Viewer>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Viewer {
    pub fn picture(&self) -> gtk::Picture {
        self.imp().picture.get()
    }

    pub fn set_timecode(&self, text: &str) {
        self.imp().timecode.set_text(text);
    }

    /// Switches the play button between play and pause.
    pub fn set_playing(&self, playing: bool) {
        let (icon, tooltip) = if playing {
            ("media-playback-pause-symbolic", gettext("Pause"))
        } else {
            ("media-playback-start-symbolic", gettext("Play"))
        };
        let button = &self.imp().play_button;
        button.set_icon_name(icon);
        button.set_tooltip_text(Some(&tooltip));
    }

    /// Shows a rendered frame; fails if GTK cannot import a DMA-BUF frame.
    /// The frame (shared with the engine's cache) stays alive while GTK
    /// uses it.
    pub fn show_frame(&self, rendered: &Arc<RenderedFrame>) -> Result<(), glib::Error> {
        let imp = self.imp();
        let texture = match &rendered.image {
            FrameImage::Pixels(image) => gdk::MemoryTexture::new(
                image.width as i32,
                image.height as i32,
                gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from(&image.data[..]),
                image.stride,
            )
            .upcast(),
            FrameImage::Dmabuf(frame) => {
                let builder = gdk::DmabufTextureBuilder::new()
                    .set_display(&imp.picture.display())
                    .set_width(frame.width)
                    .set_height(frame.height)
                    .set_fourcc(frame.fourcc)
                    .set_modifier(frame.modifier)
                    .set_n_planes(1)
                    .set_offset(0, frame.offset)
                    .set_stride(0, frame.stride);
                let keep = rendered.clone();
                // SAFETY: the fd stays open until GTK calls the release
                // function, which drops our reference to the frame (the
                // last one closes the fd and returns the image to the
                // compositor's pool).
                unsafe {
                    let builder = builder.set_fd(0, frame.fd.as_raw_fd());
                    builder.build_with_release_func(move || drop(keep))?
                }
            }
        };
        imp.picture.set_paintable(Some(&texture));
        Ok(())
    }

    pub fn clear(&self) {
        self.imp().picture.set_paintable(gdk::Paintable::NONE);
    }
}
