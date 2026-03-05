use anyhow::Result;
use gdk;
use glib;
use glib::prelude::*;

use super::backend::{RenderBackend, RenderBackendType, VideoFrame};

// ---------------------------------------------------------------------------
// SoftwareRenderer – CPU-based texture upload via gdk::MemoryTexture
// ---------------------------------------------------------------------------

/// A pure-software rendering backend that converts raw RGBA pixel data into
/// [`gdk::MemoryTexture`] objects.  No GPU interaction at all – useful as a
/// baseline and guaranteed to work everywhere.
pub struct SoftwareRenderer;

impl SoftwareRenderer {
    pub fn new() -> Self {
        log::info!("Initialised SoftwareRenderer backend");
        Self
    }
}

impl RenderBackend for SoftwareRenderer {
    fn name(&self) -> &str {
        "Software"
    }

    fn backend_type(&self) -> RenderBackendType {
        RenderBackendType::Software
    }

    fn upload_frame(&self, frame: &VideoFrame) -> Result<gdk::Texture> {
        let bytes = glib::Bytes::from(&frame.data);
        let texture = gdk::MemoryTexture::new(
            frame.width as i32,
            frame.height as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            frame.stride(),
        );
        Ok(texture.upcast())
    }

    fn supports_hw_decode(&self) -> bool {
        false
    }
}
