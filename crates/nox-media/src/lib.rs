//! Media I/O on top of FFmpeg (`ffmpeg-next`): probing, demuxing, (hardware)
//! decoding, encoding and muxing. Plain Rust types only; the GUI wraps them
//! in GObjects.

mod audio;
mod decode;
mod encode;
mod frames;
mod hw;
mod hwaccel;
mod probe;
mod stream;
mod thumbnail;
mod tiles;
pub mod waveform;

pub use audio::{AudioDecoder, CHANNELS};
pub use decode::{DecodeApi, Decoding, HwDecode, VideoDecoder};
pub use encode::{
    AudioCodec, Container, EncoderProbe, MediaWriter, OutputSettings, Quality as EncodeQuality,
    VideoCodec, container_for, probe_encoders,
};
pub use ffmpeg;
pub use frames::{
    color_tags, display_size, input_transform, orientation, rgba_cpu, solid_frame, to_software,
    tone_mapping, with_cpu_frame,
};
pub use hw::HwDevice;
pub use hwaccel::{Accel, with_gpu_input};
pub use nox_color::OutputEncoding;
pub use nox_gpu::{Image, Orientation};
pub use probe::{AudioInfo, MediaInfo, VideoInfo, probe};
pub use thumbnail::{ThumbnailReader, thumbnail};

use std::sync::Once;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Ffmpeg(#[from] ffmpeg::Error),
    #[error("no video stream")]
    NoVideoStream,
    #[error("no audio stream")]
    NoAudioStream,
    /// Decoding was stopped by the caller (e.g. a newer request).
    #[error("interrupted")]
    Interrupted,
    #[error("{0}")]
    Encode(String),
    #[error("no decodable video frame")]
    NoFrame,
    #[error("{0}")]
    Gpu(#[from] nox_gpu::Error),
    #[error("Vulkan: {0}")]
    Vulkan(&'static str),
    #[error("{}: {source}", path.display())]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
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
