//! Vulkan core: wraps the device FFmpeg created for hardware decoding,
//! compute pipelines for color conversion and compositing, and frame
//! export (readback now; DMA-BUF and GL interop later).

mod compositor;
pub mod compute;
mod converter;
mod create;
mod device;
mod export;
mod interop;
mod scope_layout;
mod scopes;
mod vk_util;

pub use ash::vk;
pub use compositor::{
    BlendMode, Compositor, LayerHook, LayerPicture, LayerStyle, MAX_LAYERS, Placement, Rect,
    Transform, YuvTarget,
};
pub use converter::{
    BufferFrame, Converter, CpuFrame, ImageSync, Input, Layout, Orientation, Plane, PlaneFormat,
    VkFrame, VkPlane,
};
pub use create::{QueueFamily, VulkanDevice};
pub use device::{ExternalDevice, Features, GpuDevice, QueueLock};
pub use export::{DmabufFd, DmabufFormat, DmabufFrame, FOURCC_ABGR8888, FOURCC_ABGR16161616F};
pub use interop::{
    BufferTarget, ExportedMemory, FrameLight, GpuTarget, LinearFormat, LinearLayout, MemoryHandle,
    SharedBuffer,
};
pub use scopes::ScopeImages;
/// Sizes of the scope images.
pub mod scope_sizes {
    pub use crate::scope_layout::{
        HISTOGRAM_BINS, HISTOGRAM_HEIGHT, LEVELS, PARADE_WIDTH, VECTOR_SIZE, WAVEFORM_WIDTH,
    };
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Vulkan: {0}")]
    Vulkan(#[from] vk::Result),
    #[error("GPU memory: {0}")]
    Allocation(#[from] gpu_allocator::AllocationError),
    #[error("no compute queue")]
    NoComputeQueue,
    #[error("invalid frame: {0}")]
    InvalidFrame(&'static str),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("effect: {0}")]
    Effect(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// An 8-bit RGBA image in system memory, not premultiplied.
#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: usize,
    pub data: Vec<u8>,
}
