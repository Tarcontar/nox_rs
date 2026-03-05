//! Thumbnail generation for media files.
//!
//! Decoding and scaling happens on a background thread.  Results are polled
//! from the GTK main loop via `glib::timeout_add_local`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;

use crate::media::decoder::{FfmpegDecoder, VideoDecoder};
use crate::render::VideoFrame;

/// The width of a generated thumbnail (height is calculated to preserve aspect
/// ratio, but we cap it as well).
pub const THUMBNAIL_WIDTH: u32 = 160;
pub const THUMBNAIL_HEIGHT: u32 = 90;

/// Manages background thumbnail generation.
pub struct ThumbnailGenerator;

impl ThumbnailGenerator {
    pub fn new() -> Self {
        Self
    }

    /// Generate a thumbnail frame synchronously (for use on a background thread).
    /// Returns raw RGBA pixel data as a [`VideoFrame`].
    pub fn generate_frame(&self, path: &Path) -> Result<VideoFrame> {
        // Open the file and seek to ~10% of its duration (or 1s).
        let mut decoder = FfmpegDecoder::new(path)?;
        let seek_pos = (decoder.stream_info().duration_secs * 0.1).max(0.5);
        let frame = decoder.decode_frame_at(seek_pos)?;

        // Scale down to thumbnail size.
        Ok(scale_frame(&frame, THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT))
    }

    /// Spawn a background thread that generates a thumbnail.
    /// Returns a shared handle that can be polled from the main thread.
    pub fn generate_async(path: &Path, _media_id: u64) -> Arc<Mutex<Option<VideoFrame>>> {
        let path = path.to_path_buf();
        let result: Arc<Mutex<Option<VideoFrame>>> = Arc::new(Mutex::new(None));
        let result_clone = result.clone();

        thread::spawn(move || {
            let gen = ThumbnailGenerator::new();
            if let Ok(frame) = gen.generate_frame(&path) {
                *result_clone.lock().unwrap() = Some(frame);
            }
        });

        result
    }
}

/// Naive nearest-neighbour down-scale of an RGBA frame.
fn scale_frame(
    src: &crate::render::VideoFrame,
    dst_w: u32,
    dst_h: u32,
) -> crate::render::VideoFrame {
    // Preserve aspect ratio.
    let aspect = src.width as f64 / src.height as f64;
    let (out_w, out_h) = if aspect > dst_w as f64 / dst_h as f64 {
        (dst_w, (dst_w as f64 / aspect).round() as u32)
    } else {
        ((dst_h as f64 * aspect).round() as u32, dst_h)
    };

    let mut data = vec![0u8; (out_w * out_h * 4) as usize];
    for y in 0..out_h {
        for x in 0..out_w {
            let sx = (x as f64 / out_w as f64 * src.width as f64) as u32;
            let sy = (y as f64 / out_h as f64 * src.height as f64) as u32;
            let si = ((sy * src.width + sx) * 4) as usize;
            let di = ((y * out_w + x) * 4) as usize;
            if si + 3 < src.data.len() {
                data[di..di + 4].copy_from_slice(&src.data[si..si + 4]);
            }
        }
    }

    crate::render::VideoFrame {
        data,
        width: out_w,
        height: out_h,
        timestamp_secs: src.timestamp_secs,
    }
}
