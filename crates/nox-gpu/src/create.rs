//! The Vulkan device Nox creates itself and hands to FFmpeg
//! (`nox_media::HwDevice`), so Nox decides what it enables: everything
//! FFmpeg 8 and 9 enable for their decoders, encoders and filters (their
//! `hwcontext_vulkan.c`: the same extensions, features and queue
//! families), and what Nox's own kernels use on top (cooperative matrices,
//! NVIDIA's second version among them).
//!
//! FFmpeg reads what's enabled from the extension lists and the feature
//! chain (`features2`), which therefore live as long as the device.

use ash::vk;
use std::ffi::{CStr, CString, c_char, c_void};

use crate::device::Features;
use crate::{Error, Result};

/// Device extensions enabled where the GPU has them: FFmpeg 8's and 9's
/// optional ones, then Nox's. Not `VK_KHR_internally_synchronized_queues`:
/// with it, FFmpeg 9 drops the queue locks nox-gpu submits under.
const DEVICE_EXTENSIONS: &[&CStr] = &[
    c"VK_KHR_portability_subset",
    c"VK_KHR_push_descriptor",
    c"VK_EXT_physical_device_drm",
    c"VK_EXT_shader_atomic_float",
    c"VK_KHR_cooperative_matrix",
    c"VK_EXT_shader_object",
    c"VK_KHR_shader_subgroup_rotate",
    c"VK_EXT_host_image_copy",
    c"VK_KHR_workgroup_memory_explicit_layout",
    c"VK_KHR_shader_relaxed_extended_instruction",
    c"VK_EXT_shader_long_vector",
    c"VK_EXT_shader_replicated_composites",
    c"VK_EXT_zero_initialize_device_memory",
    c"VK_KHR_shader_expect_assume",
    c"VK_EXT_descriptor_buffer",
    c"VK_KHR_video_maintenance1",
    c"VK_KHR_video_maintenance2",
    c"VK_KHR_external_memory_fd",
    c"VK_EXT_external_memory_dma_buf",
    c"VK_EXT_image_drm_format_modifier",
    c"VK_KHR_external_semaphore_fd",
    c"VK_EXT_external_memory_host",
    c"VK_KHR_external_memory_win32",
    c"VK_KHR_external_semaphore_win32",
    c"VK_KHR_video_queue",
    c"VK_KHR_video_encode_queue",
    c"VK_KHR_video_decode_queue",
    c"VK_KHR_video_encode_h264",
    c"VK_KHR_video_decode_h264",
    c"VK_KHR_video_encode_h265",
    c"VK_KHR_video_decode_h265",
    c"VK_KHR_video_decode_vp9",
    c"VK_KHR_video_encode_av1",
    c"VK_KHR_video_decode_av1",
    // Nox's: tensor cores with workgroup-wide matrices and tensor
    // addressing (NVIDIA).
    c"VK_NV_cooperative_matrix2",
];

/// Feature structures of extensions (all `VkBool32`s after sType and
/// pNext), chained when their extension is enabled: (extension, sType,
/// the booleans to enable if supported). The booleans are FFmpeg's choice
/// for its extensions (e.g. not descriptor buffer capture/replay).
const EXTENSION_FEATURES: &[(&CStr, i32, &[usize])] = &[
    // cooperativeMatrix
    (c"VK_KHR_cooperative_matrix", 1_000_506_000, &[0]),
    // workgroup scope, flexible dimensions, reductions, conversions,
    // per-element operations, tensor addressing, block loads
    (
        c"VK_NV_cooperative_matrix2",
        1_000_593_000,
        &[0, 1, 2, 3, 4, 5, 6],
    ),
    // shaderSubgroupRotate
    (c"VK_KHR_shader_subgroup_rotate", 1_000_416_000, &[0]),
    (c"VK_EXT_host_image_copy", 1_000_270_000, &[0]),
    (c"VK_EXT_shader_object", 1_000_482_000, &[0]),
    // shaderBufferFloat32Atomics, shaderBufferFloat32AtomicAdd
    (c"VK_EXT_shader_atomic_float", 1_000_260_000, &[0, 1]),
    (
        c"VK_KHR_workgroup_memory_explicit_layout",
        1_000_336_000,
        &[0, 1, 2, 3],
    ),
    (c"VK_KHR_video_maintenance1", 1_000_515_000, &[0]),
    (c"VK_KHR_video_maintenance2", 1_000_586_000, &[0]),
    (c"VK_KHR_video_decode_vp9", 1_000_514_000, &[0]),
    (c"VK_KHR_video_encode_av1", 1_000_513_004, &[0]),
    // descriptorBuffer, descriptorBufferPushDescriptors
    (c"VK_EXT_descriptor_buffer", 1_000_316_002, &[0, 3]),
    (
        c"VK_KHR_shader_relaxed_extended_instruction",
        1_000_558_000,
        &[0],
    ),
    (c"VK_KHR_shader_expect_assume", 1_000_544_000, &[0]),
    (c"VK_EXT_shader_long_vector", 1_000_635_000, &[0]),
    (c"VK_EXT_shader_replicated_composites", 1_000_564_000, &[0]),
    (c"VK_EXT_zero_initialize_device_memory", 1_000_620_000, &[0]),
];

/// An extension's feature structure (room for more booleans than any of
/// them has; the driver reads and writes only its own).
#[repr(C)]
#[derive(Clone, Copy)]
struct BoolFeatures {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    flags: [vk::Bool32; 16],
}

impl BoolFeatures {
    fn new(s_type: i32) -> Self {
        Self {
            s_type: vk::StructureType::from_raw(s_type),
            p_next: std::ptr::null_mut(),
            flags: [0; 16],
        }
    }
}

/// The features enabled, as one chain from `device` (pointers into this
/// box, which never moves).
struct FeatureChain {
    device: vk::PhysicalDeviceFeatures2<'static>,
    v11: vk::PhysicalDeviceVulkan11Features<'static>,
    v12: vk::PhysicalDeviceVulkan12Features<'static>,
    v13: vk::PhysicalDeviceVulkan13Features<'static>,
    extensions: Vec<BoolFeatures>,
}

impl FeatureChain {
    /// Structures for `extensions`' features, unlinked.
    fn new(extensions: &[CString]) -> Box<Self> {
        let extensions = EXTENSION_FEATURES
            .iter()
            .filter(|(name, ..)| extensions.iter().any(|e| e.as_c_str() == *name))
            .map(|&(_, s_type, _)| BoolFeatures::new(s_type))
            .collect();
        Box::new(Self {
            device: vk::PhysicalDeviceFeatures2::default(),
            v11: vk::PhysicalDeviceVulkan11Features::default(),
            v12: vk::PhysicalDeviceVulkan12Features::default(),
            v13: vk::PhysicalDeviceVulkan13Features::default(),
            extensions,
        })
    }

    /// Links the structures (again, after copying values in).
    fn link(&mut self) {
        let mut next: *mut c_void = std::ptr::null_mut();
        for e in self.extensions.iter_mut().rev() {
            e.p_next = next;
            next = std::ptr::from_mut(e).cast();
        }
        self.v13.p_next = next;
        self.v12.p_next = std::ptr::from_mut(&mut self.v13).cast();
        self.v11.p_next = std::ptr::from_mut(&mut self.v12).cast();
        self.device.p_next = std::ptr::from_mut(&mut self.v11).cast();
    }
}

/// A queue family the device uses, as FFmpeg lists them.
#[derive(Debug, Clone, Copy)]
pub struct QueueFamily {
    pub index: u32,
    /// Queues created in it.
    pub count: u32,
    pub flags: vk::QueueFlags,
    pub video_caps: vk::VideoCodecOperationFlagsKHR,
}

/// A Vulkan instance and device created by Nox.
pub struct VulkanDevice {
    entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    instance_extensions: Vec<CString>,
    instance_extension_ptrs: Vec<*const c_char>,
    device_extensions: Vec<CString>,
    device_extension_ptrs: Vec<*const c_char>,
    queue_families: Vec<QueueFamily>,
    features: Box<FeatureChain>,
    name: String,
}

// SAFETY: the raw pointers are into owned, never-modified data (extension
// names, the feature chain); Vulkan handles are thread-safe to share.
unsafe impl Send for VulkanDevice {}
unsafe impl Sync for VulkanDevice {}

impl VulkanDevice {
    /// Creates the device on the GPU `selection` names (an index, or part
    /// of its name, as FFmpeg takes them; else the first), with
    /// `VK_EXT_debug_utils` on the instance if `debug_utils`.
    pub fn create(selection: Option<&str>, debug_utils: bool) -> Result<Self> {
        // SAFETY: plain Vulkan setup; everything created here is owned by
        // the result, or destroyed on the error paths.
        unsafe {
            let entry = ash::Entry::load()
                .map_err(|e| Error::Unsupported(format!("no Vulkan loader: {e}")))?;
            let version = entry
                .try_enumerate_instance_version()?
                .unwrap_or(vk::API_VERSION_1_0);
            if version < vk::API_VERSION_1_3 {
                return Err(Error::Unsupported("Vulkan 1.3 is needed".into()));
            }
            let available: Vec<CString> = entry
                .enumerate_instance_extension_properties(None)?
                .iter()
                .filter_map(|p| p.extension_name_as_c_str().ok().map(CStr::to_owned))
                .collect();
            let mut wanted: Vec<&CStr> = vec![c"VK_KHR_portability_enumeration"];
            if debug_utils {
                wanted.push(c"VK_EXT_debug_utils");
            }
            let instance_extensions: Vec<CString> = wanted
                .into_iter()
                .filter(|w| available.iter().any(|a| a.as_c_str() == *w))
                .map(CStr::to_owned)
                .collect();
            let instance_extension_ptrs: Vec<*const c_char> =
                instance_extensions.iter().map(|e| e.as_ptr()).collect();
            let app = vk::ApplicationInfo::default()
                .application_name(c"Nox")
                .api_version(version.min(vk::make_api_version(0, 1, 4, 0)));
            let mut flags = vk::InstanceCreateFlags::empty();
            if instance_extensions
                .iter()
                .any(|e| e.as_c_str() == c"VK_KHR_portability_enumeration")
            {
                flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
            }
            let instance = entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app)
                    .enabled_extension_names(&instance_extension_ptrs)
                    .flags(flags),
                None,
            )?;
            match Self::with_instance(&instance, selection) {
                Ok((
                    physical_device,
                    device,
                    device_extensions,
                    queue_families,
                    features,
                    name,
                )) => {
                    let device_extension_ptrs =
                        device_extensions.iter().map(|e| e.as_ptr()).collect();
                    Ok(Self {
                        entry,
                        instance,
                        physical_device,
                        device,
                        instance_extensions,
                        instance_extension_ptrs,
                        device_extensions,
                        device_extension_ptrs,
                        queue_families,
                        features,
                        name,
                    })
                }
                Err(err) => {
                    instance.destroy_instance(None);
                    Err(err)
                }
            }
        }
    }

    /// Picks the GPU and creates the device on it.
    #[allow(clippy::type_complexity)]
    unsafe fn with_instance(
        instance: &ash::Instance,
        selection: Option<&str>,
    ) -> Result<(
        vk::PhysicalDevice,
        ash::Device,
        Vec<CString>,
        Vec<QueueFamily>,
        Box<FeatureChain>,
        String,
    )> {
        // SAFETY: a valid instance (the caller's).
        unsafe {
            let gpus = instance.enumerate_physical_devices()?;
            let name_of = |gpu: vk::PhysicalDevice| {
                let properties = instance.get_physical_device_properties(gpu);
                CStr::from_ptr(properties.device_name.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            };
            let chosen = match selection {
                None => gpus.first().copied(),
                Some(s) => match s.parse::<usize>() {
                    Ok(index) => gpus.get(index).copied(),
                    Err(_) => gpus.iter().copied().find(|&g| name_of(g).contains(s)),
                },
            };
            let physical_device = chosen.ok_or_else(|| {
                Error::Unsupported(format!(
                    "no Vulkan GPU {}",
                    selection.map_or(String::new(), |s| format!("matching {s:?}"))
                ))
            })?;
            let name = name_of(physical_device);

            let supported: Vec<CString> = instance
                .enumerate_device_extension_properties(physical_device)?
                .iter()
                .filter_map(|p| p.extension_name_as_c_str().ok().map(CStr::to_owned))
                .collect();
            let device_extensions: Vec<CString> = DEVICE_EXTENSIONS
                .iter()
                .filter(|w| supported.iter().any(|s| s.as_c_str() == **w))
                .map(|w| (*w).to_owned())
                .collect();
            let has = |name: &CStr| device_extensions.iter().any(|e| e.as_c_str() == name);

            // What the GPU supports, then what's enabled of it.
            let mut available = FeatureChain::new(&device_extensions);
            available.link();
            instance.get_physical_device_features2(physical_device, &mut available.device);
            let mut enabled = FeatureChain::new(&device_extensions);
            enable_features(&available, &mut enabled);
            enabled.link();

            let queue_families = pick_queue_families(instance, physical_device, &has);
            let priorities: Vec<Vec<f32>> = queue_families
                .iter()
                .map(|q| vec![1.0; q.count as usize])
                .collect();
            let queue_infos: Vec<vk::DeviceQueueCreateInfo> = queue_families
                .iter()
                .zip(&priorities)
                .map(|(q, p)| {
                    vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(q.index)
                        .queue_priorities(p)
                })
                .collect();
            let extension_ptrs: Vec<*const c_char> =
                device_extensions.iter().map(|e| e.as_ptr()).collect();
            let mut info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_infos)
                .enabled_extension_names(&extension_ptrs);
            info.p_next = std::ptr::from_ref(&enabled.device).cast();
            let device = instance.create_device(physical_device, &info, None)?;
            Ok((
                physical_device,
                device,
                device_extensions,
                queue_families,
                enabled,
                name,
            ))
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn get_instance_proc_addr(&self) -> vk::PFN_vkGetInstanceProcAddr {
        self.entry.static_fn().get_instance_proc_addr
    }

    pub fn instance(&self) -> vk::Instance {
        self.instance.handle()
    }

    pub fn physical_device(&self) -> vk::PhysicalDevice {
        self.physical_device
    }

    pub fn device(&self) -> vk::Device {
        self.device.handle()
    }

    /// Enabled instance extensions (C strings living as long as `self`).
    pub fn instance_extensions(&self) -> &[*const c_char] {
        &self.instance_extension_ptrs
    }

    /// Enabled device extensions (C strings living as long as `self`).
    pub fn device_extensions(&self) -> &[*const c_char] {
        &self.device_extension_ptrs
    }

    pub fn device_extension_names(&self) -> &[CString] {
        &self.device_extensions
    }

    pub fn has_instance_extension(&self, name: &CStr) -> bool {
        self.instance_extensions
            .iter()
            .any(|e| e.as_c_str() == name)
    }

    pub fn queue_families(&self) -> &[QueueFamily] {
        &self.queue_families
    }

    /// The enabled features, as a chain (living as long as `self`), for
    /// FFmpeg's `AVVulkanDeviceContext::device_features`.
    pub fn features2(&self) -> &vk::PhysicalDeviceFeatures2<'static> {
        &self.features.device
    }

    /// What nox-gpu's kernels may use.
    pub fn features(&self) -> Features {
        let f = &self.features;
        let extension = |s_type: i32, flag: usize| {
            f.extensions
                .iter()
                .any(|e| e.s_type.as_raw() == s_type && e.flags[flag] != 0)
        };
        Features {
            storage_16bit: f.v11.storage_buffer16_bit_access != 0,
            float16: f.v12.shader_float16 != 0,
            memory_model: f.v12.vulkan_memory_model != 0,
            cooperative_matrix: extension(1_000_506_000, 0),
            // Workgroup scope, flexible dimensions and tensor addressing:
            // what a GEMM needs.
            cooperative_matrix2: extension(1_000_593_000, 0)
                && extension(1_000_593_000, 1)
                && extension(1_000_593_000, 5),
        }
    }
}

impl Drop for VulkanDevice {
    fn drop(&mut self) {
        // SAFETY: the last user is gone (FFmpeg's context and nox-gpu keep
        // this alive while they use it).
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Copies into `enabled` the features FFmpeg enables and Nox uses that
/// `available` has.
fn enable_features(available: &FeatureChain, enabled: &mut FeatureChain) {
    macro_rules! copy {
        ($part:ident . $($field:ident).+) => {
            enabled.$part.$($field).+ = available.$part.$($field).+;
        };
    }
    // FFmpeg 9's choice (a superset of 8's).
    copy!(device.features.shader_image_gather_extended);
    copy!(device.features.shader_storage_image_read_without_format);
    copy!(device.features.shader_storage_image_write_without_format);
    copy!(device.features.fragment_stores_and_atomics);
    copy!(device.features.vertex_pipeline_stores_and_atomics);
    copy!(device.features.shader_int64);
    copy!(device.features.shader_int16);
    copy!(device.features.shader_float64);

    copy!(v11.sampler_ycbcr_conversion);
    copy!(v11.storage_push_constant16);
    copy!(v11.storage_buffer16_bit_access);
    copy!(v11.uniform_and_storage_buffer16_bit_access);

    copy!(v12.timeline_semaphore);
    copy!(v12.scalar_block_layout);
    copy!(v12.buffer_device_address);
    copy!(v12.host_query_reset);
    copy!(v12.storage_push_constant8);
    copy!(v12.shader_int8);
    copy!(v12.storage_buffer8_bit_access);
    copy!(v12.uniform_and_storage_buffer8_bit_access);
    copy!(v12.shader_float16);
    copy!(v12.shader_buffer_int64_atomics);
    copy!(v12.shader_shared_int64_atomics);
    copy!(v12.vulkan_memory_model);
    copy!(v12.vulkan_memory_model_device_scope);
    copy!(v12.vulkan_memory_model_availability_visibility_chains);
    copy!(v12.uniform_buffer_standard_layout);
    copy!(v12.runtime_descriptor_array);
    copy!(v12.shader_subgroup_extended_types);
    copy!(v12.shader_uniform_buffer_array_non_uniform_indexing);
    copy!(v12.shader_sampled_image_array_non_uniform_indexing);
    copy!(v12.shader_storage_buffer_array_non_uniform_indexing);
    copy!(v12.shader_storage_image_array_non_uniform_indexing);

    copy!(v13.dynamic_rendering);
    copy!(v13.maintenance4);
    copy!(v13.synchronization2);
    copy!(v13.compute_full_subgroups);
    copy!(v13.subgroup_size_control);
    copy!(v13.shader_zero_initialize_workgroup_memory);

    for (enable, have) in enabled.extensions.iter_mut().zip(&available.extensions) {
        let (_, _, wanted) = EXTENSION_FEATURES
            .iter()
            .find(|(_, s_type, _)| *s_type == have.s_type.as_raw())
            .expect("listed");
        for &flag in *wanted {
            enable.flags[flag] = have.flags[flag];
        }
    }
}

/// The queue families FFmpeg would use, picked as it does: for graphics,
/// compute and transfer the family with the fewest other capabilities,
/// then a video family for each codec operation, each pick making that
/// family a little less likely next time. Every queue of a family, except
/// one graphics queue on NVIDIA's driver.
unsafe fn pick_queue_families(
    instance: &ash::Instance,
    gpu: vk::PhysicalDevice,
    has: &dyn Fn(&CStr) -> bool,
) -> Vec<QueueFamily> {
    // SAFETY: valid handles (the caller's).
    let (families, video) = unsafe {
        let count = instance.get_physical_device_queue_family_properties2_len(gpu);
        let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); count];
        let mut families: Vec<vk::QueueFamilyProperties2> = (0..count)
            .map(|_| vk::QueueFamilyProperties2::default())
            .collect();
        if has(c"VK_KHR_video_queue") {
            for (f, v) in families.iter_mut().zip(video.iter_mut()) {
                f.p_next = std::ptr::from_mut(v).cast();
            }
        }
        instance.get_physical_device_queue_family_properties2(gpu, &mut families);
        let families: Vec<vk::QueueFamilyProperties> =
            families.iter().map(|f| f.queue_family_properties).collect();
        let video: Vec<vk::VideoCodecOperationFlagsKHR> =
            video.iter().map(|v| v.video_codec_operations).collect();
        (families, video)
    };
    // SAFETY: as above.
    let nvidia = unsafe {
        let mut driver = vk::PhysicalDeviceDriverProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
        instance.get_physical_device_properties2(gpu, &mut properties);
        driver.driver_id == vk::DriverId::NVIDIA_PROPRIETARY
    };
    let mut picked = vec![0u32; families.len()];
    let mut out: Vec<QueueFamily> = Vec::new();
    let add = |index: Option<usize>,
               flag: vk::QueueFlags,
               op: vk::VideoCodecOperationFlagsKHR,
               out: &mut Vec<QueueFamily>| {
        let Some(index) = index else { return };
        if let Some(q) = out.iter_mut().find(|q| q.index == index as u32) {
            q.flags |= flag;
            q.video_caps |= op;
            return;
        }
        let mut count = families[index].queue_count;
        if nvidia && flag == vk::QueueFlags::GRAPHICS {
            count = count.min(1);
        }
        out.push(QueueFamily {
            index: index as u32,
            count,
            flags: flag,
            video_caps: op,
        });
    };
    let pick = |flag: vk::QueueFlags, picked: &mut [u32]| {
        let mut best: Option<(usize, u32)> = None;
        for (i, f) in families.iter().enumerate() {
            let mut flags = f.queue_flags;
            // Graphics and compute families may leave transfer unsaid.
            if flag == vk::QueueFlags::TRANSFER
                && flags.intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            {
                flags |= vk::QueueFlags::TRANSFER;
            }
            if flags.intersects(flag) {
                let score = flags.as_raw().count_ones() + picked[i];
                if best.is_none_or(|(_, s)| score < s) {
                    best = Some((i, score));
                }
            }
        }
        if let Some((i, _)) = best {
            picked[i] += 1;
        }
        best.map(|(i, _)| i)
    };
    for flag in [
        vk::QueueFlags::GRAPHICS,
        vk::QueueFlags::COMPUTE,
        vk::QueueFlags::TRANSFER,
    ] {
        let index = pick(flag, &mut picked);
        add(index, flag, vk::VideoCodecOperationFlagsKHR::NONE, &mut out);
    }
    let video_flags = vk::QueueFlags::VIDEO_ENCODE_KHR | vk::QueueFlags::VIDEO_DECODE_KHR;
    let pick_video = |op: vk::VideoCodecOperationFlagsKHR, picked: &mut [u32]| {
        let mut best: Option<(usize, u32)> = None;
        for (i, f) in families.iter().enumerate() {
            if !f.queue_flags.intersects(video_flags) || !video[i].intersects(op) {
                continue;
            }
            let score = video[i].as_raw().count_ones() + picked[i];
            if best.is_none_or(|(_, s)| score < s) {
                best = Some((i, score));
            }
        }
        if let Some((i, _)) = best {
            picked[i] += 1;
        }
        best.map(|(i, _)| i)
    };
    if has(c"VK_KHR_video_queue") {
        let op = vk::VideoCodecOperationFlagsKHR::from_raw;
        for (flag, raw) in [
            (vk::QueueFlags::VIDEO_ENCODE_KHR, 0x0001_0000), // H.264 encode
            (vk::QueueFlags::VIDEO_DECODE_KHR, 0x0000_0001), // H.264 decode
            (vk::QueueFlags::VIDEO_ENCODE_KHR, 0x0002_0000), // H.265 encode
            (vk::QueueFlags::VIDEO_DECODE_KHR, 0x0000_0002), // H.265 decode
            (vk::QueueFlags::VIDEO_DECODE_KHR, 0x0000_0008), // VP9 decode
            (vk::QueueFlags::VIDEO_ENCODE_KHR, 0x0004_0000), // AV1 encode
            (vk::QueueFlags::VIDEO_DECODE_KHR, 0x0000_0004), // AV1 decode
        ] {
            let index = pick_video(op(raw), &mut picked);
            add(index, flag, op(raw), &mut out);
        }
    }
    out
}
