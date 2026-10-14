//! Hardware encoders that take their frames on the GPU, so rendered
//! pictures never pass through system memory.
//!
//! Every encoder API wants frames in its own kind of memory. A backend
//! (`GpuFrames`) owns the encoder's frames pool and lends each frame to the
//! renderer as a `nox_gpu::GpuTarget` the compositor writes Y'CbCr into,
//! then hands it to FFmpeg as a hardware frame:
//!
//! - `vulkan`: Vulkan Video (`*_vulkan`). The frames are images of the
//!   Vulkan device Nox renders with (`GpuTarget::Images`).
//! - `cuda`: NVENC (`*_nvenc`). The frames are Vulkan buffers exported to
//!   CUDA on the same GPU (`GpuTarget::Buffer`).
//!
//! Other APIs fit the same shape: VAAPI through DMA-BUF (images or buffers
//! exported as `DRM_PRIME`), D3D11 for AMF, QSV and NVENC on Windows through
//! shared NT handles, VideoToolbox through IOSurface. Each needs a
//! `nox_gpu::MemoryHandle` kind, a backend here, and its encoders in
//! `open`.
//!
//! Encoders without a backend, or whose backend can't be set up (e.g. no
//! CUDA driver), take frames from memory.

mod cuda;
mod vulkan;

use ffmpeg::{ffi, format, frame};
use nox_color::ycbcr::{self, Coefficients, Quantization};
use nox_gpu::GpuTarget;

use crate::encode::OutputSettings;
use crate::{Error, Result};

/// Writes one frame into the target it's given (the compositor).
pub(crate) type Render<'a> = &'a mut dyn FnMut(GpuTarget<'_>) -> nox_gpu::Result<()>;

/// The frames pool of a hardware encoder that takes frames on the GPU.
pub(crate) trait GpuFrames {
    /// The API the frames belong to, for logs.
    fn api(&self) -> &'static str;

    /// The encoder's input pixel format (a hardware format).
    fn pixel_format(&self) -> format::Pixel;

    /// The `AVHWFramesContext` the encoder is opened with (borrowed).
    fn frames_context(&self) -> *mut ffi::AVBufferRef;

    /// The `AVHWDeviceContext` of the frames (borrowed).
    fn device_context(&self) -> *mut ffi::AVBufferRef;

    /// Takes a free frame, has `render` write it and returns it for the
    /// encoder. The frame goes back to the pool once FFmpeg releases it.
    fn next(&mut self, render: Render<'_>) -> Result<frame::Video>;
}

/// The frames pool for encoder `name`, or `None` to feed it from memory.
pub(crate) fn open(name: &str, settings: &OutputSettings) -> Result<Option<Box<dyn GpuFrames>>> {
    let (width, height) = (settings.width, settings.height);
    let deep = settings.encoding.is_hdr();
    if name.ends_with("_vulkan") {
        let device = settings
            .device
            .as_ref()
            .ok_or_else(|| Error::Encode("no shared Vulkan device".into()))?;
        return Ok(Some(Box::new(vulkan::VulkanFrames::new(
            device, width, height, deep,
        )?)));
    }
    if name.ends_with("_nvenc") {
        if std::env::var_os("NOX_NVENC_MEMORY").is_some() {
            return Ok(None);
        }
        let Some(gpu) = &settings.gpu else {
            return Ok(None);
        };
        return match cuda::CudaFrames::new(gpu, width, height, deep) {
            Ok(frames) => Ok(Some(Box::new(frames))),
            Err(err) => {
                tracing::info!("{name} takes frames from memory: {err}");
                Ok(None)
            }
        };
    }
    Ok(None)
}

/// R'G'B' to normalized `(Y, Cb, Cr)` values of the frames: BT.709 NV12,
/// or with `deep` BT.2020 P010 (10 bits in the high bits of 16).
fn ycbcr_matrix(deep: bool) -> nox_color::Matrix3x4 {
    if deep {
        ycbcr::rgb_to_ycbcr(
            Coefficients::BT2020,
            Quantization {
                bit_depth: 10,
                container_bits: 16,
                msb_aligned: true,
                full_range: false,
            },
        )
    } else {
        ycbcr::rgb_to_ycbcr(Coefficients::BT709, Quantization::eight_bit(false))
    }
}
