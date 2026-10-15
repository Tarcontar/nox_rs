//! Audio decoding to interleaved stereo `f32` at a fixed rate, with random
//! access by sample position.

use ffmpeg::{ChannelLayout, codec, ffi, format, frame, media, software};
use std::collections::VecDeque;
use std::path::Path;

use crate::{Error, Result};

/// Output channels: everything is mixed to stereo.
pub const CHANNELS: usize = 2;

/// Decodes the first audio stream of a file.
pub struct AudioDecoder {
    input: format::context::Input,
    decoder: codec::decoder::Audio,
    stream_index: usize,
    time_base: ffmpeg::Rational,
    /// Stream start in `time_base`.
    start: i64,
    rate: u32,
    resampler: Option<software::resampling::Context>,
    /// Decoded samples (interleaved) starting at sample `position`.
    buffer: VecDeque<f32>,
    position: i64,
    /// Where the next decoded sample goes, if known (unknown after a seek
    /// until the first frame arrives).
    next: Option<i64>,
    eof: bool,
}

impl AudioDecoder {
    /// Opens `path`; samples come out at `rate` Hz.
    pub fn open(path: &Path, rate: u32) -> Result<Self> {
        crate::init();
        let mut input = format::input(path)?;
        let stream = input
            .streams()
            .best(media::Type::Audio)
            .ok_or(Error::NoAudioStream)?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let start = match stream.start_time() {
            ffi::AV_NOPTS_VALUE => 0,
            start => start,
        };
        let context = codec::Context::from_parameters(stream.parameters())?;
        let decoder = context.decoder().audio()?;
        // Only the audio is read: less to demux, and Matroska's seeks
        // otherwise stop at video blocks ("keyframes not correctly marked").
        for mut other in input.streams_mut().filter(|s| s.index() != stream_index) {
            // SAFETY: the stream's own field, before reading packets.
            unsafe { (*other.as_mut_ptr()).discard = ffi::AVDiscard::AVDISCARD_ALL };
        }
        Ok(Self {
            input,
            decoder,
            stream_index,
            time_base,
            start,
            rate,
            resampler: None,
            buffer: VecDeque::new(),
            position: 0,
            next: Some(0),
            eof: false,
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The length of the file in seconds, if the container knows it.
    pub fn duration_seconds(&self) -> Option<f64> {
        let duration = self.input.duration();
        (duration > 0).then(|| duration as f64 / f64::from(ffi::AV_TIME_BASE))
    }

    /// Fills `out` (interleaved stereo) with the samples from sample `start`
    /// on (counted from the start of the stream at the output rate). Seeks
    /// if needed; past the end and before the start there is silence.
    pub fn read(&mut self, start: i64, out: &mut [f32]) -> Result<()> {
        let frames = (out.len() / CHANNELS) as i64;
        let end = start + frames;
        let buffered_end = self.position + (self.buffer.len() / CHANNELS) as i64;
        // Continue if the request starts within (or just past) what we have
        // decoded; otherwise jump.
        let continuous = start >= self.position && start <= buffered_end + self.rate as i64 / 4;
        if !continuous {
            self.seek(start)?;
        }
        while !self.eof && self.buffered_end() < end {
            self.decode_more()?;
            // Drop what lies before the request to keep the buffer small.
            self.discard_before(start);
        }
        self.discard_before(start);
        out.fill(0.0);
        let offset = ((self.position - start).max(0) as usize) * CHANNELS;
        if offset < out.len() {
            for (dst, src) in out[offset..].iter_mut().zip(&self.buffer) {
                *dst = *src;
            }
        }
        Ok(())
    }

    fn buffered_end(&self) -> i64 {
        self.position + (self.buffer.len() / CHANNELS) as i64
    }

    fn discard_before(&mut self, sample: i64) {
        let drop = ((sample - self.position).max(0) as usize * CHANNELS).min(self.buffer.len());
        self.buffer.drain(..drop);
        self.position += (drop / CHANNELS) as i64;
    }

    fn seek(&mut self, sample: i64) -> Result<()> {
        // A little early, so the decoder has settled at the requested sample.
        let seconds = (sample as f64 / f64::from(self.rate) - 0.1).max(0.0);
        let ts = self.start + (seconds / f64::from(self.time_base)) as i64;
        // SAFETY: valid context; seeks to the last position at or before `ts`.
        let result = unsafe {
            ffi::avformat_seek_file(
                self.input.as_mut_ptr(),
                self.stream_index as i32,
                i64::MIN,
                ts,
                ts,
                0,
            )
        };
        if result < 0 {
            return Err(ffmpeg::Error::from(result).into());
        }
        self.decoder.flush();
        self.resampler = None;
        self.buffer.clear();
        self.position = sample;
        self.next = None;
        self.eof = false;
        Ok(())
    }

    /// Decodes at least one more frame (or reaches the end).
    fn decode_more(&mut self) -> Result<()> {
        let mut decoded = frame::Audio::empty();
        loop {
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                self.append(&decoded)?;
                return Ok(());
            }
            match self.input.packets().next() {
                Some((stream, packet)) => {
                    if stream.index() == self.stream_index {
                        // Corrupt packets are skipped, as players do.
                        let _ = self.decoder.send_packet(&packet);
                    }
                }
                None => {
                    let _ = self.decoder.send_eof();
                    if self.decoder.receive_frame(&mut decoded).is_ok() {
                        self.append(&decoded)?;
                    } else {
                        self.eof = true;
                    }
                    return Ok(());
                }
            }
        }
    }

    /// Resamples a decoded frame and appends it to the buffer.
    fn append(&mut self, decoded: &frame::Audio) -> Result<()> {
        // Frame position at the output rate, from its timestamp.
        let at = decoded.pts().map(|pts| {
            let seconds = (pts - self.start) as f64 * f64::from(self.time_base);
            (seconds * f64::from(self.rate)).round() as i64
        });
        let layout = match decoded.channel_layout() {
            layout if layout.is_empty() => ChannelLayout::default(decoded.channels().into()),
            layout => layout,
        };
        if self.resampler.is_none() {
            self.resampler = Some(software::resampling::Context::get(
                decoded.format(),
                layout,
                decoded.rate(),
                format::Sample::F32(format::sample::Type::Packed),
                ChannelLayout::STEREO,
                self.rate,
            )?);
        }
        let resampler = self.resampler.as_mut().expect("resampler");
        let in_count = decoded.samples() as i32;
        // SAFETY: the context is initialized; buffers are sized by
        // swr_get_out_samples, and the input pointers come from a valid frame.
        let samples = unsafe {
            let ctx = resampler.as_mut_ptr();
            let capacity = ffi::swr_get_out_samples(ctx, in_count).max(0) as usize;
            let mut out = vec![0f32; capacity * CHANNELS];
            let out_ptr = out.as_mut_ptr().cast::<u8>();
            let n = ffi::swr_convert(
                ctx,
                &out_ptr,
                capacity as i32,
                (*decoded.as_ptr()).extended_data as *mut *const u8,
                in_count,
            );
            if n < 0 {
                return Err(ffmpeg::Error::from(n).into());
            }
            out.truncate(n as usize * CHANNELS);
            out
        };
        // After a seek the first frame tells where we are.
        let next = match (self.next, at) {
            (Some(next), _) => next,
            (None, Some(at)) => at,
            (None, None) => self.position,
        };
        if self.buffer.is_empty() {
            self.position = next;
        } else if next > self.buffered_end() {
            // A gap in the stream: fill with silence.
            let gap = ((next - self.buffered_end()) as usize).min(self.rate as usize * 10);
            self.buffer.extend(std::iter::repeat_n(0.0, gap * CHANNELS));
        }
        let appended = (samples.len() / CHANNELS) as i64;
        self.buffer.extend(samples);
        self.next = Some(self.buffered_end().max(next + appended));
        Ok(())
    }
}
