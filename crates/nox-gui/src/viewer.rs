//! Preview of the sequence at the playhead, with transport controls.

use gtk::{gdk, glib, prelude::*};
use nox_engine::FrameImage;
use std::os::fd::AsRawFd;

pub struct Viewer {
    pub widget: gtk::Box,
    picture: gtk::Picture,
    pub timecode: gtk::Label,
}

impl Viewer {
    pub fn new() -> Self {
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .css_classes(["viewer-canvas"])
            .build();

        let transport = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .spacing(2)
            .hexpand(true)
            .build();
        // Buttons without an implemented action (play, go to end) are
        // insensitive until the action exists.
        for (icon, tooltip, action) in [
            (
                "media-skip-backward-symbolic",
                "Go to Start (Home)",
                "win.go-to-start",
            ),
            (
                "media-seek-backward-symbolic",
                "Previous Frame (←)",
                "win.previous-frame",
            ),
            ("media-playback-start-symbolic", "Play", "win.play"),
            (
                "media-seek-forward-symbolic",
                "Next Frame (→)",
                "win.next-frame",
            ),
            ("media-skip-forward-symbolic", "Go to End", "win.go-to-end"),
        ] {
            transport.append(
                &gtk::Button::builder()
                    .icon_name(icon)
                    .tooltip_text(tooltip)
                    .action_name(action)
                    .css_classes(["flat"])
                    .build(),
            );
        }

        let timecode = gtk::Label::builder()
            .label("00:00:00:00")
            .css_classes(["timecode"])
            .build();

        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        // Mirror the timecode's width on the left so the transport stays centered.
        let spacer = gtk::Label::builder()
            .label("00:00:00:00")
            .css_classes(["timecode"])
            .opacity(0.0)
            .build();
        controls.append(&spacer);
        controls.append(&transport);
        controls.append(&timecode);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&picture);
        widget.append(&controls);

        Self {
            widget,
            picture,
            timecode,
        }
    }

    /// Shows a frame; fails if GTK cannot import a DMA-BUF frame.
    pub fn show_frame(&self, image: FrameImage) -> Result<(), glib::Error> {
        let texture = match image {
            FrameImage::Pixels(image) => gdk::MemoryTexture::new(
                image.width as i32,
                image.height as i32,
                gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(image.data),
                image.stride,
            )
            .upcast(),
            FrameImage::Dmabuf(frame) => {
                let builder = gdk::DmabufTextureBuilder::new()
                    .set_display(&self.picture.display())
                    .set_width(frame.width)
                    .set_height(frame.height)
                    .set_fourcc(frame.fourcc)
                    .set_modifier(frame.modifier)
                    .set_n_planes(1)
                    .set_offset(0, frame.offset)
                    .set_stride(0, frame.stride);
                // SAFETY: the fd stays open until GTK calls the release
                // function, which drops the frame (closing the fd and
                // returning the image to the converter's pool).
                unsafe {
                    let builder = builder.set_fd(0, frame.fd.as_raw_fd());
                    builder.build_with_release_func(move || drop(frame))?
                }
            }
        };
        self.picture.set_paintable(Some(&texture));
        Ok(())
    }

    pub fn clear(&self) {
        self.picture.set_paintable(gdk::Paintable::NONE);
    }
}
