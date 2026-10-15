//! Composites several frames into one picture on the GPU.
//!
//! Each layer is converted into the linear working space (ACEScg, see
//! `nox_color::working`) at its placed size by the `Converter`
//! (`ycbcr_to_working`, RGBA32F), then all layers are blended bottom to top
//! in linear light into a 32-bit float accumulator (`blend.comp`) over a
//! black background. The output transform (`output.comp`) then writes BT.709
//! R'G'B': 8 bit to read back, 8 bit or half float to export as DMA-BUF, or
//! as Y'CbCr into an encoder's image (`output_yuv.comp`) or into a buffer
//! shared with another API (`output_linear.comp`, see `interop`).

use ash::vk;
use std::sync::Arc;

use crate::converter::{
    GpuBuffer, GpuImage, Readback, WORKING_FORMAT, color_range, image_barrier, queue_transfer,
    record_readback,
};
use crate::export::{DmabufFormat, ExportPool};
use crate::interop::{BufferTarget, FrameLight, GpuTarget, LinearFormat};
use crate::scopes::{ScopeImages, Scopes};
use crate::vk_util::{
    OneShot, Pipeline, alloc_set, buffer_write, create_pipeline, image_write, push, semaphore_infos,
};
use crate::{
    Converter, DmabufFrame, Error, GpuDevice, Image, ImageSync, Input, PlaneFormat, Result, VkPlane,
};
use nox_color::{Grade, Matrix3x4, OutputEncoding};

const BLEND_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blend.comp.spv"));
const OUTPUT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/output.comp.spv"));
const OUTPUT_F16_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/output_f16.comp.spv"));
const OUTPUT_YUV_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/output_yuv.comp.spv"));
const OUTPUT_DEEP_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/output_deep.comp.spv"));
const OUTPUT_YUV_DEEP_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/output_yuv_deep.comp.spv"));
const OUTPUT_LINEAR_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/output_linear.comp.spv"));
const WORKGROUP_SIZE: u32 = 16;
/// Layers per picture.
pub const MAX_LAYERS: usize = 16;

/// How a layer combines with the picture below it; matches `blend.comp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendMode {
    #[default]
    Normal = 0,
    Add = 1,
    Multiply = 2,
    Screen = 3,
}

/// Where a layer goes in the output, in pixels. May extend past the edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// How a source is placed in the output; see `Rect::place`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    #[default]
    Fit,
    Fill,
    /// 1:1 with the sequence's pixels.
    CenterCrop,
    Stretch,
}

impl Rect {
    /// Where a source of `source` display size goes in an output of
    /// `out` size that shows a sequence of `sequence` size (the output may
    /// be a scaled-down preview).
    pub fn place(
        placement: Placement,
        source: (u32, u32),
        out: (u32, u32),
        sequence: (u32, u32),
    ) -> Self {
        let (sw, sh) = (f64::from(source.0), f64::from(source.1));
        let (ow, oh) = (f64::from(out.0), f64::from(out.1));
        let aspect = sw / sh;
        let centered = |w: f64, h: f64| {
            let (w, h) = (w.round().max(1.0), h.round().max(1.0));
            Rect {
                x: ((ow - w) / 2.0).round() as i32,
                y: ((oh - h) / 2.0).round() as i32,
                width: w as u32,
                height: h as u32,
            }
        };
        match placement {
            Placement::Fit => Self::fit(aspect, out.0, out.1),
            Placement::Fill => {
                if aspect >= ow / oh {
                    centered(oh * aspect, oh)
                } else {
                    centered(ow, ow / aspect)
                }
            }
            Placement::CenterCrop => {
                let scale = ow / f64::from(sequence.0.max(1));
                centered(sw * scale, sh * scale)
            }
            Placement::Stretch => Rect {
                x: 0,
                y: 0,
                width: out.0,
                height: out.1,
            },
        }
    }

    /// The largest rectangle with the aspect ratio `width / height` that
    /// fits `out_width` × `out_height`, centered (letterboxed or
    /// pillarboxed). `aspect` is the source's display aspect ratio.
    pub fn fit(aspect: f64, out_width: u32, out_height: u32) -> Self {
        let (ow, oh) = (f64::from(out_width), f64::from(out_height));
        let (w, h) = if aspect >= ow / oh {
            (ow, ow / aspect)
        } else {
            (oh * aspect, oh)
        };
        let (w, h) = (w.round().max(1.0), h.round().max(1.0));
        Self {
            x: ((ow - w) / 2.0).round() as i32,
            y: ((oh - h) / 2.0).round() as i32,
            width: w as u32,
            height: h as u32,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BlendParams {
    offset: [i32; 2],
    size: [i32; 2],
    out_size: [i32; 2],
    opacity: f32,
    mode: i32,
    /// The grade's coefficients (`nox_color::GradeCoefficients`); `w` of
    /// `slope` is 1 if the layer is graded at all, `w` of `add` 1 if it is
    /// transformed.
    slope: [f32; 4],
    add: [f32; 4],
    power: [f32; 4],
    /// Transformed layers: the inverse of the scale and rotation (rows),
    /// the transformed center and the placed half size (pixels), and the
    /// uncropped part (u0, v0, u1, v1). 128 bytes in all, the least every
    /// device offers for push constants.
    inverse: [f32; 4],
    geometry: [f32; 4],
    crop: [f32; 4],
}

/// How a layer is drawn: graded, transformed, then blended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerStyle {
    pub opacity: f32,
    pub mode: BlendMode,
    /// Applied before blending, in ACEScct (`nox_color::grade`).
    pub grade: Grade,
    /// Moves, scales, rotates and crops the placed layer.
    pub transform: Transform,
}

/// A layer's transform, applied to its placed rectangle around the
/// rectangle's center: crop, then scale, rotate and move. Independent of
/// the output size, so a preview matches the export.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// Movement as a fraction of the output's width and height (right and
    /// down are positive).
    pub offset: [f32; 2],
    pub scale: f32,
    /// Degrees, clockwise.
    pub rotation: f32,
    /// What is cut off each edge (left, top, right, bottom), as a fraction
    /// of the layer; the rest stays where it was.
    pub crop: [f32; 4],
}

impl Transform {
    pub const IDENTITY: Self = Self {
        offset: [0.0; 2],
        scale: 1.0,
        rotation: 0.0,
        crop: [0.0; 4],
    };

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    /// Output point of the layer point `(u, v)` (0–1 across `rect`) in an
    /// output of `out` size.
    fn apply(&self, rect: Rect, out: (u32, u32), (u, v): (f32, f32)) -> (f32, f32) {
        let (w, h) = (rect.width as f32, rect.height as f32);
        let center = (
            rect.x as f32 + w / 2.0 + self.offset[0] * out.0 as f32,
            rect.y as f32 + h / 2.0 + self.offset[1] * out.1 as f32,
        );
        let (qx, qy) = ((u - 0.5) * w * self.scale, (v - 0.5) * h * self.scale);
        let (sin, cos) = self.rotation.to_radians().sin_cos();
        (
            center.0 + cos * qx - sin * qy,
            center.1 + sin * qx + cos * qy,
        )
    }

    /// The output pixels the cropped layer covers: x, y, width, height,
    /// clipped to the output (`None` if nothing is left).
    fn bounds(&self, rect: Rect, out: (u32, u32)) -> Option<Rect> {
        let [left, top, right, bottom] = self.crop;
        let corners = [
            (left, top),
            (1.0 - right, top),
            (left, 1.0 - bottom),
            (1.0 - right, 1.0 - bottom),
        ]
        .map(|corner| self.apply(rect, out, corner));
        let min = |f: fn(&(f32, f32)) -> f32| corners.iter().map(f).fold(f32::INFINITY, f32::min);
        let max =
            |f: fn(&(f32, f32)) -> f32| corners.iter().map(f).fold(f32::NEG_INFINITY, f32::max);
        let x0 = (min(|c| c.0).floor() as i32).max(0);
        let y0 = (min(|c| c.1).floor() as i32).max(0);
        let x1 = (max(|c| c.0).ceil() as i32).min(out.0 as i32);
        let y1 = (max(|c| c.1).ceil() as i32).min(out.1 as i32);
        (x1 > x0 && y1 > y0 && left + right < 1.0 && top + bottom < 1.0).then(|| Rect {
            x: x0,
            y: y0,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }

    /// The size to convert a layer of `rect` at: larger when scaled up, so
    /// the picture stays sharp (up to 4× and 8192 pixels).
    fn image_size(&self, rect: Rect) -> (u32, u32) {
        let factor = self.scale.clamp(1.0, 4.0);
        let size = |n: u32| ((n as f32 * factor).round() as u32).clamp(1, 8192);
        (size(rect.width), size(rect.height))
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Default for LayerStyle {
    fn default() -> Self {
        Self::blend(1.0, BlendMode::Normal)
    }
}

impl LayerStyle {
    /// Ungraded.
    pub fn blend(opacity: f32, mode: BlendMode) -> Self {
        Self {
            opacity,
            mode,
            grade: Grade::NEUTRAL,
            transform: Transform::IDENTITY,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct OutputParams {
    size: [i32; 2],
    /// `nox_color::OutputEncoding`.
    encoding: i32,
}

/// A clip's picture as a `LayerHook` gets it: RGBA32F in the linear
/// working space (ACEScg), straight alpha, `width` × `height`, in GENERAL
/// layout with the converter's writes done.
pub struct LayerPicture {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub width: u32,
    pub height: u32,
}

/// Changes a clip's picture after conversion and before blending, e.g.
/// writes a matte into its alpha, with submissions of its own on the
/// compositor's device (`compute`). The picture must stay in GENERAL
/// layout; blending multiplies by its alpha.
pub trait LayerHook {
    fn process(&mut self, picture: &LayerPicture) -> Result<()>;
}

fn run_hook(
    hook: Option<&mut dyn LayerHook>,
    image: &GpuImage,
    width: u32,
    height: u32,
) -> Result<()> {
    match hook {
        Some(hook) => hook.process(&LayerPicture {
            image: image.image,
            view: image.view,
            width,
            height,
        }),
        None => Ok(()),
    }
}

/// A converted layer waiting to be blended.
struct Layer {
    rect: Rect,
    style: LayerStyle,
    source: Source,
}

/// Where a layer's converted pixels are.
#[derive(Clone, Copy)]
enum Source {
    /// A per-picture image, written for this picture.
    Slot(usize),
    /// An entry of the still cache.
    Cached(usize),
}

/// Converted stills kept between pictures.
const MAX_CACHED: usize = 8;

/// A converted still, kept so it isn't uploaded and converted again for
/// every picture (a 30 MP photo is ~45 MB per frame otherwise).
struct CachedLayer {
    key: u64,
    image: GpuImage,
    width: u32,
    height: u32,
    /// GENERAL right after conversion, SHADER_READ_ONLY_OPTIMAL once blended.
    layout: vk::ImageLayout,
    used: u64,
}

/// A cached layer image and its size.
struct LayerImage {
    image: GpuImage,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct YuvParams {
    rows: Matrix3x4,
    size: [i32; 2],
    encoding: i32,
    /// Measure the light (`light.glsl`).
    measure: i32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LinearParams {
    rows: Matrix3x4,
    size: [i32; 2],
    encoding: i32,
    deep: i32,
    /// In 32-bit words.
    luma_offset: u32,
    chroma_offset: u32,
    pitch: u32,
    measure: i32,
}

/// An NV12 image of another user (e.g. a video encoder's input frame) to
/// write the picture into, with its synchronization state; see
/// `Compositor::finish_to_yuv`.
pub struct YuvTarget {
    pub width: u32,
    pub height: u32,
    /// Plane 0, viewed as `R8`.
    pub luma: VkPlane,
    /// Plane 1, viewed as `R8G8`.
    pub chroma: VkPlane,
    /// One entry per distinct image of the planes.
    pub images: Vec<ImageSync>,
    /// R'G'B' to normalized `(Y, Cb, Cr)` texel values.
    pub matrix: Matrix3x4,
}

enum Output<'a> {
    Readback(&'a Readback),
    Export(vk::Image, vk::ImageView),
    /// Luma and chroma plane views of the target; measures the light when
    /// the flag is set.
    Yuv(&'a mut YuvTarget, vk::ImageView, vk::ImageView, bool),
    /// Measures the light when the flag is set.
    Buffer(&'a BufferTarget<'a>, bool),
}

/// The views the output shader writes.
enum OutputViews {
    Rgba(vk::ImageView),
    /// Luma, chroma and the light measurements.
    Yuv(vk::ImageView, vk::ImageView, vk::Buffer),
    /// The frame buffer and the light measurements.
    Buffer(vk::Buffer, vk::Buffer),
}

pub struct Compositor {
    converter: Converter,
    blend: Pipeline,
    /// To RGBA8.
    output: Pipeline,
    /// To RGBA16F (`AB4H` export); uses `output_set` (identical set
    /// layouts).
    output_f16: Pipeline,
    output_yuv: Pipeline,
    /// RGBA16 readback and P010 planes, for HDR exports.
    output_deep: Pipeline,
    output_yuv_deep: Pipeline,
    /// Into a shared buffer, NV12 or P010.
    output_linear: Pipeline,
    /// How pictures leave the working space.
    encoding: OutputEncoding,
    descriptor_pool: vk::DescriptorPool,
    blend_sets: Vec<vk::DescriptorSet>,
    output_set: vk::DescriptorSet,
    output_yuv_set: vk::DescriptorSet,
    output_linear_set: vk::DescriptorSet,
    /// Per-workgroup light measurements of `output_linear` (host-visible),
    /// and how many it holds.
    light: Option<(GpuBuffer, u64)>,
    sampler: vk::Sampler,
    one_shot: OneShot,
    width: u32,
    height: u32,
    accum: Option<(GpuImage, u32, u32)>,
    images: Vec<LayerImage>,
    /// Slots used by the current picture.
    slots_used: usize,
    layers: Vec<Layer>,
    cache: Vec<CachedLayer>,
    clock: u64,
    readback: Option<Readback>,
    export: Option<ExportPool>,
    /// Measured for every picture while enabled (`set_scopes`).
    scopes: Option<Scopes>,
    scope_images: Option<ScopeImages>,
}

impl Compositor {
    pub fn new(gpu: Arc<GpuDevice>) -> Result<Self> {
        let converter = Converter::new(gpu.clone())?;
        let device = &gpu.device;
        let storage = |binding| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let sampled = vk::DescriptorSetLayoutBinding::default()
            .binding(1)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::COMPUTE);
        // SAFETY: plain object creation on a valid device; everything is
        // destroyed in Drop.
        unsafe {
            let blend = create_pipeline(
                device,
                BLEND_SHADER,
                &[storage(0), sampled],
                size_of::<BlendParams>(),
            )?;
            let output = create_pipeline(
                device,
                OUTPUT_SHADER,
                &[storage(0), storage(1)],
                size_of::<OutputParams>(),
            )?;
            let output_f16 = create_pipeline(
                device,
                OUTPUT_F16_SHADER,
                &[storage(0), storage(1)],
                size_of::<OutputParams>(),
            )?;
            let buffer = |binding| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
            };
            let output_yuv = create_pipeline(
                device,
                OUTPUT_YUV_SHADER,
                &[storage(0), storage(1), storage(2), buffer(3)],
                size_of::<YuvParams>(),
            )?;
            let output_deep = create_pipeline(
                device,
                OUTPUT_DEEP_SHADER,
                &[storage(0), storage(1)],
                size_of::<OutputParams>(),
            )?;
            let output_yuv_deep = create_pipeline(
                device,
                OUTPUT_YUV_DEEP_SHADER,
                &[storage(0), storage(1), storage(2), buffer(3)],
                size_of::<YuvParams>(),
            )?;
            let output_linear = create_pipeline(
                device,
                OUTPUT_LINEAR_SHADER,
                &[storage(0), buffer(1), buffer(2)],
                size_of::<LinearParams>(),
            )?;
            let pool_sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: MAX_LAYERS as u32 + 6,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_BUFFER,
                    descriptor_count: 3,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: MAX_LAYERS as u32,
                },
            ];
            let descriptor_pool = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_LAYERS as u32 + 3)
                    .pool_sizes(&pool_sizes),
                None,
            )?;
            let blend_layouts = vec![blend.set_layout; MAX_LAYERS];
            let blend_sets = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&blend_layouts),
            )?;
            let output_set = alloc_set(device, descriptor_pool, output.set_layout)?;
            let output_yuv_set = alloc_set(device, descriptor_pool, output_yuv.set_layout)?;
            let output_linear_set = alloc_set(device, descriptor_pool, output_linear.set_layout)?;
            // Layers are blended 1:1, texel by texel.
            let sampler = device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )?;
            let one_shot = OneShot::new(device, gpu.queue_family)?;
            Ok(Self {
                converter,
                blend,
                output,
                output_f16,
                output_yuv,
                output_deep,
                output_yuv_deep,
                output_linear,
                encoding: OutputEncoding::Sdr,
                descriptor_pool,
                blend_sets,
                output_set,
                output_yuv_set,
                output_linear_set,
                light: None,
                sampler,
                one_shot,
                width: 0,
                height: 0,
                accum: None,
                images: Vec::new(),
                slots_used: 0,
                layers: Vec::new(),
                cache: Vec::new(),
                clock: 0,
                readback: None,
                export: None,
                scopes: None,
                scope_images: None,
            })
        }
    }

    pub fn gpu(&self) -> &Arc<GpuDevice> {
        self.converter.gpu()
    }

    /// Enables `finish_dmabuf`. `importable` lists the (DRM fourcc,
    /// modifier) pairs the consumer can import; half float (`AB4H`) is
    /// preferred over `AB24`.
    pub fn enable_dmabuf(&mut self, importable: &[(u32, u64)]) -> Result<()> {
        let pool = ExportPool::new(self.gpu().clone(), DmabufFormat::Rgba16f, importable).or_else(
            |err| {
                tracing::info!("no half-float DMA-BUF ({err}), using 8 bit");
                ExportPool::new(self.gpu().clone(), DmabufFormat::Rgba8, importable)
            },
        )?;
        self.export = Some(pool);
        Ok(())
    }

    /// Format of `finish_dmabuf` frames, once enabled.
    pub fn dmabuf_format(&self) -> Option<DmabufFormat> {
        self.export.as_ref().map(ExportPool::format)
    }

    /// Measures scopes (`ScopeImages`) of every picture finished from now
    /// on as RGBA or DMA-BUF (not into encoder images), until disabled.
    pub fn set_scopes(&mut self, enabled: bool) -> Result<()> {
        match (enabled, self.scopes.take()) {
            (true, Some(scopes)) => self.scopes = Some(scopes),
            (true, None) => self.scopes = Some(Scopes::new(&self.converter)?),
            (false, Some(scopes)) => scopes.destroy(&self.converter),
            (false, None) => {}
        }
        Ok(())
    }

    /// The scopes of the last finished picture, if enabled.
    pub fn take_scopes(&mut self) -> Option<ScopeImages> {
        self.scope_images.take()
    }

    pub fn dmabuf_enabled(&self) -> bool {
        self.export.is_some()
    }

    /// Starts a picture of `width` × `height`.
    pub fn begin(&mut self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidFrame("empty output"));
        }
        self.width = width;
        self.height = height;
        self.layers.clear();
        self.slots_used = 0;
        self.clock += 1;
        Ok(())
    }

    /// Whether a layer at `rect` with `style` shows anything.
    fn visible(&self, rect: Rect, style: &LayerStyle) -> bool {
        rect.width > 0
            && rect.height > 0
            && style.opacity > 0.0
            && style
                .transform
                .bounds(rect, (self.width, self.height))
                .is_some()
    }

    /// Queues the still cached under `key` at `rect`'s size, if there is
    /// one; returns whether it was (else convert it with
    /// `add_cached_layer`). Invisible layers count as queued.
    pub fn try_cached_layer(&mut self, key: u64, rect: Rect, style: LayerStyle) -> bool {
        if !self.visible(rect, &style) {
            return true;
        }
        let size = style.transform.image_size(rect);
        let Some(index) = self
            .cache
            .iter()
            .position(|c| c.key == key && (c.width, c.height) == size)
        else {
            return false;
        };
        if self.layers.len() >= MAX_LAYERS {
            return true;
        }
        self.cache[index].used = self.clock;
        self.layers.push(Layer {
            rect,
            style: LayerStyle {
                opacity: style.opacity.min(1.0),
                ..style
            },
            source: Source::Cached(index),
        });
        true
    }

    /// Like `add_layer`, but keeps the converted image under `key` for
    /// later pictures (`try_cached_layer`). For stills.
    pub fn add_cached_layer<'f, 'a: 'f>(
        &mut self,
        key: u64,
        input: impl Into<Input<'f, 'a>>,
        rect: Rect,
        style: LayerStyle,
    ) -> Result<()> {
        self.add_cached_layer_with(key, input, rect, style, None)
    }

    /// `add_cached_layer` with `hook` changing the picture before it's
    /// kept (the key must say so).
    pub fn add_cached_layer_with<'f, 'a: 'f>(
        &mut self,
        key: u64,
        input: impl Into<Input<'f, 'a>>,
        rect: Rect,
        style: LayerStyle,
        hook: Option<&mut dyn LayerHook>,
    ) -> Result<()> {
        if self.layers.len() >= MAX_LAYERS {
            return Err(Error::Unsupported(format!("more than {MAX_LAYERS} layers")));
        }
        if !self.visible(rect, &style) {
            return Ok(());
        }
        let (width, height) = style.transform.image_size(rect);
        let image = self.converter.create_image(
            WORKING_FORMAT,
            width,
            height,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
        )?;
        let converted = self
            .converter
            .convert_into(input, &image, width, height)
            .and_then(|()| run_hook(hook, &image, width, height));
        if let Err(err) = converted {
            self.converter.destroy_image(image);
            return Err(err);
        }
        let entry = CachedLayer {
            key,
            image,
            width,
            height,
            layout: vk::ImageLayout::GENERAL,
            used: self.clock,
        };
        // Replace the least recently used entry not in this picture.
        let index = if self.cache.len() < MAX_CACHED {
            self.cache.push(entry);
            self.cache.len() - 1
        } else {
            let in_use: Vec<usize> = self
                .layers
                .iter()
                .filter_map(|l| match l.source {
                    Source::Cached(i) => Some(i),
                    Source::Slot(_) => None,
                })
                .collect();
            let victim = (0..self.cache.len())
                .filter(|i| !in_use.contains(i))
                .min_by_key(|&i| self.cache[i].used)
                .expect("a cache entry not in use");
            let old = std::mem::replace(&mut self.cache[victim], entry);
            self.converter.destroy_image(old.image);
            victim
        };
        self.layers.push(Layer {
            rect,
            style: LayerStyle {
                opacity: style.opacity.min(1.0),
                ..style
            },
            source: Source::Cached(index),
        });
        Ok(())
    }

    /// The image a layer is blended from.
    fn layer_image(&self, layer: &Layer) -> &GpuImage {
        match layer.source {
            Source::Slot(i) => &self.images[i].image,
            Source::Cached(i) => &self.cache[i].image,
        }
    }

    /// Converts `input` to `rect` and queues it for blending on top of the
    /// layers added so far. Layers entirely outside the picture or fully
    /// transparent are skipped.
    pub fn add_layer<'f, 'a: 'f>(
        &mut self,
        input: impl Into<Input<'f, 'a>>,
        rect: Rect,
        style: LayerStyle,
    ) -> Result<()> {
        self.add_layer_with(input, rect, style, None)
    }

    /// `add_layer` with `hook` changing the picture before it's blended.
    pub fn add_layer_with<'f, 'a: 'f>(
        &mut self,
        input: impl Into<Input<'f, 'a>>,
        rect: Rect,
        style: LayerStyle,
        hook: Option<&mut dyn LayerHook>,
    ) -> Result<()> {
        if self.layers.len() >= MAX_LAYERS {
            return Err(Error::Unsupported(format!("more than {MAX_LAYERS} layers")));
        }
        if !self.visible(rect, &style) {
            return Ok(());
        }
        let index = self.slots_used;
        let (width, height) = style.transform.image_size(rect);
        if self
            .images
            .get(index)
            .is_none_or(|i| (i.width, i.height) != (width, height))
        {
            let image = self.converter.create_image(
                WORKING_FORMAT,
                width,
                height,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            )?;
            let layer_image = LayerImage {
                image,
                width,
                height,
            };
            if index < self.images.len() {
                let old = std::mem::replace(&mut self.images[index], layer_image);
                self.converter.destroy_image(old.image);
            } else {
                self.images.push(layer_image);
            }
        }
        let target = &self.images[index].image;
        self.converter.convert_into(input, target, width, height)?;
        run_hook(hook, target, width, height)?;
        self.slots_used += 1;
        self.layers.push(Layer {
            rect,
            style: LayerStyle {
                opacity: style.opacity.min(1.0),
                ..style
            },
            source: Source::Slot(index),
        });
        Ok(())
    }

    /// How pictures leave the working space: SDR BT.709 (the default) or
    /// BT.2020 PQ or HLG for HDR exports. With HDR, HDR sources keep their
    /// light, and `finish` reads back RGBA16.
    pub fn set_output_encoding(&mut self, encoding: OutputEncoding) {
        self.encoding = encoding;
        self.converter.set_keep_hdr(encoding.is_hdr());
    }

    pub fn output_encoding(&self) -> OutputEncoding {
        self.encoding
    }

    /// Blends the layers and reads the picture back: RGBA8, or RGBA16 for
    /// an HDR encoding.
    pub fn finish(&mut self) -> Result<Image> {
        let (width, height) = (self.width, self.height);
        let bytes = if self.encoding.is_hdr() { 8 } else { 4 };
        if self
            .readback
            .as_ref()
            .is_none_or(|r| (r.width, r.height, r.bytes_per_pixel) != (width, height, bytes))
        {
            if let Some(old) = self.readback.take() {
                self.converter.destroy_image(old.image);
                self.converter.destroy_buffer(old.buffer);
            }
            let deep = self.encoding.is_hdr();
            self.readback = Some(self.converter.create_readback(width, height, deep)?);
        }
        let readback = self.readback.take().expect("readback");
        let result = self.composite(Output::Readback(&readback));
        let image = result.map(|()| readback.read());
        self.readback = Some(readback);
        image
    }

    /// Blends the layers into an exported image, without copies.
    pub fn finish_dmabuf(&mut self) -> Result<DmabufFrame> {
        let pool = self
            .export
            .as_ref()
            .ok_or(Error::Unsupported("DMA-BUF export not enabled".into()))?;
        let image = pool.acquire(self.width, self.height)?;
        self.composite(Output::Export(image.image, image.view))?;
        self.export.as_ref().expect("export pool").export(image)
    }

    /// Blends the layers and writes the picture as 4:2:0 Y'CbCr into
    /// `target`, an NV12 image of another user on this device (e.g. a
    /// Vulkan Video encoder's input): no copies. Follows the same protocol
    /// as reading decoder images: waits for `target`'s semaphores, signals
    /// the next values and records the state it leaves the images in.
    /// With `measure`, also returns the light of the frame (for HDR10
    /// metadata).
    pub fn finish_to_yuv(
        &mut self,
        target: &mut YuvTarget,
        measure: bool,
    ) -> Result<Option<FrameLight>> {
        if (target.width, target.height) != (self.width, self.height) {
            return Err(Error::InvalidFrame("target size differs from the picture"));
        }
        // One workgroup per 32×32 pixels (2×2 blocks).
        let (groups_x, groups_y) = (
            self.width.div_ceil(2 * WORKGROUP_SIZE),
            self.height.div_ceil(2 * WORKGROUP_SIZE),
        );
        let groups = u64::from(groups_x) * u64::from(groups_y);
        self.ensure_light(groups)?;
        let luma = self.plane_view(&target.luma)?;
        let chroma = match self.plane_view(&target.chroma) {
            Ok(view) => view,
            Err(err) => {
                // SAFETY: just created, unused.
                unsafe { self.gpu().device.destroy_image_view(luma, None) };
                return Err(err);
            }
        };
        let result = self.composite(Output::Yuv(target, luma, chroma, measure));
        // SAFETY: the submission has completed (or failed); views are unused.
        unsafe {
            self.gpu().device.destroy_image_view(luma, None);
            self.gpu().device.destroy_image_view(chroma, None);
        }
        result?;
        Ok(measure.then(|| self.read_light(groups)))
    }

    /// A light buffer (`light.glsl`) for at least `groups` workgroups.
    fn ensure_light(&mut self, groups: u64) -> Result<()> {
        if self.light.as_ref().is_none_or(|(_, count)| *count < groups) {
            if let Some((old, _)) = self.light.take() {
                self.converter.destroy_buffer(old);
            }
            let buffer = self.converter.create_buffer(
                groups * 8,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                gpu_allocator::MemoryLocation::GpuToCpu,
            )?;
            self.light = Some((buffer, groups));
        }
        Ok(())
    }

    /// Sums up the light the last output measured, over `groups`
    /// workgroups.
    fn read_light(&self, groups: u64) -> FrameLight {
        let (buffer, _) = self.light.as_ref().expect("light buffer");
        let partials = buffer
            .allocation
            .as_ref()
            .and_then(|a| a.mapped_slice())
            .expect("mapped light buffer");
        let (mut peak, mut sum) = (0f32, 0f64);
        for partial in partials[..groups as usize * 8].as_chunks::<8>().0 {
            let value =
                |i: usize| f32::from_le_bytes(partial[i..i + 4].try_into().expect("4 bytes"));
            peak = peak.max(value(0));
            sum += f64::from(value(4));
        }
        let pixels = f64::from(self.width) * f64::from(self.height);
        FrameLight {
            peak,
            average: (sum / pixels.max(1.0)) as f32,
        }
    }

    /// Blends the layers and writes the picture as 4:2:0 Y'CbCr into
    /// `target`, a buffer shared with another API (see `interop`): no
    /// copies. The buffer is acquired from and released to the external
    /// queue family; the writes are complete when this returns. With
    /// `measure`, also returns the light of the frame (for HDR10 metadata).
    pub fn finish_to_buffer(
        &mut self,
        target: &BufferTarget<'_>,
        measure: bool,
    ) -> Result<Option<FrameLight>> {
        let layout = &target.layout;
        if layout.format.full_height_chroma() {
            return Err(Error::Unsupported(
                "the compositor writes 4:2:0 (NV12, P010) only".into(),
            ));
        }
        if (layout.width, layout.height) != (self.width, self.height) {
            return Err(Error::InvalidFrame("target size differs from the picture"));
        }
        if !layout.pitch.is_multiple_of(4)
            || !layout.luma_offset.is_multiple_of(4)
            || !layout.chroma_offset.is_multiple_of(4)
            || layout.pitch < layout.width * layout.format.bytes_per_sample()
            || layout.chroma_offset
                < layout.luma_offset + u64::from(layout.pitch) * u64::from(layout.height)
            || layout.size() > target.buffer.size()
        {
            return Err(Error::InvalidFrame("target layout doesn't fit its buffer"));
        }
        let groups = self.linear_groups(layout.format);
        self.ensure_light(groups)?;
        self.composite(Output::Buffer(target, measure))?;
        Ok(measure.then(|| self.read_light(groups)))
    }

    /// Workgroups of `output_linear` across the picture, in X and Y.
    fn linear_dispatch(&self, format: LinearFormat) -> (u32, u32) {
        // 8-bit formats pack 4 samples per word, 16-bit ones 2.
        let columns = 4 / format.bytes_per_sample();
        (
            self.width.div_ceil(columns * WORKGROUP_SIZE),
            self.height.div_ceil(2 * WORKGROUP_SIZE),
        )
    }

    fn linear_groups(&self, format: LinearFormat) -> u64 {
        let (x, y) = self.linear_dispatch(format);
        u64::from(x) * u64::from(y)
    }

    /// Blends the layers into a hardware encoder's frame, whichever kind
    /// it is (`finish_to_yuv`, `finish_to_buffer`). With `measure`, also
    /// returns the light of the frame, where the output can measure it
    /// (buffers).
    pub fn finish_to_gpu(
        &mut self,
        target: GpuTarget<'_>,
        measure: bool,
    ) -> Result<Option<FrameLight>> {
        match target {
            GpuTarget::Images(target) => self.finish_to_yuv(target, measure),
            GpuTarget::Buffer(target) => self.finish_to_buffer(&target, measure),
        }
    }

    /// A storage view of one plane of another user's image.
    fn plane_view(&self, plane: &VkPlane) -> Result<vk::ImageView> {
        // SAFETY: valid image of this device; destroyed by the caller.
        Ok(unsafe { plane.view(&self.gpu().device, vk::ImageUsageFlags::STORAGE)? })
    }

    fn ensure_accum(&mut self) -> Result<()> {
        let (width, height) = (self.width, self.height);
        if self
            .accum
            .as_ref()
            .is_none_or(|(_, w, h)| (*w, *h) != (width, height))
        {
            if let Some((old, _, _)) = self.accum.take() {
                self.converter.destroy_image(old);
            }
            let image = self.converter.create_image(
                WORKING_FORMAT,
                width,
                height,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_DST,
            )?;
            self.accum = Some((image, width, height));
        }
        Ok(())
    }

    fn write_descriptors(&self, output: &OutputViews) {
        let accum = self.accum.as_ref().expect("accum").0.view;
        let storage = |view| {
            vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::GENERAL)
        };
        let accum_info = storage(accum);
        let (first, second) = match *output {
            OutputViews::Rgba(view) => (view, None),
            OutputViews::Yuv(luma, chroma, _) => (luma, Some(chroma)),
            OutputViews::Buffer(..) => (vk::ImageView::null(), None),
        };
        let first_info = storage(first);
        let second_info = second.map(storage);
        let output_set = if second.is_some() {
            self.output_yuv_set
        } else {
            self.output_set
        };
        let yuv_light = match *output {
            OutputViews::Yuv(_, _, light) => Some(
                vk::DescriptorBufferInfo::default()
                    .buffer(light)
                    .range(vk::WHOLE_SIZE),
            ),
            _ => None,
        };
        let buffer_infos = match *output {
            OutputViews::Buffer(frame, light) => Some([
                vk::DescriptorBufferInfo::default()
                    .buffer(frame)
                    .range(vk::WHOLE_SIZE),
                vk::DescriptorBufferInfo::default()
                    .buffer(light)
                    .range(vk::WHOLE_SIZE),
            ]),
            _ => None,
        };
        let layer_infos: Vec<_> = self
            .layers
            .iter()
            .enumerate()
            .map(|(i, _)| {
                vk::DescriptorImageInfo::default()
                    .sampler(self.sampler)
                    .image_view(self.layer_image(&self.layers[i]).view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            })
            .collect();
        let mut writes = Vec::new();
        for (i, layer_info) in layer_infos.iter().enumerate() {
            writes.push(image_write(
                self.blend_sets[i],
                0,
                vk::DescriptorType::STORAGE_IMAGE,
                &accum_info,
            ));
            writes.push(image_write(
                self.blend_sets[i],
                1,
                vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                layer_info,
            ));
        }
        if let Some(infos) = &buffer_infos {
            writes.push(image_write(
                self.output_linear_set,
                0,
                vk::DescriptorType::STORAGE_IMAGE,
                &accum_info,
            ));
            for (i, info) in infos.iter().enumerate() {
                writes.push(buffer_write(self.output_linear_set, 1 + i as u32, info));
            }
        }
        let outputs = if buffer_infos.is_some() {
            [None, None, None]
        } else {
            [Some(&accum_info), Some(&first_info), second_info.as_ref()]
        };
        for (binding, info) in outputs.into_iter().enumerate() {
            if let Some(info) = info {
                writes.push(image_write(
                    output_set,
                    binding as u32,
                    vk::DescriptorType::STORAGE_IMAGE,
                    info,
                ));
            }
        }
        if let Some(info) = &yuv_light {
            writes.push(buffer_write(self.output_yuv_set, 3, info));
        }
        // SAFETY: no submission using the sets is pending.
        unsafe { self.gpu().device.update_descriptor_sets(&writes, &[]) };
    }

    /// Records and runs: clear, blend every layer, write the output.
    fn composite(&mut self, mut output: Output<'_>) -> Result<()> {
        self.ensure_accum()?;
        let (out_image, views) = match &output {
            Output::Readback(r) => (r.image.image, OutputViews::Rgba(r.image.view)),
            Output::Export(image, view) => (*image, OutputViews::Rgba(*view)),
            Output::Yuv(_, luma, chroma, _) => (
                vk::Image::null(),
                OutputViews::Yuv(
                    *luma,
                    *chroma,
                    self.light.as_ref().expect("light buffer").0.buffer,
                ),
            ),
            Output::Buffer(target, _) => (
                vk::Image::null(),
                OutputViews::Buffer(
                    target.buffer.buffer,
                    self.light.as_ref().expect("light buffer").0.buffer,
                ),
            ),
        };
        self.write_descriptors(&views);
        let gpu = self.gpu().clone();
        let device = &gpu.device;
        let cmd = self.one_shot.cmd;
        let accum = self.accum.as_ref().expect("accum").0.image;
        let (width, height) = (self.width, self.height);
        // SAFETY: all handles are valid; the previous submission has
        // completed (we wait on the fence at the end of every call).
        unsafe {
            self.one_shot.begin(device)?;
            // Layers converted by earlier submissions on this queue become
            // readable; cached stills blended before already are.
            let mut converted: Vec<vk::Image> = Vec::new();
            for layer in &self.layers {
                let fresh = match layer.source {
                    Source::Slot(_) => true,
                    Source::Cached(i) => self.cache[i].layout == vk::ImageLayout::GENERAL,
                };
                let image = self.layer_image(layer).image;
                if fresh && !converted.contains(&image) {
                    converted.push(image);
                }
            }
            let mut barriers: Vec<_> = converted
                .iter()
                .map(|&image| {
                    image_barrier(
                        image,
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    )
                    .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                })
                .collect();
            barriers.push(
                image_barrier(
                    accum,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                )
                .dst_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE),
            );
            device.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&barriers),
            );
            device.cmd_clear_color_image(
                cmd,
                accum,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
                &[color_range()],
            );
            let to_general = image_barrier(
                accum,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::GENERAL,
            )
            .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
            .dst_access_mask(
                vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
            );
            device.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default()
                    .image_memory_barriers(std::slice::from_ref(&to_general)),
            );

            // Each layer reads what the previous one wrote.
            let accum_dependency = vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(
                    vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
                );
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.blend.pipeline);
            for (i, layer) in self.layers.iter().enumerate() {
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    self.blend.layout,
                    0,
                    &[self.blend_sets[i]],
                    &[],
                );
                let k = layer.style.grade.coefficients();
                let graded = if layer.style.grade.is_neutral() {
                    0.0
                } else {
                    1.0
                };
                let t = layer.style.transform;
                let transformed = !t.is_identity();
                // Untransformed layers are blended texel by texel over their
                // rectangle; transformed ones over the pixels they cover.
                let region = if transformed {
                    match t.bounds(layer.rect, (width, height)) {
                        Some(region) => region,
                        None => continue,
                    }
                } else {
                    layer.rect
                };
                let (sin, cos) = t.rotation.to_radians().sin_cos();
                let inverse_scale = 1.0 / t.scale.max(1e-6);
                let (w, h) = (layer.rect.width as f32, layer.rect.height as f32);
                let params = BlendParams {
                    offset: [region.x, region.y],
                    size: [region.width as i32, region.height as i32],
                    out_size: [width as i32, height as i32],
                    opacity: layer.style.opacity,
                    mode: layer.style.mode as i32,
                    slope: [k.slope[0], k.slope[1], k.slope[2], graded],
                    add: [
                        k.add[0],
                        k.add[1],
                        k.add[2],
                        f32::from(u8::from(transformed)),
                    ],
                    power: [k.power[0], k.power[1], k.power[2], 0.0],
                    // R(-θ) / scale, as rows.
                    inverse: [
                        cos * inverse_scale,
                        sin * inverse_scale,
                        -sin * inverse_scale,
                        cos * inverse_scale,
                    ],
                    geometry: [
                        layer.rect.x as f32 + w / 2.0 + t.offset[0] * width as f32,
                        layer.rect.y as f32 + h / 2.0 + t.offset[1] * height as f32,
                        w / 2.0,
                        h / 2.0,
                    ],
                    crop: [t.crop[0], t.crop[1], 1.0 - t.crop[2], 1.0 - t.crop[3]],
                };
                push(device, cmd, self.blend.layout, &params);
                device.cmd_dispatch(
                    cmd,
                    region.width.div_ceil(WORKGROUP_SIZE),
                    region.height.div_ceil(WORKGROUP_SIZE),
                    1,
                );
                device.cmd_pipeline_barrier2(
                    cmd,
                    &vk::DependencyInfo::default()
                        .memory_barriers(std::slice::from_ref(&accum_dependency)),
                );
            }

            if let Output::Yuv(target, _, _, measure) = &output {
                // Acquire the target from its last user. Its contents are
                // overwritten, so the old layout's data doesn't matter.
                let own = gpu.queue_family;
                let acquire: Vec<_> = target
                    .images
                    .iter()
                    .map(|sync| {
                        let (src, dst) = queue_transfer(sync.queue_family, own);
                        image_barrier(sync.image, sync.layout, vk::ImageLayout::GENERAL)
                            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                            .src_access_mask(sync.access)
                            .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                            .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                            .src_queue_family_index(src)
                            .dst_queue_family_index(dst)
                    })
                    .collect();
                device.cmd_pipeline_barrier2(
                    cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&acquire),
                );
                // 16-bit planes (P010) for 10-bit HDR.
                let pipeline = if target.luma.format == PlaneFormat::R16 {
                    &self.output_yuv_deep
                } else {
                    &self.output_yuv
                };
                device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline.pipeline);
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    pipeline.layout,
                    0,
                    &[self.output_yuv_set],
                    &[],
                );
                push(
                    device,
                    cmd,
                    pipeline.layout,
                    &YuvParams {
                        rows: target.matrix,
                        size: [width as i32, height as i32],
                        encoding: self.encoding as i32,
                        measure: i32::from(*measure),
                    },
                );
                // One invocation per 2×2 block.
                device.cmd_dispatch(
                    cmd,
                    width.div_ceil(2 * WORKGROUP_SIZE),
                    height.div_ceil(2 * WORKGROUP_SIZE),
                    1,
                );
            } else if let Output::Buffer(target, measure) = &output {
                // Acquire the buffer from the other API (which has finished
                // reading the previous frame; see `interop`). It's
                // overwritten, so its contents don't matter.
                let acquire = vk::BufferMemoryBarrier2::default()
                    .buffer(target.buffer.buffer)
                    .size(vk::WHOLE_SIZE)
                    .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                    .dst_queue_family_index(gpu.queue_family)
                    .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE);
                device.cmd_pipeline_barrier2(
                    cmd,
                    &vk::DependencyInfo::default()
                        .buffer_memory_barriers(std::slice::from_ref(&acquire)),
                );
                let pipeline = &self.output_linear;
                device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline.pipeline);
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    pipeline.layout,
                    0,
                    &[self.output_linear_set],
                    &[],
                );
                let layout = &target.layout;
                push(
                    device,
                    cmd,
                    pipeline.layout,
                    &LinearParams {
                        rows: target.matrix,
                        size: [width as i32, height as i32],
                        encoding: self.encoding as i32,
                        deep: i32::from(layout.format.bytes_per_sample() == 2),
                        luma_offset: (layout.luma_offset / 4) as u32,
                        chroma_offset: (layout.chroma_offset / 4) as u32,
                        pitch: layout.pitch / 4,
                        measure: i32::from(*measure),
                    },
                );
                let (x, y) = self.linear_dispatch(layout.format);
                device.cmd_dispatch(cmd, x, y, 1);
            } else {
                let to_output = image_barrier(
                    out_image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                )
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE);
                device.cmd_pipeline_barrier2(
                    cmd,
                    &vk::DependencyInfo::default()
                        .image_memory_barriers(std::slice::from_ref(&to_output)),
                );
                let half_float = matches!(output, Output::Export(..))
                    && self.dmabuf_format() == Some(DmabufFormat::Rgba16f);
                let deep = matches!(output, Output::Readback(r) if r.bytes_per_pixel == 8);
                let pipeline = if half_float {
                    &self.output_f16
                } else if deep {
                    &self.output_deep
                } else {
                    &self.output
                };
                // The viewer (DMA-BUF) always shows SDR.
                let encoding = if deep {
                    self.encoding
                } else {
                    OutputEncoding::Sdr
                };
                device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline.pipeline);
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    pipeline.layout,
                    0,
                    &[self.output_set],
                    &[],
                );
                push(
                    device,
                    cmd,
                    pipeline.layout,
                    &OutputParams {
                        size: [width as i32, height as i32],
                        encoding: encoding as i32,
                    },
                );
                device.cmd_dispatch(
                    cmd,
                    width.div_ceil(WORKGROUP_SIZE),
                    height.div_ceil(WORKGROUP_SIZE),
                    1,
                );
            }
            match &output {
                Output::Readback(readback) => record_readback(device, cmd, readback, width, height),
                Output::Export(image, _) => {
                    let release =
                        image_barrier(*image, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL)
                            .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                            .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                            .src_queue_family_index(gpu.queue_family)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL);
                    device.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default()
                            .image_memory_barriers(std::slice::from_ref(&release)),
                    );
                }
                // The semaphore signal below makes the writes available to
                // the next user, who acquires the images itself; the light
                // measured becomes readable on the host.
                Output::Yuv(.., measure) => {
                    if *measure {
                        let to_host = vk::BufferMemoryBarrier2::default()
                            .buffer(self.light.as_ref().expect("light buffer").0.buffer)
                            .size(vk::WHOLE_SIZE)
                            .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                            .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                            .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                            .dst_access_mask(vk::AccessFlags2::HOST_READ);
                        device.cmd_pipeline_barrier2(
                            cmd,
                            &vk::DependencyInfo::default()
                                .buffer_memory_barriers(std::slice::from_ref(&to_host)),
                        );
                    }
                }
                Output::Buffer(target, measure) => {
                    let release = vk::BufferMemoryBarrier2::default()
                        .buffer(target.buffer.buffer)
                        .size(vk::WHOLE_SIZE)
                        .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                        .src_queue_family_index(gpu.queue_family)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL);
                    let mut barriers = vec![release];
                    if *measure {
                        barriers.push(
                            vk::BufferMemoryBarrier2::default()
                                .buffer(self.light.as_ref().expect("light buffer").0.buffer)
                                .size(vk::WHOLE_SIZE)
                                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                                .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                                .dst_access_mask(vk::AccessFlags2::HOST_READ),
                        );
                    }
                    device.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().buffer_memory_barriers(&barriers),
                    );
                }
            }
            // The accumulator is still in GENERAL, its writes visible to
            // compute shaders (the barrier after the last layer).
            let measure = matches!(output, Output::Readback(_) | Output::Export(..));
            if let Some(scopes) = self.scopes.as_ref().filter(|_| measure) {
                let view = self.accum.as_ref().expect("accum").0.view;
                scopes.record(device, cmd, view, width, height);
            }
            let (waits, signals) = match &output {
                Output::Yuv(target, ..) => {
                    semaphore_infos(&target.images, vk::PipelineStageFlags2::ALL_COMMANDS)
                }
                _ => (Vec::new(), Vec::new()),
            };
            self.one_shot.submit_and_wait(&gpu, &waits, &signals)?;
            for layer in &self.layers {
                if let Source::Cached(i) = layer.source {
                    self.cache[i].layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                }
            }
            // Done: record the state the images are in for their next user.
            if let Output::Yuv(target, ..) = &mut output {
                for sync in &mut target.images {
                    sync.advance(vk::ImageLayout::GENERAL, gpu.queue_family);
                }
            }
            if let Some(scopes) = self.scopes.as_ref().filter(|_| measure) {
                self.scope_images = Some(scopes.read());
            }
        }
        Ok(())
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        for layer in std::mem::take(&mut self.images) {
            self.converter.destroy_image(layer.image);
        }
        for cached in std::mem::take(&mut self.cache) {
            self.converter.destroy_image(cached.image);
        }
        if let Some((accum, _, _)) = self.accum.take() {
            self.converter.destroy_image(accum);
        }
        if let Some(readback) = self.readback.take() {
            self.converter.destroy_image(readback.image);
            self.converter.destroy_buffer(readback.buffer);
        }
        self.export = None;
        if let Some((light, _)) = self.light.take() {
            self.converter.destroy_buffer(light);
        }
        if let Some(scopes) = self.scopes.take() {
            scopes.destroy(&self.converter);
        }
        let gpu = self.gpu().clone();
        let device = &gpu.device;
        // SAFETY: no submission is pending (every call waits for completion).
        unsafe {
            self.one_shot.destroy(device);
            device.destroy_sampler(self.sampler, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            for pipeline in [
                &self.blend,
                &self.output,
                &self.output_f16,
                &self.output_yuv,
                &self.output_deep,
                &self.output_yuv_deep,
                &self.output_linear,
            ] {
                pipeline.destroy(device);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placements() {
        let rect = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        let (preview, seq) = ((960, 540), (1920, 1080));
        // 4:3 source in a 16:9 preview.
        let sd = (1440, 1080);
        assert_eq!(
            Rect::place(Placement::Fit, sd, preview, seq),
            rect(120, 0, 720, 540)
        );
        assert_eq!(
            Rect::place(Placement::Fill, sd, preview, seq),
            rect(0, -90, 960, 720)
        );
        assert_eq!(
            Rect::place(Placement::Stretch, sd, preview, seq),
            rect(0, 0, 960, 540)
        );
        // UHD in an HD sequence, pixels 1:1: only the middle quarter shows.
        let uhd = (3840, 2160);
        assert_eq!(
            Rect::place(Placement::CenterCrop, uhd, preview, seq),
            rect(-480, -270, 1920, 1080)
        );
        // 720p in an HD sequence, 1:1: a border around it.
        let hd = (1280, 720);
        assert_eq!(
            Rect::place(Placement::CenterCrop, hd, preview, seq),
            rect(160, 90, 640, 360)
        );
    }

    #[test]
    fn fit_letterboxes_and_pillarboxes() {
        assert_eq!(
            Rect::fit(16.0 / 9.0, 1920, 1080),
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
        // 4:3 in 16:9: pillarbox.
        assert_eq!(
            Rect::fit(4.0 / 3.0, 1920, 1080),
            Rect {
                x: 240,
                y: 0,
                width: 1440,
                height: 1080
            }
        );
        // 2.39:1 in 16:9: letterbox.
        let wide = Rect::fit(2.39, 1920, 1080);
        assert_eq!((wide.x, wide.width, wide.height), (0, 1920, 803));
        assert_eq!(wide.y, 139);
    }
}
