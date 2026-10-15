//! Writing media files: encodes rendered RGBA frames and mixed audio, and
//! muxes them into a container (`Container`: chosen, or by the file
//! extension).
//!
//! Video is SDR, tagged BT.709, or HDR (`OutputEncoding::Pq`, `Hlg`): 10-bit
//! BT.2020 with PQ or HLG, for HEVC (Main 10), AV1, VP9 and ProRes, with the mastering
//! display in the file for PQ (HDR10). Encoders are tried in order:
//!
//! 1. Vulkan Video (`h264_vulkan`, `hevc_vulkan`) on the device Nox renders
//!    with. Not for HDR by default (see `open_video`).
//! 2. NVENC, fed from GPU memory through CUDA (from system memory without
//!    a CUDA driver, or with `NOX_NVENC_MEMORY=1`).
//! 3. VAAPI (`h264_vaapi`, `hevc_vaapi`; Linux, AMD and Intel) on the GPU
//!    Nox renders with: the compositor writes into its surfaces, mapped to
//!    Vulkan.
//! 4. Software (x264, x265, SVT-AV1, libvpx; ProRes is always software).
//!
//! Hardware encoders that take frames on the GPU (`hwaccel`) are written by
//! the compositor directly (`write_gpu`), no copies; frames in system
//! memory (`write_video`) are converted with swscale.

use ffmpeg::{ChannelLayout, Dictionary, codec, encoder, ffi, format, frame, software};
use nox_project::Rational;
use std::path::{Path, PathBuf};

use crate::hwaccel::{self, Accel, GpuSink};
use crate::{CHANNELS, Error, Image, Result};
use nox_color::OutputEncoding;
use nox_gpu::{FrameLight, GpuTarget};

/// Video codecs Nox can export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    /// ProRes 422 (HQ, standard or LT by quality), 10 bit.
    ProRes,
    /// AV1: the smallest files, for the web.
    Av1,
    /// VP9: for the web (WebM).
    Vp9,
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 5] = [Self::H264, Self::Hevc, Self::ProRes, Self::Av1, Self::Vp9];

    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::ProRes => "ProRes 422",
            Self::Av1 => "AV1",
            Self::Vp9 => "VP9",
        }
    }

    /// FFmpeg encoders to try, best first.
    fn encoders(self, hardware: bool) -> &'static [&'static str] {
        match (self, hardware) {
            (Self::H264, true) => &["h264_vulkan", "h264_nvenc", "h264_vaapi", "libx264"],
            (Self::H264, false) => &["libx264"],
            (Self::Hevc, true) => &["hevc_vulkan", "hevc_nvenc", "hevc_vaapi", "libx265"],
            (Self::Hevc, false) => &["libx265"],
            (Self::ProRes, _) => &["prores_ks"],
            (Self::Av1, true) => &["av1_vulkan", "av1_nvenc", "av1_vaapi", "libsvtav1"],
            (Self::Av1, false) => &["libsvtav1"],
            (Self::Vp9, true) => &["vp9_vaapi", "libvpx-vp9"],
            (Self::Vp9, false) => &["libvpx-vp9"],
        }
    }

    /// The usual container.
    pub fn container(self) -> Container {
        match self {
            Self::ProRes => Container::Mov,
            Self::Vp9 => Container::Webm,
            _ => Container::Mp4,
        }
    }

    /// The usual file extension.
    pub fn extension(self) -> &'static str {
        self.container().extension()
    }

    /// Whether the codec carries HDR (10-bit BT.2020).
    pub fn does_hdr(self) -> bool {
        self != Self::H264
    }
}

/// Audio codecs Nox can export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// AAC, 320 kb/s: plays everywhere.
    Aac,
    /// Opus, 192 kb/s: for the web (WebM).
    Opus,
    /// MP3, 320 kb/s.
    Mp3,
    /// FLAC, 24 bit: lossless, about half of PCM's size.
    Flac,
    /// Uncompressed 16-bit PCM.
    Pcm16,
    /// Uncompressed 24-bit PCM, as in post-production.
    Pcm24,
}

impl AudioCodec {
    pub const ALL: [AudioCodec; 6] = [
        Self::Aac,
        Self::Opus,
        Self::Mp3,
        Self::Flac,
        Self::Pcm16,
        Self::Pcm24,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Aac => "AAC",
            Self::Opus => "Opus",
            Self::Mp3 => "MP3",
            Self::Flac => "FLAC",
            Self::Pcm16 => "PCM 16 bit",
            Self::Pcm24 => "PCM 24 bit",
        }
    }

    /// Whether it keeps the audio exactly.
    pub fn lossless(self) -> bool {
        matches!(self, Self::Flac | Self::Pcm16 | Self::Pcm24)
    }

    /// FFmpeg's encoder, the sample format it takes and whether that's
    /// planar.
    fn encoder(self) -> (&'static str, format::Sample, bool) {
        use format::sample::Type::{Packed, Planar};
        match self {
            Self::Aac => ("aac", format::Sample::F32(Planar), true),
            Self::Opus => ("libopus", format::Sample::F32(Packed), false),
            Self::Mp3 => ("libmp3lame", format::Sample::F32(Planar), true),
            Self::Flac => ("flac", format::Sample::I32(Packed), false),
            Self::Pcm16 => ("pcm_s16le", format::Sample::I16(Packed), false),
            Self::Pcm24 => ("pcm_s24le", format::Sample::I32(Packed), false),
        }
    }

    fn bit_rate(self) -> Option<usize> {
        match self {
            Self::Aac | Self::Mp3 => Some(320_000),
            Self::Opus => Some(192_000),
            _ => None,
        }
    }
}

/// File formats Nox can export to. Each holds some of the video codecs
/// and audio codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// MP4: plays everywhere.
    Mp4,
    /// QuickTime: for post-production.
    Mov,
    /// Matroska: holds every codec.
    Mkv,
    /// WebM (Matroska for the web): AV1 or VP9 with Opus audio.
    Webm,
    /// AVI: H.264 with 16-bit PCM audio, for old players and devices.
    Avi,
}

impl Container {
    pub const ALL: [Container; 5] = [Self::Mp4, Self::Mov, Self::Mkv, Self::Webm, Self::Avi];

    pub fn name(self) -> &'static str {
        match self {
            Self::Mp4 => "MP4",
            Self::Mov => "QuickTime (MOV)",
            Self::Mkv => "Matroska (MKV)",
            Self::Webm => "WebM",
            Self::Avi => "AVI",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::Mkv => "mkv",
            Self::Webm => "webm",
            Self::Avi => "avi",
        }
    }

    /// The container files with `extension` are in.
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "mp4" | "m4v" => Some(Self::Mp4),
            "mov" | "qt" => Some(Self::Mov),
            "mkv" => Some(Self::Mkv),
            "webm" => Some(Self::Webm),
            "avi" => Some(Self::Avi),
            _ => None,
        }
    }

    /// The container `path`'s extension names.
    pub fn for_path(path: &Path) -> Option<Self> {
        Self::from_extension(path.extension()?.to_str()?)
    }

    /// FFmpeg's muxer.
    fn muxer(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::Mkv => "matroska",
            Self::Webm => "webm",
            Self::Avi => "avi",
        }
    }

    /// The video codecs it holds, the usual first.
    pub fn codecs(self) -> &'static [VideoCodec] {
        use VideoCodec::*;
        match self {
            Self::Mp4 => &[H264, Hevc, Av1, Vp9],
            Self::Mov => &[ProRes, H264, Hevc],
            Self::Mkv => &[H264, Hevc, Av1, Vp9, ProRes],
            Self::Webm => &[Vp9, Av1],
            Self::Avi => &[H264],
        }
    }

    pub fn holds(self, codec: VideoCodec) -> bool {
        self.codecs().contains(&codec)
    }

    /// The audio codecs it holds, the usual first.
    pub fn audio_codecs(self) -> &'static [AudioCodec] {
        use AudioCodec::*;
        match self {
            Self::Mp4 => &[Aac, Opus, Mp3, Flac],
            Self::Mov => &[Aac, Pcm24, Pcm16],
            Self::Mkv => &[Aac, Opus, Mp3, Flac, Pcm24, Pcm16],
            Self::Webm => &[Opus],
            // Not MP3: its encoder delay shifts the video too, and AVI
            // fills the gap with an empty frame.
            Self::Avi => &[Pcm16],
        }
    }

    pub fn holds_audio(self, codec: AudioCodec) -> bool {
        self.audio_codecs().contains(&codec)
    }

    /// The audio codec that usually goes with `video` in it.
    pub fn default_audio(self, video: VideoCodec) -> AudioCodec {
        match (self, video) {
            // ProRes goes with uncompressed audio, as in post-production.
            (Self::Mov | Self::Mkv, VideoCodec::ProRes) => AudioCodec::Pcm24,
            (Self::Mkv, VideoCodec::Av1 | VideoCodec::Vp9) => AudioCodec::Opus,
            _ => self.audio_codecs()[0],
        }
    }
}

/// How much quality to trade for size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    /// Visually lossless for most material.
    #[default]
    High,
    Medium,
    /// Small files, e.g. for review.
    Low,
}

/// What to write.
#[derive(Debug, Clone)]
pub struct OutputSettings {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub frame_rate: Rational,
    pub codec: VideoCodec,
    /// The file format; `None` for the one the extension names (else the
    /// codec's usual one).
    pub container: Option<Container>,
    pub quality: Quality,
    /// Try hardware encoders first.
    pub hardware: bool,
    /// Use this FFmpeg encoder of `codec` (e.g. `hevc_nvenc`) rather than
    /// the first that works.
    pub encoder: Option<String>,
    /// The devices frames are rendered on; enables hardware encoding
    /// without copies.
    pub accel: Option<Accel>,
    /// Stereo audio at this rate, or no audio track.
    pub audio_rate: Option<u32>,
    /// The audio codec; `None` for the container's usual one with `codec`.
    pub audio_codec: Option<AudioCodec>,
    /// SDR, or HDR (PQ or HLG; HEVC and ProRes only). Frames from memory
    /// are then RGBA16 in this encoding.
    pub encoding: OutputEncoding,
    /// Peak of the mastering display in nits, written for PQ (HDR10).
    pub peak_nits: f32,
}

impl OutputSettings {
    /// The container written: the chosen one, else the extension's, else
    /// the codec's usual one.
    pub fn container(&self) -> Container {
        self.container
            .or_else(|| Container::for_path(&self.path))
            .unwrap_or_else(|| self.codec.container())
    }
}

struct VideoOutput {
    encoder: encoder::Video,
    stream: usize,
    time_base: ffmpeg::Rational,
    input: VideoInput,
    width: u32,
    height: u32,
    name: &'static str,
}

/// How frames reach the encoder.
enum VideoInput {
    /// RGBA in system memory, converted by swscale.
    Memory {
        scaler: software::scaling::Context,
        rgba: frame::Video,
        yuv: frame::Video,
    },
    /// Hardware frames on the GPU, written by the compositor.
    Gpu(Box<dyn GpuSink>),
}

struct AudioOutput {
    encoder: encoder::Audio,
    stream: usize,
    time_base: ffmpeg::Rational,
    /// Samples per encoded frame.
    frame_size: usize,
    /// Interleaved samples not yet encoded.
    pending: Vec<f32>,
    next_pts: i64,
    planar: bool,
}

/// An output file being written. Call `finish` to complete it; dropping a
/// writer leaves an incomplete file.
pub struct MediaWriter {
    output: format::context::Output,
    video: VideoOutput,
    audio: Option<AudioOutput>,
    frames: i64,
    path: PathBuf,
    encoding: OutputEncoding,
    /// For HDR10: the light levels of the frames written so far.
    light: Option<LightLevels>,
}

/// MaxCLL and MaxFALL (CTA-861.3) of HDR10 frames, measured as they are
/// written: the brightest pixel, and the brightest frame average, of the
/// largest of R, G and B in nits.
#[derive(Debug, Clone, Default)]
pub struct LightLevels {
    pub max_cll: f32,
    pub max_fall: f32,
    /// Frames measured.
    frames: i64,
    /// Nits of every 16-bit PQ code.
    table: Vec<f32>,
}

impl LightLevels {
    fn new() -> Self {
        let table = (0..=u16::MAX)
            .map(|code| nox_color::hdr::pq_eotf(f32::from(code) / 65535.0) * 10_000.0)
            .collect();
        Self {
            table,
            ..Self::default()
        }
    }

    /// Adds a frame measured on the GPU.
    fn add_measured(&mut self, light: FrameLight) {
        self.max_cll = self.max_cll.max(light.peak);
        self.max_fall = self.max_fall.max(light.average);
        self.frames += 1;
    }

    /// Measures one RGBA16 (little-endian) frame of PQ R'G'B'.
    fn add(&mut self, image: &Image) {
        let rows: Vec<&[u8]> = image
            .data
            .chunks(image.stride)
            .take(image.height as usize)
            .collect();
        let table = &self.table;
        // Rows in parallel: a UHD frame is 8 million pixels.
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let per = rows.len().div_ceil(threads).max(1);
        let (peak, sum) = std::thread::scope(|scope| {
            let jobs: Vec<_> = rows
                .chunks(per)
                .map(|chunk| {
                    scope.spawn(move || {
                        let (mut peak, mut sum) = (0f32, 0f64);
                        for row in chunk {
                            for pixel in row[..image.width as usize * 8].as_chunks::<8>().0 {
                                let code = |i: usize| u16::from_le_bytes([pixel[i], pixel[i + 1]]);
                                let brightest = code(0).max(code(2)).max(code(4));
                                let nits = table[usize::from(brightest)];
                                peak = peak.max(nits);
                                sum += f64::from(nits);
                            }
                        }
                        (peak, sum)
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().expect("light meter"))
                .fold((0f32, 0f64), |(p, s), (q, t)| (p.max(q), s + t))
        });
        let pixels = f64::from(image.width) * f64::from(image.height);
        self.add_measured(FrameLight {
            peak,
            average: (sum / pixels.max(1.0)) as f32,
        });
    }
}

impl MediaWriter {
    pub fn create(settings: &OutputSettings) -> Result<Self> {
        crate::init();
        if !settings.width.is_multiple_of(2) || !settings.height.is_multiple_of(2) {
            return Err(Error::Encode(format!(
                "{}×{}: width and height must be even",
                settings.width, settings.height
            )));
        }
        if settings.encoding.is_hdr() && settings.codec == VideoCodec::H264 {
            return Err(Error::Encode(
                "HDR needs HEVC, AV1, VP9 or ProRes (H.264 exports are SDR)".into(),
            ));
        }
        let container = settings.container();
        if !container.holds(settings.codec) {
            return Err(Error::Encode(format!(
                "{} files can't hold {}",
                container.name(),
                settings.codec.name()
            )));
        }
        let audio_codec = settings
            .audio_codec
            .unwrap_or_else(|| container.default_audio(settings.codec));
        if settings.audio_rate.is_some() && !container.holds_audio(audio_codec) {
            return Err(Error::Encode(format!(
                "{} files can't hold {} audio",
                container.name(),
                audio_codec.name()
            )));
        }
        let mut output = format::output_as(&settings.path, container.muxer())?;
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);
        let video = open_video(&mut output, settings, global_header)?;
        let audio = match settings.audio_rate {
            Some(rate) => Some(open_audio(&mut output, rate, global_header, audio_codec)?),
            None => None,
        };
        let mut options = Dictionary::new();
        // MP4/MOV: index at the front, so files play while downloading.
        if matches!(container, Container::Mp4 | Container::Mov) {
            options.set("movflags", "+faststart");
        }
        output.write_header_with(options)?;
        tracing::info!(
            "writing {} ({}×{}, {} with {}, frames from {}{})",
            settings.path.display(),
            settings.width,
            settings.height,
            settings.codec.name(),
            video.name,
            match &video.input {
                VideoInput::Gpu(frames) => frames.api(),
                VideoInput::Memory { .. } => "memory",
            },
            match &audio {
                Some(audio) => format!(", {} audio", audio.encoder.id().name()),
                None => String::new(),
            }
        );
        Ok(Self {
            output,
            video,
            audio,
            frames: 0,
            path: settings.path.clone(),
            encoding: settings.encoding,
            light: (settings.encoding == OutputEncoding::Pq).then(LightLevels::new),
        })
    }

    /// The FFmpeg encoder in use, e.g. `h264_vulkan`.
    pub fn video_encoder(&self) -> &'static str {
        self.video.name
    }

    /// Whether frames go in on the GPU (`write_gpu`) rather than from
    /// memory (`write_video`).
    pub fn takes_gpu_frames(&self) -> bool {
        matches!(self.video.input, VideoInput::Gpu(_))
    }

    /// Appends one frame (RGBA8 at the output size, RGBA16 for HDR). Not
    /// for encoders that take frames on the GPU.
    pub fn write_video(&mut self, image: &Image) -> Result<()> {
        let video = &mut self.video;
        let VideoInput::Memory { scaler, rgba, yuv } = &mut video.input else {
            return Err(Error::Encode(format!("{} takes GPU frames", video.name)));
        };
        if (image.width, image.height) != (video.width, video.height) {
            return Err(Error::Encode(format!(
                "frame is {}×{}, output {}×{}",
                image.width, image.height, video.width, video.height
            )));
        }
        let stride = rgba.stride(0);
        let bytes = if self.encoding.is_hdr() { 8 } else { 4 };
        let row = image.width as usize * bytes;
        if image.stride < row {
            return Err(Error::Encode("frame has the wrong pixel size".into()));
        }
        let data = rgba.data_mut(0);
        for y in 0..image.height as usize {
            data[y * stride..][..row].copy_from_slice(&image.data[y * image.stride..][..row]);
        }
        scaler.run(rgba, yuv)?;
        let mut frame = yuv.clone();
        if let Some(light) = &mut self.light {
            light.add(image);
        }
        self.send_video(&mut frame)
    }

    /// Appends one frame that `render` writes into the encoder's next
    /// input frame on the GPU (see `takes_gpu_frames`), without copies.
    /// `render` gets whether to measure the frame's light (for HDR10
    /// metadata) and returns the measurement if it took one.
    pub fn write_gpu(
        &mut self,
        render: impl FnOnce(GpuTarget<'_>, bool) -> nox_gpu::Result<Option<FrameLight>>,
    ) -> Result<()> {
        let VideoInput::Gpu(frames) = &mut self.video.input else {
            return Err(Error::Encode(format!(
                "{} takes frames from memory",
                self.video.name
            )));
        };
        let measure = self.light.is_some();
        let mut render = Some(render);
        let mut light = None;
        let mut frame = frames.next(&mut |target| {
            let render = render.take().expect("one frame rendered once");
            light = render(target, measure)?;
            Ok(())
        })?;
        if let (Some(levels), Some(frame_light)) = (&mut self.light, light) {
            levels.add_measured(frame_light);
        }
        self.send_video(&mut frame)
    }

    fn send_video(&mut self, frame: &mut frame::Video) -> Result<()> {
        frame.set_pts(Some(self.frames));
        // One frame long (the time base is one frame), so the container's
        // duration and average rate count the last frame too.
        // SAFETY: plain field write on a valid frame.
        unsafe { (*frame.as_mut_ptr()).duration = 1 };
        set_color_tags(frame, self.encoding);
        self.video.encoder.send_frame(frame)?;
        self.frames += 1;
        self.drain_video()
    }

    /// Appends interleaved stereo samples (any amount).
    pub fn write_audio(&mut self, samples: &[f32]) -> Result<()> {
        let Some(audio) = &mut self.audio else {
            return Ok(());
        };
        audio.pending.extend_from_slice(samples);
        while audio.pending.len() >= audio.frame_size * CHANNELS {
            let chunk: Vec<f32> = audio.pending.drain(..audio.frame_size * CHANNELS).collect();
            send_audio(audio, &chunk)?;
        }
        self.drain_audio()
    }

    /// Flushes the encoders and completes the file.
    pub fn finish(mut self) -> Result<PathBuf> {
        if let Some(audio) = &mut self.audio
            && !audio.pending.is_empty()
        {
            let rest = std::mem::take(&mut audio.pending);
            send_audio(audio, &rest)?;
        }
        if let Some(audio) = &mut self.audio {
            audio.encoder.send_eof()?;
        }
        self.drain_audio()?;
        self.video.encoder.send_eof()?;
        self.drain_video()?;
        // MP4 and MOV write their header (`moov`, with `clli`) last, so the
        // measured light levels still get in (Matroska rewrites its track
        // header at the end too). Light levels are only written
        // when every frame was measured.
        let frames = self.frames;
        if let Some(light) = self
            .light
            .as_ref()
            .filter(|light| frames > 0 && light.frames == frames)
        {
            // SAFETY: the stream's own parameters, before the trailer.
            unsafe {
                let stream = self.output.stream(self.video.stream).expect("stream");
                add_content_light((*stream.as_ptr()).codecpar, light.max_cll, light.max_fall);
            }
            tracing::info!(
                "HDR10 light levels: MaxCLL {:.0}, MaxFALL {:.0} nits",
                light.max_cll,
                light.max_fall
            );
        }
        self.output.write_trailer()?;
        tracing::info!("wrote {} ({} frames)", self.path.display(), self.frames);
        Ok(self.path)
    }

    fn drain_video(&mut self) -> Result<()> {
        let video = &mut self.video;
        drain(
            &mut video.encoder,
            video.stream,
            video.time_base,
            &mut self.output,
        )
    }

    fn drain_audio(&mut self) -> Result<()> {
        match &mut self.audio {
            Some(audio) => drain(
                &mut audio.encoder,
                audio.stream,
                audio.time_base,
                &mut self.output,
            ),
            None => Ok(()),
        }
    }
}

/// Writes the packets `encoder` has ready to `stream` of `output`.
fn drain(
    encoder: &mut encoder::Encoder,
    stream: usize,
    time_base: ffmpeg::Rational,
    output: &mut format::context::Output,
) -> Result<()> {
    let stream_tb = output.stream(stream).expect("stream").time_base();
    let mut packet = ffmpeg::Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(stream);
        packet.rescale_ts(time_base, stream_tb);
        packet.write_interleaved(output)?;
    }
    Ok(())
}

/// Opens the first encoder of `settings.codec` that works.
fn open_video(
    output: &mut format::context::Output,
    settings: &OutputSettings,
    global_header: bool,
) -> Result<VideoOutput> {
    let mut failures = Vec::new();
    let mut names = settings.codec.encoders(settings.hardware);
    if let Some(wanted) = &settings.encoder {
        let all = settings.codec.encoders(true);
        let Some(i) = all.iter().position(|name| name == wanted) else {
            return Err(Error::Encode(format!(
                "{wanted} isn't an encoder for {}",
                settings.codec.name()
            )));
        };
        names = &all[i..=i];
    }
    // Encoders that take frames on the GPU Nox renders with first (no
    // copies), then those fed from memory (e.g. NVENC on another GPU than
    // the one rendering), each in order of preference.
    let mut tried = Vec::new();
    for gpu_only in [true, false] {
        for &name in names {
            if tried.contains(&name) {
                continue;
            }
            let codec = match find_encoder(name, settings) {
                Ok(codec) => codec,
                Err(err) => {
                    failures.push(format!("{name}: {err}"));
                    tried.push(name);
                    continue;
                }
            };
            let gpu_frames = match hwaccel::open_sink(name, settings) {
                Ok(frames) => frames,
                Err(err) => {
                    failures.push(format!("{name}: {err}"));
                    tried.push(name);
                    continue;
                }
            };
            if gpu_only && gpu_frames.is_none() {
                continue;
            }
            tried.push(name);
            match open_video_encoder(codec, name, settings, global_header, gpu_frames.as_deref()) {
                Ok((encoder, pixel_format)) => {
                    let time_base = ffmpeg::Rational::new(
                        settings.frame_rate.den as i32,
                        settings.frame_rate.num as i32,
                    );
                    let mut stream = output.add_stream(codec)?;
                    stream.set_parameters(&encoder);
                    stream.set_time_base(time_base);
                    if settings.encoding == OutputEncoding::Pq {
                        // SAFETY: the stream's own parameters; FFmpeg owns the
                        // side data it allocates.
                        unsafe {
                            let par = (*stream.as_mut_ptr()).codecpar;
                            add_hdr10_stream_data(par, settings.peak_nits);
                        }
                    }
                    let rate = ffmpeg::Rational::new(
                        settings.frame_rate.num as i32,
                        settings.frame_rate.den as i32,
                    );
                    stream.set_avg_frame_rate(rate);
                    stream.set_rate(rate);
                    let index = stream.index();
                    let (w, h) = (settings.width, settings.height);
                    if !failures.is_empty() {
                        tracing::info!("video encoder {name} ({})", failures.join("; "));
                    }
                    let hdr = settings.encoding.is_hdr();
                    if let Some(frames) = gpu_frames {
                        return Ok(VideoOutput {
                            encoder,
                            stream: index,
                            time_base,
                            input: VideoInput::Gpu(frames),
                            width: w,
                            height: h,
                            name,
                        });
                    }
                    let source = if hdr {
                        format::Pixel::RGBA64LE
                    } else {
                        format::Pixel::RGBA
                    };
                    let mut scaler = software::scaling::Context::get(
                        source,
                        w,
                        h,
                        pixel_format,
                        w,
                        h,
                        software::scaling::Flags::BICUBIC
                            | software::scaling::Flags::FULL_CHR_H_INT
                            | software::scaling::Flags::ACCURATE_RND,
                    )?;
                    // Full-range R'G'B' in, limited-range Y'CbCr out (BT.709,
                    // or BT.2020 for HDR).
                    // SAFETY: valid context; coefficient tables are static.
                    unsafe {
                        let space = if hdr {
                            ffi::SWS_CS_BT2020
                        } else {
                            ffi::SWS_CS_ITU709
                        };
                        let coefficients = ffi::sws_getCoefficients(space);
                        ffi::sws_setColorspaceDetails(
                            scaler.as_mut_ptr(),
                            coefficients,
                            1,
                            coefficients,
                            0,
                            0,
                            1 << 16,
                            1 << 16,
                        );
                    }
                    return Ok(VideoOutput {
                        encoder,
                        stream: index,
                        time_base,
                        input: VideoInput::Memory {
                            scaler,
                            rgba: frame::Video::new(source, w, h),
                            yuv: frame::Video::new(pixel_format, w, h),
                        },
                        width: w,
                        height: h,
                        name,
                    });
                }
                Err(err) => failures.push(format!("{name}: {err}")),
            }
        }
    }
    Err(Error::Encode(format!(
        "no {} encoder could be opened ({})",
        settings.codec.name(),
        failures.join("; ")
    )))
}

/// FFmpeg's encoder `name`, if Nox may use it for `settings`.
fn find_encoder(name: &str, settings: &OutputSettings) -> Result<ffmpeg::Codec> {
    let codec = encoder::find_by_name(name).ok_or_else(|| Error::Encode("not available".into()))?;
    // 10-bit P010 input to Vulkan Video encoders came out wrong on NVIDIA
    // 580 (chroma near zero, luma striped), although the writes follow the
    // spec (16-bit views of the planes); writing through an invalid 8-bit
    // view "fixes" it, so the driver and the spec disagree about the
    // planes. HDR goes to NVENC (through CUDA) instead, unless
    // NOX_HDR_VULKAN_ENCODE=1 tries Vulkan Video.
    if name.ends_with("_vulkan")
        && settings.encoding.is_hdr()
        && std::env::var_os("NOX_HDR_VULKAN_ENCODE").is_none()
    {
        return Err(Error::Encode("not used for HDR".into()));
    }
    // FFmpeg 9's Vulkan AV1 encoder writes undecodable streams on NVIDIA
    // 580 at any quantizer but its default (18, far too large files), and
    // FFmpeg 8's loses the device. Off unless NOX_AV1_VULKAN_ENCODE=1.
    if name == "av1_vulkan" && std::env::var_os("NOX_AV1_VULKAN_ENCODE").is_none() {
        return Err(Error::Encode("off (broken output)".into()));
    }
    // FFmpeg 8's AV1 Vulkan encoder loses the device on NVIDIA 580
    // (VK_ERROR_DEVICE_LOST on the first frame); FFmpeg 9's works.
    if name == "av1_vulkan" && ffmpeg::codec::version() >> 16 < 63 {
        return Err(Error::Encode("needs FFmpeg 9 (libavcodec 63)".into()));
    }
    Ok(codec)
}

/// An encoder that works on this system, from `probe_encoders`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderProbe {
    /// FFmpeg's name, e.g. `hevc_nvenc` (for `OutputSettings::encoder`).
    pub name: &'static str,
    /// Runs on the GPU's video engine rather than the CPU.
    pub hardware: bool,
    /// The API it takes frames on the GPU through (`Vulkan`, `CUDA`), or
    /// `None` for frames from memory.
    pub gpu_frames: Option<&'static str>,
}

/// The encoders of `codec` that open for `encoding` on this system, best
/// first: each is actually opened (at 1920×1080), as an encoder can be
/// built into FFmpeg without the hardware or driver it needs. Takes a
/// moment, mostly for hardware encoders.
pub fn probe_encoders(
    codec: VideoCodec,
    encoding: OutputEncoding,
    accel: Option<Accel>,
) -> Vec<EncoderProbe> {
    crate::init();
    if encoding.is_hdr() && codec == VideoCodec::H264 {
        return Vec::new();
    }
    let settings = OutputSettings {
        path: PathBuf::new(),
        width: 1920,
        height: 1080,
        frame_rate: Rational::new(30, 1),
        codec,
        container: None,
        quality: Quality::High,
        hardware: true,
        encoder: None,
        accel,
        audio_rate: None,
        audio_codec: None,
        encoding,
        peak_nits: 1000.0,
    };
    // Encoders that don't open say why on FFmpeg's log; here that's an
    // answer, not an error.
    crate::stream::with_quiet_log(|| {
        codec
            .encoders(true)
            .iter()
            .filter_map(|&name| {
                let found = find_encoder(name, &settings).ok()?;
                let frames = hwaccel::open_sink(name, &settings).ok()?;
                if let Err(err) =
                    open_video_encoder(found, name, &settings, false, frames.as_deref())
                {
                    tracing::debug!("{name} doesn't open: {err}");
                    return None;
                }
                Some(EncoderProbe {
                    name,
                    hardware: !matches!(name, "libx264" | "libx265" | "prores_ks"),
                    gpu_frames: frames.map(|f| f.api()),
                })
            })
            .collect()
    })
}

/// Opens encoder `name`, taking frames from `gpu_frames` if given, and
/// returns it with its input pixel format.
fn open_video_encoder(
    codec: ffmpeg::Codec,
    name: &str,
    settings: &OutputSettings,
    global_header: bool,
    gpu_frames: Option<&dyn GpuSink>,
) -> Result<(encoder::Video, format::Pixel)> {
    let hdr = settings.encoding.is_hdr();
    let pixel_format = match name {
        _ if let Some(frames) = gpu_frames => frames.pixel_format(),
        "prores_ks" => format::Pixel::YUV422P10LE,
        "hevc_nvenc" | "av1_nvenc" if hdr => format::Pixel::P010LE,
        _ if hdr => format::Pixel::YUV420P10LE,
        _ => format::Pixel::YUV420P,
    };
    let mut video = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()?;
    video.set_width(settings.width);
    video.set_height(settings.height);
    video.set_format(pixel_format);
    let fps = settings.frame_rate;
    video.set_time_base(ffmpeg::Rational::new(fps.den as i32, fps.num as i32));
    video.set_frame_rate(Some(ffmpeg::Rational::new(fps.num as i32, fps.den as i32)));
    // Keyframes every 2 s: easy to seek, still efficient.
    video.set_gop((2.0 * fps.as_f64()).round() as u32);
    // FFmpeg's own threads (all cores), as the ffmpeg tool uses: libvpx
    // and ProRes depend on them (libvpx was ten times slower without);
    // x264, x265 and SVT-AV1 run threads of their own.
    video.set_threading(codec::threading::Config {
        kind: codec::threading::Type::Slice,
        count: 0,
    });
    if global_header {
        video.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    // AVI keeps no presentation times, so frames must come in display
    // order: no B-frames.
    if settings.container() == Container::Avi {
        video.set_max_b_frames(0);
    }
    // SAFETY: plain field writes on an unopened context.
    unsafe {
        let ctx = video.as_mut_ptr();
        let (primaries, transfer, space) = ffi_color(settings.encoding);
        (*ctx).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        (*ctx).colorspace = space;
        (*ctx).color_primaries = primaries;
        (*ctx).color_trc = transfer;
        // HDR10: encoders that write SEI (x265, NVENC) take the mastering
        // display from here.
        if settings.encoding == OutputEncoding::Pq {
            add_hdr10_encoder_data(ctx, settings.peak_nits);
        }
    }
    if let Some(frames) = gpu_frames {
        // SAFETY: valid contexts; the encoder context gets references of its
        // own.
        unsafe {
            let ctx = video.as_mut_ptr();
            (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames.frames_context());
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(frames.device_context());
        }
    }
    let mut options = Dictionary::new();
    let level = |high, medium, low| match settings.quality {
        Quality::High => high,
        Quality::Medium => medium,
        Quality::Low => low,
    };
    match name {
        "libx264" | "libx265" => {
            options.set("crf", level("18", "23", "28"));
            options.set("preset", "medium");
            if name == "libx265" {
                options.set("x265-params", "log-level=error");
            }
        }
        "h264_nvenc" | "hevc_nvenc" => {
            options.set("rc", "vbr");
            options.set("cq", level("19", "24", "29"));
            options.set("preset", "p5");
            options.set("b:v", "0");
            if hdr {
                options.set("profile", "main10");
            }
        }
        "av1_nvenc" => {
            options.set("rc", "vbr");
            options.set("cq", level("24", "30", "36"));
            options.set("preset", "p5");
            options.set("b:v", "0");
        }
        "libsvtav1" => {
            options.set("crf", level("24", "30", "36"));
            // Fast enough for 4K, still far smaller than H.264.
            options.set("preset", "8");
        }
        "libvpx-vp9" => {
            // Constant quality.
            options.set("crf", level("24", "31", "38"));
            options.set("b:v", "0");
            options.set("deadline", "good");
            options.set("cpu-used", "4");
            options.set("row-mt", "1");
        }
        // AV1 and VP9 quantizers run to 255.
        "av1_vulkan" => {
            options.set("rc_mode", "cqp");
            options.set("qp", level("90", "130", "170"));
        }
        "av1_vaapi" | "vp9_vaapi" => {
            options.set("rc_mode", "CQP");
            options.set("global_quality", level("90", "130", "170"));
        }
        "prores_ks" => {
            options.set("profile", level("3", "2", "1"));
        }
        "h264_vulkan" | "hevc_vulkan" => {
            options.set("rc_mode", "cqp");
            options.set("qp", level("20", "25", "30"));
        }
        "h264_vaapi" | "hevc_vaapi" => {
            options.set("rc_mode", "CQP");
            options.set("qp", level("20", "25", "30"));
        }
        _ => {}
    }
    Ok((video.open_as_with(codec, options)?, pixel_format))
}

fn open_audio(
    output: &mut format::context::Output,
    rate: u32,
    global_header: bool,
    audio_codec: AudioCodec,
) -> Result<AudioOutput> {
    let (name, sample_format, planar) = audio_codec.encoder();
    let codec = encoder::find_by_name(name)
        .ok_or_else(|| Error::Encode(format!("{name} encoder not available")))?;
    let mut audio = codec::context::Context::new_with_codec(codec)
        .encoder()
        .audio()?;
    audio.set_rate(rate as i32);
    audio.set_channel_layout(ChannelLayout::STEREO);
    audio.set_format(sample_format);
    audio.set_time_base(ffmpeg::Rational::new(1, rate as i32));
    if let Some(rate) = audio_codec.bit_rate() {
        audio.set_bit_rate(rate);
    }
    if audio_codec == AudioCodec::Flac {
        // SAFETY: plain field write on an unopened context.
        unsafe { (*audio.as_mut_ptr()).bits_per_raw_sample = 24 };
    }
    if global_header {
        audio.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let audio = audio.open_as(codec)?;
    let frame_size = match audio.frame_size() {
        0 => 1024, // PCM takes any size
        size => size as usize,
    };
    let mut stream = output.add_stream(codec)?;
    stream.set_parameters(&audio);
    let time_base = ffmpeg::Rational::new(1, rate as i32);
    stream.set_time_base(time_base);
    Ok(AudioOutput {
        stream: stream.index(),
        encoder: audio,
        time_base,
        frame_size,
        pending: Vec::new(),
        next_pts: 0,
        planar,
    })
}

/// Encodes interleaved stereo `samples` (at most one frame's worth).
fn send_audio(audio: &mut AudioOutput, samples: &[f32]) -> Result<()> {
    let count = samples.len() / CHANNELS;
    let format = audio.encoder.format();
    let mut frame = frame::Audio::new(format, count, ChannelLayout::STEREO);
    frame.set_rate(audio.encoder.rate());
    match format {
        // AAC, MP3: a plane per channel.
        _ if audio.planar => {
            for channel in 0..CHANNELS {
                let plane = frame.plane_mut::<f32>(channel);
                for (dst, src) in plane
                    .iter_mut()
                    .zip(samples.iter().skip(channel).step_by(CHANNELS))
                {
                    *dst = *src;
                }
            }
        }
        // Opus: interleaved floats, as mixed.
        format::Sample::F32(_) => {
            frame.plane_mut::<f32>(0)[..samples.len()].copy_from_slice(samples);
        }
        // 16-bit PCM (AVI).
        format::Sample::I16(_) => {
            let data = frame.plane_mut::<i16>(0);
            for (dst, src) in data.iter_mut().zip(samples) {
                *dst = (src.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
            }
        }
        // 24-bit PCM and FLAC: packed, in 32-bit containers (the low
        // byte is dropped).
        _ => {
            let data = frame.plane_mut::<i32>(0);
            for (dst, src) in data.iter_mut().zip(samples) {
                *dst = (src.clamp(-1.0, 1.0) * i32::MAX as f32) as i32;
            }
        }
    }
    frame.set_pts(Some(audio.next_pts));
    audio.next_pts += count as i64;
    audio.encoder.send_frame(&frame)?;
    Ok(())
}

fn set_color_tags(frame: &mut frame::Video, encoding: OutputEncoding) {
    let (primaries, transfer, space) = ffi_color(encoding);
    // SAFETY: plain field writes on a valid frame.
    unsafe {
        let f = frame.as_mut_ptr();
        (*f).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        (*f).color_primaries = primaries;
        (*f).color_trc = transfer;
        (*f).colorspace = space;
    }
}

/// FFmpeg's color tags for `encoding`.
fn ffi_color(
    encoding: OutputEncoding,
) -> (
    ffi::AVColorPrimaries,
    ffi::AVColorTransferCharacteristic,
    ffi::AVColorSpace,
) {
    use ffi::{AVColorPrimaries as P, AVColorSpace as S, AVColorTransferCharacteristic as T};
    match encoding {
        OutputEncoding::Sdr => (P::AVCOL_PRI_BT709, T::AVCOL_TRC_BT709, S::AVCOL_SPC_BT709),
        OutputEncoding::Pq => (
            P::AVCOL_PRI_BT2020,
            T::AVCOL_TRC_SMPTE2084,
            S::AVCOL_SPC_BT2020_NCL,
        ),
        OutputEncoding::Hlg => (
            P::AVCOL_PRI_BT2020,
            T::AVCOL_TRC_ARIB_STD_B67,
            S::AVCOL_SPC_BT2020_NCL,
        ),
    }
}

/// libavutil's `AVMasteringDisplayMetadata` (not in the generated
/// bindings; its layout is part of FFmpeg's ABI).
#[repr(C)]
struct MasteringDisplay {
    display_primaries: [[ffi::AVRational; 2]; 3],
    white_point: [ffi::AVRational; 2],
    min_luminance: ffi::AVRational,
    max_luminance: ffi::AVRational,
    has_primaries: i32,
    has_luminance: i32,
}

/// The mastering display written for HDR10: P3 primaries with a D65 white
/// (as most HDR is graded on), `peak` nits, 0.0001 nits black.
fn mastering_display(peak: f32) -> MasteringDisplay {
    // Chromaticities in SMPTE ST 2086 units (1/50000).
    let xy = |x: f64, y: f64| {
        let r = |v: f64| ffi::AVRational {
            num: (v * 50_000.0).round() as i32,
            den: 50_000,
        };
        [r(x), r(y)]
    };
    MasteringDisplay {
        display_primaries: [xy(0.680, 0.320), xy(0.265, 0.690), xy(0.150, 0.060)],
        white_point: xy(0.3127, 0.3290),
        min_luminance: ffi::AVRational {
            num: 1,
            den: 10_000,
        },
        max_luminance: ffi::AVRational {
            num: peak.round().max(1.0) as i32,
            den: 1,
        },
        has_primaries: 1,
        has_luminance: 1,
    }
}

/// Puts the HDR10 mastering display where encoders look for it.
///
/// # Safety
/// `ctx` must be an unopened encoder context.
unsafe fn add_hdr10_encoder_data(ctx: *mut ffi::AVCodecContext, peak: f32) {
    // SAFETY: guaranteed by the caller; FFmpeg allocates and owns the data.
    unsafe {
        let data = ffi::av_frame_side_data_new(
            &mut (*ctx).decoded_side_data,
            &mut (*ctx).nb_decoded_side_data,
            ffi::AVFrameSideDataType::AV_FRAME_DATA_MASTERING_DISPLAY_METADATA,
            size_of::<MasteringDisplay>(),
            0,
        );
        if !data.is_null() {
            (*data)
                .data
                .cast::<MasteringDisplay>()
                .write_unaligned(mastering_display(peak));
        }
    }
}

/// libavutil's `AVContentLightMetadata`.
#[repr(C)]
struct ContentLight {
    max_cll: u32,
    max_fall: u32,
}

/// Adds MaxCLL and MaxFALL (nits) to the stream, where the MP4 and MOV
/// muxers write them (`clli`).
///
/// # Safety
/// `par` must be the stream's codec parameters, before the trailer is
/// written.
unsafe fn add_content_light(par: *mut ffi::AVCodecParameters, max_cll: f32, max_fall: f32) {
    let light = ContentLight {
        max_cll: max_cll.round().clamp(0.0, 65535.0) as u32,
        max_fall: max_fall.round().clamp(0.0, 65535.0) as u32,
    };
    // SAFETY: guaranteed by the caller.
    unsafe {
        add_coded_side_data(
            par,
            ffi::AVPacketSideDataType::AV_PKT_DATA_CONTENT_LIGHT_LEVEL,
            light,
        );
    }
}

/// Puts the HDR10 mastering display into the stream, where the MP4 and MOV
/// muxers write it (`mdcv`).
///
/// # Safety
/// `par` must be the stream's codec parameters, before the header is
/// written.
unsafe fn add_hdr10_stream_data(par: *mut ffi::AVCodecParameters, peak: f32) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        add_coded_side_data(
            par,
            ffi::AVPacketSideDataType::AV_PKT_DATA_MASTERING_DISPLAY_METADATA,
            mastering_display(peak),
        );
    }
}

/// Adds `value` (the C struct FFmpeg expects for `kind`) to the stream's
/// side data.
///
/// # Safety
/// `par` must be valid stream parameters, and `T` must be the layout of
/// `kind`'s data.
unsafe fn add_coded_side_data<T>(
    par: *mut ffi::AVCodecParameters,
    kind: ffi::AVPacketSideDataType,
    value: T,
) {
    // SAFETY: guaranteed by the caller; FFmpeg allocates and owns the data.
    unsafe {
        let data = ffi::av_packet_side_data_new(
            &mut (*par).coded_side_data,
            &mut (*par).nb_coded_side_data,
            kind,
            size_of::<T>(),
            0,
        );
        if !data.is_null() {
            (*data).data.cast::<T>().write_unaligned(value);
        }
    }
}

/// The container FFmpeg would pick for `path`, if any.
pub fn container_for(path: &Path) -> Option<String> {
    crate::init();
    let name = std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok()?;
    // SAFETY: valid C string; returns a static description or null.
    unsafe {
        let format = ffi::av_guess_format(std::ptr::null(), name.as_ptr(), std::ptr::null());
        (!format.is_null()).then(|| {
            std::ffi::CStr::from_ptr((*format).name)
                .to_string_lossy()
                .into_owned()
        })
    }
}
