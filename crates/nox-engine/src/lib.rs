//! Playback and render engine: turns a `nox_project` timeline into
//! composited frames on the GPU, caches them, and plays the sequence with
//! audio as the master clock.

mod audio;
mod cache;
mod engine;
mod export;
mod render;

pub use audio::Mixer;
pub use engine::Engine;
pub use export::{
    EXPORT_AUDIO_RATE, Export, ExportError, ExportEvent, ExportSettings, ExportSummary, export,
    full_range,
};
pub use nox_gpu::{DmabufFrame, FOURCC_ABGR8888, Image};
pub use nox_media::{EncodeQuality, VideoCodec};

/// Display-referred RGBA at the requested output size.
#[derive(Debug)]
pub enum FrameImage {
    /// In system memory.
    Pixels(Image),
    /// On the GPU, exported as DMA-BUF.
    Dmabuf(DmabufFrame),
}

/// A rendered sequence frame.
#[derive(Debug)]
pub struct RenderedFrame {
    /// Sequence frame number.
    pub frame: i64,
    pub image: FrameImage,
    /// `false` for quick stand-ins shown while scrubbing (layers may show
    /// a nearby keyframe); the exact frame follows.
    pub exact: bool,
}

impl RenderedFrame {
    pub fn size(&self) -> (u32, u32) {
        match &self.image {
            FrameImage::Pixels(image) => (image.width, image.height),
            FrameImage::Dmabuf(frame) => (frame.width, frame.height),
        }
    }

    /// Approximate memory use.
    pub fn bytes(&self) -> usize {
        let (width, height) = self.size();
        width as usize * height as usize * 4
    }
}

/// Reported from the render thread.
#[derive(Debug)]
pub enum EngineEvent {
    /// A requested frame is ready.
    Frame(std::sync::Arc<RenderedFrame>),
    /// A requested frame could not be rendered.
    Error { frame: i64, message: String },
}
