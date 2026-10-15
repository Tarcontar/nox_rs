//! Writing media files: encodes rendered RGBA frames and mixed audio, and
//! muxes them into a container chosen by the file extension.
//!
//! Video is SDR, tagged BT.709, or HDR (`OutputEncoding::Pq`, `Hlg`): 10-bit
//! BT.2020 with PQ or HLG, for HEVC (Main 10) and ProRes, with the mastering
//! display in the file for PQ (HDR10). Encoders are tried in order:
//!
//! 1. Vulkan Video (`h264_vulkan`, `hevc_vulkan`) on the device Nox renders
//!    with. Not for HDR by default (see `open_video`).
//! 2. NVENC, fed from GPU memory through CUDA (from system memory without
//!    a CUDA driver, or with `NOX_NVENC_MEMORY=1`).
//! 3. VAAPI (`h264_vaapi`, `hevc_vaapi`; Linux, AMD and Intel) on the GPU
//!    Nox renders with: the compositor writes into its surfaces, mapped to
//!    Vulkan.
//! 4. Software (x264, x265; ProRes is always software).
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
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 3] = [Self::H264, Self::Hevc, Self::ProRes];

    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::ProRes => "ProRes 422",
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
        }
    }

    /// The usual file extension.
    pub fn extension(self) -> &'static str {
        match self {
            Self::ProRes => "mov",
            _ => "mp4",
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
    pub quality: Quality,
    /// Try hardware encoders first.
    pub hardware: bool,
    /// Use this FFmpeg encoder of `codec` (e.g. `hevc_nvenc`) rather than
    /// the first that works.
    pub encoder: Option<String>,
    /// The devices frames are rendered on; enables hardware encoding
    /// without copies.
    pub accel: Option<Accel>,
    /// Stereo audio at this rate (AAC, or PCM in MOV), or no audio track.
    pub audio_rate: Option<u32>,
    /// SDR, or HDR (PQ or HLG; HEVC and ProRes only). Frames from memory
    /// are then RGBA16 in this encoding.
    pub encoding: OutputEncoding,
    /// Peak of the mastering display in nits, written for PQ (HDR10).
    pub peak_nits: f32,
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
                "HDR needs HEVC or ProRes (H.264 exports are SDR)".into(),
            ));
        }
        let mut output = format::output(&settings.path)?;
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);
        let video = open_video(&mut output, settings, global_header)?;
        let audio = match settings.audio_rate {
            Some(rate) => Some(open_audio(
                &mut output,
                rate,
                global_header,
                settings.codec,
            )?),
            None => None,
        };
        let mut options = Dictionary::new();
        // MP4/MOV: index at the front, so files play while downloading.
        options.set("movflags", "+faststart");
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
            if audio.is_some() { ", AAC audio" } else { "" }
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
        // measured light levels still get in. Light levels are only written
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
        let stream_tb = self
            .output
            .stream(video.stream)
            .expect("stream")
            .time_base();
        let mut packet = ffmpeg::Packet::empty();
        while video.encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(video.stream);
            packet.rescale_ts(video.time_base, stream_tb);
            packet.write_interleaved(&mut self.output)?;
        }
        Ok(())
    }

    fn drain_audio(&mut self) -> Result<()> {
        let Some(audio) = &mut self.audio else {
            return Ok(());
        };
        let stream_tb = self
            .output
            .stream(audio.stream)
            .expect("stream")
            .time_base();
        let mut packet = ffmpeg::Packet::empty();
        while audio.encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(audio.stream);
            packet.rescale_ts(audio.time_base, stream_tb);
            packet.write_interleaved(&mut self.output)?;
        }
        Ok(())
    }
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
        quality: Quality::High,
        hardware: true,
        encoder: None,
        accel,
        audio_rate: None,
        encoding,
        peak_nits: 1000.0,
    };
    // Encoders that don't open say why on FFmpeg's log; here that's an
    // answer, not an error.
    // SAFETY: plain global setters.
    let level = unsafe { ffi::av_log_get_level() };
    unsafe { ffi::av_log_set_level(ffi::AV_LOG_QUIET) };
    let found = codec
        .encoders(true)
        .iter()
        .filter_map(|&name| {
            let found = find_encoder(name, &settings).ok()?;
            let frames = hwaccel::open_sink(name, &settings).ok()?;
            if let Err(err) = open_video_encoder(found, name, &settings, false, frames.as_deref()) {
                tracing::debug!("{name} doesn't open: {err}");
                return None;
            }
            Some(EncoderProbe {
                name,
                hardware: !matches!(name, "libx264" | "libx265" | "prores_ks"),
                gpu_frames: frames.map(|f| f.api()),
            })
        })
        .collect();
    // SAFETY: as above.
    unsafe { ffi::av_log_set_level(level) };
    found
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
        "hevc_nvenc" if hdr => format::Pixel::P010LE,
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
    if global_header {
        video.set_flags(codec::Flags::GLOBAL_HEADER);
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
    video_codec: VideoCodec,
) -> Result<AudioOutput> {
    // ProRes goes with uncompressed audio in MOV, as in post-production.
    let (name, sample_format, planar) = match video_codec {
        VideoCodec::ProRes => (
            "pcm_s24le",
            format::Sample::I32(format::sample::Type::Packed),
            false,
        ),
        _ => (
            "aac",
            format::Sample::F32(format::sample::Type::Planar),
            true,
        ),
    };
    let codec = encoder::find_by_name(name)
        .ok_or_else(|| Error::Encode(format!("{name} encoder not available")))?;
    let mut audio = codec::context::Context::new_with_codec(codec)
        .encoder()
        .audio()?;
    audio.set_rate(rate as i32);
    audio.set_channel_layout(ChannelLayout::STEREO);
    audio.set_format(sample_format);
    audio.set_time_base(ffmpeg::Rational::new(1, rate as i32));
    if name == "aac" {
        audio.set_bit_rate(320_000);
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
    if audio.planar {
        for channel in 0..CHANNELS {
            let plane = frame.plane_mut::<f32>(channel);
            for (dst, src) in plane
                .iter_mut()
                .zip(samples.iter().skip(channel).step_by(CHANNELS))
            {
                *dst = *src;
            }
        }
    } else {
        // Packed 24-bit PCM in 32-bit containers.
        let data = frame.plane_mut::<i32>(0);
        for (dst, src) in data.iter_mut().zip(samples) {
            *dst = (src.clamp(-1.0, 1.0) * i32::MAX as f32) as i32;
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
    // SAFETY: guaranteed by the caller; FFmpeg allocates and owns the data.
    unsafe {
        let data = ffi::av_packet_side_data_new(
            &mut (*par).coded_side_data,
            &mut (*par).nb_coded_side_data,
            ffi::AVPacketSideDataType::AV_PKT_DATA_CONTENT_LIGHT_LEVEL,
            size_of::<ContentLight>(),
            0,
        );
        if !data.is_null() {
            (*data)
                .data
                .cast::<ContentLight>()
                .write_unaligned(ContentLight {
                    max_cll: max_cll.round().clamp(0.0, 65535.0) as u32,
                    max_fall: max_fall.round().clamp(0.0, 65535.0) as u32,
                });
        }
    }
}

/// Puts the HDR10 mastering display into the stream, where the MP4 and MOV
/// muxers write it (`mdcv`).
///
/// # Safety
/// `par` must be the stream's codec parameters, before the header is
/// written.
unsafe fn add_hdr10_stream_data(par: *mut ffi::AVCodecParameters, peak: f32) {
    // SAFETY: guaranteed by the caller; FFmpeg allocates and owns the data.
    unsafe {
        let data = ffi::av_packet_side_data_new(
            &mut (*par).coded_side_data,
            &mut (*par).nb_coded_side_data,
            ffi::AVPacketSideDataType::AV_PKT_DATA_MASTERING_DISPLAY_METADATA,
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
