//! Media I/O on top of FFmpeg (`ffmpeg-next`): probing, demuxing, (hardware)
//! decoding, encoding and muxing. Plain Rust types only; the GUI wraps them
//! in GObjects.

mod decode;
mod frames;
mod hw;
mod probe;
mod thumbnail;
mod vulkan_frame;

pub use decode::VideoDecoder;
pub use ffmpeg;
pub use frames::{color_tags, display_size, rgba_cpu, to_software, with_cpu_frame};
pub use hw::HwDevice;
pub use nox_gpu::Image;
pub use probe::{AudioInfo, MediaInfo, VideoInfo, probe};
pub use thumbnail::thumbnail;
pub use vulkan_frame::with_gpu_input;

use std::sync::Once;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Ffmpeg(#[from] ffmpeg::Error),
    #[error("no video stream")]
    NoVideoStream,
    #[error("no decodable video frame")]
    NoFrame,
    #[error("{0}")]
    Gpu(#[from] nox_gpu::Error),
    #[error("Vulkan: {0}")]
    Vulkan(&'static str),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Initializes FFmpeg once per process. Called by every entry point, so
/// callers don't need to.
pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        ffmpeg::init().expect("FFmpeg initialization failed");
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Error);
    });
}

/// FFmpeg library versions as `(major, minor, micro)`.
#[derive(Debug, Clone, Copy)]
pub struct FfmpegVersions {
    pub avutil: (u32, u32, u32),
    pub avcodec: (u32, u32, u32),
    pub avformat: (u32, u32, u32),
}

pub fn ffmpeg_versions() -> FfmpegVersions {
    let parts = |v: u32| (v >> 16, (v >> 8) & 0xff, v & 0xff);
    FfmpegVersions {
        avutil: parts(ffmpeg::util::version()),
        avcodec: parts(ffmpeg::codec::version()),
        avformat: parts(ffmpeg::format::version()),
    }
}

pub(crate) fn rational(r: ffmpeg::Rational) -> Option<nox_project::Rational> {
    (r.numerator() > 0 && r.denominator() > 0)
        .then(|| nox_project::Rational::new(r.numerator().into(), r.denominator().into()))
}
