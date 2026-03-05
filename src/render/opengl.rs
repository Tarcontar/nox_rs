//! Placeholder module for a future OpenGL-accelerated rendering backend.
//!
//! When implemented this will:
//! - Upload decoded frames as GL textures via PBO or direct mapping.
//! - Optionally interop with hardware video decoders (VAAPI / NVDEC) to
//!   avoid GPU→CPU→GPU round-trips.
//! - Render the final composite through a `GtkGLArea`.

use anyhow::{bail, Result};
use gdk;

use super::backend::{RenderBackend, RenderBackendType, VideoFrame};

/// OpenGL-accelerated rendering backend (not yet implemented).
pub struct OpenGLRenderer;

impl OpenGLRenderer {
    #[allow(dead_code)]
    pub fn new() -> Result<Self> {
        bail!("OpenGL backend is not yet implemented");
    }
}

impl RenderBackend for OpenGLRenderer {
    fn name(&self) -> &str {
        "OpenGL"
    }

    fn backend_type(&self) -> RenderBackendType {
        RenderBackendType::OpenGL
    }

    fn upload_frame(&self, _frame: &VideoFrame) -> Result<gdk::Texture> {
        bail!("OpenGL backend is not yet implemented")
    }

    fn supports_hw_decode(&self) -> bool {
        // Will return true once VAAPI/NVDEC interop is implemented.
        false
    }
}
