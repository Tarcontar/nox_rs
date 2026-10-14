//! The Vulkan device shared between FFmpeg (hardware decoding) and nox-gpu
//! (color conversion, compositing).
//!
//! FFmpeg creates the device so it enables exactly the video queues,
//! extensions and features its decoders need; nox-gpu then loads the same
//! handles into ash. `NOX_GPU` selects the device (index or name substring,
//! as accepted by FFmpeg). For validation, run under `tools/vk-validate.sh`
//! rather than FFmpeg's `debug` option, whose layer settings crash Vulkan
//! Video decoding on NVIDIA.

use ffmpeg::ffi;
use nox_gpu::{ExternalDevice, GpuDevice, QueueLock, vk};
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
        let device_name = std::env::var("NOX_GPU")
            .ok()
            .and_then(|s| CString::new(s).ok());
        let options = ffmpeg::Dictionary::new();

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
            queue_lock: Some(Box::new(FfmpegQueueLock(self.clone()))),
            owner: Box::new(self.clone()),
        };
        // SAFETY: handles come from FFmpeg's live device (which requires
        // Vulkan 1.3 and synchronization2); `owner` keeps it alive.
        unsafe { GpuDevice::from_external(external) }.map_err(Error::Gpu)
    }
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
