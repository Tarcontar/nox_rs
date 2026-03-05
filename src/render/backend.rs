use anyhow::Result;
use gdk;

// ---------------------------------------------------------------------------
// VideoFrame – a decoded frame in RGBA format, ready for display
// ---------------------------------------------------------------------------

/// A decoded video frame stored as raw RGBA pixel data.
///
/// This is the format-agnostic interchange type between the decoder layer
/// and the rendering layer.  Every backend accepts this as input.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Raw RGBA pixel data, row-major, 4 bytes per pixel.
    pub data: Vec<u8>,
    /// Width of the frame in pixels.
    pub width: u32,
    /// Height of the frame in pixels.
    pub height: u32,
    /// Presentation timestamp in seconds.
    pub timestamp_secs: f64,
}

impl VideoFrame {
    /// Number of bytes per row (stride) assuming tightly packed RGBA.
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }
}

// ---------------------------------------------------------------------------
// RenderBackendType – selects which GPU / software path to use
// ---------------------------------------------------------------------------

/// Enumerates the available rendering backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderBackendType {
    /// Pure CPU path – creates `gdk::MemoryTexture` from raw pixels.
    Software,
    /// OpenGL accelerated path (future).
    OpenGL,
    /// Vulkan accelerated path (future).
    Vulkan,
}

// ---------------------------------------------------------------------------
// RenderBackend trait – GPU-agnostic texture upload
// ---------------------------------------------------------------------------

/// Trait that abstracts how decoded video frames are converted into
/// `gdk::Texture` objects suitable for display in GTK widgets.
///
/// Implementations may use:
/// - CPU memory copies (`SoftwareRenderer`)
/// - OpenGL texture uploads (`OpenGLRenderer`)
/// - Vulkan image uploads (`VulkanRenderer`)
///
/// The trait is object-safe so backends can be swapped at runtime.
pub trait RenderBackend: Send + Sync {
    /// Human-readable name of this backend (e.g. "Software", "OpenGL").
    fn name(&self) -> &str;

    /// The backend type enum variant.
    fn backend_type(&self) -> RenderBackendType;

    /// Convert a decoded [`VideoFrame`] into a [`gdk::Texture`] that can be
    /// displayed in a `gtk::Picture` or painted by a snapshot.
    fn upload_frame(&self, frame: &VideoFrame) -> Result<gdk::Texture>;

    /// Whether this backend can leverage hardware-accelerated video decoding.
    ///
    /// When `true`, the caller may pass GPU-resident frame handles instead of
    /// copying decoded pixels through system memory.
    fn supports_hw_decode(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Factory helper
// ---------------------------------------------------------------------------

/// Create a [`RenderBackend`] for the requested type.
///
/// Returns an `Rc` so the backend instance can be cheaply shared between
/// the preview panel, media library, and any other consumer on the main
/// thread without being re-created per frame.
///
/// Falls back to [`SoftwareRenderer`] if the requested backend is not yet
/// implemented.
pub fn create_backend(ty: RenderBackendType) -> std::rc::Rc<dyn RenderBackend> {
    match ty {
        RenderBackendType::Software => std::rc::Rc::new(super::software::SoftwareRenderer::new()),
        RenderBackendType::OpenGL => {
            log::warn!("OpenGL backend not yet implemented – falling back to software");
            std::rc::Rc::new(super::software::SoftwareRenderer::new())
        }
        RenderBackendType::Vulkan => {
            log::warn!("Vulkan backend not yet implemented – falling back to software");
            std::rc::Rc::new(super::software::SoftwareRenderer::new())
        }
    }
}
