//! Stream information for imported media.

use ffmpeg::{codec, ffi, format, media};
use nox_color::Cicp;
use nox_project::{Rational, Time, timecode};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::{Result, rational};

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub path: PathBuf,
    /// Demuxer name, e.g. `mov,mp4,m4a,3gp,3g2,mj2`.
    pub container: String,
    /// `None` for still images and streams of unknown length.
    pub duration: Option<Time>,
    pub video: Option<VideoInfo>,
    pub audio: Vec<AudioInfo>,
}

impl MediaInfo {
    pub fn is_still_image(&self) -> bool {
        self.video.is_some() && self.audio.is_empty() && self.duration.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub stream_index: usize,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate: Option<Rational>,
    pub pixel_format: String,
    /// Bits per component, if the container or codec reports it.
    pub bit_depth: Option<u32>,
    pub color: Cicp,
}

#[derive(Debug, Clone)]
pub struct AudioInfo {
    pub stream_index: usize,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
}

/// Opens `path` and describes its main video stream and all audio streams.
pub fn probe(path: &Path) -> Result<MediaInfo> {
    crate::init();
    let input = format::input(path)?;

    let mut video = match input.streams().best(media::Type::Video) {
        Some(stream) if !is_cover_art(&stream) => Some(video_info(&stream)?),
        _ => None,
    };
    let audio = input
        .streams()
        .filter(|s| s.parameters().medium() == media::Type::Audio)
        .map(|s| audio_info(&s))
        .collect::<Result<Vec<_>>>()?;

    let container = input.format().name().to_owned();
    // Photos in tiles (HEIC): the picture's size, not a tile's.
    let grid = crate::tiles::tile_grid(&input);
    if let (Some(video), Some(grid)) = (video.as_mut(), &grid) {
        video.width = grid.width;
        video.height = grid.height;
    }
    let is_image = is_image_demuxer(&container) || grid.is_some();
    if let Some(video) = video.as_mut().filter(|_| is_image) {
        video.frame_rate = None;
    }
    Ok(MediaInfo {
        path: path.to_owned(),
        duration: if is_image {
            None
        } else {
            duration(&input, video.as_ref())
        },
        container,
        video,
        audio,
    })
}

/// Image demuxers report a one-frame duration; their files are stills.
pub(crate) fn is_image_demuxer(name: &str) -> bool {
    name == "image2" || name.ends_with("_pipe")
}

fn is_cover_art(stream: &format::stream::Stream) -> bool {
    stream
        .disposition()
        .contains(format::stream::Disposition::ATTACHED_PIC)
}

fn video_info(stream: &format::stream::Stream) -> Result<VideoInfo> {
    let parameters = stream.parameters();
    // SAFETY: `parameters` points to the stream's live AVCodecParameters.
    let bits = unsafe { (*parameters.as_ptr()).bits_per_raw_sample };
    let decoder = codec::Context::from_parameters(parameters)?
        .decoder()
        .video()?;
    let frame_rate = rational(stream.avg_frame_rate()).or_else(|| rational(stream.rate()));

    Ok(VideoInfo {
        stream_index: stream.index(),
        codec: decoder.id().name().to_owned(),
        width: decoder.width(),
        height: decoder.height(),
        frame_rate,
        pixel_format: decoder
            .format()
            .descriptor()
            .map_or("unknown", |d| d.name())
            .to_owned(),
        bit_depth: (bits > 0).then_some(bits as u32),
        color: Cicp {
            primaries: ffi::AVColorPrimaries::from(decoder.color_primaries()) as u8,
            transfer: ffi::AVColorTransferCharacteristic::from(
                decoder.color_transfer_characteristic(),
            ) as u8,
            matrix: ffi::AVColorSpace::from(decoder.color_space()) as u8,
            full_range: decoder.color_range() == ffmpeg::color::Range::JPEG,
        },
    })
}

fn audio_info(stream: &format::stream::Stream) -> Result<AudioInfo> {
    let decoder = codec::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
    Ok(AudioInfo {
        stream_index: stream.index(),
        codec: decoder.id().name().to_owned(),
        sample_rate: decoder.rate(),
        channels: decoder.channels().into(),
    })
}

/// Duration of the main stream, falling back to the container's duration.
fn duration(input: &format::context::Input, video: Option<&VideoInfo>) -> Option<Time> {
    let index = video
        .map(|v| v.stream_index)
        .or_else(|| input.streams().best(media::Type::Audio).map(|s| s.index()));
    let from_stream = index.and_then(|i| input.stream(i)).and_then(|stream| {
        let timebase = rational(stream.time_base())?;
        (stream.duration() > 0).then(|| Time::new(stream.duration(), timebase))
    });
    from_stream.or_else(|| {
        let us = input.duration();
        (us > 0 && us != ffi::AV_NOPTS_VALUE)
            .then(|| Time::new(us, Rational::new(1, ffi::AV_TIME_BASE.into())))
    })
}

/// Multi-line, human-readable description (used by `nox-cli probe` and
/// media pool tooltips).
impl fmt::Display for MediaInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "container  {}", self.container)?;
        match (
            self.duration,
            self.video.as_ref().and_then(|v| v.frame_rate),
        ) {
            (Some(duration), Some(fps)) => {
                let frames = duration.rescale(fps.inverse()).ticks;
                writeln!(
                    f,
                    "duration   {} ({:.3} s)",
                    timecode(frames, fps),
                    duration.as_seconds()
                )?;
            }
            (Some(duration), None) => writeln!(f, "duration   {:.3} s", duration.as_seconds())?,
            (None, _) if self.is_still_image() => writeln!(f, "duration   still image")?,
            (None, _) => writeln!(f, "duration   unknown")?,
        }
        if let Some(v) = &self.video {
            let fps = v
                .frame_rate
                .map_or(String::new(), |r| format!(" {:.3} fps", r.as_f64()));
            let depth = v.bit_depth.map_or(String::new(), |b| format!(", {b} bit"));
            writeln!(
                f,
                "video #{}   {} {}x{}{fps}, {}{depth}",
                v.stream_index, v.codec, v.width, v.height, v.pixel_format
            )?;
            writeln!(f, "color      {}", v.color)?;
        }
        for a in &self.audio {
            writeln!(
                f,
                "audio #{}   {} {} Hz, {} ch",
                a.stream_index, a.codec, a.sample_rate, a.channels
            )?;
        }
        Ok(())
    }
}
