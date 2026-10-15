//! Photos stored as a grid of tiles: HEIF/HEIC (phones' photos) and AVIF
//! split large images into independently coded tiles (512×512 on
//! iPhones). FFmpeg demuxes each tile as a stream of its own and describes
//! the layout in a tile grid stream group; the picture is the tiles placed
//! side by side, cropped to the grid's output rectangle.
//!
//! The tiles are decoded once, in software (a photo's are small), and
//! assembled into one frame in the tiles' own (planar) pixel format.

use ffmpeg::{codec, ffi, format, frame};
use std::collections::VecDeque;

use crate::{Error, Result};

/// Where a picture's tiles go.
pub(crate) struct TileGrid {
    /// Stream index of each tile, and where its top left corner goes.
    tiles: Vec<(usize, i32, i32)>,
    coded_width: i32,
    coded_height: i32,
    /// The picture within the coded area.
    left: i32,
    top: i32,
    pub width: u32,
    pub height: u32,
}

/// The first tile grid of `input`, if it has one.
pub(crate) fn tile_grid(input: &format::context::Input) -> Option<TileGrid> {
    // SAFETY: the format context's stream groups, valid as long as `input`.
    unsafe {
        let ctx = input.as_ptr();
        let groups = (*ctx).stream_groups;
        for g in 0..(*ctx).nb_stream_groups as usize {
            let group = &**groups.add(g);
            if group.type_ != ffi::AVStreamGroupParamsType::AV_STREAM_GROUP_PARAMS_TILE_GRID {
                continue;
            }
            let grid = &*group.params.tile_grid;
            let tiles = (0..grid.nb_tiles as usize)
                .map(|t| {
                    let offset = &*grid.offsets.add(t);
                    let stream = *group.streams.add(offset.idx as usize);
                    ((*stream).index as usize, offset.horizontal, offset.vertical)
                })
                .collect();
            return Some(TileGrid {
                tiles,
                coded_width: grid.coded_width,
                coded_height: grid.coded_height,
                left: grid.horizontal_offset,
                top: grid.vertical_offset,
                width: grid.width.max(1) as u32,
                height: grid.height.max(1) as u32,
            });
        }
        None
    }
}

impl TileGrid {
    /// Decodes every tile of `input` and assembles the picture.
    pub(crate) fn decode(&self, input: &mut format::context::Input) -> Result<frame::Video> {
        let first = self.tiles.first().ok_or(Error::NoFrame)?.0;
        let parameters = input.stream(first).ok_or(Error::NoFrame)?.parameters();
        let mut decoder = codec::Context::from_parameters(parameters)?
            .decoder()
            .video()?;
        // Tiles come out in the order they went in (intra images, no
        // reordering); remember which is which.
        let mut sent = VecDeque::new();
        let mut decoded: Vec<Option<frame::Video>> = vec![None; self.tiles.len()];
        let receive = |decoder: &mut ffmpeg::decoder::Video,
                       sent: &mut VecDeque<usize>,
                       decoded: &mut Vec<Option<frame::Video>>| {
            let mut tile = frame::Video::empty();
            while decoder.receive_frame(&mut tile).is_ok() {
                if let Some(t) = sent.pop_front() {
                    decoded[t] = Some(std::mem::replace(&mut tile, frame::Video::empty()));
                }
            }
        };
        for (stream, packet) in input.packets() {
            let Some(t) = self.tiles.iter().position(|&(s, ..)| s == stream.index()) else {
                continue;
            };
            if decoder.send_packet(&packet).is_ok() {
                sent.push_back(t);
            }
            receive(&mut decoder, &mut sent, &mut decoded);
        }
        decoder.send_eof()?;
        receive(&mut decoder, &mut sent, &mut decoded);

        let reference = decoded.iter().flatten().next().ok_or(Error::NoFrame)?;
        let pixel = reference.format();
        let layout = PlanarLayout::of(pixel)
            .ok_or_else(|| Error::Encode(format!("tiled image in {pixel:?}")))?;
        let mut picture =
            frame::Video::new(pixel, self.coded_width as u32, self.coded_height as u32);
        for ((_, x, y), tile) in self.tiles.iter().zip(&decoded) {
            if let Some(tile) = tile {
                layout.place(tile, &mut picture, *x, *y);
            }
        }
        // SAFETY: valid frames; the crop lies within the coded area (FFmpeg
        // checks it).
        unsafe {
            let dst = picture.as_mut_ptr();
            ffi::av_frame_copy_props(dst, reference.as_ptr());
            (*dst).crop_left = self.left.max(0) as usize;
            (*dst).crop_top = self.top.max(0) as usize;
            (*dst).crop_right = (self.coded_width - self.left - self.width as i32).max(0) as usize;
            (*dst).crop_bottom =
                (self.coded_height - self.top - self.height as i32).max(0) as usize;
            let ret = ffi::av_frame_apply_cropping(dst, ffi::AV_FRAME_CROP_UNALIGNED as i32);
            if ret < 0 {
                return Err(ffmpeg::Error::from(ret).into());
            }
        }
        Ok(picture)
    }
}

/// The planes of a planar pixel format: bytes per sample and chroma
/// subsampling.
struct PlanarLayout {
    planes: usize,
    bytes: usize,
    log2_chroma: (u32, u32),
}

impl PlanarLayout {
    fn of(pixel: format::Pixel) -> Option<Self> {
        let descriptor = pixel.descriptor()?;
        // SAFETY: descriptors are static.
        let raw = unsafe { &*descriptor.as_ptr() };
        let planar = raw.flags & ffi::AV_PIX_FMT_FLAG_PLANAR as u64 != 0;
        let components = raw.nb_components as usize;
        // One component per plane (Y, Cb, Cr, maybe alpha), same depth.
        if !planar || (0..components).any(|c| raw.comp[c].plane as usize != c) {
            return None;
        }
        Some(Self {
            planes: components,
            bytes: (raw.comp[0].depth as usize).div_ceil(8),
            log2_chroma: (
                u32::from(descriptor.log2_chroma_w()),
                u32::from(descriptor.log2_chroma_h()),
            ),
        })
    }

    /// Copies `tile` into `picture` with its top left corner at (x, y),
    /// clipped to the picture.
    fn place(&self, tile: &frame::Video, picture: &mut frame::Video, x: i32, y: i32) {
        for plane in 0..self.planes {
            let (sx, sy) = if plane == 1 || plane == 2 {
                self.log2_chroma
            } else {
                (0, 0)
            };
            let (x, y) = ((x.max(0) as usize) >> sx, (y.max(0) as usize) >> sy);
            let (tile_w, tile_h) = (
                tile.plane_width(plane) as usize,
                tile.plane_height(plane) as usize,
            );
            let (dst_w, dst_h) = (
                picture.plane_width(plane) as usize,
                picture.plane_height(plane) as usize,
            );
            if x >= dst_w || y >= dst_h {
                continue;
            }
            let row = tile_w.min(dst_w - x) * self.bytes;
            let rows = tile_h.min(dst_h - y);
            let (src_stride, dst_stride) = (tile.stride(plane), picture.stride(plane));
            let src = tile.data(plane);
            let dst = picture.data_mut(plane);
            for r in 0..rows {
                let from = r * src_stride;
                let to = (y + r) * dst_stride + x * self.bytes;
                dst[to..to + row].copy_from_slice(&src[from..from + row]);
            }
        }
    }
}
