//! The shared Vulkan device.
//!
//! FFmpeg creates the device (so it gets exactly the video extensions and
//! features its decoders need) and Nox loads it into ash; see
//! `nox_media::HwDevice`. Nox never destroys the device itself.

use ash::vk;
use gpu_allocator::vulkan::{Allocator, AllocatorCreateDesc};
use std::any::Any;
use std::ffi::{CStr, CString};
use std::mem::ManuallyDrop;
use std::sync::{Mutex, MutexGuard};

use crate::{Error, Result};

/// Serializes queue access with the device's other user (FFmpeg).
pub trait QueueLock: Send + Sync {
    fn lock(&self, family: u32, index: u32);
    fn unlock(&self, family: u32, index: u32);
}

/// Handles of a device created elsewhere.
pub struct ExternalDevice {
    pub get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    pub instance: vk::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: vk::Device,
    pub compute_queue_family: u32,
    /// Device extensions enabled at creation.
    pub enabled_extensions: Vec<CString>,
    pub queue_lock: Option<Box<dyn QueueLock>>,
    /// Keeps the owner of the handles alive (e.g. FFmpeg's device reference).
    pub owner: Box<dyn Any + Send + Sync>,
}

pub struct GpuDevice {
    pub(crate) instance: ash::Instance,
    pub(crate) physical_device: vk::PhysicalDevice,
    pub(crate) device: ash::Device,
    extensions: Vec<CString>,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    pub(crate) queue_family: u32,
    queue: vk::Queue,
    queue_lock: Option<Box<dyn QueueLock>>,
    allocator: ManuallyDrop<Mutex<Allocator>>,
    name: String,
    _owner: Box<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for GpuDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuDevice").finish_non_exhaustive()
    }
}

impl GpuDevice {
    /// Wraps an existing device.
    ///
    /// # Safety
    /// The handles must be valid, the device must support Vulkan 1.3 with
    /// synchronization2, and `owner` must keep them alive.
    pub unsafe fn from_external(ext: ExternalDevice) -> Result<Self> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let entry = ash::Entry::from_static_fn(ash::StaticFn {
                get_instance_proc_addr: ext.get_instance_proc_addr,
            });
            let instance = ash::Instance::load(entry.static_fn(), ext.instance);
            let device = ash::Device::load(instance.fp_v1_0(), ext.device);
            let properties = instance.get_physical_device_properties(ext.physical_device);
            let name = CStr::from_ptr(properties.device_name.as_ptr())
                .to_string_lossy()
                .into_owned();
            let queue = device.get_device_queue(ext.compute_queue_family, 0);
            let memory_properties =
                instance.get_physical_device_memory_properties(ext.physical_device);
            let allocator = Allocator::new(&AllocatorCreateDesc {
                instance: instance.clone(),
                device: device.clone(),
                physical_device: ext.physical_device,
                debug_settings: Default::default(),
                buffer_device_address: false,
                allocation_sizes: Default::default(),
            })?;
            tracing::info!(
                "GPU: {name} (compute queue family {})",
                ext.compute_queue_family
            );
            Ok(Self {
                instance,
                physical_device: ext.physical_device,
                device,
                extensions: ext.enabled_extensions,
                memory_properties,
                queue_family: ext.compute_queue_family,
                queue,
                queue_lock: ext.queue_lock,
                allocator: ManuallyDrop::new(Mutex::new(allocator)),
                name,
                _owner: ext.owner,
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The device's UUID, which other APIs (CUDA) use to find the same
    /// GPU.
    pub fn uuid(&self) -> [u8; 16] {
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        // SAFETY: valid physical device (Vulkan 1.1 core query).
        unsafe {
            self.instance
                .get_physical_device_properties2(self.physical_device, &mut properties)
        };
        id.device_uuid
    }

    pub fn has_extension(&self, name: &CStr) -> bool {
        self.extensions.iter().any(|e| e.as_c_str() == name)
    }

    /// Index of a memory type allowed by `type_bits` with `flags`.
    pub(crate) fn memory_type(
        &self,
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        let props = &self.memory_properties;
        (0..props.memory_type_count).find(|&i| {
            type_bits & (1 << i) != 0
                && props.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
    }

    pub(crate) fn allocator(&self) -> MutexGuard<'_, Allocator> {
        self.allocator.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn submit(&self, submit: &vk::SubmitInfo2<'_>, fence: vk::Fence) -> Result<()> {
        if let Some(lock) = &self.queue_lock {
            lock.lock(self.queue_family, 0);
        }
        // SAFETY: the queue is externally synchronized via the lock.
        let result = unsafe {
            self.device
                .queue_submit2(self.queue, std::slice::from_ref(submit), fence)
        };
        if let Some(lock) = &self.queue_lock {
            lock.unlock(self.queue_family, 0);
        }
        result.map_err(Error::from)
    }
}

impl Drop for GpuDevice {
    fn drop(&mut self) {
        // The allocator frees its memory blocks; the device itself belongs
        // to the owner, which is dropped afterwards.
        // SAFETY: not used after this point.
        unsafe { ManuallyDrop::drop(&mut self.allocator) };
    }
}
