//! Vulkan Video encoders (`h264_vulkan`, `hevc_vulkan`) on the device Nox
//! renders with: the compositor writes Y'CbCr straight into the encoder's
//! input images (`nox_gpu::GpuTarget::Images`).

use ffmpeg::{ffi, format, frame};
use nox_gpu::{GpuTarget, ImageSync, PlaneFormat, VkPlane, YuvTarget, vk};

use super::{GpuFrames, Render, ycbcr_matrix};
use crate::vulkan_frame::FrameLock;
use crate::{Error, HwDevice, Result};

/// The encoder's frames pool: NV12 images, or P010 for HDR.
pub(crate) struct VulkanFrames {
    device: HwDevice,
    frames: *mut ffi::AVBufferRef,
    matrix: nox_color::Matrix3x4,
    deep: bool,
}

impl VulkanFrames {
    pub fn new(device: &HwDevice, width: u32, height: u32, deep: bool) -> Result<Self> {
        // SAFETY: a valid Vulkan device context.
        let frames = unsafe { vulkan_frames(device, width, height, deep)? };
        Ok(Self {
            device: device.clone(),
            frames,
            matrix: ycbcr_matrix(deep),
            deep,
        })
    }
}

impl Drop for VulkanFrames {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { ffi::av_buffer_unref(&mut self.frames) };
    }
}

impl GpuFrames for VulkanFrames {
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
        // SAFETY: `frame` is a Vulkan frame of `frames`, alive for the call.
        unsafe {
            render_into(self.frames, &frame, self.matrix, self.deep, |target| {
                render(GpuTarget::Images(target))
            })?
        };
        Ok(frame)
    }
}

/// An NV12 (with `deep` P010) frames pool on `device` whose images the
/// compositor can write (storage views of the planes) and a Vulkan encoder
/// can read.
///
/// # Safety
/// `device` must be a valid Vulkan device context.
unsafe fn vulkan_frames(
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

/// Describes the NV12 Vulkan `frame` for the compositor and runs `render`
/// on it, following FFmpeg's frame protocol (lock, semaphores, recorded
/// state, unlock).
///
/// # Safety
/// `frame` must be a valid Vulkan frame from `frames`.
unsafe fn render_into(
    frames: *mut ffi::AVBufferRef,
    frame: &frame::Video,
    matrix: nox_color::Matrix3x4,
    deep: bool,
    render: impl FnOnce(&mut YuvTarget) -> nox_gpu::Result<()>,
) -> Result<()> {
    // SAFETY: guaranteed by the caller; lock/unlock bracket all access to
    // the AVVkFrame state, as FFmpeg requires.
    unsafe {
        let ctx = (*frames).data.cast::<ffi::AVHWFramesContext>();
        let vkf = (*frame.as_ptr()).data[0].cast::<ffi::AVVkFrame>();
        let (width, height) = ((*ctx).width as u32, (*ctx).height as u32);
        let multi_planar = (*vkf).img[1] == 0;
        let plane = |i: usize, format: PlaneFormat, w: u32, h: u32| VkPlane {
            image: vk::Handle::from_raw((*vkf).img[if multi_planar { 0 } else { i }]),
            aspect: if multi_planar {
                [vk::ImageAspectFlags::PLANE_0, vk::ImageAspectFlags::PLANE_1][i]
            } else {
                vk::ImageAspectFlags::COLOR
            },
            format,
            width: w,
            height: h,
        };
        let _lock = FrameLock::new(ctx, vkf);
        let count = if multi_planar { 1 } else { 2 };
        let mut target = YuvTarget {
            width,
            height,
            luma: plane(
                0,
                if deep {
                    PlaneFormat::R16
                } else {
                    PlaneFormat::R8
                },
                width,
                height,
            ),
            chroma: plane(
                1,
                if deep {
                    PlaneFormat::Rg16
                } else {
                    PlaneFormat::Rg8
                },
                width.div_ceil(2),
                height.div_ceil(2),
            ),
            images: (0..count)
                .map(|i| ImageSync {
                    image: vk::Handle::from_raw((*vkf).img[i]),
                    layout: vk::ImageLayout::from_raw((*vkf).layout[i]),
                    access: vk::AccessFlags::from_raw((*vkf).access[i] as u32),
                    queue_family: (*vkf).queue_family[i],
                    semaphore: vk::Handle::from_raw((*vkf).sem[i]),
                    value: (*vkf).sem_value[i],
                })
                .collect(),
            matrix,
        };
        let result = render(&mut target);
        for (i, sync) in target.images.iter().enumerate() {
            (*vkf).layout[i] = sync.layout.as_raw();
            (*vkf).access[i] = sync.access.as_raw() as _;
            (*vkf).queue_family[i] = sync.queue_family;
            (*vkf).sem_value[i] = sync.value;
        }
        result.map_err(Error::from)
    }
}
