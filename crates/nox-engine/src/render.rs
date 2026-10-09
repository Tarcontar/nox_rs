//! Renders sequence frames: decodes every layer, composites them on the
//! GPU and hands out the result as DMA-BUF or in system memory.

use nox_gpu::{BlendMode, Compositor, Image, Placement, Rect, YuvTarget};
use nox_media::{HwDevice, VideoDecoder};
use nox_project::{Time, Timeline};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{FrameImage, RenderedFrame};

/// Open decoders, so switching between clips (or showing two parts of one
/// file at once) doesn't reopen files.
const MAX_DECODERS: usize = 8;
/// Decoders per file.
const MAX_PER_FILE: usize = 2;

struct PooledDecoder {
    path: PathBuf,
    decoder: VideoDecoder,
    used: u64,
}

/// Rendering a frame exactly, or a quick stand-in for scrubbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quality {
    Exact,
    /// Each layer shows the nearest frame that is cheap to decode (at most
    /// `PREVIEW_COST` frames): usually the exact one, else the keyframe
    /// before it.
    Preview,
}

/// Decoding more frames than this for one layer is worth a preview first.
pub(crate) const PREVIEW_COST: i64 = 8;

#[derive(Default)]
struct DecoderPool {
    decoders: Vec<PooledDecoder>,
    clock: u64,
    /// `clock` at the start of the current frame; decoders used since are
    /// busy.
    frame_start: u64,
}

impl DecoderPool {
    /// Starts a new output frame: all decoders are free again.
    fn begin_frame(&mut self) {
        self.clock += 1;
        self.frame_start = self.clock;
    }

    /// A decoder for `path` well placed to decode around file time `time`:
    /// one just before it (decoding forward is cheap), else the least
    /// recently used one of that file (seeking is much cheaper than
    /// opening), else a new one. A second decoder of a file is only opened
    /// when the file shows on two layers of one frame.
    fn get(
        &mut self,
        path: &Path,
        time: Time,
        hw: Option<&HwDevice>,
    ) -> nox_media::Result<(&mut VideoDecoder, i64)> {
        self.clock += 1;
        let frame_start = self.frame_start;
        let same_file: Vec<usize> = (0..self.decoders.len())
            .filter(|&i| self.decoders[i].path == path && self.decoders[i].used < frame_start)
            .collect();
        let in_use = self
            .decoders
            .iter()
            .filter(|d| d.path == path && d.used >= frame_start)
            .count();
        let index_of = |d: &PooledDecoder| time.frame(d.decoder.frame_rate());
        // The free decoder of this file that gets there with the least
        // decoding (seeking an open one is much cheaper than opening).
        let cheapest = same_file.iter().copied().min_by_key(|&i| {
            let index = index_of(&self.decoders[i]);
            self.decoders[i].decoder.decode_cost(index)
        });
        let chosen = match cheapest {
            Some(i) => i,
            // More layers of one file than decoders: share a busy one.
            None if in_use >= MAX_PER_FILE => (0..self.decoders.len())
                .filter(|&i| self.decoders[i].path == path)
                .min_by_key(|&i| self.decoders[i].used)
                .expect("busy decoder"),
            None => {
                if self.decoders.len() >= MAX_DECODERS {
                    let lru = (0..self.decoders.len())
                        .min_by_key(|&i| self.decoders[i].used)
                        .expect("decoders");
                    self.decoders.swap_remove(lru);
                }
                tracing::debug!("opening decoder for {}", path.display());
                self.decoders.push(PooledDecoder {
                    path: path.to_owned(),
                    decoder: VideoDecoder::open(path, hw)?,
                    used: 0,
                });
                self.decoders.len() - 1
            }
        };
        let entry = &mut self.decoders[chosen];
        let index = index_of(entry).max(0);
        entry.used = self.clock;
        Ok((&mut entry.decoder, index))
    }
}

/// How frames reach the consumer.
#[derive(Debug, Clone, PartialEq)]
enum PresentPath {
    /// GPU compositing, exported without copies.
    Dmabuf { modifier: u64 },
    /// GPU compositing, copied to system memory.
    Readback { reason: String },
    /// No Vulkan device: top layer only, converted by swscale.
    Cpu,
}

pub(crate) struct Renderer {
    gpu: Option<(HwDevice, Compositor)>,
    decoders: DecoderPool,
    dmabuf_allowed: Arc<AtomicBool>,
    /// Why DMA-BUF is not used, if it isn't.
    readback_reason: String,
    present_path: Option<PresentPath>,
    /// Files that failed to open, logged once.
    failed: std::collections::HashSet<PathBuf>,
}

impl Renderer {
    pub fn new(dmabuf_modifiers: &[u64], dmabuf_allowed: Arc<AtomicBool>) -> Self {
        let mut readback_reason =
            "viewer offered no DMA-BUF formats (NOX_PRESENT=readback or unsupported)".to_owned();
        let gpu = match HwDevice::vulkan().and_then(|(hw, gpu)| Ok((hw, Compositor::new(gpu)?))) {
            Ok((hw, mut compositor)) => {
                if !dmabuf_modifiers.is_empty()
                    && let Err(err) = compositor.enable_dmabuf(dmabuf_modifiers)
                {
                    readback_reason = format!("DMA-BUF export unavailable: {err}");
                }
                Some((hw, compositor))
            }
            Err(err) => {
                tracing::warn!("no Vulkan device: {err}");
                None
            }
        };
        Self {
            gpu,
            decoders: DecoderPool::default(),
            dmabuf_allowed,
            readback_reason,
            present_path: None,
            failed: Default::default(),
        }
    }

    /// The video layers of `frame`: file, time in it, opacity, blend mode.
    fn layers(timeline: &Timeline, frame: i64) -> Vec<Layer> {
        let time = Time::from_frames(frame, timeline.frame_rate());
        timeline
            .video_layers(time)
            .into_iter()
            .filter_map(|layer| {
                let path = layer.clip.media_reference()?.path()?;
                Some((path, layer.file_time(), layer.opacity, layer.blend_mode()))
            })
            .collect()
    }

    /// The most frames any layer of `frame` needs decoded to show it
    /// exactly.
    pub fn decode_cost(&mut self, timeline: &Timeline, frame: i64) -> i64 {
        self.decoders.begin_frame();
        let hw = self.gpu.as_ref().map(|(hw, _)| hw.clone());
        let mut cost = 0;
        for (path, time, _, _) in Self::layers(timeline, frame) {
            if self.failed.contains(&path) {
                continue;
            }
            if let Ok((decoder, index)) = self.decoders.get(&path, time, hw.as_ref()) {
                cost = cost.max(decoder.decode_cost(index));
            }
        }
        cost
    }

    /// Renders sequence frame `frame` of `timeline` at `width` × `height`.
    /// Exact renders give up with `Error::Interrupted` once `stop` is true.
    pub fn render(
        &mut self,
        timeline: &Timeline,
        frame: i64,
        width: u32,
        height: u32,
        quality: Quality,
        stop: &dyn Fn() -> bool,
    ) -> nox_media::Result<RenderedFrame> {
        let layers = Self::layers(timeline, frame);
        let framing = Framing::of(timeline);
        self.decoders.begin_frame();
        let image = if self.gpu.is_some() {
            self.compose(&layers, &framing, width, height, quality, stop)?;
            self.finish()?
        } else {
            self.render_cpu(&layers, &framing, width, height, quality, stop)?
        };
        let path = match (&image, &self.gpu) {
            (FrameImage::Dmabuf(dmabuf), _) => PresentPath::Dmabuf {
                modifier: dmabuf.modifier,
            },
            (FrameImage::Pixels(_), Some(_)) => PresentPath::Readback {
                reason: self.readback_reason.clone(),
            },
            (FrameImage::Pixels(_), None) => PresentPath::Cpu,
        };
        self.log_present_path(path);
        Ok(RenderedFrame {
            frame,
            image,
            exact: quality == Quality::Exact,
        })
    }

    /// The shared Vulkan device, if rendering on the GPU.
    pub fn hw_device(&self) -> Option<HwDevice> {
        self.gpu.as_ref().map(|(hw, _)| hw.clone())
    }

    /// Renders sequence frame `frame` exactly into `target` (e.g. a Vulkan
    /// Video encoder's input image), without copies. GPU only.
    pub fn render_to_yuv(
        &mut self,
        timeline: &Timeline,
        frame: i64,
        target: &mut YuvTarget,
    ) -> nox_media::Result<()> {
        if self.gpu.is_none() {
            return Err(nox_gpu::Error::Unsupported("no Vulkan device".into()).into());
        }
        let layers = Self::layers(timeline, frame);
        let framing = Framing::of(timeline);
        self.decoders.begin_frame();
        self.compose(
            &layers,
            &framing,
            target.width,
            target.height,
            Quality::Exact,
            &|| false,
        )?;
        let (_, compositor) = self.gpu.as_mut().expect("gpu");
        compositor.finish_to_yuv(target)?;
        Ok(())
    }

    /// Decodes and places every layer on the GPU (output pending).
    fn compose(
        &mut self,
        layers: &[Layer],
        framing: &Framing,
        width: u32,
        height: u32,
        quality: Quality,
        stop: &dyn Fn() -> bool,
    ) -> nox_media::Result<()> {
        let (hw, compositor) = self.gpu.as_mut().expect("gpu");
        compositor.begin(width, height)?;
        for (path, time, opacity, mode) in layers {
            if self.failed.contains(path) {
                continue;
            }
            let decoded = self
                .decoders
                .get(path, *time, Some(hw))
                .and_then(|(decoder, index)| {
                    let still = decoder.is_still();
                    decode(decoder, index, quality, stop).map(|frame| (frame, still))
                });
            let (decoded, still) = match decoded {
                Ok(decoded) => decoded,
                Err(nox_media::Error::Interrupted) => return Err(nox_media::Error::Interrupted),
                Err(err) => {
                    tracing::warn!("{}: {err}", path.display());
                    self.failed.insert(path.clone());
                    continue;
                }
            };
            let rect = framing.place(nox_media::display_size(&decoded), (width, height));
            let (opacity, mode) = (*opacity as f32, blend_mode(*mode));
            if still {
                // Stills are converted once and kept on the GPU.
                let key = still_key(path);
                if !compositor.try_cached_layer(key, rect, opacity, mode) {
                    nox_media::with_gpu_input(&decoded, |input| {
                        compositor.add_cached_layer(key, input, rect, opacity, mode)
                    })??;
                }
            } else {
                nox_media::with_gpu_input(&decoded, |input| {
                    compositor.add_layer(input, rect, opacity, mode)
                })??;
            }
        }
        Ok(())
    }

    /// The composed picture for the viewer: DMA-BUF if possible.
    fn finish(&mut self) -> nox_media::Result<FrameImage> {
        let (_, compositor) = self.gpu.as_mut().expect("gpu");
        if compositor.dmabuf_enabled() {
            if !self.dmabuf_allowed.load(Ordering::Relaxed) {
                self.readback_reason = "consumer failed to import DMA-BUF".into();
            } else {
                match compositor.finish_dmabuf() {
                    Ok(dmabuf) => return Ok(FrameImage::Dmabuf(dmabuf)),
                    Err(err) => {
                        self.dmabuf_allowed.store(false, Ordering::Relaxed);
                        self.readback_reason = format!("DMA-BUF export failed: {err}");
                    }
                }
            }
        }
        Ok(FrameImage::Pixels(compositor.finish()?))
    }

    /// Without a GPU: the top layer only, fitted onto black.
    fn render_cpu(
        &mut self,
        layers: &[Layer],
        framing: &Framing,
        width: u32,
        height: u32,
        quality: Quality,
        stop: &dyn Fn() -> bool,
    ) -> nox_media::Result<FrameImage> {
        let mut canvas = Image {
            width,
            height,
            stride: width as usize * 4,
            data: [0, 0, 0, 255].repeat(width as usize * height as usize),
        };
        if let Some((path, time, _, _)) = layers.last() {
            let (decoder, index) = self.decoders.get(path, *time, None)?;
            let decoded = decode(decoder, index, quality, stop)?;
            let rect = framing.place(nox_media::display_size(&decoded), (width, height));
            let picture = nox_media::rgba_cpu(&decoded, rect.width, rect.height)?;
            // Copy the part inside the canvas (fill and center crop overhang).
            let x0 = rect.x.max(0);
            let x1 = (rect.x + rect.width as i32).min(width as i32);
            let y0 = rect.y.max(0);
            let y1 = (rect.y + rect.height as i32).min(height as i32);
            if x0 < x1 {
                let row = (x1 - x0) as usize * 4;
                for y in y0..y1 {
                    let dst = y as usize * canvas.stride + x0 as usize * 4;
                    let src = (y - rect.y) as usize * picture.stride + (x0 - rect.x) as usize * 4;
                    canvas.data[dst..dst + row].copy_from_slice(&picture.data[src..src + row]);
                }
            }
        }
        Ok(FrameImage::Pixels(canvas))
    }

    fn log_present_path(&mut self, path: PresentPath) {
        if self.present_path.as_ref() == Some(&path) {
            return;
        }
        match &path {
            PresentPath::Dmabuf { modifier } => {
                tracing::info!("presenting via DMA-BUF (zero copy, DRM modifier {modifier:#x})")
            }
            PresentPath::Readback { reason } => {
                tracing::info!("presenting via GPU readback ({reason})")
            }
            PresentPath::Cpu => {
                tracing::info!("presenting via CPU conversion (no Vulkan device; top layer only)")
            }
        }
        self.present_path = Some(path);
    }
}

/// How sources are placed: the sequence's scaling mode and size.
struct Framing {
    placement: Placement,
    sequence: (u32, u32),
}

impl Framing {
    fn of(timeline: &Timeline) -> Self {
        let settings = &timeline.settings;
        let placement = match settings.scaling {
            nox_project::ScalingMode::Fit => Placement::Fit,
            nox_project::ScalingMode::Fill => Placement::Fill,
            nox_project::ScalingMode::CenterCrop => Placement::CenterCrop,
            nox_project::ScalingMode::Stretch => Placement::Stretch,
        };
        Self {
            placement,
            sequence: (settings.width, settings.height),
        }
    }

    /// Where a source of `source` display size goes in an output of `out`.
    fn place(&self, source: (u32, u32), out: (u32, u32)) -> Rect {
        Rect::place(self.placement, source, out, self.sequence)
    }
}

/// The still cache key of a file.
fn still_key(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// A video layer: file, time in it, opacity, blend mode.
type Layer = (PathBuf, Time, f64, nox_project::BlendMode);

fn decode(
    decoder: &mut VideoDecoder,
    index: i64,
    quality: Quality,
    stop: &dyn Fn() -> bool,
) -> nox_media::Result<nox_media::ffmpeg::frame::Video> {
    match quality {
        Quality::Exact => decoder.frame_at_until(index, stop),
        Quality::Preview => decoder.frame_near(index, PREVIEW_COST),
    }
}

fn blend_mode(mode: nox_project::BlendMode) -> BlendMode {
    match mode {
        nox_project::BlendMode::Normal => BlendMode::Normal,
        nox_project::BlendMode::Add => BlendMode::Add,
        nox_project::BlendMode::Multiply => BlendMode::Multiply,
        nox_project::BlendMode::Screen => BlendMode::Screen,
    }
}
