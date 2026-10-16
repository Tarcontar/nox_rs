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

/// Device features that kernels may use, as enabled at creation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Features {
    /// Storage buffers of 16-bit values (`storageBuffer16BitAccess`).
    pub storage_16bit: bool,
    /// 16-bit float arithmetic in shaders (`shaderFloat16`).
    pub float16: bool,
    /// The Vulkan memory model (`vulkanMemoryModel`).
    pub memory_model: bool,
    /// Cooperative matrices (`VK_KHR_cooperative_matrix`'s feature).
    pub cooperative_matrix: bool,
    /// NVIDIA's cooperative matrices 2 with workgroup scope, flexible
    /// dimensions and tensor addressing (`VK_NV_cooperative_matrix2`).
    pub cooperative_matrix2: bool,
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
    /// Features enabled at creation.
    pub features: Features,
    /// Whether `VK_EXT_debug_utils` was enabled on the instance (debug
    /// labels for profilers).
    pub debug_utils: bool,
    pub queue_lock: Option<Box<dyn QueueLock>>,
    /// Keeps the owner of the handles alive (e.g. FFmpeg's device reference).
    pub owner: Box<dyn Any + Send + Sync>,
}

pub struct GpuDevice {
    pub(crate) instance: ash::Instance,
    pub(crate) physical_device: vk::PhysicalDevice,
    pub(crate) device: ash::Device,
    extensions: Vec<CString>,
    features: Features,
    /// 16 × 16 × 16 cooperative matrices of 16-bit floats with 32-bit
    /// sums, in 32-wide subgroups, are usable.
    tensor_f16: bool,
    /// Debug labels, if `VK_EXT_debug_utils` is enabled.
    pub(crate) debug_utils: Option<ash::ext::debug_utils::Device>,
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
            let tensor_f16 = tensor_f16(&entry, &instance, &ext);
            let debug_utils = ext
                .debug_utils
                .then(|| ash::ext::debug_utils::Device::new(&instance, &device));
            tracing::info!(
                "GPU: {name} (compute queue family {}, {:?}, 16-bit tensor cores: {tensor_f16})",
                ext.compute_queue_family,
                ext.features
            );
            Ok(Self {
                instance,
                physical_device: ext.physical_device,
                device,
                extensions: ext.enabled_extensions,
                features: ext.features,
                tensor_f16,
                debug_utils,
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

    /// The device's DRM render node (`/dev/dri/renderD128` and so on),
    /// which other APIs on Linux (VAAPI) use to find the same GPU. `None`
    /// if the driver doesn't say (no `VK_EXT_physical_device_drm`).
    pub fn drm_render_node(&self) -> Option<std::path::PathBuf> {
        // SAFETY: valid physical device; the DRM properties are only
        // queried if the device supports the extension.
        unsafe {
            let supported = self
                .instance
                .enumerate_device_extension_properties(self.physical_device)
                .ok()?
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(ash::ext::physical_device_drm::NAME));
            if !supported {
                return None;
            }
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            self.instance
                .get_physical_device_properties2(self.physical_device, &mut properties);
            (drm.has_render == vk::TRUE)
                .then(|| format!("/dev/dri/renderD{}", drm.render_minor).into())
        }
    }

    pub fn has_extension(&self, name: &CStr) -> bool {
        self.extensions.iter().any(|e| e.as_c_str() == name)
    }

    /// Whether shaders may keep 16-bit floats in storage buffers
    /// (`storageBuffer16BitAccess` was enabled).
    pub fn storage_16bit(&self) -> bool {
        self.features.storage_16bit
    }

    /// The features kernels may use.
    pub fn features(&self) -> Features {
        self.features
    }

    /// Whether 16 × 16 × 16 cooperative matrices (tensor cores) multiply
    /// 16-bit floats into 32-bit sums here, in subgroups of 32: the
    /// features are enabled, the GPU lists that shape at subgroup scope,
    /// and compute subgroups are 32 wide.
    pub fn tensor_f16(&self) -> bool {
        self.tensor_f16
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

/// Whether `ext`'s device multiplies 16 × 16 × 16 cooperative matrices of
/// 16-bit floats into 32-bit sums at subgroup scope, with 32-wide
/// subgroups (what nox-ai's tensor-core kernels are written for).
///
/// # Safety
/// The handles must be valid.
unsafe fn tensor_f16(entry: &ash::Entry, instance: &ash::Instance, ext: &ExternalDevice) -> bool {
    let f = ext.features;
    let extension = ash::khr::cooperative_matrix::NAME;
    if !(f.storage_16bit && f.float16 && f.memory_model && f.cooperative_matrix)
        || !ext
            .enabled_extensions
            .iter()
            .any(|e| e.as_c_str() == extension)
    {
        return false;
    }
    // SAFETY: guaranteed by the caller.
    unsafe {
        let mut subgroup = vk::PhysicalDeviceSubgroupProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut subgroup);
        instance.get_physical_device_properties2(ext.physical_device, &mut properties);
        let in_compute = subgroup
            .supported_stages
            .contains(vk::ShaderStageFlags::COMPUTE);
        if subgroup.subgroup_size != 32 || !in_compute {
            return false;
        }
        let cooperative = ash::khr::cooperative_matrix::Instance::new(entry, instance);
        let Ok(shapes) =
            cooperative.get_physical_device_cooperative_matrix_properties(ext.physical_device)
        else {
            return false;
        };
        shapes.iter().any(|p| {
            (p.m_size, p.n_size, p.k_size) == (16, 16, 16)
                && p.a_type == vk::ComponentTypeKHR::FLOAT16
                && p.b_type == vk::ComponentTypeKHR::FLOAT16
                && p.c_type == vk::ComponentTypeKHR::FLOAT32
                && p.result_type == vk::ComponentTypeKHR::FLOAT32
                && p.scope == vk::ScopeKHR::SUBGROUP
        })
    }
}
