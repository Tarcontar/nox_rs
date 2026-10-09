//! Frame-accurate video decoding.
//!
//! `VideoDecoder::frame_at` returns the frame shown at a given frame index:
//! it decodes forward for nearby requests (playback, scrubbing forward) and
//! seeks to the preceding keyframe otherwise. With a `HwDevice`, codecs that
//! FFmpeg can decode with Vulkan Video produce `AV_PIX_FMT_VULKAN` frames;
//! if hardware initialization fails FFmpeg falls back to software decoding.

use ffmpeg::{codec, ffi, format, frame, media};
use nox_project::{Rational, Time};
use std::path::{Path, PathBuf};

use crate::{Error, HwDevice, Result, rational};

/// Requests at most this many seconds ahead decode forward instead of seeking.
const FORWARD_DECODE_SECONDS: f64 = 2.0;

pub struct VideoDecoder {
    path: PathBuf,
    input: format::context::Input,
    decoder: codec::decoder::Video,
    stream_index: usize,
    time_base: Rational,
    frame_rate: Rational,
    /// Stream start time in `time_base`.
    start: i64,
    eof: bool,
    /// Last returned frame and its index.
    last: Option<(i64, frame::Video)>,
    /// Decoded but not yet returned (we decoded past the request).
    pending: Option<(i64, frame::Video)>,
    logged_path: bool,
    /// Nothing has been read since opening or rewinding.
    at_start: bool,
    /// A still image: one frame, shown at every position.
    still: bool,
}

impl VideoDecoder {
    pub fn open(path: &Path, hw: Option<&HwDevice>) -> Result<Self> {
        crate::init();
        let input = format::input(path)?;
        let still = crate::probe::is_image_demuxer(input.format().name());
        let stream = input
            .streams()
            .best(media::Type::Video)
            .ok_or(Error::NoVideoStream)?;
        let stream_index = stream.index();
        let time_base = rational(stream.time_base()).ok_or(Error::NoVideoStream)?;
        let frame_rate = rational(stream.avg_frame_rate())
            .or_else(|| rational(stream.rate()))
            .unwrap_or(Rational::new(25, 1));
        let start = match stream.start_time() {
            ffi::AV_NOPTS_VALUE => 0,
            start => start,
        };

        let mut context = codec::Context::from_parameters(stream.parameters())?;
        let codec = codec::decoder::find(context.id()).ok_or(ffmpeg::Error::DecoderNotFound)?;
        match hw.filter(|_| supports_vulkan(&codec)) {
            // SAFETY: the context is not opened yet; FFmpeg takes its own
            // reference to the device.
            Some(hw) => unsafe {
                let ctx = context.as_mut_ptr();
                (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw.as_ptr());
                (*ctx).get_format = Some(prefer_vulkan);
            },
            None => context.set_threading(codec::threading::Config::kind(
                codec::threading::Type::Frame,
            )),
        }
        let decoder = context.decoder().open_as(codec)?.video()?;

        Ok(Self {
            path: path.to_owned(),
            input,
            decoder,
            stream_index,
            time_base,
            frame_rate,
            start,
            eof: false,
            last: None,
            pending: None,
            logged_path: false,
            still,
            at_start: true,
        })
    }

    /// Whether the file is a still image (shows the same frame throughout).
    pub fn is_still(&self) -> bool {
        self.still
    }

    /// The frame index to decode for `index`: stills have only frame 0
    /// (seeking would reopen and decode the image again).
    fn effective(&self, index: i64) -> i64 {
        if self.still { 0 } else { index.max(0) }
    }

    pub fn frame_rate(&self) -> Rational {
        self.frame_rate
    }

    /// The frame displayed at `index` (counted from the stream start at the
    /// stream's frame rate). Past the end, returns the last frame.
    pub fn frame_at(&mut self, index: i64) -> Result<frame::Video> {
        self.frame_at_until(index, &|| false)
    }

    /// Like `frame_at`, but gives up with `Error::Interrupted` as soon as
    /// `stop` returns true (checked after every decoded frame). Frames
    /// decoded until then still count: asking again continues from there.
    pub fn frame_at_until(&mut self, index: i64, stop: &dyn Fn() -> bool) -> Result<frame::Video> {
        let index = self.effective(index);
        if let Some((i, frame)) = &self.last
            && *i == index
        {
            return Ok(reference(frame));
        }
        let forward = self.can_decode_forward(index);
        // Where we last sought to, for backing off further.
        let mut sought = None;
        if !forward {
            self.seek(index)?;
            sought = Some(index);
        }

        loop {
            let next = match self.pending.take() {
                Some(pending) => Some(pending),
                None => self.decode_next()?,
            };
            let Some((i, frame)) = next else {
                // End of stream: hold the last frame.
                return self
                    .last
                    .as_ref()
                    .map(|(_, f)| reference(f))
                    .ok_or(Error::NoFrame);
            };
            if i > index
                && let Some((_, last)) = &self.last
            {
                // `last` (i < index) is still on screen at `index`.
                let result = reference(last);
                self.pending = Some((i, frame));
                return Ok(result);
            }
            if i > index
                && let Some(from) = sought.filter(|&from| from > 0)
            {
                // The first frame after the seek is already past the target:
                // demuxers seek by decode timestamps, which run ahead of
                // presentation timestamps with B-frames. Back off further.
                let one_second = self.frame_rate.as_f64().ceil() as i64;
                let back = ((index - from) * 2).max(one_second);
                let earlier = (index - back).max(0);
                self.seek(earlier)?;
                sought = Some(earlier);
                continue;
            }
            // Can't get before the target (inexact index); best effort.
            let result = reference(&frame);
            self.last = Some((i, frame));
            if i >= index {
                return Ok(result);
            }
            if stop() {
                return Err(Error::Interrupted);
            }
        }
    }

    /// Whether `index` is best reached by decoding on from the current
    /// position rather than seeking: it lies ahead, and either close or in
    /// the same group of pictures (seeking would land on a keyframe at or
    /// before where we are).
    fn can_decode_forward(&mut self, index: i64) -> bool {
        let window = FORWARD_DECODE_SECONDS * self.frame_rate.as_f64();
        match &self.last {
            Some((i, _)) => {
                let last = *i;
                index > last
                    && ((index - last) as f64 <= window
                        || self.keyframe_before(index).is_some_and(|k| k <= last))
            }
            // Freshly opened: decode from the start rather than seek.
            None => {
                self.at_start
                    && self.pending.is_none()
                    && ((index as f64) <= window || self.keyframe_before(index) == Some(0))
            }
        }
    }

    /// The last keyframe at or before frame `index`, from the demuxer's
    /// index (`None` if the container has none).
    ///
    /// Index entries carry decode timestamps, which with B-frames run ahead
    /// of presentation by the reorder delay; the first keyframe (shown
    /// first) tells by how much.
    pub fn keyframe_before(&mut self, index: i64) -> Option<i64> {
        // SAFETY: valid stream of our open input; entries stay valid until
        // the next demuxer call, and are only read here.
        let (first, found) = unsafe {
            let stream = *(*self.input.as_ptr()).streams.add(self.stream_index);
            let first = ffi::avformat_index_get_entry(stream, 0);
            if first.is_null() {
                return None;
            }
            let first = (*first).timestamp;
            let delay = self.start - first;
            let entry = ffi::avformat_index_get_entry_from_timestamp(
                stream,
                self.pts_of(index) - delay,
                ffi::AVSEEK_FLAG_BACKWARD,
            );
            (first, (!entry.is_null()).then(|| (*entry).timestamp))
        };
        let delay = self.start - first;
        Some(self.index_of(found? + delay).max(0))
    }

    /// About how many frames must be decoded to get frame `index`.
    pub fn decode_cost(&mut self, index: i64) -> i64 {
        let index = self.effective(index);
        if self.last.as_ref().is_some_and(|(i, _)| *i == index) {
            return 0;
        }
        if self.can_decode_forward(index) {
            let from = self.last.as_ref().map_or(0, |(i, _)| *i);
            return index - from;
        }
        match self.keyframe_before(index) {
            Some(keyframe) => index - keyframe + 1,
            // No index: assume a typical group of pictures.
            None => (FORWARD_DECODE_SECONDS * self.frame_rate.as_f64()) as i64,
        }
    }

    /// A frame at or shortly before `index` that is cheap to get: the exact
    /// frame if that takes at most `max_cost` decoded frames, otherwise
    /// the frame we are at (if it is earlier in the same group of pictures)
    /// or the keyframe before `index`. Afterwards the decoder is positioned
    /// to continue towards `index`.
    pub fn frame_near(&mut self, index: i64, max_cost: i64) -> Result<frame::Video> {
        let index = self.effective(index);
        if self.decode_cost(index) <= max_cost {
            return self.frame_at(index);
        }
        if self.can_decode_forward(index)
            && let Some((_, frame)) = &self.last
        {
            return Ok(reference(frame));
        }
        // Seek and take the first frame: the keyframe the demuxer lands on.
        self.seek(index)?;
        match self.decode_next()? {
            Some((i, frame)) if i <= index => {
                let result = reference(&frame);
                self.last = Some((i, frame));
                Ok(result)
            }
            // Landed past the target (decode timestamps) or at the end.
            Some((i, frame)) => {
                self.last = Some((i, frame));
                self.frame_at(index)
            }
            None => self.frame_at(index),
        }
    }

    fn seek(&mut self, index: i64) -> Result<()> {
        let target = self.pts_of(index);
        // SAFETY: valid context; seeks to the last keyframe at or before
        // `target` in the stream's time base.
        let ret = unsafe {
            ffi::avformat_seek_file(
                self.input.as_mut_ptr(),
                self.stream_index as i32,
                i64::MIN,
                target,
                target,
                0,
            )
        };
        if ret < 0 {
            // Some demuxers (e.g. image2) can't seek; start over instead.
            tracing::debug!(
                "{}: seek failed ({}), rewinding",
                self.path.display(),
                ffmpeg::Error::from(ret)
            );
            return self.rewind();
        }
        self.decoder.flush();
        self.eof = false;
        self.last = None;
        self.pending = None;
        self.at_start = false;
        Ok(())
    }

    /// Reopens the input and decodes from the beginning.
    fn rewind(&mut self) -> Result<()> {
        self.input = format::input(&self.path)?;
        self.decoder.flush();
        self.eof = false;
        self.last = None;
        self.pending = None;
        self.at_start = true;
        Ok(())
    }

    fn decode_next(&mut self) -> Result<Option<(i64, frame::Video)>> {
        loop {
            let mut frame = frame::Video::empty();
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    self.log_decode_path(&frame);
                    let pts = frame.timestamp().or(frame.pts()).unwrap_or(self.start);
                    return Ok(Some((self.index_of(pts), frame)));
                }
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(ffmpeg::Error::Other { errno })
                    if errno == ffmpeg::util::error::EAGAIN && !self.eof =>
                {
                    self.feed()?
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    return Ok(None);
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Sends the next packet of our stream, or EOF.
    fn feed(&mut self) -> Result<()> {
        let mut packet = ffmpeg::Packet::empty();
        loop {
            match packet.read(&mut self.input) {
                Ok(()) if packet.stream() == self.stream_index => {
                    self.at_start = false;
                    if let Err(err) = self.decoder.send_packet(&packet) {
                        // Corrupt packets are skipped; the decoder recovers.
                        tracing::debug!("decoder rejected packet: {err}");
                    }
                    return Ok(());
                }
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) => {
                    self.decoder.send_eof()?;
                    self.eof = true;
                    return Ok(());
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn pts_of(&self, index: i64) -> i64 {
        self.start
            + Time::new(index, self.frame_rate.inverse())
                .rescale(self.time_base)
                .ticks
    }

    fn index_of(&self, pts: i64) -> i64 {
        Time::new(pts - self.start, self.time_base)
            .rescale(self.frame_rate.inverse())
            .ticks
    }

    fn log_decode_path(&mut self, frame: &frame::Video) {
        if !self.logged_path {
            self.logged_path = true;
            let path = if frame.format() == format::Pixel::VULKAN {
                "Vulkan Video"
            } else {
                "software"
            };
            tracing::info!("decoding {} with {path}", self.decoder.id().name());
        }
    }
}

/// A new reference to the same frame data (no copy; works for GPU frames).
pub(crate) fn reference(frame: &frame::Video) -> frame::Video {
    // SAFETY: av_frame_clone returns a new reference or null on OOM.
    unsafe {
        let cloned = ffi::av_frame_clone(frame.as_ptr());
        assert!(!cloned.is_null(), "out of memory");
        frame::Video::wrap(cloned)
    }
}

fn supports_vulkan(codec: &ffmpeg::Codec) -> bool {
    (0..)
        .map_while(|i| {
            // SAFETY: returns null past the last config.
            let config = unsafe { ffi::avcodec_get_hw_config(codec.as_ptr(), i) };
            // SAFETY: non-null configs are static.
            (!config.is_null()).then(|| unsafe { &*config })
        })
        .any(|config| {
            config.device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN
                && config.methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32 != 0
        })
}

/// `get_format` callback: Vulkan if offered, otherwise FFmpeg's default
/// (software) choice.
unsafe extern "C" fn prefer_vulkan(
    ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // SAFETY: FFmpeg passes a list terminated by AV_PIX_FMT_NONE.
    unsafe {
        let mut format = formats;
        while *format != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *format == ffi::AVPixelFormat::AV_PIX_FMT_VULKAN {
                return *format;
            }
            format = format.add(1);
        }
        ffi::avcodec_default_get_format(ctx, formats)
    }
}
