//! Vulkan core: wraps the device FFmpeg created for hardware decoding,
//! compute pipelines for color conversion and compositing, and frame
//! export (readback now; DMA-BUF and GL interop later).

mod converter;
mod device;
mod export;

pub use ash::vk;
pub use converter::{
    Converter, CpuFrame, ImageSync, Input, Layout, Plane, PlaneFormat, VkFrame, VkPlane,
};
pub use device::{ExternalDevice, GpuDevice, QueueLock};
pub use export::{DmabufFrame, FOURCC_ABGR8888};

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
