//! Playback and render engine: turns a `nox_project` sequence into decode
//! requests, composites frames on the GPU and drives the audio clock.
//!
//! For now: `Preview`, which shows single frames of a source clip.

mod preview;

pub use nox_gpu::{DmabufFrame, FOURCC_ABGR8888, Image};
pub use preview::{FrameImage, Preview, PreviewFrame};
