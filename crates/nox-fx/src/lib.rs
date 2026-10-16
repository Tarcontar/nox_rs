//! Effects on clips (and later tracks and the output): the registry of
//! what Nox renders (`registry`), a clip's effects as a graph (`graph`),
//! and running that graph on a compositor layer's picture on the GPU
//! (`Effects`).
//!
//! A clip's effects run on its picture as the compositor has converted it
//! (RGBA32F, linear ACEScg, straight alpha, at the size it is placed at),
//! before its transform, grade and blending: through a `LayerHook`. Sizes
//! are in percent of the picture's height, so they look the same in the
//! viewer (at its size) and in exports. Mattes are planes of 32-bit floats
//! at their own resolution (`nox_ai::Matte`), resampled where they're
//! used.

mod ai;
pub mod graph;
pub mod registry;

use nox_ai::Matte;
use nox_gpu::compute::{Binding, BindingKind, Buffer, Image, Kernel, MemoryUse, Recording};
use nox_gpu::{GpuDevice, LayerHook, LayerPicture, Result, vk};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub use graph::{
    Graph, Resolved, Source, Wire, is_linear, materialize, output_wire, resolve, simplify, wires,
};
pub use registry::{
    Category, EffectDef, ParamDef, ParamKind, ParamValue, PortDef, PortKind, Unit, effects, find,
    values,
};

/// The clip an effect runs for.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// Identifies the clip (e.g. its media and trim): models with state
    /// (video matting) keep it per clip.
    pub clip: u64,
    /// The frame in the clip's media; `None` for stills.
    pub frame: Option<i64>,
}

/// Runs effect stacks on Nox's device: the kernels, scratch pictures and
/// loaded models, kept between frames.
pub struct Effects {
    gpu: Arc<GpuDevice>,
    blur: Kernel,
    sharpen: Kernel,
    kuwahara: Kernel,
    chroma_key: Kernel,
    apply_matte: Kernel,
    mix: Kernel,
    invert_matte: Kernel,
    blur_matte: Kernel,
    alpha_to_matte: Kernel,
    thumb_picture: Kernel,
    thumb_matte: Kernel,
    /// Two intermediate pictures for effects' own passes, at least as
    /// large as the largest picture so far.
    scratch: Vec<Image>,
    /// Pictures between nodes of a graph (a picture read by more than one
    /// node is kept in one of these).
    pictures: Vec<Image>,
    ai: ai::Ai,
}

macro_rules! spv {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".comp.spv"))
    };
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurPush {
    size: [u32; 2],
    vertical: u32,
    taps: i32,
    step: i32,
    inv_two_sigma2: f32,
    premultiply: u32,
    unpremultiply: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SharpenPush {
    size: [u32; 2],
    amount: f32,
    threshold: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct KuwaharaPush {
    size: [u32; 2],
    radius: i32,
    step: i32,
    sharpness: f32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ChromaKeyPush {
    size: [u32; 2],
    pad: [u32; 2],
    key: [f32; 4],
    tolerance: f32,
    softness: f32,
    spill: f32,
    dominant: u32,
    show_matte: u32,
    pad2: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ApplyMattePush {
    size: [u32; 2],
    matte_size: [u32; 2],
    mode: u32,
    invert: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MixPush {
    size: [u32; 2],
    matte_size: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CountPush {
    n: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurMattePush {
    size: [u32; 2],
    vertical: u32,
    taps: i32,
    step: i32,
    inv_two_sigma2: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SizePush {
    size: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ThumbPicturePush {
    rows: [[f32; 4]; 3],
    size: [u32; 2],
    thumb: [u32; 2],
    offset: u32,
    pad: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ThumbMattePush {
    size: [u32; 2],
    thumb: [u32; 2],
    offset: u32,
}

/// A small picture of what a node outputs (its picture, else its matte),
/// for the node editor: sRGB, RGBA8, row by row. `node` is the effect's
/// key, or `nox_project::SOURCE` for the clip's picture and `OUTPUT` for
/// what's composited.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumbnail {
    pub node: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The `node` of the output's thumbnail.
pub const OUTPUT: &str = "output";

/// The Gaussian taps for a blur of `radius` pixels: out to 3σ, σ/3 apart
/// at most (σ, step, taps each side).
fn gaussian(radius: f64) -> (f64, f64, i32) {
    let sigma = (radius / 2.0).max(0.3);
    let step = (sigma / 3.0).floor().max(1.0);
    let taps = ((3.0 * sigma) / step).ceil() as i32;
    (sigma, step, taps)
}

impl Effects {
    pub fn new(gpu: Arc<GpuDevice>) -> Result<Self> {
        let two = [BindingKind::Image, BindingKind::Image];
        let image_buffer = [BindingKind::Image, BindingKind::Buffer];
        let buffers = [BindingKind::Buffer, BindingKind::Buffer];
        Ok(Self {
            apply_matte: Kernel::new(
                &gpu,
                spv!("apply_matte"),
                &image_buffer,
                size_of::<ApplyMattePush>(),
            )?,
            mix: Kernel::new(
                &gpu,
                spv!("mix"),
                &[BindingKind::Image, BindingKind::Image, BindingKind::Buffer],
                size_of::<MixPush>(),
            )?,
            invert_matte: Kernel::new(
                &gpu,
                spv!("invert_matte"),
                &buffers,
                size_of::<CountPush>(),
            )?,
            blur_matte: Kernel::new(
                &gpu,
                spv!("blur_matte"),
                &buffers,
                size_of::<BlurMattePush>(),
            )?,
            alpha_to_matte: Kernel::new(
                &gpu,
                spv!("alpha_to_matte"),
                &image_buffer,
                size_of::<SizePush>(),
            )?,
            thumb_picture: Kernel::new(
                &gpu,
                spv!("thumb_picture"),
                &image_buffer,
                size_of::<ThumbPicturePush>(),
            )?,
            thumb_matte: Kernel::new(
                &gpu,
                spv!("thumb_matte"),
                &buffers,
                size_of::<ThumbMattePush>(),
            )?,
            blur: Kernel::new(&gpu, spv!("blur"), &two, size_of::<BlurPush>())?,
            sharpen: Kernel::new(&gpu, spv!("sharpen"), &two, size_of::<SharpenPush>())?,
            kuwahara: Kernel::new(&gpu, spv!("kuwahara"), &two, size_of::<KuwaharaPush>())?,
            chroma_key: Kernel::new(
                &gpu,
                spv!("chroma_key"),
                &[BindingKind::Image],
                size_of::<ChromaKeyPush>(),
            )?,
            scratch: Vec::new(),
            pictures: Vec::new(),
            ai: ai::Ai::new(gpu.clone()),
            gpu,
        })
    }

    /// Tries loading models again (e.g. after they were downloaded).
    pub fn reload(&mut self) {
        self.ai.reload();
    }

    /// How often a model ran (to check the matte cache).
    pub fn model_runs(&self) -> usize {
        self.ai.runs
    }

    /// `graph` (an item's `resolve`d effects) ready to run for `context`'s
    /// clip: models loaded, and a key for what decides its pixels.
    pub fn prepare(&mut self, graph: Graph, context: Context) -> Prepared<'_> {
        let models = graph
            .nodes
            .iter()
            .map(|node| match node.def.task {
                Some(_) => self.ai.removers(node.choice("model"), &context),
                None => Vec::new(),
            })
            .collect();
        // What made each node's output, for caching mattes.
        let mut upstream: Vec<u64> = Vec::with_capacity(graph.nodes.len());
        for node in &graph.nodes {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            node.hash_into(&mut hasher);
            for source in node.inputs.iter().flatten() {
                if let Source::Node { index, .. } = source {
                    upstream[*index].hash(&mut hasher);
                }
            }
            upstream.push(hasher.finish());
        }
        Prepared {
            effects: self,
            graph,
            models,
            upstream,
            context,
            thumbnails: None,
            taken: Vec::new(),
        }
    }

    /// A free pooled picture of at least `width` × `height`, marked in
    /// `used` (one flag per pooled picture, grown with the pool).
    fn take_picture(&mut self, used: &mut Vec<bool>, width: u32, height: u32) -> Result<usize> {
        let fits = |i: &Image| i.width() >= width && i.height() >= height;
        let free = |i: usize| !used.get(i).copied().unwrap_or(false);
        let i = if let Some(i) =
            (0..self.pictures.len()).find(|&i| free(i) && fits(&self.pictures[i]))
        {
            i
        } else if let Some(i) = (0..self.pictures.len()).find(|&i| free(i)) {
            self.pictures[i] = Image::new(&self.gpu, width, height)?;
            i
        } else {
            self.pictures.push(Image::new(&self.gpu, width, height)?);
            self.pictures.len() - 1
        };
        used.resize(self.pictures.len(), false);
        used[i] = true;
        Ok(i)
    }

    /// A new matte buffer of `width` × `height`.
    fn new_matte(&self, width: u32, height: u32) -> Result<Matte> {
        let bytes = u64::from(width) * u64::from(height) * 4;
        Ok(Matte {
            buffer: Buffer::new(&self.gpu, bytes, MemoryUse::Device)?,
            width,
            height,
        })
    }

    /// Records putting `matte` into `picture`'s alpha.
    fn record_apply_matte(
        &self,
        recording: &mut Recording,
        picture: &LayerPicture,
        matte: &Matte,
        replace: bool,
        invert: bool,
    ) -> Result<()> {
        let push = ApplyMattePush {
            size: [picture.width, picture.height],
            matte_size: [matte.width, matte.height],
            mode: u32::from(replace),
            invert: u32::from(invert),
        };
        recording.dispatch(
            &self.apply_matte,
            &[Binding::Image(picture.view), (&matte.buffer).into()],
            bytemuck::bytes_of(&push),
            groups_2d(picture.width, picture.height),
        )
    }

    /// Records a matte-only effect on `matte` into a new matte.
    fn record_matte_effect(
        &self,
        recording: &mut Recording,
        effect: &Resolved,
        matte: &Matte,
    ) -> Result<Matte> {
        let out = self.new_matte(matte.width, matte.height)?;
        match effect.def.id {
            registry::INVERT_MATTE => {
                let n = matte.width * matte.height;
                recording.dispatch(
                    &self.invert_matte,
                    &[(&matte.buffer).into(), (&out.buffer).into()],
                    bytemuck::bytes_of(&CountPush { n }),
                    [n.div_ceil(256), 1, 1],
                )?;
            }
            registry::FEATHER_MATTE => {
                let radius = effect.number("radius") / 100.0 * f64::from(matte.height);
                let tmp = self.new_matte(matte.width, matte.height)?;
                let (sigma, step, taps) = gaussian(radius.max(0.0));
                let size = [matte.width, matte.height];
                for (vertical, from, to) in [
                    (0, &matte.buffer, &tmp.buffer),
                    (1, &tmp.buffer, &out.buffer),
                ] {
                    let push = BlurMattePush {
                        size,
                        vertical,
                        taps: if radius < 0.5 { 0 } else { taps },
                        step: step as i32,
                        inv_two_sigma2: (1.0 / (2.0 * sigma * sigma)) as f32,
                    };
                    recording.dispatch(
                        &self.blur_matte,
                        &[from.into(), to.into()],
                        bytemuck::bytes_of(&push),
                        groups_2d(matte.width, matte.height),
                    )?;
                }
            }
            other => unreachable!("{other} isn't a matte effect"),
        }
        Ok(out)
    }

    /// At least `count` scratch pictures of `width` × `height` or more.
    fn ensure_scratch(&mut self, count: usize, width: u32, height: u32) -> Result<()> {
        let fits = |i: &Image| i.width() >= width && i.height() >= height;
        if self.scratch.len() >= count && self.scratch.iter().all(fits) {
            return Ok(());
        }
        let (w, h) = self.scratch.iter().fold((width, height), |(w, h), i| {
            (w.max(i.width()), h.max(i.height()))
        });
        self.scratch.clear();
        for _ in 0..count.max(2) {
            self.scratch.push(Image::new(&self.gpu, w, h)?);
        }
        Ok(())
    }

    /// Records `effect` on `picture`; false if it does nothing at these
    /// values (nothing recorded).
    fn record(
        &self,
        recording: &mut Recording,
        effect: &Resolved,
        picture: &LayerPicture,
    ) -> Result<bool> {
        let size = [picture.width, picture.height];
        let groups = groups_2d(picture.width, picture.height);
        let pixels = |percent: f64| percent / 100.0 * f64::from(picture.height);
        let view = picture.view;
        let (a, b) = (&self.scratch[0], &self.scratch[1]);
        match effect.def.id {
            registry::GAUSSIAN_BLUR => {
                let radius = pixels(effect.number("radius"));
                if radius < 0.5 {
                    return Ok(false);
                }
                self.record_blur(recording, size, radius, view, a.view(), view)?;
            }
            registry::SHARPEN => {
                let radius = pixels(effect.number("radius")).max(0.5);
                let amount = effect.number("amount") / 100.0;
                if amount <= 0.0 {
                    return Ok(false);
                }
                self.record_blur(recording, size, radius, view, a.view(), b.view())?;
                let push = SharpenPush {
                    size,
                    amount: amount as f32,
                    threshold: (effect.number("threshold") / 100.0) as f32,
                };
                recording.dispatch(
                    &self.sharpen,
                    &[Binding::Image(view), Binding::Image(b.view())],
                    bytemuck::bytes_of(&push),
                    groups,
                )?;
            }
            registry::PAINTERLY => {
                let radius = pixels(effect.number("radius")).round() as i32;
                if radius < 1 {
                    return Ok(false);
                }
                // About 13 × 13 taps at most: wider ones skip pixels.
                let push = KuwaharaPush {
                    size,
                    radius,
                    step: (radius / 6).max(1),
                    sharpness: effect.number("sharpness") as f32,
                    pad: 0,
                };
                recording.dispatch(
                    &self.kuwahara,
                    &[Binding::Image(view), Binding::Image(a.view())],
                    bytemuck::bytes_of(&push),
                    groups,
                )?;
                recording.copy_image(a.handle(), picture.image, picture.width, picture.height);
            }
            registry::CHROMA_KEY => {
                let key = working_rgb(effect.color("color"));
                let sum = key.iter().sum::<f32>().max(1e-6);
                let dominant = (0..3)
                    .max_by(|&i, &j| key[i].total_cmp(&key[j]))
                    .unwrap_or(1);
                // Chromaticity distances: a saturated key is ~0.5 from gray.
                let push = ChromaKeyPush {
                    size,
                    pad: [0; 2],
                    key: [key[0] / sum, key[1] / sum, key[2] / sum, 0.0],
                    tolerance: (effect.number("tolerance") / 100.0 * 0.4) as f32,
                    softness: (effect.number("softness") / 100.0 * 0.4) as f32,
                    spill: (effect.number("spill") / 100.0) as f32,
                    dominant: dominant as u32,
                    show_matte: u32::from(effect.bool("show_matte")),
                    pad2: [0; 3],
                };
                recording.dispatch(
                    &self.chroma_key,
                    &[Binding::Image(view)],
                    bytemuck::bytes_of(&push),
                    groups,
                )?;
            }
            other => unreachable!("{other} isn't rendered on the GPU here"),
        }
        Ok(true)
    }

    /// A Gaussian blur of `src` into `dst` through `tmp` (`src` and `dst`
    /// may be the same).
    fn record_blur(
        &self,
        recording: &mut Recording,
        size: [u32; 2],
        radius: f64,
        src: vk::ImageView,
        tmp: vk::ImageView,
        dst: vk::ImageView,
    ) -> Result<()> {
        let (sigma, step, taps) = gaussian(radius);
        let groups = groups_2d(size[0], size[1]);
        for (vertical, from, to) in [(0, src, tmp), (1, tmp, dst)] {
            let push = BlurPush {
                size,
                vertical,
                taps,
                step: step as i32,
                inv_two_sigma2: (1.0 / (2.0 * sigma * sigma)) as f32,
                premultiply: u32::from(vertical == 0),
                unpremultiply: vertical,
            };
            recording.dispatch(
                &self.blur,
                &[Binding::Image(from), Binding::Image(to)],
                bytemuck::bytes_of(&push),
                groups,
            )?;
        }
        Ok(())
    }
}

/// Workgroups of 16 × 16 over `width` × `height`.
fn groups_2d(width: u32, height: u32) -> [u32; 3] {
    [width.div_ceil(16), height.div_ceil(16), 1]
}

/// An sRGB color as picked, in the linear working space.
fn working_rgb(srgb: [f64; 3]) -> [f32; 3] {
    let linear = srgb.map(|c| nox_color::Transfer::Srgb.eotf(c as f32));
    let m = nox_color::Primaries::Bt709.conversion_to(nox_color::WORKING_PRIMARIES);
    [0, 1, 2].map(|r| (0..3).map(|c| m[r][c] * linear[c]).sum())
}

/// A graph ready to run on one clip's picture (`Effects::prepare`).
pub struct Prepared<'a> {
    effects: &'a mut Effects,
    graph: Graph,
    /// For AI nodes, the models that loaded, most preferred first.
    models: Vec<Vec<String>>,
    /// What made each node's output (a hash).
    upstream: Vec<u64>,
    context: Context,
    /// Thumbnails asked for: their largest size.
    thumbnails: Option<(u32, u32)>,
    /// Thumbnails made by the last run.
    taken: Vec<Thumbnail>,
}

/// Where thumbnails are being recorded to.
struct Thumbs {
    buffer: Buffer,
    size: (u32, u32),
    /// The node of each slot written.
    slots: Vec<String>,
}

/// A picture while a graph runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picture {
    /// The layer's own (where the output must end up).
    Layer,
    /// One of `Effects::pictures`.
    Pool(usize),
}

impl Prepared<'_> {
    pub fn is_empty(&self) -> bool {
        self.graph.is_empty()
    }

    /// Also makes a thumbnail of the source, of every node's output and of
    /// the output, at most `max` (width, height), keeping the picture's
    /// aspect (`take_thumbnails` after running).
    pub fn with_thumbnails(mut self, max: (u32, u32)) -> Self {
        self.thumbnails = Some(max);
        self
    }

    /// The thumbnails the last run made.
    pub fn take_thumbnails(&mut self) -> Vec<Thumbnail> {
        std::mem::take(&mut self.taken)
    }

    /// Records a thumbnail of `picture` (`Ok`) or `matte` (`Err`) for
    /// `node` into the next slot of `thumbs`.
    fn record_thumbnail(
        &self,
        recording: &mut Recording,
        thumbs: &mut Thumbs,
        node: &str,
        what: std::result::Result<&LayerPicture, &Matte>,
    ) -> Result<()> {
        let (tw, th) = thumbs.size;
        let offset = thumbs.slots.len() as u32 * tw * th;
        match what {
            Ok(picture) => {
                let m = nox_color::WORKING_PRIMARIES.conversion_to(nox_color::Primaries::Bt709);
                let push = ThumbPicturePush {
                    rows: [0, 1, 2].map(|r| [m[r][0], m[r][1], m[r][2], 0.0]),
                    size: [picture.width, picture.height],
                    thumb: [tw, th],
                    offset,
                    pad: [0; 3],
                };
                recording.dispatch(
                    &self.effects.thumb_picture,
                    &[Binding::Image(picture.view), (&thumbs.buffer).into()],
                    bytemuck::bytes_of(&push),
                    groups_2d(tw, th),
                )?;
            }
            Err(matte) => {
                let push = ThumbMattePush {
                    size: [matte.width, matte.height],
                    thumb: [tw, th],
                    offset,
                };
                recording.dispatch(
                    &self.effects.thumb_matte,
                    &[(&matte.buffer).into(), (&thumbs.buffer).into()],
                    bytemuck::bytes_of(&push),
                    groups_2d(tw, th),
                )?;
            }
        }
        thumbs.slots.push(node.to_owned());
        Ok(())
    }

    /// What decides the output's pixels: the graph and which models run
    /// (a still cached with effects is only valid for these).
    pub fn key(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.graph.hash_into(&mut hasher);
        for models in &self.models {
            models.first().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// `picture` as a `LayerPicture` of `layer`'s size.
    fn view(&self, picture: Picture, layer: &LayerPicture) -> LayerPicture {
        match picture {
            Picture::Layer => LayerPicture {
                image: layer.image,
                view: layer.view,
                width: layer.width,
                height: layer.height,
            },
            Picture::Pool(i) => {
                let image = &self.effects.pictures[i];
                LayerPicture {
                    image: image.handle(),
                    view: image.view(),
                    width: layer.width,
                    height: layer.height,
                }
            }
        }
    }

    fn handle(&self, picture: Picture, layer: &LayerPicture) -> vk::Image {
        self.view(picture, layer).image
    }
}

/// Room for one node's work in a recording: dispatches (a masked sharpen
/// takes five, and its thumbnail one more) and bindings.
const DISPATCHES_PER_NODE: u32 = 7;

impl LayerHook for Prepared<'_> {
    fn process(&mut self, layer: &LayerPicture) -> Result<()> {
        let (width, height) = (layer.width, layer.height);
        let nodes = std::mem::take(&mut self.graph.nodes);
        let output = self.graph.output();
        // How often each source is still to be read (inputs and output).
        let mut reads: HashMap<Source, usize> = HashMap::new();
        for source in nodes.iter().flat_map(|n| n.inputs.iter().flatten()) {
            *reads.entry(*source).or_default() += 1;
        }
        *reads.entry(output).or_default() += 1;
        let result = (|| {
            self.effects.ensure_scratch(2, width, height)?;
            let mut used = vec![false; self.effects.pictures.len()];
            let mut pictures: HashMap<Source, Picture> = HashMap::new();
            pictures.insert(Source::Clip, Picture::Layer);
            let mut mattes: HashMap<Source, Arc<Matte>> = HashMap::new();
            let new_recording = |gpu: &Arc<GpuDevice>| {
                Recording::new(gpu, DISPATCHES_PER_NODE * (nodes.len() as u32 + 1), 3)
            };
            let mut recording = new_recording(&self.effects.gpu)?;
            let mut recorded = false;
            let mut thumbs = match self.thumbnails {
                Some((max_w, max_h)) => {
                    // The picture's aspect, inside the largest size.
                    let scale = (f64::from(max_w) / f64::from(width))
                        .min(f64::from(max_h) / f64::from(height));
                    let size = (
                        ((f64::from(width) * scale).round() as u32).max(1),
                        ((f64::from(height) * scale).round() as u32).max(1),
                    );
                    let bytes = u64::from(size.0 * size.1 * 4) * (nodes.len() as u64 + 2);
                    let mut thumbs = Thumbs {
                        buffer: Buffer::new(&self.effects.gpu, bytes, MemoryUse::Download)?,
                        size,
                        slots: Vec::new(),
                    };
                    self.record_thumbnail(
                        &mut recording,
                        &mut thumbs,
                        nox_project::SOURCE,
                        Ok(layer),
                    )?;
                    recorded = true;
                    Some(thumbs)
                }
                None => None,
            };
            for (index, node) in nodes.iter().enumerate() {
                let picture_out = Source::Node {
                    index,
                    port: "picture",
                };
                let matte_out = Source::Node {
                    index,
                    port: "matte",
                };
                let matte_in = |name: &str| node.input(name).and_then(|s| mattes.get(&s).cloned());
                if !node.def.outputs_picture() {
                    // Mattes from mattes, or from a picture (the AI).
                    let matte = if node.def.task.is_some() {
                        let source = node.input("picture").unwrap_or(Source::Clip);
                        let picture = pictures[&source];
                        if recorded {
                            recording.run()?;
                            recording = new_recording(&self.effects.gpu)?;
                            recorded = false;
                        }
                        let view = self.view(picture, layer);
                        let upstream = match source {
                            Source::Node { index, .. } => self.upstream[index],
                            Source::Clip => 0,
                        };
                        self.effects
                            .ai
                            .matte(&self.models[index], &self.context, &view, upstream)
                    } else {
                        match matte_in("matte") {
                            Some(matte) => {
                                recorded = true;
                                Some(Arc::new(self.effects.record_matte_effect(
                                    &mut recording,
                                    node,
                                    &matte,
                                )?))
                            }
                            None => None,
                        }
                    };
                    if let Some(matte) = matte {
                        if let Some(thumbs) = &mut thumbs {
                            self.record_thumbnail(&mut recording, thumbs, &node.key, Err(&matte))?;
                            recorded = true;
                        }
                        mattes.insert(matte_out, matte);
                    }
                } else {
                    let source = node.input("picture").unwrap_or(Source::Clip);
                    let input = pictures[&source];
                    let remaining = reads.get(&source).copied().unwrap_or(1).saturating_sub(1);
                    let mask = matte_in("mask");
                    // In place when nothing later reads the input (and,
                    // masked, its unchanged copy is kept aside).
                    let (target, original) = if remaining == 0 {
                        let original = match &mask {
                            Some(_) => {
                                let i = self.effects.take_picture(&mut used, width, height)?;
                                let copy = Picture::Pool(i);
                                recording.copy_image(
                                    self.handle(input, layer),
                                    self.handle(copy, layer),
                                    width,
                                    height,
                                );
                                Some(copy)
                            }
                            None => None,
                        };
                        (input, original)
                    } else {
                        let i = self.effects.take_picture(&mut used, width, height)?;
                        let target = Picture::Pool(i);
                        recording.copy_image(
                            self.handle(input, layer),
                            self.handle(target, layer),
                            width,
                            height,
                        );
                        (target, mask.is_some().then_some(input))
                    };
                    recorded = true;
                    let view = self.view(target, layer);
                    match node.def.id {
                        registry::APPLY_MATTE => {
                            if let Some(matte) = matte_in("matte") {
                                self.effects.record_apply_matte(
                                    &mut recording,
                                    &view,
                                    &matte,
                                    node.choice("mode") == "replace",
                                    node.bool("invert"),
                                )?;
                            }
                        }
                        _ => {
                            self.effects.record(&mut recording, node, &view)?;
                        }
                    }
                    if node.def.id == registry::CHROMA_KEY {
                        // What the key kept, as a matte.
                        let matte = self.effects.new_matte(width, height)?;
                        recording.dispatch(
                            &self.effects.alpha_to_matte,
                            &[Binding::Image(view.view), (&matte.buffer).into()],
                            bytemuck::bytes_of(&SizePush {
                                size: [width, height],
                            }),
                            groups_2d(width, height),
                        )?;
                        mattes.insert(matte_out, Arc::new(matte));
                    }
                    if let (Some(mask), Some(original)) = (&mask, original) {
                        let push = MixPush {
                            size: [width, height],
                            matte_size: [mask.width, mask.height],
                        };
                        recording.dispatch(
                            &self.effects.mix,
                            &[
                                Binding::Image(view.view),
                                Binding::Image(self.view(original, layer).view),
                                (&mask.buffer).into(),
                            ],
                            bytemuck::bytes_of(&push),
                            groups_2d(width, height),
                        )?;
                        // A copy made for the mix is done with.
                        if let Picture::Pool(i) = original
                            && !pictures.values().any(|p| *p == original)
                        {
                            used[i] = false;
                        }
                    }
                    if let Some(thumbs) = &mut thumbs {
                        let view = self.view(target, layer);
                        self.record_thumbnail(&mut recording, thumbs, &node.key, Ok(&view))?;
                    }
                    pictures.insert(picture_out, target);
                }
                // Inputs read for the last time free their pictures.
                for source in node.inputs.iter().flatten() {
                    let count = reads.entry(*source).or_default();
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        mattes.remove(source);
                        if let Some(Picture::Pool(i)) = pictures.get(source).copied()
                            && !pictures
                                .iter()
                                .any(|(s, p)| s != source && *p == Picture::Pool(i))
                        {
                            used[i] = false;
                        }
                    }
                }
            }
            // The output into the layer's picture.
            let out = pictures.get(&output).copied().unwrap_or(Picture::Layer);
            if out != Picture::Layer {
                recording.copy_image(self.handle(out, layer), layer.image, width, height);
                recorded = true;
            }
            if let Some(thumbs) = &mut thumbs {
                self.record_thumbnail(&mut recording, thumbs, OUTPUT, Ok(layer))?;
                recorded = true;
            }
            if recorded {
                recording.run()?;
            }
            if let Some(thumbs) = thumbs {
                let bytes: Vec<u8> = thumbs.buffer.read()?;
                let (w, h) = thumbs.size;
                let each = (w * h * 4) as usize;
                self.taken = thumbs
                    .slots
                    .into_iter()
                    .enumerate()
                    .map(|(i, node)| Thumbnail {
                        node,
                        width: w,
                        height: h,
                        rgba: bytes[i * each..(i + 1) * each].to_vec(),
                    })
                    .collect();
            }
            Ok(())
        })();
        self.graph.nodes = nodes;
        result
    }
}
