//! Video decoder abstraction layer.
//!
//! The [`VideoDecoder`] trait defines a backend-independent interface for
//! decoding video frames.  The default implementation [`FfmpegDecoder`] uses
//! FFmpeg via `ffmpeg-sys-next` for CPU (software) decoding.
//!
//! Future implementors can wrap GPU-accelerated decoders (VAAPI, NVDEC,
//! VideoToolbox, Vulkan Video, …) behind the same trait so the rest of the
//! application stays unchanged.

use std::ffi::CString;
use std::path::Path;
use std::ptr;

use anyhow::{bail, Context, Result};
use ffmpeg_sys_next as ffi;

use crate::render::VideoFrame;

// ---------------------------------------------------------------------------
// Stream metadata
// ---------------------------------------------------------------------------

/// Metadata extracted from the video stream.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    /// Duration of the stream in seconds.
    pub duration_secs: f64,
    /// Frames-per-second (average).
    pub fps: f64,
    /// Name of the codec (e.g. "h264", "vp9").
    pub codec_name: String,
}

// ---------------------------------------------------------------------------
// VideoDecoder trait
// ---------------------------------------------------------------------------

/// Backend-independent video decoder.
///
/// This is the seam that allows swapping CPU decoding for GPU decoding in the
/// future without touching the rest of the application.
pub trait VideoDecoder: Send {
    /// Open a media file at `path` and prepare to decode its first video
    /// stream.
    fn open(path: &Path) -> Result<Self>
    where
        Self: Sized;

    /// Return metadata about the opened video stream.
    fn stream_info(&self) -> &StreamInfo;

    /// Seek to (approximately) the given timestamp.  The next call to
    /// [`decode_next_frame`] will return a frame at or after this time.
    fn seek(&mut self, timestamp_secs: f64) -> Result<()>;

    /// Decode the next frame in presentation order.
    /// Returns `None` when the stream is exhausted.
    fn decode_next_frame(&mut self) -> Result<Option<VideoFrame>>;

    /// Convenience: seek + decode to produce a frame at the given time.
    fn decode_frame_at(&mut self, timestamp_secs: f64) -> Result<VideoFrame> {
        self.seek(timestamp_secs)?;
        self.decode_next_frame()?
            .ok_or_else(|| anyhow::anyhow!("No frame at {timestamp_secs:.3}s"))
    }
}

// ---------------------------------------------------------------------------
// FfmpegDecoder – CPU software decoding via FFmpeg
// ---------------------------------------------------------------------------

/// FFmpeg-based software video decoder.
///
/// Wraps the low-level `ffmpeg-sys-next` API in a safe Rust interface.
/// All FFmpeg pointers are exclusively owned and freed on [`Drop`].
pub struct FfmpegDecoder {
    format_ctx: *mut ffi::AVFormatContext,
    codec_ctx: *mut ffi::AVCodecContext,
    video_stream_idx: usize,
    sws_ctx: *mut ffi::SwsContext,
    frame: *mut ffi::AVFrame,
    rgba_frame: *mut ffi::AVFrame,
    rgba_buffer: Vec<u8>,
    info: StreamInfo,
    /// Stream time-base numerator / denominator for timestamp conversion.
    time_base: (i32, i32),
}

// The decoder is single-threaded but we need Send so it can live in a
// background decode thread.
unsafe impl Send for FfmpegDecoder {}

impl VideoDecoder for FfmpegDecoder {
    fn open(path: &Path) -> Result<Self> {
        Self::new(path)
    }

    fn stream_info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, timestamp_secs: f64) -> Result<()> {
        self.seek_internal(timestamp_secs)
    }

    fn decode_next_frame(&mut self) -> Result<Option<VideoFrame>> {
        self.decode_next_internal()
    }
}

impl FfmpegDecoder {
    /// Open the file at `path` and set up the decoder pipeline.
    pub fn new(path: &Path) -> Result<Self> {
        let path_str = path.to_str().context("Path contains invalid UTF-8")?;
        let c_path = CString::new(path_str)?;

        unsafe {
            // -- Open input -------------------------------------------------
            let mut format_ctx: *mut ffi::AVFormatContext = ptr::null_mut();
            if ffi::avformat_open_input(
                &mut format_ctx,
                c_path.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
            ) < 0
            {
                bail!("Could not open input file: {}", path_str);
            }

            if ffi::avformat_find_stream_info(format_ctx, ptr::null_mut()) < 0 {
                ffi::avformat_close_input(&mut format_ctx);
                bail!("Could not find stream info");
            }

            // -- Find best video stream ------------------------------------
            let video_stream_idx = ffi::av_find_best_stream(
                format_ctx,
                ffi::AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                ptr::null_mut(),
                0,
            );
            if video_stream_idx < 0 {
                ffi::avformat_close_input(&mut format_ctx);
                bail!("No video stream found");
            }
            let video_stream_idx = video_stream_idx as usize;

            let streams = std::slice::from_raw_parts(
                (*format_ctx).streams,
                (*format_ctx).nb_streams as usize,
            );
            let stream = *streams
                .get(video_stream_idx)
                .context("Stream index out of bounds")?;
            let codecpar = (*stream).codecpar;

            // -- Set up codec context --------------------------------------
            let codec = ffi::avcodec_find_decoder((*codecpar).codec_id);
            if codec.is_null() {
                ffi::avformat_close_input(&mut format_ctx);
                bail!("Unsupported codec");
            }

            let codec_ctx = ffi::avcodec_alloc_context3(codec);
            if codec_ctx.is_null() {
                ffi::avformat_close_input(&mut format_ctx);
                bail!("Could not allocate codec context");
            }

            if ffi::avcodec_parameters_to_context(codec_ctx, codecpar) < 0 {
                ffi::avcodec_free_context(&mut (codec_ctx as *mut _));
                ffi::avformat_close_input(&mut format_ctx);
                bail!("Could not copy codec params to context");
            }

            if ffi::avcodec_open2(codec_ctx, codec, ptr::null_mut()) < 0 {
                ffi::avcodec_free_context(&mut (codec_ctx as *mut _));
                ffi::avformat_close_input(&mut format_ctx);
                bail!("Could not open codec");
            }

            // -- Allocate frames -------------------------------------------
            let frame = ffi::av_frame_alloc();
            let rgba_frame = ffi::av_frame_alloc();
            if frame.is_null() || rgba_frame.is_null() {
                bail!("Could not allocate frames");
            }

            let width = (*codec_ctx).width as u32;
            let height = (*codec_ctx).height as u32;

            // RGBA buffer
            let rgba_size = (width * height * 4) as usize;
            let mut rgba_buffer = vec![0u8; rgba_size];

            // Set up rgba_frame to point into our buffer
            (*rgba_frame).width = width as i32;
            (*rgba_frame).height = height as i32;
            (*rgba_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_RGBA as i32;

            ffi::av_image_fill_arrays(
                (*rgba_frame).data.as_mut_ptr(),
                (*rgba_frame).linesize.as_mut_ptr(),
                rgba_buffer.as_mut_ptr(),
                ffi::AVPixelFormat::AV_PIX_FMT_RGBA,
                width as i32,
                height as i32,
                1,
            );

            // -- Scaler context (source format → RGBA) ---------------------
            let src_fmt = (*codec_ctx).pix_fmt;
            let sws_ctx = ffi::sws_getContext(
                width as i32,
                height as i32,
                src_fmt,
                width as i32,
                height as i32,
                ffi::AVPixelFormat::AV_PIX_FMT_RGBA,
                ffi::SWS_BILINEAR as i32,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if sws_ctx.is_null() {
                bail!("Could not create sws context");
            }

            // -- Extract metadata ------------------------------------------
            let tb = (*stream).time_base;
            let time_base = (tb.num, tb.den);

            let duration_secs = if (*stream).duration > 0 {
                (*stream).duration as f64 * tb.num as f64 / tb.den as f64
            } else if (*format_ctx).duration > 0 {
                (*format_ctx).duration as f64 / ffi::AV_TIME_BASE as f64
            } else {
                0.0
            };

            let r = (*stream).r_frame_rate;
            let fps = if r.den > 0 {
                r.num as f64 / r.den as f64
            } else {
                30.0
            };

            let codec_name = if !(*codec).name.is_null() {
                std::ffi::CStr::from_ptr((*codec).name)
                    .to_string_lossy()
                    .into_owned()
            } else {
                "unknown".to_string()
            };

            let info = StreamInfo {
                width,
                height,
                duration_secs,
                fps,
                codec_name,
            };

            log::info!(
                "Opened {:?}: {}×{} {:.2}fps {:.1}s ({})",
                path,
                width,
                height,
                fps,
                duration_secs,
                info.codec_name,
            );

            Ok(Self {
                format_ctx,
                codec_ctx,
                video_stream_idx,
                sws_ctx,
                frame,
                rgba_frame,
                rgba_buffer,
                info,
                time_base,
            })
        }
    }

    // -- Internal helpers ---------------------------------------------------

    fn seek_internal(&mut self, timestamp_secs: f64) -> Result<()> {
        unsafe {
            let ts = (timestamp_secs * self.time_base.1 as f64 / self.time_base.0 as f64) as i64;
            let ret = ffi::av_seek_frame(
                self.format_ctx,
                self.video_stream_idx as i32,
                ts,
                ffi::AVSEEK_FLAG_BACKWARD as i32,
            );
            if ret < 0 {
                bail!("Seek to {timestamp_secs:.3}s failed (err {ret})");
            }
            ffi::avcodec_flush_buffers(self.codec_ctx);
        }
        Ok(())
    }

    fn decode_next_internal(&mut self) -> Result<Option<VideoFrame>> {
        unsafe {
            let mut packet: ffi::AVPacket = std::mem::zeroed();

            loop {
                let ret = ffi::av_read_frame(self.format_ctx, &mut packet);
                if ret < 0 {
                    // End-of-file or error – try to drain the decoder.
                    ffi::avcodec_send_packet(self.codec_ctx, ptr::null());
                    return self.receive_frame();
                }

                if packet.stream_index as usize != self.video_stream_idx {
                    ffi::av_packet_unref(&mut packet);
                    continue;
                }

                let send_ret = ffi::avcodec_send_packet(self.codec_ctx, &packet);
                ffi::av_packet_unref(&mut packet);

                if send_ret < 0 {
                    bail!("Error sending packet to decoder (err {send_ret})");
                }

                if let Some(frame) = self.receive_frame()? {
                    return Ok(Some(frame));
                }
                // Decoder needs more packets – keep reading.
            }
        }
    }

    /// Try to receive a decoded frame and convert it to RGBA.
    fn receive_frame(&mut self) -> Result<Option<VideoFrame>> {
        unsafe {
            let ret = ffi::avcodec_receive_frame(self.codec_ctx, self.frame);
            if ret == ffi::AVERROR(ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
                return Ok(None);
            }
            if ret < 0 {
                bail!("Error receiving frame from decoder (err {ret})");
            }

            // Convert to RGBA
            ffi::sws_scale(
                self.sws_ctx,
                (*self.frame).data.as_ptr() as *const *const u8,
                (*self.frame).linesize.as_ptr(),
                0,
                (*self.frame).height,
                (*self.rgba_frame).data.as_ptr(),
                (*self.rgba_frame).linesize.as_ptr(),
            );

            let pts = (*self.frame).pts;
            let timestamp_secs = if pts != ffi::AV_NOPTS_VALUE {
                pts as f64 * self.time_base.0 as f64 / self.time_base.1 as f64
            } else {
                0.0
            };

            let width = self.info.width;
            let height = self.info.height;
            let size = (width * height * 4) as usize;

            Ok(Some(VideoFrame {
                data: self.rgba_buffer[..size].to_vec(),
                width,
                height,
                timestamp_secs,
            }))
        }
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.sws_ctx.is_null() {
                ffi::sws_freeContext(self.sws_ctx);
            }
            if !self.rgba_frame.is_null() {
                ffi::av_frame_free(&mut self.rgba_frame);
            }
            if !self.frame.is_null() {
                ffi::av_frame_free(&mut self.frame);
            }
            if !self.codec_ctx.is_null() {
                ffi::avcodec_free_context(&mut self.codec_ctx);
            }
            if !self.format_ctx.is_null() {
                ffi::avformat_close_input(&mut self.format_ctx);
            }
        }
    }
}
