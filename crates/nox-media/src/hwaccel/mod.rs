//! Hardware video decoding and encoding with frames that stay on the GPU.
//!
//! Every hardware API keeps frames in its own kind of memory; a backend
//! moves them between that API and the device Nox renders with, without
//! system memory:
//!
//! - decoding: a decoded hardware frame becomes a `nox_gpu::Input` for the
//!   converter (`with_gpu_input`);
//! - encoding: a sink (`GpuSink`) owns the encoder's frames pool and lends
//!   each frame to the renderer as a `nox_gpu::GpuTarget` the compositor
//!   writes Y'CbCr into, then hands it to FFmpeg as a hardware frame.
//!
//! Backends:
//!
//! - `vulkan`: Vulkan Video, both ways. The frames are images of the
//!   Vulkan device Nox renders with (`Input::Vulkan`, `GpuTarget::Images`).
//! - `cuda`: NVDEC and NVENC (`*_nvenc`). The frames are Vulkan buffers
//!   exported to CUDA on the same GPU: decoded frames are copied into them
//!   once (`Input::Buffer`), encoded frames are written into them
//!   (`GpuTarget::Buffer`).
//! - `vaapi`: VAAPI (Linux), both ways. Its frames, DMA-BUFs, are mapped to
//!   Vulkan images by FFmpeg: decoded frames are read in place
//!   (`Input::Vulkan`), encoders' surfaces written in place
//!   (`GpuTarget::Images`).
//!
//! Other APIs fit the same shape: VAAPI through DMA-BUF (images or buffers
//! exported as `DRM_PRIME`), D3D11 for AMF, QSV and NVENC on Windows through
//! shared NT handles, VideoToolbox through IOSurface. Each needs a
//! `nox_gpu::MemoryHandle` kind, a backend here, its device in `Accel`,
//! and its encoders in `open_sink`.
//!
//! Encoders without a backend, or whose backend can't be set up (e.g. no
//! CUDA driver), take frames from memory; decoders decode in software.

mod cuda;
mod vaapi;
mod vulkan;

use ffmpeg::{ffi, format, frame};
use nox_color::ycbcr::{self, Coefficients, Quantization};
use nox_gpu::{BufferFrame, GpuDevice, GpuTarget, Input, LinearFormat, PlaneFormat};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::encode::OutputSettings;
use crate::{Error, HwDevice, Result};

/// The GPU devices hardware decoders and encoders use, all on the GPU Nox
/// renders with: FFmpeg's Vulkan device (which nox-gpu shares) and the
/// devices of other APIs, opened when first needed and kept. Cheap to
/// clone.
#[derive(Clone)]
pub struct Accel(Arc<Devices>);

struct Devices {
    vulkan: HwDevice,
    gpu: Arc<GpuDevice>,
    cuda: OnceLock<std::result::Result<Arc<cuda::CudaDevice>, String>>,
    /// Buffers decoded CUDA frames are copied into.
    cuda_decoded: cuda::DecodeBuffers,
    vaapi: OnceLock<std::result::Result<Arc<vaapi::VaapiDevice>, String>>,
    /// Vulkan frames contexts VAAPI frames are mapped into.
    vaapi_mapped: vaapi::MappedFrames,
}

impl std::fmt::Debug for Accel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accel").finish_non_exhaustive()
    }
}

impl Accel {
    /// Opens the Vulkan device (`NOX_GPU` selects one; see `HwDevice`).
    pub fn new() -> Result<Self> {
        let (vulkan, gpu) = HwDevice::vulkan()?;
        Ok(Self(Arc::new(Devices {
            vulkan,
            gpu,
            cuda: OnceLock::new(),
            cuda_decoded: cuda::DecodeBuffers::default(),
            vaapi: OnceLock::new(),
            vaapi_mapped: vaapi::MappedFrames::default(),
        })))
    }

    /// FFmpeg's Vulkan device.
    pub fn vulkan(&self) -> &HwDevice {
        &self.0.vulkan
    }

    /// The same device for nox-gpu.
    pub fn gpu(&self) -> &Arc<GpuDevice> {
        &self.0.gpu
    }

    /// FFmpeg's VAAPI device on the same GPU, if there is one (Linux, with
    /// a VA driver for it).
    pub(crate) fn vaapi(&self) -> Result<Arc<vaapi::VaapiDevice>> {
        self.0
            .vaapi
            .get_or_init(|| {
                vaapi::VaapiDevice::for_gpu(&self.0.gpu)
                    .map(Arc::new)
                    .map_err(|err| err.to_string())
            })
            .clone()
            .map_err(Error::Encode)
    }

    /// FFmpeg's CUDA device on the same GPU, if there is one (NVIDIA, with
    /// its driver's libcuda).
    pub(crate) fn cuda(&self) -> Result<Arc<cuda::CudaDevice>> {
        self.0
            .cuda
            .get_or_init(|| {
                cuda::CudaDevice::for_gpu(&self.0.gpu)
                    .map(Arc::new)
                    .map_err(|err| err.to_string())
            })
            .clone()
            .map_err(Error::Encode)
    }
}

/// Calls `f` with the best GPU input for the decoded `frame`: Vulkan
/// frames' own images (no copies), VAAPI frames mapped to Vulkan images
/// (no copies), CUDA frames copied once into a buffer on the GPU (VAAPI
/// and CUDA with `accel`, whose devices decoded them), otherwise a CPU view
/// (downloading hardware frames).
pub fn with_gpu_input<R>(
    frame: &frame::Video,
    accel: Option<&Accel>,
    f: impl FnOnce(Input<'_, '_>) -> R,
) -> Result<R> {
    if frame.format() == format::Pixel::VULKAN {
        return vulkan::with_gpu_input(frame, f);
    }
    if frame.format() == format::Pixel::CUDA
        && let Some(accel) = accel
    {
        match cuda_input(accel, frame) {
            Ok((copied, picture)) => {
                log_once(&CUDA_IN_PLACE, || {
                    "copying NVDEC-decoded frames on the GPU (no download)".into()
                });
                let input = BufferFrame {
                    buffer: copied.buffer(),
                    layout: copied.layout,
                    width: picture.width,
                    height: picture.height,
                    matrix: picture.matrix,
                    chroma_offset: picture.chroma_offset,
                    color: picture.color,
                };
                return Ok(f(Input::Buffer(&input)));
            }
            Err(err) => log_once(&CUDA_DOWNLOAD, || {
                format!("downloading NVDEC-decoded frames: {err}")
            }),
        }
    }
    if frame.format() == format::Pixel::VAAPI
        && let Some(accel) = accel
    {
        // SAFETY: a VAAPI frame from a decoder on `accel`'s VAAPI device.
        match unsafe { accel.0.vaapi_mapped.map(accel.vulkan(), frame) } {
            Ok(mapped) => {
                log_once(&VAAPI_MAPPED, || {
                    "mapping VAAPI-decoded frames to Vulkan (DMA-BUF, no copies)".into()
                });
                return vulkan::with_gpu_input(&mapped, f);
            }
            Err(err) => log_once(&VAAPI_DOWNLOAD, || {
                format!("downloading VAAPI-decoded frames: {err}")
            }),
        }
    }
    crate::with_cpu_frame(frame, |cpu| f(Input::Cpu(cpu)))
}

static VAAPI_MAPPED: AtomicBool = AtomicBool::new(false);
static VAAPI_DOWNLOAD: AtomicBool = AtomicBool::new(false);
static CUDA_IN_PLACE: AtomicBool = AtomicBool::new(false);
static CUDA_DOWNLOAD: AtomicBool = AtomicBool::new(false);

/// Logs a message once per process (per flag).
fn log_once(flag: &AtomicBool, message: impl FnOnce() -> String) {
    if !flag.swap(true, Ordering::Relaxed) {
        tracing::info!("{}", message());
    }
}

/// How to read a copied frame's samples.
struct Picture {
    width: u32,
    height: u32,
    matrix: nox_color::Matrix3x4,
    chroma_offset: [f32; 2],
    color: nox_color::InputTransform,
}

/// Copies a decoded CUDA frame into a shared buffer; returns it with how
/// to read it.
fn cuda_input<'a>(
    accel: &'a Accel,
    frame: &frame::Video,
) -> Result<(cuda::CopiedFrame<'a>, Picture)> {
    let unsupported = |what: &str| Error::Encode(format!("{what} decoder output"));
    // SAFETY: a hardware frame carries its frames context.
    let sw_format = unsafe {
        let frames = (*frame.as_ptr()).hw_frames_ctx;
        if frames.is_null() {
            return Err(unsupported("no frames context for"));
        }
        format::Pixel::from((*(*frames).data.cast::<ffi::AVHWFramesContext>()).sw_format)
    };
    let classified =
        crate::frames::classify(sw_format, frame).ok_or_else(|| unsupported("unknown"))?;
    let format = match (classified.layout, &classified.planes[..]) {
        (nox_gpu::Layout::TwoPlane, [(PlaneFormat::R8, 0, 0), (PlaneFormat::Rg8, 1, v)]) => {
            if *v == 1 {
                LinearFormat::Nv12
            } else {
                LinearFormat::Nv16
            }
        }
        (nox_gpu::Layout::TwoPlane, [(PlaneFormat::R16, 0, 0), (PlaneFormat::Rg16, 1, v)]) => {
            if *v == 1 {
                LinearFormat::P010
            } else {
                LinearFormat::P210
            }
        }
        _ => return Err(unsupported(&format!("{sw_format:?}"))),
    };
    if classified.swap_chroma {
        return Err(unsupported("swapped-chroma"));
    }
    let device = accel.cuda()?;
    // SAFETY: a CUDA frame from a decoder on `accel`'s CUDA device, with
    // two planes in `format` (classified above).
    let copied = unsafe {
        accel
            .0
            .cuda_decoded
            .copy(&accel.0.gpu, &device, frame, format)?
    };
    let picture = Picture {
        width: frame.width(),
        height: frame.height(),
        matrix: classified.matrix,
        chroma_offset: classified.chroma_offset,
        color: classified.color,
    };
    Ok((copied, picture))
}

/// Writes one frame into the target it's given (the compositor).
pub(crate) type Render<'a> = &'a mut dyn FnMut(GpuTarget<'_>) -> nox_gpu::Result<()>;

/// The frames pool of a hardware encoder that takes frames on the GPU.
pub(crate) trait GpuSink {
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
pub(crate) fn open_sink(name: &str, settings: &OutputSettings) -> Result<Option<Box<dyn GpuSink>>> {
    let (width, height) = (settings.width, settings.height);
    let deep = settings.encoding.is_hdr();
    if name.ends_with("_vulkan") {
        let accel = settings
            .accel
            .as_ref()
            .ok_or_else(|| Error::Encode("no shared Vulkan device".into()))?;
        return Ok(Some(Box::new(vulkan::VulkanSink::new(
            accel.vulkan(),
            width,
            height,
            deep,
        )?)));
    }
    if name.ends_with("_vaapi") {
        // VAAPI encoders take VAAPI surfaces only: without a sink they
        // aren't usable.
        let accel = settings
            .accel
            .as_ref()
            .ok_or_else(|| Error::Encode("no GPU".into()))?;
        let sink = vaapi::VaapiSink::new(accel.vaapi()?, accel.vulkan(), width, height, deep)?;
        return Ok(Some(Box::new(sink)));
    }
    if name.ends_with("_nvenc") {
        if std::env::var_os("NOX_NVENC_MEMORY").is_some() {
            return Ok(None);
        }
        let Some(accel) = &settings.accel else {
            return Ok(None);
        };
        let sink = accel
            .cuda()
            .and_then(|device| cuda::CudaSink::new(accel.gpu(), device, width, height, deep));
        return match sink {
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
