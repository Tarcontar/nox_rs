//! VAAPI on the GPU Nox renders with (Linux: AMD and Intel; NVIDIA
//! through nvidia-vaapi-driver, which decodes only), both ways. VAAPI
//! frames are DMA-BUFs, which FFmpeg maps to Vulkan images on Nox's device
//! (VAAPI → DRM PRIME → `AVVkFrame`, imported with their DRM modifiers):
//!
//! - decoding: each decoded `AV_PIX_FMT_VAAPI` frame is mapped for reading,
//!   and the converter reads it in place like Vulkan Video's frames
//!   (`MappedFrames`);
//! - encoding (`h264_vaapi`, `hevc_vaapi`): each of the encoder's surfaces
//!   is mapped for writing, and the compositor writes Y'CbCr into it as
//!   into Vulkan Video's (`VaapiSink`).
//!
//! No copies either way.
//!
//! The VAAPI device is the one on the Vulkan device's DRM render node
//! (`VK_EXT_physical_device_drm`), so both are the same GPU.

use ffmpeg::{ffi, format, frame};
use nox_gpu::GpuDevice;
use std::ffi::CString;
use std::ptr;
use std::sync::{Arc, Mutex};

use super::{GpuSink, Render, ycbcr_matrix};
use crate::{Error, HwDevice, Result};

/// FFmpeg's VAAPI device on the GPU of a Vulkan device.
pub(crate) struct VaapiDevice {
    buffer: *mut ffi::AVBufferRef,
}

// SAFETY: AVBufferRef reference counting is atomic; FFmpeg serializes its
// own use of the VA display.
unsafe impl Send for VaapiDevice {}
unsafe impl Sync for VaapiDevice {}

impl VaapiDevice {
    /// The VAAPI device on the same GPU as `gpu` (its DRM render node).
    pub(crate) fn for_gpu(gpu: &GpuDevice) -> Result<Self> {
        let node = gpu
            .drm_render_node()
            .ok_or_else(|| Error::Encode("the Vulkan driver names no DRM render node".into()))?;
        let name = CString::new(node.to_string_lossy().into_owned())
            .map_err(|_| Error::Encode("bad render node path".into()))?;
        let mut buffer = ptr::null_mut();
        // GPUs without a VA driver are an answer here, not an error for
        // FFmpeg's log.
        // SAFETY: valid out-pointer and arguments.
        let ret = crate::stream::with_quiet_log(|| unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut buffer,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                name.as_ptr(),
                ptr::null_mut(),
                0,
            )
        });
        if ret < 0 {
            return Err(Error::Encode(format!(
                "no VAAPI on {}: {}",
                node.display(),
                ffmpeg::Error::from(ret)
            )));
        }
        Ok(Self { buffer })
    }

    /// FFmpeg's device context (borrowed), for decoders.
    pub(crate) fn as_ptr(&self) -> *mut ffi::AVBufferRef {
        self.buffer
    }
}

impl Drop for VaapiDevice {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { ffi::av_buffer_unref(&mut self.buffer) };
    }
}

/// Vulkan frames contexts derived from decoders' VAAPI frames contexts,
/// which VAAPI frames are mapped into. Kept per source (a decoder's frames
/// all share one), the most recent few.
#[derive(Default)]
pub(crate) struct MappedFrames(Mutex<Vec<Derived>>);

/// Sources whose derived contexts are kept.
const KEPT_SOURCES: usize = 4;

struct Derived {
    /// A reference to the source, so its address can't be reused while
    /// it's the key.
    source: *mut ffi::AVBufferRef,
    derived: *mut ffi::AVBufferRef,
}

// SAFETY: AVBufferRef reference counting is atomic.
unsafe impl Send for Derived {}

impl Drop for Derived {
    fn drop(&mut self) {
        // SAFETY: we own one reference to each.
        unsafe {
            ffi::av_buffer_unref(&mut self.derived);
            ffi::av_buffer_unref(&mut self.source);
        }
    }
}

impl MappedFrames {
    /// The decoded VAAPI `frame` as a Vulkan frame on `vulkan` (its
    /// DMA-BUFs imported, no copies), with the frame's properties.
    ///
    /// # Safety
    /// `frame` must be a VAAPI frame decoded on the GPU of `vulkan`.
    pub(crate) unsafe fn map(
        &self,
        vulkan: &HwDevice,
        frame: &frame::Video,
    ) -> Result<frame::Video> {
        // SAFETY: guaranteed by the caller; FFmpeg keeps the frames context
        // alive as long as the frame.
        unsafe {
            let source = (*frame.as_ptr()).hw_frames_ctx;
            if source.is_null() {
                return Err(Error::Encode("VAAPI frame without frames context".into()));
            }
            let derived = {
                let mut cache = self.0.lock().expect("mapped frames lock");
                let found = cache
                    .iter()
                    .find(|d| (*d.source).data == (*source).data)
                    .map(|d| d.derived);
                let derived = match found {
                    Some(derived) => derived,
                    None => {
                        let mut derived = ptr::null_mut();
                        let ret = ffi::av_hwframe_ctx_create_derived(
                            &mut derived,
                            ffi::AVPixelFormat::AV_PIX_FMT_VULKAN,
                            vulkan.as_ptr(),
                            source,
                            ffi::AV_HWFRAME_MAP_READ as i32,
                        );
                        if ret < 0 {
                            return Err(Error::Encode(format!(
                                "no Vulkan frames for VAAPI's: {}",
                                ffmpeg::Error::from(ret)
                            )));
                        }
                        if cache.len() >= KEPT_SOURCES {
                            cache.remove(0);
                        }
                        cache.push(Derived {
                            source: ffi::av_buffer_ref(source),
                            derived,
                        });
                        derived
                    }
                };
                ffi::av_buffer_ref(derived)
            };
            let mut mapped = frame::Video::empty();
            let dst = mapped.as_mut_ptr();
            (*dst).format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as i32;
            (*dst).hw_frames_ctx = derived;
            let ret = ffi::av_hwframe_map(dst, frame.as_ptr(), ffi::AV_HWFRAME_MAP_READ as i32);
            if ret < 0 {
                return Err(Error::Encode(format!(
                    "cannot map VAAPI frames to Vulkan: {}",
                    ffmpeg::Error::from(ret)
                )));
            }
            // Color tags, timestamps: what the converter reads.
            ffi::av_frame_copy_props(dst, frame.as_ptr());
            Ok(mapped)
        }
    }
}

/// A VAAPI encoder's surfaces (NV12, or P010 for HDR), mapped to Vulkan
/// for the compositor to write.
pub(crate) struct VaapiSink {
    device: Arc<VaapiDevice>,
    /// The encoder's `AV_PIX_FMT_VAAPI` frames.
    frames: *mut ffi::AVBufferRef,
    /// Vulkan frames the surfaces are mapped into.
    mapped: *mut ffi::AVBufferRef,
    matrix: nox_color::Matrix3x4,
    deep: bool,
}

impl VaapiSink {
    pub fn new(
        device: Arc<VaapiDevice>,
        vulkan: &HwDevice,
        width: u32,
        height: u32,
        deep: bool,
    ) -> Result<Self> {
        // SAFETY: valid device contexts; fields are set before init, and
        // every reference made here is owned by the sink (or released on
        // error).
        unsafe {
            let mut frames = super::frames_context(
                device.as_ptr(),
                ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
                width,
                height,
                deep,
                // A pool that grows as the encoder needs surfaces.
                |ctx| (*ctx).initial_pool_size = 0,
            )?;
            let mut mapped = ptr::null_mut();
            let ret = ffi::av_hwframe_ctx_create_derived(
                &mut mapped,
                ffi::AVPixelFormat::AV_PIX_FMT_VULKAN,
                vulkan.as_ptr(),
                frames,
                ffi::AV_HWFRAME_MAP_WRITE as i32 | ffi::AV_HWFRAME_MAP_OVERWRITE as i32,
            );
            if ret < 0 {
                ffi::av_buffer_unref(&mut frames);
                return Err(Error::Encode(format!(
                    "no Vulkan frames for VAAPI's: {}",
                    ffmpeg::Error::from(ret)
                )));
            }
            Ok(Self {
                device,
                frames,
                mapped,
                matrix: ycbcr_matrix(deep),
                deep,
            })
        }
    }
}

impl Drop for VaapiSink {
    fn drop(&mut self) {
        // SAFETY: we own one reference to each.
        unsafe {
            ffi::av_buffer_unref(&mut self.mapped);
            ffi::av_buffer_unref(&mut self.frames);
        }
    }
}

impl GpuSink for VaapiSink {
    fn api(&self) -> &'static str {
        "VAAPI"
    }

    fn pixel_format(&self) -> format::Pixel {
        format::Pixel::VAAPI
    }

    fn frames_context(&self) -> *mut ffi::AVBufferRef {
        self.frames
    }

    fn device_context(&self) -> *mut ffi::AVBufferRef {
        self.device.as_ptr()
    }

    fn next(&mut self, render: Render<'_>) -> Result<frame::Video> {
        let mut surface = frame::Video::empty();
        // SAFETY: valid frames context; the frame takes a pool surface.
        let ret = unsafe { ffi::av_hwframe_get_buffer(self.frames, surface.as_mut_ptr(), 0) };
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret).into());
        }
        {
            let mut mapped = frame::Video::empty();
            // SAFETY: the surface maps into our derived Vulkan frames; the
            // mapping (dropped at the end of this block, after the
            // compositor's work has completed) keeps the surface's memory.
            unsafe {
                let dst = mapped.as_mut_ptr();
                (*dst).format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as i32;
                (*dst).hw_frames_ctx = ffi::av_buffer_ref(self.mapped);
                let ret = ffi::av_hwframe_map(
                    dst,
                    surface.as_ptr(),
                    ffi::AV_HWFRAME_MAP_WRITE as i32 | ffi::AV_HWFRAME_MAP_OVERWRITE as i32,
                );
                if ret < 0 {
                    return Err(Error::Encode(format!(
                        "cannot map VAAPI surfaces to Vulkan: {}",
                        ffmpeg::Error::from(ret)
                    )));
                }
                super::vulkan::render_into(&mapped, self.matrix, self.deep, render)?;
            }
        }
        Ok(surface)
    }
}
