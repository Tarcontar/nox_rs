//! Placeholder module for a future Vulkan-accelerated rendering backend.
//!
//! When implemented this will:
//! - Upload decoded frames to VkImage objects.
//! - Optionally interop with hardware video decoders (Vulkan Video, VAAPI)
//!   for zero-copy frame presentation.
//! - Render the final composite through a custom `GskRenderer` or a
//!   `GtkGLArea` with Vulkan interop.

use anyhow::{bail, Result};
use gdk;

use super::backend::{RenderBackend, RenderBackendType, VideoFrame};

/// Vulkan-accelerated rendering backend (not yet implemented).
pub struct VulkanRenderer;

impl VulkanRenderer {
    #[allow(dead_code)]
    pub fn new() -> Result<Self> {
        bail!("Vulkan backend is not yet implemented");
    }
}

impl RenderBackend for VulkanRenderer {
    fn name(&self) -> &str {
        "Vulkan"
    }

    fn backend_type(&self) -> RenderBackendType {
        RenderBackendType::Vulkan
    }

    fn upload_frame(&self, _frame: &VideoFrame) -> Result<gdk::Texture> {
        bail!("Vulkan backend is not yet implemented")
    }

    fn supports_hw_decode(&self) -> bool {
        // Will return true once Vulkan Video interop is implemented.
        false
    }
}
