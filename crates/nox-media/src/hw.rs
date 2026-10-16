//! The Vulkan device shared between FFmpeg (hardware decoding, encoding)
//! and nox-gpu (color conversion, compositing, AI).
//!
//! Nox creates the device (`nox_gpu::VulkanDevice`: the extensions,
//! features and queue families FFmpeg would enable, and what Nox's kernels
//! use on top, such as NVIDIA's cooperative matrices 2) and hands it to
//! FFmpeg. If that fails, or with `NOX_VULKAN_DEVICE=ffmpeg`, FFmpeg
//! creates it as before. Either way nox-gpu loads the same handles into
//! ash. `NOX_GPU` selects the GPU (index or part of its name, as FFmpeg
//! takes them). For validation, run under `tools/vk-validate.sh` rather
//! than FFmpeg's `debug` option, whose layer settings crash Vulkan Video
//! decoding on NVIDIA.

use ffmpeg::ffi;
use nox_gpu::{ExternalDevice, Features, GpuDevice, QueueLock, vk};
use std::ffi::CString;
use std::ptr;
use std::sync::Arc;

use crate::{Error, Result};

/// A reference to FFmpeg's `AVHWDeviceContext` for Vulkan.
pub struct HwDevice {
    buffer: *mut ffi::AVBufferRef,
}

// SAFETY: AVBufferRef reference counting is atomic and the Vulkan device
// context is internally synchronized (queues via lock_queue).
unsafe impl Send for HwDevice {}
unsafe impl Sync for HwDevice {}

impl std::fmt::Debug for HwDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HwDevice").finish_non_exhaustive()
    }
}

impl Clone for HwDevice {
    fn clone(&self) -> Self {
        // SAFETY: `buffer` is a valid reference.
        Self {
            buffer: unsafe { ffi::av_buffer_ref(self.buffer) },
        }
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { ffi::av_buffer_unref(&mut self.buffer) };
    }
}

impl HwDevice {
    /// Creates the Vulkan device and a `GpuDevice` sharing it.
    pub fn vulkan() -> Result<(Self, Arc<GpuDevice>)> {
        crate::init();
        let selection = std::env::var("NOX_GPU").ok().filter(|s| !s.is_empty());
        // Debug labels for profilers (Nsight), which name GPU work by them.
        let labels = std::env::var_os("NOX_GPU_LABELS").is_some_and(|v| !v.is_empty() && v != "0");
        if std::env::var_os("NOX_VULKAN_DEVICE").is_none_or(|v| v != "ffmpeg") {
            match Self::vulkan_by_nox(selection.as_deref(), labels) {
                Ok(created) => return Ok(created),
                Err(err) => {
                    tracing::warn!("creating the Vulkan device failed ({err}); FFmpeg creates it");
                }
            }
        }
        Self::vulkan_by_ffmpeg(selection.as_deref(), labels)
    }

    /// Nox's device (`nox_gpu::VulkanDevice`) in an FFmpeg device context.
    fn vulkan_by_nox(selection: Option<&str>, labels: bool) -> Result<(Self, Arc<GpuDevice>)> {
        let device =
            Arc::new(nox_gpu::VulkanDevice::create(selection, labels).map_err(Error::Gpu)?);
        // SAFETY: a fresh Vulkan device context, filled with the handles
        // and lists of `device`, which the context keeps alive (its `free`
        // releases it) and whose lists and feature chain live as long.
        unsafe {
            let mut buffer =
                ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN);
            if buffer.is_null() {
                return Err(Error::Vulkan("no FFmpeg Vulkan device context"));
            }
            let ctx = (*buffer).data.cast::<ffi::AVHWDeviceContext>();
            let hw = &mut *(*ctx).hwctx.cast::<ffi::AVVulkanDeviceContext>();
            hw.get_proc_addr = Some(std::mem::transmute::<
                vk::PFN_vkGetInstanceProcAddr,
                unsafe extern "C" fn(
                    ffi::VkInstance,
                    *const std::ffi::c_char,
                ) -> ffi::PFN_vkVoidFunction,
            >(device.get_instance_proc_addr()));
            hw.inst = vk::Handle::as_raw(device.instance()) as usize as ffi::VkInstance;
            hw.phys_dev =
                vk::Handle::as_raw(device.physical_device()) as usize as ffi::VkPhysicalDevice;
            hw.act_dev = vk::Handle::as_raw(device.device()) as usize as ffi::VkDevice;
            // The same structure (sType, pNext, the core features).
            const {
                assert!(
                    size_of::<ffi::VkPhysicalDeviceFeatures2>()
                        == size_of::<vk::PhysicalDeviceFeatures2>()
                );
            }
            ptr::copy_nonoverlapping(
                ptr::from_ref(device.features2()).cast::<u8>(),
                ptr::from_mut(&mut hw.device_features).cast::<u8>(),
                size_of::<ffi::VkPhysicalDeviceFeatures2>(),
            );
            hw.enabled_inst_extensions = device.instance_extensions().as_ptr();
            hw.nb_enabled_inst_extensions = device.instance_extensions().len() as i32;
            hw.enabled_dev_extensions = device.device_extensions().as_ptr();
            hw.nb_enabled_dev_extensions = device.device_extensions().len() as i32;
            for (slot, family) in hw.qf.iter_mut().zip(device.queue_families()) {
                *slot = ffi::AVVulkanDeviceQueueFamily {
                    idx: family.index as i32,
                    num: family.count as i32,
                    flags: family.flags.as_raw(),
                    video_caps: family.video_caps.as_raw() as i32,
                };
            }
            hw.nb_qf = device.queue_families().len().min(hw.qf.len()) as i32;
            // The queue locks are left to FFmpeg (its mutexes).
            (*ctx).user_opaque = Arc::into_raw(device.clone()).cast_mut().cast();
            (*ctx).free = Some(release_device);
            let ret = ffi::av_hwdevice_ctx_init(buffer);
            if ret < 0 {
                // Frees the context, and with it its reference.
                ffi::av_buffer_unref(&mut buffer);
                return Err(Error::Ffmpeg(ffmpeg::Error::from(ret)));
            }
            tracing::info!("Vulkan device created by Nox on {}", device.name());
            let hw = Self { buffer };
            let gpu = hw.gpu_device()?;
            Ok((hw, Arc::new(gpu)))
        }
    }

    /// The device FFmpeg creates (with its own choice of extensions and
    /// features).
    fn vulkan_by_ffmpeg(selection: Option<&str>, labels: bool) -> Result<(Self, Arc<GpuDevice>)> {
        let device_name = selection.and_then(|s| CString::new(s).ok());
        let mut options = ffmpeg::Dictionary::new();
        if labels {
            options.set("instance_extensions", "VK_EXT_debug_utils");
        }

        let mut buffer = ptr::null_mut();
        // SAFETY: valid out-pointer and arguments; the options dictionary is
        // consumed by FFmpeg via disown.
        let ret = unsafe {
            let mut opts = options.disown();
            let ret = ffi::av_hwdevice_ctx_create(
                &mut buffer,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                device_name.as_ref().map_or(ptr::null(), |n| n.as_ptr()),
                opts,
                0,
            );
            ffi::av_dict_free(&mut opts);
            ret
        };
        if ret < 0 {
            return Err(Error::Ffmpeg(ffmpeg::Error::from(ret)));
        }
        let hw = Self { buffer };
        let gpu = hw.gpu_device()?;
        Ok((hw, Arc::new(gpu)))
    }

    pub(crate) fn as_ptr(&self) -> *mut ffi::AVBufferRef {
        self.buffer
    }

    fn device_context(&self) -> *mut ffi::AVHWDeviceContext {
        // SAFETY: `buffer` wraps an AVHWDeviceContext.
        unsafe { (*self.buffer).data.cast() }
    }

    fn vulkan_context(&self) -> &ffi::AVVulkanDeviceContext {
        // SAFETY: the device type is Vulkan, so hwctx is an
        // AVVulkanDeviceContext, which lives as long as `buffer`.
        unsafe {
            &*(*self.device_context())
                .hwctx
                .cast::<ffi::AVVulkanDeviceContext>()
        }
    }

    /// The codecs Vulkan Video decodes on this device.
    pub fn decode_operations(&self) -> vk::VideoCodecOperationFlagsKHR {
        let ctx = self.vulkan_context();
        ctx.qf[..ctx.nb_qf.max(0) as usize]
            .iter()
            .filter(|qf| qf.flags & vk::QueueFlags::VIDEO_DECODE_KHR.as_raw() != 0)
            .fold(vk::VideoCodecOperationFlagsKHR::empty(), |all, qf| {
                all | vk::VideoCodecOperationFlagsKHR::from_raw(qf.video_caps as u32)
            })
    }

    fn gpu_device(&self) -> Result<GpuDevice> {
        let ctx = self.vulkan_context();
        let families = &ctx.qf[..ctx.nb_qf.max(0) as usize];
        let compute = families
            .iter()
            .find(|qf| qf.flags & vk::QueueFlags::COMPUTE.as_raw() != 0 && qf.num > 0)
            .ok_or(Error::Gpu(nox_gpu::Error::NoComputeQueue))?;
        let get_instance_proc_addr = ctx
            .get_proc_addr
            .ok_or(Error::Vulkan("no vkGetInstanceProcAddr"))?;

        let decode_codecs: Vec<_> = families
            .iter()
            .filter(|qf| qf.flags & vk::QueueFlags::VIDEO_DECODE_KHR.as_raw() != 0)
            .map(|qf| vk::VideoCodecOperationFlagsKHR::from_raw(qf.video_caps as u32))
            .collect();
        tracing::info!("Vulkan video decode: {decode_codecs:?}");
        tracing::debug!("Vulkan device extensions: {:?}", device_extensions(ctx));

        let external = ExternalDevice {
            // SAFETY: identical C ABI and signature (vkGetInstanceProcAddr).
            get_instance_proc_addr: unsafe {
                std::mem::transmute::<
                    unsafe extern "C" fn(
                        ffi::VkInstance,
                        *const std::ffi::c_char,
                    ) -> ffi::PFN_vkVoidFunction,
                    vk::PFN_vkGetInstanceProcAddr,
                >(get_instance_proc_addr)
            },
            instance: vk::Handle::from_raw(ctx.inst as u64),
            physical_device: vk::Handle::from_raw(ctx.phys_dev as u64),
            device: vk::Handle::from_raw(ctx.act_dev as u64),
            compute_queue_family: compute.idx as u32,
            enabled_extensions: device_extensions(ctx)
                .into_iter()
                .filter_map(|e| std::ffi::CString::new(e).ok())
                .collect(),
            features: features(ctx),
            debug_utils: instance_extensions(ctx)
                .iter()
                .any(|e| e == "VK_EXT_debug_utils"),
            queue_lock: Some(Box::new(FfmpegQueueLock(self.clone()))),
            owner: Box::new(self.clone()),
        };
        // SAFETY: handles come from FFmpeg's live device (which requires
        // Vulkan 1.3 and synchronization2); `owner` keeps it alive.
        unsafe { GpuDevice::from_external(external) }.map_err(Error::Gpu)
    }
}

/// The features FFmpeg enabled that nox-gpu's kernels may use, from its
/// chain of Vulkan feature structures.
fn features(ctx: &ffi::AVVulkanDeviceContext) -> Features {
    let mut features = Features::default();
    let mut next = ctx.device_features.pNext.cast_const();
    while !next.is_null() {
        // SAFETY: FFmpeg's chain of Vulkan feature structures, each
        // starting with sType and pNext.
        unsafe {
            let base = &*next.cast::<vk::BaseInStructure>();
            match base.s_type {
                vk::StructureType::PHYSICAL_DEVICE_VULKAN_1_1_FEATURES => {
                    let f = &*next.cast::<vk::PhysicalDeviceVulkan11Features>();
                    features.storage_16bit |= f.storage_buffer16_bit_access != 0;
                }
                vk::StructureType::PHYSICAL_DEVICE_16BIT_STORAGE_FEATURES => {
                    let f = &*next.cast::<vk::PhysicalDevice16BitStorageFeatures>();
                    features.storage_16bit |= f.storage_buffer16_bit_access != 0;
                }
                vk::StructureType::PHYSICAL_DEVICE_VULKAN_1_2_FEATURES => {
                    let f = &*next.cast::<vk::PhysicalDeviceVulkan12Features>();
                    features.float16 |= f.shader_float16 != 0;
                    features.memory_model |= f.vulkan_memory_model != 0;
                }
                vk::StructureType::PHYSICAL_DEVICE_COOPERATIVE_MATRIX_FEATURES_KHR => {
                    let f = &*next.cast::<vk::PhysicalDeviceCooperativeMatrixFeaturesKHR>();
                    features.cooperative_matrix |= f.cooperative_matrix != 0;
                }
                // VK_NV_cooperative_matrix2 (newer than ash): workgroup
                // scope, flexible dimensions, ..., tensor addressing.
                s if s.as_raw() == 1_000_593_000 => {
                    let flags = next
                        .cast::<u8>()
                        .add(size_of::<vk::BaseInStructure>())
                        .cast::<[vk::Bool32; 7]>()
                        .read_unaligned();
                    features.cooperative_matrix2 |= flags[0] != 0 && flags[1] != 0 && flags[5] != 0;
                }
                _ => {}
            }
            next = base.p_next.cast();
        }
    }
    features
}

/// `AVHWDeviceContext::free` of a context holding Nox's device: lets go of
/// it once FFmpeg (decoders, frames, nox-gpu's `GpuDevice`) is done.
unsafe extern "C" fn release_device(ctx: *mut ffi::AVHWDeviceContext) {
    // SAFETY: `user_opaque` holds the reference `vulkan_by_nox` made.
    unsafe {
        drop(Arc::from_raw(
            (*ctx).user_opaque.cast::<nox_gpu::VulkanDevice>(),
        ))
    };
}

/// Extensions FFmpeg enabled on the instance.
fn instance_extensions(ctx: &ffi::AVVulkanDeviceContext) -> Vec<String> {
    let count = ctx.nb_enabled_inst_extensions.max(0) as usize;
    (0..count)
        // SAFETY: FFmpeg keeps `count` valid C strings.
        .map(|i| unsafe { std::ffi::CStr::from_ptr(*ctx.enabled_inst_extensions.add(i)) })
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

/// Extensions FFmpeg enabled on the device.
fn device_extensions(ctx: &ffi::AVVulkanDeviceContext) -> Vec<String> {
    let count = ctx.nb_enabled_dev_extensions.max(0) as usize;
    (0..count)
        // SAFETY: FFmpeg keeps `count` valid C strings.
        .map(|i| unsafe { std::ffi::CStr::from_ptr(*ctx.enabled_dev_extensions.add(i)) })
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

/// Uses FFmpeg's queue locks so our submissions don't race its decoder.
struct FfmpegQueueLock(HwDevice);

impl QueueLock for FfmpegQueueLock {
    fn lock(&self, family: u32, index: u32) {
        if let Some(lock) = self.0.vulkan_context().lock_queue {
            // SAFETY: valid device context, as FFmpeg's API requires.
            unsafe { lock(self.0.device_context(), family, index) };
        }
    }

    fn unlock(&self, family: u32, index: u32) {
        if let Some(unlock) = self.0.vulkan_context().unlock_queue {
            // SAFETY: as above; pairs with lock.
            unsafe { unlock(self.0.device_context(), family, index) };
        }
    }
}
