//! CPU thumbnails: one per file for the media pool (`thumbnail`), many
//! along a file for the timeline's filmstrips (`ThumbnailReader`).

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
    // A photo in tiles (HEIC): the whole picture, not its first tile.
    if let Some(grid) = crate::tiles::tile_grid(&input) {
        let picture = grid.decode(&mut input)?;
        return scale(&picture, max_width, max_height);
    }
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
pub(crate) fn first_frame(
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

/// Most frames decoded past the keyframe for one filmstrip thumbnail;
/// beyond, the frame reached so far stands in.
const READER_MAX_FRAMES: usize = 8;

/// Thumbnails at any position of one file, for the timeline's filmstrips.
/// Keeps the file open; each thumbnail seeks to the keyframe before its
/// time and decodes at most a few frames on (in software, sliced threads),
/// so it is near, not exactly at, the time asked for. Stills (photos, cover
/// art) are decoded once.
pub struct ThumbnailReader {
    input: format::context::Input,
    decoder: codec::decoder::Video,
    stream_index: usize,
    time_base: f64,
    start: i64,
    max_width: u32,
    max_height: u32,
    /// The picture of a still.
    still: Option<Image>,
}

impl ThumbnailReader {
    /// Opens `path` for thumbnails that fit `max_width` × `max_height`.
    pub fn open(path: &Path, max_width: u32, max_height: u32) -> Result<Self> {
        crate::init();
        let mut input = format::input(path)?;
        let tiled = crate::tiles::tile_grid(&input).is_some();
        let stream = input
            .streams()
            .best(media::Type::Video)
            .ok_or(Error::NoVideoStream)?;
        let stream_index = stream.index();
        let time_base = f64::from(stream.time_base());
        let start = crate::stream::start_time(&stream);
        let still = tiled
            || stream
                .disposition()
                .contains(format::stream::Disposition::ATTACHED_PIC)
            || crate::probe::is_image_demuxer(input.format().name());
        let mut context = codec::Context::from_parameters(stream.parameters())?;
        context.set_threading(codec::threading::Config {
            kind: codec::threading::Type::Slice,
            count: 0,
        });
        let decoder = context.decoder().video()?;
        crate::stream::discard_other_streams(&mut input, stream_index);
        let still = if still {
            Some(thumbnail(path, max_width, max_height)?)
        } else {
            None
        };
        Ok(Self {
            input,
            decoder,
            stream_index,
            time_base,
            start,
            max_width,
            max_height,
            still,
        })
    }

    /// The picture near `seconds` into the stream.
    pub fn at(&mut self, seconds: f64) -> Result<Image> {
        if let Some(still) = &self.still {
            return Ok(still.clone());
        }
        let target = self.start + (seconds.max(0.0) / self.time_base) as i64;
        if let Err(err) = crate::stream::seek_stream(&mut self.input, self.stream_index, target) {
            tracing::debug!("thumbnail seek failed: {err}");
        }
        self.decoder.flush();
        let mut best: Option<frame::Video> = None;
        let mut decoded = 0;
        let mut eof = false;
        let mut frame = frame::Video::empty();
        'decode: loop {
            while self.decoder.receive_frame(&mut frame).is_ok() {
                let pts = frame.timestamp().or(frame.pts()).unwrap_or(self.start);
                decoded += 1;
                best = Some(std::mem::replace(&mut frame, frame::Video::empty()));
                if pts >= target || decoded >= READER_MAX_FRAMES {
                    break 'decode;
                }
            }
            if eof {
                break;
            }
            eof = crate::stream::send_next_packet(
                &mut self.input,
                &mut self.decoder,
                self.stream_index,
            )?;
        }
        let frame = best.ok_or(Error::NoFrame)?;
        scale(&frame, self.max_width, self.max_height)
    }
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
