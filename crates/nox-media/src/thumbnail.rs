//! CPU thumbnails for the media pool.

use ffmpeg::{codec, ffi, format, frame, media};
use std::path::Path;

use crate::{Error, Image, Result};

/// Seek this far into a clip for its thumbnail, to skip black leaders...
const THUMBNAIL_POSITION: f64 = 0.1;
/// ...but never further than this many seconds,
const THUMBNAIL_MAX_SECONDS: f64 = 10.0;
/// ...and not at all in clips shorter than this.
const THUMBNAIL_MIN_SECONDS: f64 = 1.0;

/// Decodes a representative frame of `path` and scales it to fit within
/// `max_width` × `max_height`, keeping the display aspect ratio. Audio files
/// with embedded cover art return the cover.
pub fn thumbnail(path: &Path, max_width: u32, max_height: u32) -> Result<Image> {
    crate::init();
    let mut input = format::input(path)?;
    let stream = input
        .streams()
        .best(media::Type::Video)
        .ok_or(Error::NoVideoStream)?;
    let index = stream.index();
    let is_cover_art = stream
        .disposition()
        .contains(format::stream::Disposition::ATTACHED_PIC);
    let seconds = stream.duration() as f64 * f64::from(stream.time_base());
    let mut decoder = codec::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;

    let is_still = is_cover_art || crate::probe::is_image_demuxer(input.format().name());
    if !is_still && seconds >= THUMBNAIL_MIN_SECONDS {
        let target = (seconds * THUMBNAIL_POSITION).min(THUMBNAIL_MAX_SECONDS);
        let ts = (target * f64::from(ffi::AV_TIME_BASE)) as i64;
        match input.seek(ts, ..ts) {
            Ok(()) => {
                if let Some(frame) = first_frame(&mut input, &mut decoder, index)? {
                    return scale(&frame, max_width, max_height);
                }
                // Nothing after the seek point (bad index); start over.
                tracing::debug!("{}: no frame after thumbnail seek", path.display());
                input.seek(0, ..0)?;
                decoder.flush();
            }
            Err(err) => tracing::debug!("{}: seek for thumbnail failed: {err}", path.display()),
        }
    }
    match first_frame(&mut input, &mut decoder, index)? {
        Some(frame) => scale(&frame, max_width, max_height),
        None => Err(Error::NoFrame),
    }
}

/// Decodes the first frame of stream `index` from the current position.
fn first_frame(
    input: &mut format::context::Input,
    decoder: &mut codec::decoder::Video,
    index: usize,
) -> Result<Option<frame::Video>> {
    let mut frame = frame::Video::empty();
    for (stream, packet) in input.packets() {
        if stream.index() != index {
            continue;
        }
        // Skip corrupt packets; a later one may still decode.
        if decoder.send_packet(&packet).is_ok() && decoder.receive_frame(&mut frame).is_ok() {
            return Ok(Some(frame));
        }
    }
    decoder.send_eof()?;
    Ok(decoder.receive_frame(&mut frame).ok().map(|()| frame))
}

fn scale(frame: &frame::Video, max_width: u32, max_height: u32) -> Result<Image> {
    let (width, height) = crate::display_size(frame);
    let factor = (f64::from(max_width) / f64::from(width))
        .min(f64::from(max_height) / f64::from(height))
        .min(1.0);
    let out_width = ((f64::from(width) * factor).round() as u32).max(1);
    let out_height = ((f64::from(height) * factor).round() as u32).max(1);
    crate::rgba_cpu(frame, out_width, out_height)
}
