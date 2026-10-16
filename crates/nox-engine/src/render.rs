//! Renders sequence frames: decodes every layer, composites them on the
//! GPU and hands out the result as DMA-BUF or in system memory.

use nox_gpu::{
    BlendMode, Compositor, FrameLight, GpuTarget, Image, LayerHook, LayerStyle, Placement, Rect,
    Transform,
};
use nox_media::{Accel, Decoding, HwDecode, VideoDecoder};
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
        accel: Option<&Accel>,
        hw: HwDecode,
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
                    decoder: VideoDecoder::open_with(path, accel, hw)?,
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
    gpu: Option<(Accel, Compositor)>,
    decoders: DecoderPool,
    dmabuf_allowed: Arc<AtomicBool>,
    /// Why DMA-BUF is not used, if it isn't.
    readback_reason: String,
    present_path: Option<PresentPath>,
    /// Files that failed to open, logged once.
    failed: std::collections::HashSet<PathBuf>,
    /// Measure scopes with every frame.
    scopes: bool,
    /// How decoders opened from now on decode.
    hw: HwDecode,
    /// How each file decodes, as last reported (`decodings_changed`).
    decodings: std::collections::HashMap<PathBuf, Decoding>,
    /// Runs clips' effects; made when the first clip with effects shows.
    effects: Option<nox_fx::Effects>,
    /// The clip (`Timeline::clip_key`) whose effects' thumbnails are made.
    thumbnail_clip: Option<u64>,
    /// The still (cache key) whose thumbnails were made last: cached, it
    /// doesn't run its effects again.
    thumbnailed_still: Option<u64>,
    /// Thumbnails made by the last render.
    thumbnails: Option<(u64, Vec<nox_fx::Thumbnail>)>,
}

/// The largest thumbnails of a clip's effects.
pub(crate) const THUMBNAIL_SIZE: (u32, u32) = (160, 90);

impl Renderer {
    pub fn new(dmabuf_formats: &[(u32, u64)], dmabuf_allowed: Arc<AtomicBool>) -> Self {
        let mut readback_reason =
            "viewer offered no DMA-BUF formats (NOX_PRESENT=readback or unsupported)".to_owned();
        let gpu = match Accel::new()
            .and_then(|accel| Ok((accel.clone(), Compositor::new(accel.gpu().clone())?)))
        {
            Ok((accel, mut compositor)) => {
                if !dmabuf_formats.is_empty()
                    && let Err(err) = compositor.enable_dmabuf(dmabuf_formats)
                {
                    readback_reason = format!("DMA-BUF export unavailable: {err}");
                }
                Some((accel, compositor))
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
            scopes: false,
            hw: HwDecode::from_env(),
            decodings: Default::default(),
            effects: None,
            thumbnail_clip: None,
            thumbnailed_still: None,
            thumbnails: None,
        }
    }

    /// Makes thumbnails of clip `clip`'s effects (`Timeline::clip_key`)
    /// with every frame showing it from now on.
    pub fn set_thumbnail_clip(&mut self, clip: Option<u64>) {
        if self.thumbnail_clip != clip {
            self.thumbnail_clip = clip;
            self.thumbnailed_still = None;
        }
    }

    /// The clip and thumbnails made by the last render, if it made any.
    pub fn take_thumbnails(&mut self) -> Option<(u64, Vec<nox_fx::Thumbnail>)> {
        self.thumbnails.take()
    }

    /// Whether the GPU keeps 16-bit floats (runs fp16 models in 16 bits);
    /// `None` without a GPU.
    pub fn storage_16bit(&self) -> Option<bool> {
        self.gpu
            .as_ref()
            .map(|(accel, _)| accel.gpu().storage_16bit())
    }

    /// Tries loading effects' models again (e.g. after they were
    /// downloaded).
    pub fn reload_matting(&mut self) {
        if let Some(effects) = &mut self.effects {
            effects.reload();
        }
    }

    /// Decodes with `hw` from now on: open decoders are closed, so the
    /// next frames open them again.
    pub fn set_hw_decode(&mut self, hw: HwDecode) {
        if self.hw != hw {
            tracing::info!("decoding with {hw:?}");
            self.hw = hw;
            self.decoders = DecoderPool::default();
            self.failed.clear();
            self.decodings.clear();
        }
    }

    /// Files whose decoding is newly known or changed since the last call,
    /// and how they decode now.
    pub fn decodings_changed(&mut self) -> Vec<(PathBuf, Decoding)> {
        let mut changed = Vec::new();
        for pooled in &self.decoders.decoders {
            if let Some(decoding) = pooled.decoder.decoding()
                && self.decodings.get(&pooled.path) != Some(decoding)
            {
                self.decodings.insert(pooled.path.clone(), decoding.clone());
                changed.push((pooled.path.clone(), decoding.clone()));
            }
        }
        changed
    }

    /// Measures scopes with every frame rendered from now on (GPU only).
    pub fn set_scopes(&mut self, enabled: bool) {
        if self.scopes == enabled {
            return;
        }
        self.scopes = enabled;
        if let Some((_, compositor)) = &mut self.gpu
            && let Err(err) = compositor.set_scopes(enabled)
        {
            tracing::warn!("no scopes: {err}");
        }
    }

    /// The video layers of `frame`: file, time in it, opacity, blend mode,
    /// grade.
    fn layers(timeline: &Timeline, frame: i64) -> Vec<Layer> {
        let time = Time::from_frames(frame, timeline.frame_rate());
        timeline
            .video_layers(time)
            .into_iter()
            .filter_map(|layer| {
                let path = timeline.media_path(layer.clip.media_reference()?)?;
                let clip = timeline.clip_key(layer.clip)?;
                let source = layer.source_time.as_seconds();
                Some(Layer {
                    time: layer.file_time(),
                    opacity: layer.opacity,
                    mode: layer.blend_mode(),
                    grade: layer.clip.item.grade_at(layer.source_time.as_seconds()),
                    transform: with_wipe(
                        layer.clip.item.transform_at(layer.source_time.as_seconds()),
                        layer.wipe,
                    ),
                    dip: layer.dip,
                    effects: nox_fx::resolve(&layer.clip.item, source),
                    clip,
                    path,
                })
            })
            .collect()
    }

    /// The most frames any layer of `frame` needs decoded to show it
    /// exactly.
    pub fn decode_cost(&mut self, timeline: &Timeline, frame: i64) -> i64 {
        self.decoders.begin_frame();
        let accel = self.gpu.as_ref().map(|(accel, _)| accel.clone());
        let mut cost = 0;
        for Layer { path, time, .. } in Self::layers(timeline, frame) {
            if self.failed.contains(&path) {
                continue;
            }
            if let Ok((decoder, index)) = self.decoders.get(&path, time, accel.as_ref(), self.hw) {
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
        let (image, scopes) = if self.gpu.is_some() {
            self.compose(&layers, &framing, width, height, quality, stop)?;
            let image = self.finish()?;
            let (_, compositor) = self.gpu.as_mut().expect("gpu");
            (image, compositor.take_scopes().map(Arc::new))
        } else {
            (
                self.render_cpu(&layers, &framing, width, height, quality, stop)?,
                None,
            )
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
            scopes,
        })
    }

    /// The shared Vulkan device, if rendering on the GPU.
    /// How pictures are encoded: SDR (the default) or HDR (PQ, HLG), for
    /// exports. HDR needs the GPU; frames from `render` are then RGBA16.
    pub fn set_output_encoding(
        &mut self,
        encoding: nox_media::OutputEncoding,
    ) -> nox_media::Result<()> {
        match &mut self.gpu {
            Some((_, compositor)) => {
                compositor.set_output_encoding(encoding);
                Ok(())
            }
            None if encoding.is_hdr() => Err(nox_media::Error::Encode(
                "HDR export needs a Vulkan GPU".into(),
            )),
            None => Ok(()),
        }
    }

    /// The GPU devices decoders and encoders use, if there's a GPU.
    pub fn accel(&self) -> Option<Accel> {
        self.gpu.as_ref().map(|(accel, _)| accel.clone())
    }

    /// Renders sequence frame `frame` exactly into `target`, a hardware
    /// encoder's input frame, without copies. GPU only. With `measure`,
    /// returns the frame's light if the target can measure it.
    pub fn render_to_gpu(
        &mut self,
        timeline: &Timeline,
        frame: i64,
        target: GpuTarget<'_>,
        measure: bool,
    ) -> nox_media::Result<Option<FrameLight>> {
        if self.gpu.is_none() {
            return Err(nox_gpu::Error::Unsupported("no Vulkan device".into()).into());
        }
        let (width, height) = match &target {
            GpuTarget::Images(target) => (target.width, target.height),
            GpuTarget::Buffer(target) => (target.layout.width, target.layout.height),
        };
        let layers = Self::layers(timeline, frame);
        let framing = Framing::of(timeline);
        self.decoders.begin_frame();
        self.compose(&layers, &framing, width, height, Quality::Exact, &|| false)?;
        let (_, compositor) = self.gpu.as_mut().expect("gpu");
        Ok(compositor.finish_to_gpu(target, measure)?)
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
        let (accel, compositor) = self.gpu.as_mut().expect("gpu");
        compositor.begin(width, height)?;
        self.thumbnails = None;
        for Layer {
            path,
            time,
            opacity,
            mode,
            grade,
            transform,
            dip,
            effects,
            clip,
        } in layers
        {
            // A dip's color goes below the picture or above it.
            if let Some(dip) = dip.filter(|d| d.before) {
                add_color(compositor, &dip, width, height)?;
            }
            'picture: {
                if self.failed.contains(path) {
                    break 'picture;
                }
                let decoded = self
                    .decoders
                    .get(path, *time, Some(accel), self.hw)
                    .and_then(|(decoder, index)| {
                        let still = decoder.is_still();
                        decode(decoder, index, quality, stop).map(|frame| (frame, still, index))
                    });
                let (decoded, still, index) = match decoded {
                    Ok(decoded) => decoded,
                    Err(nox_media::Error::Interrupted) => {
                        return Err(nox_media::Error::Interrupted);
                    }
                    Err(err) => {
                        tracing::warn!("{}: {err}", path.display());
                        self.failed.insert(path.clone());
                        break 'picture;
                    }
                };
                let rect = framing.place(nox_media::display_size(&decoded), (width, height));
                let style = LayerStyle {
                    opacity: *opacity as f32,
                    mode: blend_mode(*mode),
                    grade: *grade,
                    transform: gpu_transform(transform),
                };
                // The clip's effects run on its picture, in stack order.
                // The clip shown on the Effects page runs them (if only
                // to show its picture) for thumbnails.
                let thumbnailed = self.thumbnail_clip == Some(*clip);
                let runs = !effects.is_empty() || thumbnailed;
                if runs && self.effects.is_none() {
                    match nox_fx::Effects::new(accel.gpu().clone()) {
                        Ok(created) => self.effects = Some(created),
                        Err(err) => tracing::warn!("no effects: {err}"),
                    }
                }
                let context = nox_fx::Context {
                    clip: *clip,
                    frame: (!still).then_some(index),
                };
                let mut prepared = self
                    .effects
                    .as_mut()
                    .filter(|_| runs)
                    .map(|fx| fx.prepare(effects.clone(), context))
                    .map(|p| {
                        if thumbnailed {
                            p.with_thumbnails(THUMBNAIL_SIZE)
                        } else {
                            p
                        }
                    });
                // A still's effects (values, models) make a different still.
                let effects_key = prepared
                    .as_ref()
                    .filter(|p| !p.is_empty())
                    .map(nox_fx::Prepared::key);
                let hook = prepared.as_mut().map(|h| h as &mut dyn LayerHook);
                if still {
                    // Stills are converted once and kept on the GPU (with
                    // their effects, if any), unless their thumbnails are
                    // still to be made.
                    let key = still_key(path, effects_key);
                    let thumbnail = thumbnailed && self.thumbnailed_still != Some(key);
                    if thumbnail {
                        self.thumbnailed_still = Some(key);
                    }
                    if thumbnail || !compositor.try_cached_layer(key, rect, style) {
                        nox_media::with_gpu_input(&decoded, Some(&*accel), |input| {
                            if thumbnail && compositor.has_cached_layer(key, rect, style) {
                                compositor.add_layer_with(input, rect, style, hook)
                            } else {
                                compositor.add_cached_layer_with(key, input, rect, style, hook)
                            }
                        })??;
                    }
                } else {
                    nox_media::with_gpu_input(&decoded, Some(&*accel), |input| {
                        compositor.add_layer_with(input, rect, style, hook)
                    })??;
                }
                if let Some(made) = prepared.as_mut().map(nox_fx::Prepared::take_thumbnails)
                    && !made.is_empty()
                {
                    self.thumbnails = Some((*clip, made));
                }
            }
            if let Some(dip) = dip.filter(|d| !d.before) {
                add_color(compositor, &dip, width, height)?;
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
        if let Some(Layer { path, time, .. }) = layers.last() {
            let (decoder, index) = self.decoders.get(path, *time, None, self.hw)?;
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

/// The still cache key of a file with effects of `effects`
/// (`Prepared::key`).
fn still_key(path: &Path, effects: Option<u64>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    effects.hash(&mut hasher);
    hasher.finish()
}

/// A video layer.
struct Layer {
    path: PathBuf,
    /// Time in the file.
    time: Time,
    opacity: f64,
    mode: nox_project::BlendMode,
    grade: nox_project::Grade,
    transform: nox_project::Transform,
    /// The color of a dip drawn with it.
    dip: Option<nox_project::Dip>,
    /// Its effects at this frame.
    effects: nox_fx::Graph,
    /// Identifies the clip (its media and trim), for effects with state.
    clip: u64,
}

/// Queues a solid color over the whole picture (a dip).
fn add_color(
    compositor: &mut Compositor,
    dip: &nox_project::Dip,
    width: u32,
    height: u32,
) -> nox_media::Result<()> {
    let rgb = dip.color.rgb().map(|c| (c * 255.0).round() as u8);
    let key = still_key(Path::new(&format!("nox:color:{rgb:?}")), None);
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let style = LayerStyle::blend(dip.opacity as f32, BlendMode::Normal);
    if !compositor.try_cached_layer(key, rect, style) {
        let frame = nox_media::solid_frame(rgb);
        nox_media::with_gpu_input(&frame, None, |input| {
            compositor.add_cached_layer(key, input, rect, style)
        })??;
    }
    Ok(())
}

/// A wipe crops what the clip's own crop leaves.
fn with_wipe(mut transform: nox_project::Transform, wipe: [f64; 4]) -> nox_project::Transform {
    for (crop, wipe) in transform.crop.iter_mut().zip(wipe) {
        *crop = crop.max(wipe);
    }
    transform
}

fn gpu_transform(t: &nox_project::Transform) -> Transform {
    Transform {
        offset: [t.x as f32, t.y as f32],
        scale: t.zoom as f32,
        rotation: t.rotation as f32,
        crop: t.crop.map(|c| c as f32),
    }
}

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
