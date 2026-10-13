//! Rendering a timeline to a file.
//!
//! Every frame is rendered exactly at the sequence resolution with a
//! renderer of its own (so playback and export don't share decoders) and
//! encoded with `nox_media::MediaWriter`. With a Vulkan Video encoder the
//! compositor writes straight into the encoder's input images; otherwise
//! frames are read back. Audio is mixed frame by frame alongside.

use nox_media::{EncodeQuality, MediaWriter, OutputSettings, VideoCodec};
use nox_project::{Time, Timeline};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::FrameImage;
use crate::audio::Mixer;
use crate::render::{Quality, Renderer};

/// Audio sample rate of exports.
pub const EXPORT_AUDIO_RATE: u32 = 48_000;

#[derive(Debug, Clone)]
pub struct ExportSettings {
    pub path: PathBuf,
    pub codec: VideoCodec,
    pub quality: EncodeQuality,
    /// Use hardware encoders (Vulkan Video, NVENC) when available.
    pub hardware: bool,
    /// Sequence frames to export; `None` for all.
    pub range: Option<Range<i64>>,
    pub audio: bool,
}

/// How an export went.
#[derive(Debug, Clone)]
pub struct ExportSummary {
    pub path: PathBuf,
    pub frames: i64,
    pub encoder: &'static str,
    /// Whether frames went to the encoder without copies.
    pub zero_copy: bool,
    pub elapsed: Duration,
}

impl ExportSummary {
    pub fn fps(&self) -> f64 {
        self.frames as f64 / self.elapsed.as_secs_f64().max(1e-6)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("{0}")]
    Media(#[from] nox_media::Error),
    #[error("the timeline is empty")]
    Empty,
    #[error("cancelled")]
    Cancelled,
}

/// Reported from the export thread.
#[derive(Debug)]
pub enum ExportEvent {
    /// `done` of `total` frames written.
    Progress {
        done: i64,
        total: i64,
    },
    Finished(Result<ExportSummary, String>),
}

/// The frames an export covers by default: the whole timeline.
pub fn full_range(timeline: &Timeline) -> Range<i64> {
    let fps = timeline.frame_rate();
    let duration = timeline.duration();
    let frames = duration.frame(fps);
    // A partial last frame still counts.
    let end = if Time::from_frames(frames, fps) < duration {
        frames + 1
    } else {
        frames
    };
    0..end
}

/// The frames between the timeline's In and Out marks, if both are set:
/// from the frame at In up to and including the frame Out ends.
pub fn marked_range(timeline: &Timeline) -> Option<Range<i64>> {
    let fps = timeline.frame_rate();
    let marked = timeline.settings.marked_range()?;
    let start = marked.start.frame(fps);
    let mut end = marked.end().frame(fps);
    if Time::from_frames(end, fps) < marked.end() {
        end += 1;
    }
    (start < end).then_some(start..end)
}

/// Renders and writes the timeline; `progress(done, total)` is called after
/// every frame and cancels the export by returning false.
pub fn export(
    timeline: &Timeline,
    settings: &ExportSettings,
    mut progress: impl FnMut(i64, i64) -> bool,
) -> Result<ExportSummary, ExportError> {
    let started = Instant::now();
    let range = settings
        .range
        .clone()
        .unwrap_or_else(|| full_range(timeline));
    if range.is_empty() {
        return Err(ExportError::Empty);
    }
    let fps = timeline.frame_rate();
    let (width, height) = (timeline.settings.width, timeline.settings.height);
    let mut renderer = Renderer::new(&[], Arc::new(AtomicBool::new(false)));
    let device = if settings.hardware {
        renderer.hw_device()
    } else {
        None
    };
    let output = OutputSettings {
        path: settings.path.clone(),
        width,
        height,
        frame_rate: fps,
        codec: settings.codec,
        quality: settings.quality,
        hardware: settings.hardware,
        device,
        audio_rate: settings.audio.then_some(EXPORT_AUDIO_RATE),
    };
    let mut writer = MediaWriter::create(&output)?;
    let zero_copy = writer.takes_vulkan_frames();
    let encoder = writer.video_encoder();
    let mut mixer = Mixer::new(EXPORT_AUDIO_RATE);
    let mut audio = Vec::new();
    let total = range.end - range.start;
    // Sample position of a sequence frame, exact for any frame rate.
    let sample_of = |frame: i64| {
        Time::from_frames(frame, fps)
            .rescale(nox_project::Rational::new(1, i64::from(EXPORT_AUDIO_RATE)))
            .ticks
    };
    for (done, frame) in range.clone().enumerate() {
        if zero_copy {
            writer.write_vulkan(|target| {
                renderer
                    .render_to_yuv(timeline, frame, target)
                    .map_err(|err| nox_gpu::Error::Unsupported(err.to_string()))
            })?;
        } else {
            let rendered =
                renderer.render(timeline, frame, width, height, Quality::Exact, &|| false)?;
            match &rendered.image {
                FrameImage::Pixels(image) => writer.write_video(image)?,
                FrameImage::Dmabuf(_) => unreachable!("DMA-BUF is disabled for exports"),
            }
        }
        if settings.audio {
            let (start, end) = (sample_of(frame), sample_of(frame + 1));
            audio.resize((end - start) as usize * nox_media::CHANNELS, 0.0);
            mixer.mix(timeline, start, &mut audio);
            writer.write_audio(&audio)?;
        }
        if !progress(done as i64 + 1, total) {
            // Leave no half-written file behind.
            drop(writer);
            let _ = std::fs::remove_file(&settings.path);
            return Err(ExportError::Cancelled);
        }
    }
    let path = writer.finish()?;
    let summary = ExportSummary {
        path,
        frames: total,
        encoder,
        zero_copy,
        elapsed: started.elapsed(),
    };
    tracing::info!(
        "exported {} frames in {:.1} s ({:.0} fps) with {}{}",
        summary.frames,
        summary.elapsed.as_secs_f64(),
        summary.fps(),
        summary.encoder,
        if zero_copy { ", zero copy" } else { "" }
    );
    Ok(summary)
}

/// An export running on its own thread.
pub struct Export {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Export {
    /// Starts exporting `timeline`; events arrive on the export thread.
    pub fn start(
        timeline: Arc<Timeline>,
        settings: ExportSettings,
        on_event: impl Fn(ExportEvent) + Send + 'static,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("nox-export".into())
            .spawn(move || {
                let result = export(&timeline, &settings, |done, total| {
                    on_event(ExportEvent::Progress { done, total });
                    !flag.load(Ordering::Relaxed)
                });
                on_event(ExportEvent::Finished(result.map_err(|err| err.to_string())));
            })
            .expect("failed to spawn export thread");
        Self {
            cancel,
            thread: Some(thread),
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Export {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nox_project::Rational;

    #[test]
    fn marked_range_in_frames() {
        let fps = Rational::new(30000, 1001);
        let mut timeline = Timeline::new("t", fps);
        assert_eq!(marked_range(&timeline), None);
        timeline
            .settings
            .set_mark_in(Some(Time::from_frames(10, fps)));
        assert_eq!(marked_range(&timeline), None);
        timeline
            .settings
            .set_mark_out(Some(Time::from_frames(25, fps)));
        assert_eq!(marked_range(&timeline), Some(10..25));
        // Marks off the frame grid still cover the frames they touch.
        timeline.settings.mark_out = Some(Time::new(1, Rational::new(1, 1)));
        assert_eq!(marked_range(&timeline), Some(10..30));
    }
}
