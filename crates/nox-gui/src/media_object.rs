//! GObject wrapper around an imported file and its probed `MediaInfo`, so it
//! can live in a `gio::ListStore` and drive list item widgets via bindings.

use crate::i18n::{fill, gettext};
use gtk::{gdk, gio, glib, prelude::*, subclass::prelude::*};
use nox_media::MediaInfo;
use nox_project::timecode;
use std::path::{Path, PathBuf};

/// Display size of thumbnails in the media pool.
pub const THUMBNAIL_WIDTH: u32 = 144;
pub const THUMBNAIL_HEIGHT: u32 = 81;
/// Thumbnails are decoded at twice the display size for HiDPI screens.
const THUMBNAIL_SCALE: u32 = 2;

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::MediaObject)]
    pub struct MediaObject {
        #[property(get, construct_only)]
        pub path: RefCell<String>,
        #[property(get, set, nullable)]
        pub thumbnail: RefCell<Option<gdk::Texture>>,
        /// Shown when there is no thumbnail (loading, audio, errors).
        #[property(get, set)]
        pub icon_name: RefCell<String>,
        /// One line under the name: duration, "Loading…" or the error.
        #[property(get, set)]
        pub summary: RefCell<String>,
        /// Full description for the tooltip.
        #[property(get, set)]
        pub details: RefCell<String>,
        pub info: RefCell<Option<MediaInfo>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MediaObject {
        const NAME: &'static str = "NoxMediaObject";
        type Type = super::MediaObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for MediaObject {}
}

glib::wrapper! {
    pub struct MediaObject(ObjectSubclass<imp::MediaObject>);
}

impl MediaObject {
    /// Creates the object in its loading state and starts probing in the
    /// background.
    pub fn import(path: &Path) -> Self {
        let media: Self = glib::Object::builder()
            .property("path", path.to_string_lossy().as_ref())
            .property("icon-name", "content-loading-symbolic")
            .property("summary", gettext("Loading…"))
            .property("details", path.to_string_lossy().as_ref())
            .build();
        media.load();
        media
    }

    pub fn name(&self) -> String {
        let path = self.path();
        Path::new(&path)
            .file_name()
            .map_or(path.clone(), |n| n.to_string_lossy().into_owned())
    }

    pub fn info(&self) -> Option<MediaInfo> {
        self.imp().info.borrow().clone()
    }

    fn load(&self) {
        let path = PathBuf::from(self.path());
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = media)]
            self,
            async move {
                let result = gio::spawn_blocking(move || {
                    let info = nox_media::probe(&path)?;
                    let thumbnail = info.video.as_ref().and_then(|_| {
                        nox_media::thumbnail(
                            &path,
                            THUMBNAIL_WIDTH * THUMBNAIL_SCALE,
                            THUMBNAIL_HEIGHT * THUMBNAIL_SCALE,
                        )
                        .inspect_err(|err| {
                            tracing::warn!("{}: no thumbnail: {err}", path.display())
                        })
                        .ok()
                    });
                    Ok::<_, nox_media::Error>((info, thumbnail))
                })
                .await;
                match result {
                    Ok(Ok((info, thumbnail))) => media.set_loaded(info, thumbnail),
                    Ok(Err(err)) => media.set_failed(&err.to_string()),
                    Err(_) => media.set_failed(&gettext("probing crashed")),
                }
            }
        ));
    }

    fn set_loaded(&self, info: MediaInfo, thumbnail: Option<nox_media::Image>) {
        tracing::debug!("probed {}:\n{info}", info.path.display());
        let icon = if info.video.is_none() {
            "audio-x-generic-symbolic"
        } else if info.is_still_image() {
            "image-x-generic-symbolic"
        } else {
            "video-x-generic-symbolic"
        };
        let summary = summary(&info);
        let details = format!("{}\n\n{}", info.path.display(), info.to_string().trim_end());
        // Store the info first: property notifications below may trigger
        // listeners that read it.
        self.imp().info.replace(Some(info));
        self.set_icon_name(icon);
        self.set_summary(summary);
        self.set_details(details);
        self.set_thumbnail(thumbnail.map(|image| {
            gdk::MemoryTexture::new(
                image.width as i32,
                image.height as i32,
                gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(image.data),
                image.stride,
            )
            .upcast::<gdk::Texture>()
        }));
    }

    fn set_failed(&self, error: &str) {
        tracing::warn!("cannot import {}: {error}", self.path());
        self.set_icon_name("dialog-error-symbolic");
        self.set_summary(error);
        self.set_details(format!(
            "{}\n\n{}",
            self.path(),
            fill(&gettext("Cannot import: {error}"), &[("error", &error)])
        ));
    }
}

fn summary(info: &MediaInfo) -> String {
    if info.is_still_image() {
        return gettext("Still");
    }
    match (
        info.duration,
        info.video.as_ref().and_then(|v| v.frame_rate),
    ) {
        (Some(duration), Some(fps)) => timecode(duration.rescale(fps.inverse()).ticks, fps),
        (Some(duration), None) => {
            let secs = duration.as_seconds().round() as u64;
            format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
        }
        (None, _) => gettext("Unknown length"),
    }
}
