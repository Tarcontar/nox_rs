//! Small FFmpeg helpers the readers share (decoder, thumbnails, audio,
//! probe, encoders): seeking a stream, feeding a decoder, stream start
//! times, and quieting FFmpeg's log.

use ffmpeg::{decoder, ffi, format};
use nox_project::Rational;

/// Seeks `input` to the last keyframe at or before `ts` in stream `index`'s
/// time base.
pub(crate) fn seek_stream(
    input: &mut format::context::Input,
    index: usize,
    ts: i64,
) -> Result<(), ffmpeg::Error> {
    // SAFETY: a valid, open input context.
    let ret =
        unsafe { ffi::avformat_seek_file(input.as_mut_ptr(), index as i32, i64::MIN, ts, ts, 0) };
    if ret < 0 {
        Err(ffmpeg::Error::from(ret))
    } else {
        Ok(())
    }
}

/// Sends the next packet of stream `index` to `decoder`, or the end of
/// the stream when there are no more; returns whether it was the end.
/// Corrupt packets are skipped, as players do: a later one may decode.
pub(crate) fn send_next_packet(
    input: &mut format::context::Input,
    decoder: &mut decoder::Opened,
    index: usize,
) -> Result<bool, ffmpeg::Error> {
    let mut packet = ffmpeg::Packet::empty();
    loop {
        match packet.read(input) {
            Ok(()) if packet.stream() == index => {
                if let Err(err) = decoder.send_packet(&packet) {
                    tracing::debug!("decoder rejected packet: {err}");
                }
                return Ok(false);
            }
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => {
                // Already sent is fine: the decoder is draining.
                match decoder.send_eof() {
                    Ok(()) | Err(ffmpeg::Error::Eof) => return Ok(true),
                    Err(err) => return Err(err),
                }
            }
            Err(err) => return Err(err),
        }
    }
}

/// The stream's start time in its time base (0 if unknown).
pub(crate) fn start_time(stream: &format::stream::Stream) -> i64 {
    match stream.start_time() {
        ffi::AV_NOPTS_VALUE => 0,
        start => start,
    }
}

/// The rate the stream's frames come at.
///
/// Usually the average rate. Phones record variable frame rates, though
/// (iPhone Live Photos: 30 fps, then 15 fps), and their average (780/41)
/// is no frame's rate: frames on a grid of it collide or repeat, so motion
/// jumps. Those use FFmpeg's base rate, the finest the timestamps need,
/// so every frame has a place of its own and gaps hold the frame before.
/// A base rate of twice the average is interlaced video's field rate, not
/// a variable frame rate, and isn't used.
pub(crate) fn frame_rate(stream: &format::stream::Stream) -> Option<Rational> {
    let average = crate::rational(stream.avg_frame_rate());
    let base = crate::rational(stream.rate());
    match (average, base) {
        (Some(average), Some(base)) => {
            let (a, b) = (average.as_f64(), base.as_f64());
            let near = |x: f64, y: f64| (x / y - 1.0).abs() < 0.001;
            if near(a, b) || near(b, 2.0 * a) || b < a || b > 240.0 {
                Some(average)
            } else {
                Some(base)
            }
        }
        (average, base) => average.or(base),
    }
}

/// Makes the demuxer skip every stream but `index`: less to read, and
/// Matroska's seeks otherwise stop at other streams' blocks ("keyframes
/// not correctly marked").
pub(crate) fn discard_other_streams(input: &mut format::context::Input, index: usize) {
    for mut other in input.streams_mut().filter(|s| s.index() != index) {
        // SAFETY: the stream's own field, before reading packets.
        unsafe { (*other.as_mut_ptr()).discard = ffi::AVDiscard::AVDISCARD_ALL };
    }
}

/// Runs `f` with FFmpeg's log silenced, for attempts whose failure is an
/// answer rather than an error (probing encoders and devices).
pub(crate) fn with_quiet_log<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: plain global setters.
    let level = unsafe { ffi::av_log_get_level() };
    unsafe { ffi::av_log_set_level(ffi::AV_LOG_QUIET) };
    let result = f();
    // SAFETY: as above.
    unsafe { ffi::av_log_set_level(level) };
    result
}

/// The display matrix in side data of `size` bytes at `data`, if it is one.
///
/// # Safety
/// `data` must be null or point to `size` readable bytes.
pub(crate) unsafe fn display_matrix<'a>(data: *const u8, size: usize) -> Option<&'a [i32; 9]> {
    // SAFETY: guaranteed by the caller; a display matrix is nine 32-bit
    // values, and side data is suitably aligned.
    (!data.is_null() && size >= 36).then(|| unsafe { &*data.cast::<[i32; 9]>() })
}
