//! Small GTK helpers shared by the dialogs and views.

use gtk::{gdk, glib, prelude::*};

/// A list model of `items`, e.g. for a drop-down.
pub fn string_list<S: AsRef<str>>(items: impl IntoIterator<Item = S>) -> gtk::StringList {
    let items: Vec<S> = items.into_iter().collect();
    gtk::StringList::new(&items.iter().map(AsRef::as_ref).collect::<Vec<_>>())
}

/// `image` (RGBA8, as decoded for thumbnails) as a texture.
pub fn texture(image: nox_media::Image) -> gdk::Texture {
    gdk::MemoryTexture::new(
        image.width as i32,
        image.height as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(image.data),
        image.stride,
    )
    .upcast()
}
