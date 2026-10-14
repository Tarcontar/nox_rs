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
    EXPORT_AUDIO_RATE, Export, ExportError, ExportEvent, ExportSettings, ExportSummary,
    available_encoders, export, full_range, has_audio, marked_range,
};
pub use nox_gpu::{
    DmabufFrame, FOURCC_ABGR8888, FOURCC_ABGR16161616F, Image, ScopeImages, scope_sizes,
};
pub use nox_media::{EncodeQuality, EncoderProbe, VideoCodec};

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
    /// Measured if enabled (`Engine::set_scopes`).
    pub scopes: Option<std::sync::Arc<ScopeImages>>,
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
        width as usize * height as usize * 4 + self.scopes.as_ref().map_or(0, |s| s.bytes())
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
