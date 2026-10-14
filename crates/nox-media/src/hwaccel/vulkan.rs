//! Vulkan Video on the device Nox renders with, both ways, without copies:
//!
//! - decoding: a decoded `AV_PIX_FMT_VULKAN` frame's images are described
//!   for the converter (`with_gpu_input`, `nox_gpu::Input::Vulkan`);
//! - encoding (`h264_vulkan`, `hevc_vulkan`): the compositor writes Y'CbCr
//!   into the encoder's input images (`VulkanSink`,
//!   `nox_gpu::GpuTarget::Images`).
//!
//! Both follow FFmpeg's protocol for `AVVkFrame`s (`LockedFrame`): lock the
//! frame, wait on each image's timeline semaphore at `sem_value`, signal
//! `sem_value + 1`, record the new layout, access and queue family, unlock.

use ffmpeg::{ffi, format, frame};
use nox_gpu::{GpuTarget, ImageSync, Input, Layout, PlaneFormat, VkFrame, VkPlane, YuvTarget, vk};
use std::sync::atomic::{AtomicBool, Ordering};

use super::{GpuSink, Render, ycbcr_matrix};
use crate::frames::{Classified, classify};
use crate::{Error, HwDevice, Result};

// Decoding.

/// Calls `f` with the best GPU input for `frame`: its own Vulkan images when
/// possible (no copies), otherwise a CPU view (downloading hardware frames).
pub fn with_gpu_input<R>(
    frame: &frame::Video,
    f: impl FnOnce(Input<'_, '_>) -> R,
) -> crate::Result<R> {
    if frame.format() == format::Pixel::VULKAN {
        // SAFETY: `frame` is a valid Vulkan frame.
        match unsafe { DecodedFrame::new(frame) } {
            Ok(decoded) => {
                log_path_once(None);
                // SAFETY: `decoded` was built from `frame`, which outlives the call.
                return Ok(unsafe { decoded.run(f) });
            }
            Err(reason) => log_path_once(Some(reason)),
        }
    }
    crate::with_cpu_frame(frame, |cpu| f(Input::Cpu(cpu)))
}

/// Logs (once per process and outcome) whether decoded frames are used
/// in place or downloaded.
fn log_path_once(fallback: Option<&'static str>) {
    static ZERO_COPY: AtomicBool = AtomicBool::new(false);
    static DOWNLOAD: AtomicBool = AtomicBool::new(false);
    match fallback {
        None if !ZERO_COPY.swap(true, Ordering::Relaxed) => {
            tracing::info!("using Vulkan-decoded frames in place (zero copy)")
        }
        Some(reason) if !DOWNLOAD.swap(true, Ordering::Relaxed) => {
            tracing::info!("downloading Vulkan-decoded frames: {reason}")
        }
        _ => {}
    }
}

/// A decoded frame described for the converter, gathered before locking it.
struct DecodedFrame {
    frames_ctx: *mut ffi::AVHWFramesContext,
    vk_frame: *mut ffi::AVVkFrame,
    template: VkFrame,
}

impl DecodedFrame {
    /// # Safety
    /// `frame` must be a valid `AV_PIX_FMT_VULKAN` frame.
    unsafe fn new(frame: &frame::Video) -> Result<Self, &'static str> {
        // SAFETY: guaranteed by the caller; FFmpeg keeps these alive as long
        // as the frame.
        unsafe {
            let av = frame.as_ptr();
            let frames_ref = (*av).hw_frames_ctx;
            if frames_ref.is_null() {
                return Err("frame has no hardware frames context");
            }
            let frames_ctx = (*frames_ref).data.cast::<ffi::AVHWFramesContext>();
            if vulkan_frames_context(frames_ctx).img_flags
                & vk::ImageCreateFlags::MUTABLE_FORMAT.as_raw()
                == 0
            {
                return Err(
                    "decoder images are not created with MUTABLE_FORMAT (no per-plane views)",
                );
            }
            let sw_format = format::Pixel::from((*frames_ctx).sw_format);
            let Classified {
                layout,
                swap_chroma,
                matrix,
                chroma_offset,
                color,
                planes,
            } = classify(sw_format, frame).ok_or("unsupported decoder output format")?;
            if layout == Layout::Packed {
                return Err("packed decoder output format");
            }
            let vk_frame = (*av).data[0].cast::<ffi::AVVkFrame>();
            let template = VkFrame {
                width: frame.width(),
                height: frame.height(),
                layout,
                swap_chroma,
                matrix,
                chroma_offset,
                color,
                planes: vk_planes(frames_ctx, vk_frame, &planes),
                images: Vec::new(),
            };
            Ok(Self {
                frames_ctx,
                vk_frame,
                template,
            })
        }
    }

    /// Locks the frame, runs `f` on it and writes back the new state.
    ///
    /// # Safety
    /// The frame `self` was built from must still be alive.
    unsafe fn run<R>(mut self, f: impl FnOnce(Input<'_, '_>) -> R) -> R {
        // SAFETY: guaranteed by the caller.
        let mut locked = unsafe { LockedFrame::new(self.frames_ctx, self.vk_frame) };
        self.template.images = std::mem::take(&mut locked.images);
        let result = f(Input::Vulkan(&mut self.template));
        locked.images = std::mem::take(&mut self.template.images);
        result
    }
}

// Encoding.

/// A Vulkan Video encoder's frames pool: NV12 images, or P010 for HDR,
/// that the compositor writes.
pub(crate) struct VulkanSink {
    device: HwDevice,
    frames: *mut ffi::AVBufferRef,
    matrix: nox_color::Matrix3x4,
    deep: bool,
}

impl VulkanSink {
    pub fn new(device: &HwDevice, width: u32, height: u32, deep: bool) -> Result<Self> {
        // SAFETY: a valid Vulkan device context.
        let frames = unsafe { encoder_frames(device, width, height, deep)? };
        Ok(Self {
            device: device.clone(),
            frames,
            matrix: ycbcr_matrix(deep),
            deep,
        })
    }
}

impl Drop for VulkanSink {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { ffi::av_buffer_unref(&mut self.frames) };
    }
}

impl GpuSink for VulkanSink {
    fn api(&self) -> &'static str {
        "Vulkan"
    }

    fn pixel_format(&self) -> format::Pixel {
        format::Pixel::VULKAN
    }

    fn frames_context(&self) -> *mut ffi::AVBufferRef {
        self.frames
    }

    fn device_context(&self) -> *mut ffi::AVBufferRef {
        self.device.as_ptr()
    }

    fn next(&mut self, render: Render<'_>) -> Result<frame::Video> {
        let mut frame = frame::Video::empty();
        // SAFETY: valid frames context; the frame takes a pool image.
        let ret = unsafe { ffi::av_hwframe_get_buffer(self.frames, frame.as_mut_ptr(), 0) };
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret).into());
        }
        // SAFETY: `frame` is a Vulkan frame of `frames`, alive for the call;
        // the lock brackets all access to its state.
        unsafe {
            let ctx = (*self.frames).data.cast::<ffi::AVHWFramesContext>();
            let vkf = (*frame.as_ptr()).data[0].cast::<ffi::AVVkFrame>();
            let (luma, chroma) = if self.deep {
                (PlaneFormat::R16, PlaneFormat::Rg16)
            } else {
                (PlaneFormat::R8, PlaneFormat::Rg8)
            };
            let mut planes = vk_planes(ctx, vkf, &[(luma, 0, 0), (chroma, 1, 1)]).into_iter();
            let mut locked = LockedFrame::new(ctx, vkf);
            let mut target = YuvTarget {
                width: (*ctx).width as u32,
                height: (*ctx).height as u32,
                luma: planes.next().expect("luma"),
                chroma: planes.next().expect("chroma"),
                images: std::mem::take(&mut locked.images),
                matrix: self.matrix,
            };
            let result = render(GpuTarget::Images(&mut target));
            locked.images = std::mem::take(&mut target.images);
            result.map_err(Error::from)?;
        }
        Ok(frame)
    }
}

/// An NV12 (with `deep` P010) frames pool on `device` whose images the
/// compositor can write (storage views of the planes) and a Vulkan encoder
/// can read.
///
/// # Safety
/// `device` must be a valid Vulkan device context.
unsafe fn encoder_frames(
    device: &HwDevice,
    width: u32,
    height: u32,
    deep: bool,
) -> Result<*mut ffi::AVBufferRef> {
    // SAFETY: guaranteed by the caller; fields are set before init.
    unsafe {
        let mut frames = ffi::av_hwframe_ctx_alloc(device.as_ptr());
        if frames.is_null() {
            return Err(Error::Encode("cannot allocate Vulkan frames".into()));
        }
        let ctx = (*frames).data.cast::<ffi::AVHWFramesContext>();
        (*ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN;
        (*ctx).sw_format = if deep {
            ffi::AVPixelFormat::AV_PIX_FMT_P010LE
        } else {
            ffi::AVPixelFormat::AV_PIX_FMT_NV12
        };
        (*ctx).width = width as i32;
        (*ctx).height = height as i32;
        let vk_ctx = (*ctx).hwctx.cast::<ffi::AVVulkanFramesContext>();
        // FFmpeg's defaults plus storage, for the compositor's plane views
        // (with extended usage, as NV12 itself isn't a storage format).
        (*vk_ctx).usage = (vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::STORAGE
            | vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR)
            .as_raw() as _;
        (*vk_ctx).img_flags = (vk::ImageCreateFlags::MUTABLE_FORMAT
            | vk::ImageCreateFlags::EXTENDED_USAGE
            | vk::ImageCreateFlags::VIDEO_PROFILE_INDEPENDENT_KHR)
            .as_raw() as _;
        let ret = ffi::av_hwframe_ctx_init(frames);
        if ret < 0 {
            ffi::av_buffer_unref(&mut frames);
            return Err(ffmpeg::Error::from(ret).into());
        }
        Ok(frames)
    }
}

// FFmpeg's AVVkFrame protocol, for both directions.

/// The planes of an `AVVkFrame` as views for nox-gpu: one multi-planar
/// image (plane aspects) or one image per plane. `planes` gives each
/// plane's texel format and log2 subsampling (w, h).
///
/// # Safety
/// `frames_ctx` must be the Vulkan frames context of `vkf`.
unsafe fn vk_planes(
    frames_ctx: *mut ffi::AVHWFramesContext,
    vkf: *mut ffi::AVVkFrame,
    planes: &[(PlaneFormat, u8, u8)],
) -> Vec<VkPlane> {
    const PLANE_ASPECTS: [vk::ImageAspectFlags; 3] = [
        vk::ImageAspectFlags::PLANE_0,
        vk::ImageAspectFlags::PLANE_1,
        vk::ImageAspectFlags::PLANE_2,
    ];
    // SAFETY: guaranteed by the caller.
    unsafe {
        let images = &(*vkf).img;
        let multi_planar = images[1] == 0;
        let (width, height) = ((*frames_ctx).width as u32, (*frames_ctx).height as u32);
        planes
            .iter()
            .enumerate()
            .map(|(i, &(format, log2_w, log2_h))| VkPlane {
                image: vk::Handle::from_raw(if multi_planar { images[0] } else { images[i] }),
                aspect: if multi_planar {
                    PLANE_ASPECTS[i]
                } else {
                    vk::ImageAspectFlags::COLOR
                },
                format,
                width: width.div_ceil(1 << log2_w),
                height: height.div_ceil(1 << log2_h),
            })
            .collect()
    }
}

/// An `AVVkFrame` locked for our use, with its images' synchronization
/// state as `ImageSync`s. Whatever `images` holds when it drops is written
/// back to the frame (as FFmpeg's protocol requires), then the frame is
/// unlocked, also if the work in between panicked.
struct LockedFrame {
    vkf: *mut ffi::AVVkFrame,
    images: Vec<ImageSync>,
    // Dropped after `drop` wrote the state back.
    _lock: FrameLock,
}

impl LockedFrame {
    /// # Safety
    /// `frames_ctx` must be the Vulkan frames context of `vkf`; both must
    /// outlive the value.
    unsafe fn new(frames_ctx: *mut ffi::AVHWFramesContext, vkf: *mut ffi::AVVkFrame) -> Self {
        // SAFETY: guaranteed by the caller; read under the lock.
        unsafe {
            let lock = FrameLock::new(frames_ctx, vkf);
            let frame = &*vkf;
            let count = frame.img.iter().take_while(|&&image| image != 0).count();
            let images = (0..count)
                .map(|i| ImageSync {
                    image: vk::Handle::from_raw(frame.img[i]),
                    layout: vk::ImageLayout::from_raw(frame.layout[i]),
                    access: frame.access[i].to_flags2(),
                    queue_family: frame.queue_family[i],
                    semaphore: vk::Handle::from_raw(frame.sem[i]),
                    value: frame.sem_value[i],
                })
                .collect();
            Self {
                vkf,
                images,
                _lock: lock,
            }
        }
    }
}

impl Drop for LockedFrame {
    fn drop(&mut self) {
        // SAFETY: valid per `new`; we hold the lock.
        let frame = unsafe { &mut *self.vkf };
        for (i, sync) in self.images.iter().enumerate() {
            frame.layout[i] = sync.layout.as_raw();
            frame.access[i] = AccessField::from_flags2(sync.access);
            frame.queue_family[i] = sync.queue_family;
            frame.sem_value[i] = sync.value;
        }
    }
}

/// `AVVkFrame::access`: `VkAccessFlagBits` (32 bit) up to FFmpeg 8,
/// `VkAccessFlags2` (64 bit) from FFmpeg 9. Converted by type, so it works
/// with either, without losing synchronization2's high bits (e.g. video
/// decode writes, bit 35).
trait AccessField: Copy {
    fn to_flags2(self) -> vk::AccessFlags2;
    fn from_flags2(flags: vk::AccessFlags2) -> Self;
}

impl AccessField for i32 {
    fn to_flags2(self) -> vk::AccessFlags2 {
        // The 32-bit flags are the low bits of their synchronization2 forms.
        vk::AccessFlags2::from_raw(u64::from(self as u32))
    }

    fn from_flags2(flags: vk::AccessFlags2) -> Self {
        // Only the 32-bit range exists in FFmpeg 8's field.
        flags.as_raw() as u32 as i32
    }
}

impl AccessField for u64 {
    fn to_flags2(self) -> vk::AccessFlags2 {
        vk::AccessFlags2::from_raw(self)
    }

    fn from_flags2(flags: vk::AccessFlags2) -> Self {
        flags.as_raw()
    }
}

/// FFmpeg's lock on an `AVVkFrame`, released on drop: a panic while it is
/// held must not leave the frame locked, or FFmpeg's decoder and encoder
/// threads would wait for it forever.
struct FrameLock {
    frames_ctx: *mut ffi::AVHWFramesContext,
    frame: *mut ffi::AVVkFrame,
}

impl FrameLock {
    /// # Safety
    /// `frames_ctx` must be a Vulkan frames context and `frame` one of its
    /// frames; both must outlive the lock.
    unsafe fn new(frames_ctx: *mut ffi::AVHWFramesContext, frame: *mut ffi::AVVkFrame) -> Self {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if let Some(lock) = vulkan_frames_context(frames_ctx).lock_frame {
                lock(frames_ctx, frame);
            }
        }
        Self { frames_ctx, frame }
    }
}

impl Drop for FrameLock {
    fn drop(&mut self) {
        // SAFETY: valid per `new`; we hold the lock.
        unsafe {
            if let Some(unlock) = vulkan_frames_context(self.frames_ctx).unlock_frame {
                unlock(self.frames_ctx, self.frame);
            }
        }
    }
}

/// # Safety
/// `frames_ctx` must be a valid Vulkan frames context.
unsafe fn vulkan_frames_context<'a>(
    frames_ctx: *mut ffi::AVHWFramesContext,
) -> &'a ffi::AVVulkanFramesContext {
    // SAFETY: guaranteed by the caller.
    unsafe { &*(*frames_ctx).hwctx.cast::<ffi::AVVulkanFramesContext>() }
}
