//! Writing media files: encodes rendered RGBA frames and mixed audio, and
//! muxes them into a container chosen by the file extension.
//!
//! Video is tagged BT.709 (Nox renders display-referred BT.709 R'G'B'; HDR
//! sources are tone-mapped to SDR). Encoders are tried in order:
//!
//! 1. Vulkan Video (`h264_vulkan`, `hevc_vulkan`) on the device Nox renders
//!    with: the compositor writes Y'CbCr straight into the encoder's input
//!    images (`write_vulkan`), no copies.
//! 2. NVENC, fed from system memory.
//! 3. Software (x264, x265; ProRes is always software).
//!
//! Frames in system memory (`write_video`) are converted with swscale.

use ffmpeg::{ChannelLayout, Dictionary, codec, encoder, ffi, format, frame, software};
use nox_project::Rational;
use std::path::{Path, PathBuf};

use crate::{CHANNELS, Error, HwDevice, Image, Result};
use nox_color::ycbcr::{self, Coefficients, Quantization};
use nox_gpu::{ImageSync, PlaneFormat, VkPlane, YuvTarget, vk};

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
            (Self::H264, true) => &["h264_vulkan", "h264_nvenc", "libx264"],
            (Self::H264, false) => &["libx264"],
            (Self::Hevc, true) => &["hevc_vulkan", "hevc_nvenc", "libx265"],
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
    /// The device frames are rendered on; enables Vulkan Video encoding
    /// without copies.
    pub device: Option<HwDevice>,
    /// Stereo audio at this rate (AAC, or PCM in MOV), or no audio track.
    pub audio_rate: Option<u32>,
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
    /// NV12 Vulkan images from the encoder's frames pool, written by the
    /// compositor.
    Vulkan {
        frames: *mut ffi::AVBufferRef,
        matrix: nox_color::Matrix3x4,
    },
}

impl Drop for VideoInput {
    fn drop(&mut self) {
        if let Self::Vulkan { frames, .. } = self {
            // SAFETY: we own one reference.
            unsafe { ffi::av_buffer_unref(frames) };
        }
    }
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
            "writing {} ({}×{}, {} with {}{})",
            settings.path.display(),
            settings.width,
            settings.height,
            settings.codec.name(),
            video.name,
            if audio.is_some() { ", AAC audio" } else { "" }
        );
        Ok(Self {
            output,
            video,
            audio,
            frames: 0,
            path: settings.path.clone(),
        })
    }

    /// The FFmpeg encoder in use, e.g. `h264_vulkan`.
    pub fn video_encoder(&self) -> &'static str {
        self.video.name
    }

    /// Whether frames go in as Vulkan images (`write_vulkan`) rather than
    /// from memory (`write_video`).
    pub fn takes_vulkan_frames(&self) -> bool {
        matches!(self.video.input, VideoInput::Vulkan { .. })
    }

    /// Appends one frame (RGBA8 at the output size). Not for Vulkan
    /// encoders.
    pub fn write_video(&mut self, image: &Image) -> Result<()> {
        let video = &mut self.video;
        let VideoInput::Memory { scaler, rgba, yuv } = &mut video.input else {
            return Err(Error::Encode(format!("{} takes Vulkan frames", video.name)));
        };
        if (image.width, image.height) != (video.width, video.height) {
            return Err(Error::Encode(format!(
                "frame is {}×{}, output {}×{}",
                image.width, image.height, video.width, video.height
            )));
        }
        let stride = rgba.stride(0);
        let row = image.width as usize * 4;
        let data = rgba.data_mut(0);
        for y in 0..image.height as usize {
            data[y * stride..][..row].copy_from_slice(&image.data[y * image.stride..][..row]);
        }
        scaler.run(rgba, yuv)?;
        let mut frame = yuv.clone();
        self.send_video(&mut frame)
    }

    /// Appends one frame rendered by `render` into an encoder input image
    /// (for Vulkan encoders; see `takes_vulkan_frames`), without copies.
    pub fn write_vulkan(
        &mut self,
        render: impl FnOnce(&mut YuvTarget) -> nox_gpu::Result<()>,
    ) -> Result<()> {
        let VideoInput::Vulkan { frames, matrix } = &self.video.input else {
            return Err(Error::Encode(format!(
                "{} takes frames from memory",
                self.video.name
            )));
        };
        let (frames, matrix) = (*frames, *matrix);
        let mut frame = frame::Video::empty();
        // SAFETY: valid frames context; the frame takes a pool image.
        let ret = unsafe { ffi::av_hwframe_get_buffer(frames, frame.as_mut_ptr(), 0) };
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret).into());
        }
        // SAFETY: `frame` is a Vulkan frame of `frames`, alive for the call.
        unsafe { render_into(frames, &frame, matrix, render)? };
        self.send_video(&mut frame)
    }

    fn send_video(&mut self, frame: &mut frame::Video) -> Result<()> {
        frame.set_pts(Some(self.frames));
        // One frame long (the time base is one frame), so the container's
        // duration and average rate count the last frame too.
        // SAFETY: plain field write on a valid frame.
        unsafe { (*frame.as_mut_ptr()).duration = 1 };
        set_color_tags(frame);
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
    for &name in settings.codec.encoders(settings.hardware) {
        let Some(codec) = encoder::find_by_name(name) else {
            failures.push(format!("{name}: not available"));
            continue;
        };
        if name.ends_with("_vulkan") && settings.device.is_none() {
            failures.push(format!("{name}: no shared Vulkan device"));
            continue;
        }
        match open_video_encoder(codec, name, settings, global_header) {
            Ok((encoder, pixel_format, vulkan_frames)) => {
                let time_base = ffmpeg::Rational::new(
                    settings.frame_rate.den as i32,
                    settings.frame_rate.num as i32,
                );
                let mut stream = output.add_stream(codec)?;
                stream.set_parameters(&encoder);
                stream.set_time_base(time_base);
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
                if let Some(frames) = vulkan_frames {
                    return Ok(VideoOutput {
                        encoder,
                        stream: index,
                        time_base,
                        input: VideoInput::Vulkan {
                            frames,
                            matrix: ycbcr::rgb_to_ycbcr(
                                Coefficients::BT709,
                                Quantization::eight_bit(false),
                            ),
                        },
                        width: w,
                        height: h,
                        name,
                    });
                }
                let mut scaler = software::scaling::Context::get(
                    format::Pixel::RGBA,
                    w,
                    h,
                    pixel_format,
                    w,
                    h,
                    software::scaling::Flags::BICUBIC
                        | software::scaling::Flags::FULL_CHR_H_INT
                        | software::scaling::Flags::ACCURATE_RND,
                )?;
                // Full-range R'G'B' in, limited-range BT.709 Y'CbCr out.
                // SAFETY: valid context; coefficient tables are static.
                unsafe {
                    let bt709 = ffi::sws_getCoefficients(ffi::SWS_CS_ITU709);
                    ffi::sws_setColorspaceDetails(
                        scaler.as_mut_ptr(),
                        bt709,
                        1,
                        bt709,
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
                        rgba: frame::Video::new(format::Pixel::RGBA, w, h),
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
    Err(Error::Encode(format!(
        "no {} encoder could be opened ({})",
        settings.codec.name(),
        failures.join("; ")
    )))
}

/// Opens encoder `name`; for Vulkan encoders also returns the frames pool
/// (NV12 images the compositor can write) it takes input from.
fn open_video_encoder(
    codec: ffmpeg::Codec,
    name: &str,
    settings: &OutputSettings,
    global_header: bool,
) -> Result<(encoder::Video, format::Pixel, Option<*mut ffi::AVBufferRef>)> {
    let vulkan = name.ends_with("_vulkan");
    let pixel_format = match name {
        "prores_ks" => format::Pixel::YUV422P10LE,
        _ if vulkan => format::Pixel::VULKAN,
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
        (*ctx).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        (*ctx).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
        (*ctx).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*ctx).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    }
    let frames = if vulkan {
        let device = settings.device.as_ref().expect("checked by the caller");
        // SAFETY: valid device; the frames context is set up before init and
        // its reference handed to the encoder context (which keeps its own).
        unsafe {
            let frames = vulkan_frames(device, settings.width, settings.height)?;
            let ctx = video.as_mut_ptr();
            (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(device.as_ptr());
            Some(frames)
        }
    } else {
        None
    };
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
        }
        "prores_ks" => {
            options.set("profile", level("3", "2", "1"));
        }
        "h264_vulkan" | "hevc_vulkan" => {
            options.set("rc_mode", "cqp");
            options.set("qp", level("20", "25", "30"));
        }
        _ => {}
    }
    match video.open_as_with(codec, options) {
        Ok(opened) => Ok((opened, pixel_format, frames)),
        Err(err) => {
            if let Some(mut frames) = frames {
                // SAFETY: our reference; the failed context drops its own.
                unsafe { ffi::av_buffer_unref(&mut frames) };
            }
            Err(err.into())
        }
    }
}

/// An NV12 frames pool on `device` whose images the compositor can write
/// (storage views of the planes) and a Vulkan encoder can read.
///
/// # Safety
/// `device` must be a valid Vulkan device context.
unsafe fn vulkan_frames(
    device: &HwDevice,
    width: u32,
    height: u32,
) -> Result<*mut ffi::AVBufferRef> {
    // SAFETY: guaranteed by the caller; fields are set before init.
    unsafe {
        let mut frames = ffi::av_hwframe_ctx_alloc(device.as_ptr());
        if frames.is_null() {
            return Err(Error::Encode("cannot allocate Vulkan frames".into()));
        }
        let ctx = (*frames).data.cast::<ffi::AVHWFramesContext>();
        (*ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN;
        (*ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*ctx).width = width as i32;
        (*ctx).height = height as i32;
        let vk_ctx = (*ctx).hwctx.cast::<ffi::AVVulkanFramesContext>();
        // FFmpeg's defaults plus storage, for the compositor's plane views
        // (with extended usage, as NV12 itself isn't a storage format).
        (*vk_ctx).usage = (vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::STORAGE
            | vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR)
            .as_raw() as _;
        (*vk_ctx).img_flags = (vk::ImageCreateFlags::MUTABLE_FORMAT
            | vk::ImageCreateFlags::EXTENDED_USAGE
            | vk::ImageCreateFlags::VIDEO_PROFILE_INDEPENDENT_KHR)
            .as_raw() as _;
        let ret = ffi::av_hwframe_ctx_init(frames);
        if ret < 0 {
            ffi::av_buffer_unref(&mut frames);
            return Err(ffmpeg::Error::from(ret).into());
        }
        Ok(frames)
    }
}

/// Describes the NV12 Vulkan `frame` for the compositor and runs `render`
/// on it, following FFmpeg's frame protocol (lock, semaphores, recorded
/// state, unlock).
///
/// # Safety
/// `frame` must be a valid Vulkan frame from `frames`.
unsafe fn render_into(
    frames: *mut ffi::AVBufferRef,
    frame: &frame::Video,
    matrix: nox_color::Matrix3x4,
    render: impl FnOnce(&mut YuvTarget) -> nox_gpu::Result<()>,
) -> Result<()> {
    // SAFETY: guaranteed by the caller; lock/unlock bracket all access to
    // the AVVkFrame state, as FFmpeg requires.
    unsafe {
        let ctx = (*frames).data.cast::<ffi::AVHWFramesContext>();
        let vk_ctx = &*(*ctx).hwctx.cast::<ffi::AVVulkanFramesContext>();
        let vkf = (*frame.as_ptr()).data[0].cast::<ffi::AVVkFrame>();
        let (width, height) = ((*ctx).width as u32, (*ctx).height as u32);
        let multi_planar = (*vkf).img[1] == 0;
        let plane = |i: usize, format: PlaneFormat, w: u32, h: u32| VkPlane {
            image: vk::Handle::from_raw((*vkf).img[if multi_planar { 0 } else { i }]),
            aspect: if multi_planar {
                [vk::ImageAspectFlags::PLANE_0, vk::ImageAspectFlags::PLANE_1][i]
            } else {
                vk::ImageAspectFlags::COLOR
            },
            format,
            width: w,
            height: h,
        };
        if let Some(lock) = vk_ctx.lock_frame {
            lock(ctx, vkf);
        }
        let count = if multi_planar { 1 } else { 2 };
        let mut target = YuvTarget {
            width,
            height,
            luma: plane(0, PlaneFormat::R8, width, height),
            chroma: plane(1, PlaneFormat::Rg8, width.div_ceil(2), height.div_ceil(2)),
            images: (0..count)
                .map(|i| ImageSync {
                    image: vk::Handle::from_raw((*vkf).img[i]),
                    layout: vk::ImageLayout::from_raw((*vkf).layout[i]),
                    access: vk::AccessFlags::from_raw((*vkf).access[i] as u32),
                    queue_family: (*vkf).queue_family[i],
                    semaphore: vk::Handle::from_raw((*vkf).sem[i]),
                    value: (*vkf).sem_value[i],
                })
                .collect(),
            matrix,
        };
        let result = render(&mut target);
        for (i, sync) in target.images.iter().enumerate() {
            (*vkf).layout[i] = sync.layout.as_raw();
            (*vkf).access[i] = sync.access.as_raw() as _;
            (*vkf).queue_family[i] = sync.queue_family;
            (*vkf).sem_value[i] = sync.value;
        }
        if let Some(unlock) = vk_ctx.unlock_frame {
            unlock(ctx, vkf);
        }
        result.map_err(Error::from)
    }
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

fn set_color_tags(frame: &mut frame::Video) {
    frame.set_color_range(ffmpeg::color::Range::MPEG);
    frame.set_color_space(ffmpeg::color::Space::BT709);
    frame.set_color_primaries(ffmpeg::color::Primaries::BT709);
    frame.set_color_transfer_characteristic(ffmpeg::color::TransferCharacteristic::BT709);
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
