//! Frames shared with other GPU APIs on the same device, for hardware
//! encoders that aren't Vulkan (NVENC through CUDA now; VAAPI, D3D11 and
//! others later): the compositor writes Y'CbCr 4:2:0 into a buffer whose
//! memory the other API imports, so frames never leave the GPU.
//!
//! The other side of the exchange lives with that API (`nox_media::hwenc`).
//! Buffers move between the APIs by queue family ownership transfers to
//! and from `VK_QUEUE_FAMILY_EXTERNAL`; the compositor waits for its work to
//! finish before returning, so the other API may read right after.

use ash::vk;
use std::sync::Arc;

use crate::{Error, GpuDevice, Result, YuvTarget};
use nox_color::Matrix3x4;

/// Sample layout of a linear 4:2:0 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearFormat {
    /// 8-bit samples.
    Nv12,
    /// 16-bit samples holding 10 bits in the high bits.
    P010,
}

impl LinearFormat {
    pub fn bytes_per_sample(self) -> u32 {
        match self {
            Self::Nv12 => 1,
            Self::P010 => 2,
        }
    }
}

/// Where the planes of a linear frame are: luma rows, then interleaved
/// chroma rows, as CUDA frames (and NV12/P010 surfaces generally) lay them
/// out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinearLayout {
    pub format: LinearFormat,
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next, in both planes.
    pub pitch: u32,
    pub luma_offset: u64,
    pub chroma_offset: u64,
}

impl LinearLayout {
    /// Planes back to back, rows padded to `alignment` bytes (a power of
    /// two, at least 4).
    pub fn packed(format: LinearFormat, width: u32, height: u32, alignment: u32) -> Self {
        let row = width * format.bytes_per_sample();
        let pitch = row.next_multiple_of(alignment.max(4));
        Self {
            format,
            width,
            height,
            pitch,
            luma_offset: 0,
            chroma_offset: u64::from(pitch) * u64::from(height),
        }
    }

    /// Bytes the frame takes.
    pub fn size(&self) -> u64 {
        self.chroma_offset + u64::from(self.pitch) * u64::from(self.height.div_ceil(2))
    }
}

/// How another API refers to exported memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryHandle {
    /// A POSIX file descriptor of the driver's own kind (CUDA imports
    /// these on Linux).
    OpaqueFd,
    /// A Windows NT handle of the driver's own kind (CUDA imports these on
    /// Windows).
    OpaqueWin32,
}

impl MemoryHandle {
    /// The driver-specific kind this platform's APIs import, if any (none
    /// on macOS).
    pub fn native() -> Option<Self> {
        if cfg!(windows) {
            Some(Self::OpaqueWin32)
        } else if cfg!(unix) && !cfg!(target_os = "macos") {
            Some(Self::OpaqueFd)
        } else {
            None
        }
    }

    fn vk(self) -> vk::ExternalMemoryHandleTypeFlags {
        match self {
            Self::OpaqueFd => vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD,
            Self::OpaqueWin32 => vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32,
        }
    }

    /// The device extension that exports it.
    fn extension(self) -> &'static std::ffi::CStr {
        match self {
            Self::OpaqueFd => ash::khr::external_memory_fd::NAME,
            Self::OpaqueWin32 => ash::khr::external_memory_win32::NAME,
        }
    }
}

/// Memory exported from a `SharedBuffer`, for the importing API.
pub enum ExportedMemory {
    #[cfg(unix)]
    Fd(std::os::fd::OwnedFd),
    #[cfg(windows)]
    Win32(std::os::windows::io::OwnedHandle),
}

/// A device-local buffer in memory another API can import.
pub struct SharedBuffer {
    gpu: Arc<GpuDevice>,
    pub(crate) buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
    handle: MemoryHandle,
}

// SAFETY: Vulkan handles are plain values; the buffer is used from one
// thread at a time by its owner.
unsafe impl Send for SharedBuffer {}

impl SharedBuffer {
    /// A buffer of at least `size` bytes, exportable as `handle`.
    pub fn new(gpu: &Arc<GpuDevice>, size: u64, handle: MemoryHandle) -> Result<Self> {
        if !gpu.has_extension(handle.extension()) {
            return Err(Error::Unsupported(format!(
                "the device can't export memory ({})",
                handle.extension().to_string_lossy()
            )));
        }
        let device = &gpu.device;
        // SAFETY: valid create infos; the objects are owned by the returned
        // SharedBuffer (or destroyed below on error).
        unsafe {
            let mut external =
                vk::ExternalMemoryBufferCreateInfo::default().handle_types(handle.vk());
            let buffer = device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
                    .push_next(&mut external),
                None,
            )?;
            let requirements = device.get_buffer_memory_requirements(buffer);
            let Some(memory_type) = gpu.memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            ) else {
                device.destroy_buffer(buffer, None);
                return Err(Error::Unsupported("no device-local memory to share".into()));
            };
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(handle.vk());
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let memory = match device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type)
                    .push_next(&mut export)
                    .push_next(&mut dedicated),
                None,
            ) {
                Ok(memory) => memory,
                Err(err) => {
                    device.destroy_buffer(buffer, None);
                    return Err(err.into());
                }
            };
            if let Err(err) = device.bind_buffer_memory(buffer, memory, 0) {
                device.destroy_buffer(buffer, None);
                device.free_memory(memory, None);
                return Err(err.into());
            }
            Ok(Self {
                gpu: gpu.clone(),
                buffer,
                memory,
                size: requirements.size,
                handle,
            })
        }
    }

    /// Size of the memory, as the importing API needs it.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// A new handle of the memory, for the importing API.
    pub fn export(&self) -> Result<ExportedMemory> {
        match self.handle {
            #[cfg(unix)]
            MemoryHandle::OpaqueFd => {
                use std::os::fd::{FromRawFd, OwnedFd};
                let loader =
                    ash::khr::external_memory_fd::Device::new(&self.gpu.instance, &self.gpu.device);
                // SAFETY: memory allocated exportable as this handle type.
                let fd = unsafe {
                    loader.get_memory_fd(
                        &vk::MemoryGetFdInfoKHR::default()
                            .memory(self.memory)
                            .handle_type(self.handle.vk()),
                    )?
                };
                // SAFETY: a fresh descriptor we own.
                Ok(ExportedMemory::Fd(unsafe { OwnedFd::from_raw_fd(fd) }))
            }
            #[cfg(windows)]
            MemoryHandle::OpaqueWin32 => {
                use std::os::windows::io::{FromRawHandle, OwnedHandle};
                let loader = ash::khr::external_memory_win32::Device::new(
                    &self.gpu.instance,
                    &self.gpu.device,
                );
                // SAFETY: memory allocated exportable as this handle type.
                let handle = unsafe {
                    loader.get_memory_win32_handle(
                        &vk::MemoryGetWin32HandleInfoKHR::default()
                            .memory(self.memory)
                            .handle_type(self.handle.vk()),
                    )?
                };
                // SAFETY: a fresh NT handle we own.
                Ok(ExportedMemory::Win32(unsafe {
                    OwnedHandle::from_raw_handle(handle as *mut std::ffi::c_void)
                }))
            }
            #[allow(unreachable_patterns)]
            other => Err(Error::Unsupported(format!(
                "{other:?} memory handles don't exist on this platform"
            ))),
        }
    }
}

impl Drop for SharedBuffer {
    fn drop(&mut self) {
        // SAFETY: no GPU work using the buffer is pending (the compositor
        // waits for its submissions); the importer released its mapping.
        unsafe {
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// A linear frame in a shared buffer, for `Compositor::finish_to_buffer`.
pub struct BufferTarget<'a> {
    pub buffer: &'a SharedBuffer,
    pub layout: LinearLayout,
    /// R'G'B' to normalized `(Y, Cb, Cr)` values for `layout.format`.
    pub matrix: Matrix3x4,
}

/// The light of an HDR frame, from `Compositor::finish_to_buffer`: the
/// largest of R, G and B in nits, at the brightest pixel and on average
/// (CTA-861.3 MaxCLL and frame-average light level).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FrameLight {
    pub peak: f32,
    pub average: f32,
}

/// Where a hardware encoder takes frames from: images of its own (Vulkan
/// Video) or a buffer shared with another API.
pub enum GpuTarget<'a> {
    Images(&'a mut YuvTarget),
    Buffer(BufferTarget<'a>),
}
