//! Composites several frames into one picture on the GPU.
//!
//! Each layer is converted into the linear working space (ACEScg, see
//! `nox_color::working`) at its placed size by the `Converter`
//! (`ycbcr_to_working`, RGBA32F), then all layers are blended bottom to top
//! in linear light into a 32-bit float accumulator (`blend.comp`) over a
//! black background. The output transform (`output.comp`) then writes BT.709
//! R'G'B': 8 bit to read back, 8 bit or half float to export as DMA-BUF, or
//! as Y'CbCr into an encoder's image (`output_yuv.comp`).

use ash::vk;
use std::sync::Arc;

use crate::converter::{
    GpuImage, Readback, WORKING_FORMAT, color_range, image_barrier, queue_transfer, record_readback,
};
use crate::export::{DmabufFormat, ExportPool};
use crate::scopes::{ScopeImages, Scopes};
use crate::{Converter, DmabufFrame, Error, GpuDevice, Image, ImageSync, Input, Result, VkPlane};
use nox_color::{Grade, Matrix3x4};

const BLEND_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blend.comp.spv"));
const OUTPUT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/output.comp.spv"));
const OUTPUT_F16_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/output_f16.comp.spv"));
const OUTPUT_YUV_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/output_yuv.comp.spv"));
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
#[derive(Clone, Copy)]
struct BlendParams {
    offset: [i32; 2],
    size: [i32; 2],
    out_size: [i32; 2],
    opacity: f32,
    mode: i32,
    /// The grade's coefficients (`nox_color::GradeCoefficients`); `w` of
    /// `slope` is 1 if the layer is graded at all.
    slope: [f32; 4],
    add: [f32; 4],
    power: [f32; 4],
}

/// How a layer is drawn: graded, then blended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerStyle {
    pub opacity: f32,
    pub mode: BlendMode,
    /// Applied before blending, in ACEScct (`nox_color::grade`).
    pub grade: Grade,
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
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct OutputParams {
    size: [i32; 2],
}

pub(crate) struct Pipeline {
    pub set_layout: vk::DescriptorSetLayout,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
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
#[derive(Clone, Copy)]
struct YuvParams {
    rows: Matrix3x4,
    size: [i32; 2],
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
    /// Luma and chroma plane views of the target.
    Yuv(&'a mut YuvTarget, vk::ImageView, vk::ImageView),
}

/// The views the output shader writes.
enum OutputViews {
    Rgba(vk::ImageView),
    Yuv(vk::ImageView, vk::ImageView),
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
    descriptor_pool: vk::DescriptorPool,
    blend_sets: Vec<vk::DescriptorSet>,
    output_set: vk::DescriptorSet,
    output_yuv_set: vk::DescriptorSet,
    sampler: vk::Sampler,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
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
            let output_yuv = create_pipeline(
                device,
                OUTPUT_YUV_SHADER,
                &[storage(0), storage(1), storage(2)],
                size_of::<YuvParams>(),
            )?;
            let pool_sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: MAX_LAYERS as u32 + 5,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: MAX_LAYERS as u32,
                },
            ];
            let descriptor_pool = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_LAYERS as u32 + 2)
                    .pool_sizes(&pool_sizes),
                None,
            )?;
            let blend_layouts = vec![blend.set_layout; MAX_LAYERS];
            let blend_sets = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&blend_layouts),
            )?;
            let output_set = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(std::slice::from_ref(&output.set_layout)),
            )?[0];
            let output_yuv_set = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(std::slice::from_ref(&output_yuv.set_layout)),
            )?[0];
            // Layers are blended 1:1, texel by texel.
            let sampler = device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )?;
            let command_pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(gpu.queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let command_buffer = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
            Ok(Self {
                converter,
                blend,
                output,
                output_f16,
                output_yuv,
                descriptor_pool,
                blend_sets,
                output_set,
                output_yuv_set,
                sampler,
                command_pool,
                command_buffer,
                fence,
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

    /// Whether `rect` shows anything of a layer at `opacity`.
    fn visible(&self, rect: Rect, opacity: f32) -> bool {
        rect.width > 0
            && rect.height > 0
            && rect.x < self.width as i32
            && rect.y < self.height as i32
            && rect.x + rect.width as i32 > 0
            && rect.y + rect.height as i32 > 0
            && opacity > 0.0
    }

    /// Queues the still cached under `key` at `rect`'s size, if there is
    /// one; returns whether it was (else convert it with
    /// `add_cached_layer`). Invisible layers count as queued.
    pub fn try_cached_layer(&mut self, key: u64, rect: Rect, style: LayerStyle) -> bool {
        if !self.visible(rect, style.opacity) {
            return true;
        }
        let Some(index) = self
            .cache
            .iter()
            .position(|c| c.key == key && (c.width, c.height) == (rect.width, rect.height))
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
        if self.layers.len() >= MAX_LAYERS {
            return Err(Error::Unsupported(format!("more than {MAX_LAYERS} layers")));
        }
        if !self.visible(rect, style.opacity) {
            return Ok(());
        }
        let image = self.converter.create_image(
            WORKING_FORMAT,
            rect.width,
            rect.height,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
        )?;
        if let Err(err) = self
            .converter
            .convert_into(input, &image, rect.width, rect.height)
        {
            self.converter.destroy_image(image);
            return Err(err);
        }
        let entry = CachedLayer {
            key,
            image,
            width: rect.width,
            height: rect.height,
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
        if self.layers.len() >= MAX_LAYERS {
            return Err(Error::Unsupported(format!("more than {MAX_LAYERS} layers")));
        }
        if !self.visible(rect, style.opacity) {
            return Ok(());
        }
        let index = self.slots_used;
        if self
            .images
            .get(index)
            .is_none_or(|i| (i.width, i.height) != (rect.width, rect.height))
        {
            let image = self.converter.create_image(
                WORKING_FORMAT,
                rect.width,
                rect.height,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            )?;
            let layer_image = LayerImage {
                image,
                width: rect.width,
                height: rect.height,
            };
            if index < self.images.len() {
                let old = std::mem::replace(&mut self.images[index], layer_image);
                self.converter.destroy_image(old.image);
            } else {
                self.images.push(layer_image);
            }
        }
        let target = &self.images[index].image;
        self.converter
            .convert_into(input, target, rect.width, rect.height)?;
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

    /// Blends the layers and reads the picture back.
    pub fn finish(&mut self) -> Result<Image> {
        let (width, height) = (self.width, self.height);
        if self
            .readback
            .as_ref()
            .is_none_or(|r| (r.width, r.height) != (width, height))
        {
            if let Some(old) = self.readback.take() {
                self.converter.destroy_image(old.image);
                self.converter.destroy_buffer(old.buffer);
            }
            self.readback = Some(self.converter.create_readback(width, height)?);
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
    pub fn finish_to_yuv(&mut self, target: &mut YuvTarget) -> Result<()> {
        if (target.width, target.height) != (self.width, self.height) {
            return Err(Error::InvalidFrame("target size differs from the picture"));
        }
        let luma = self.plane_view(&target.luma, vk::Format::R8_UNORM)?;
        let chroma = match self.plane_view(&target.chroma, vk::Format::R8G8_UNORM) {
            Ok(view) => view,
            Err(err) => {
                // SAFETY: just created, unused.
                unsafe { self.gpu().device.destroy_image_view(luma, None) };
                return Err(err);
            }
        };
        let result = self.composite(Output::Yuv(target, luma, chroma));
        // SAFETY: the submission has completed (or failed); views are unused.
        unsafe {
            self.gpu().device.destroy_image_view(luma, None);
            self.gpu().device.destroy_image_view(chroma, None);
        }
        result
    }

    /// A storage view of one plane of another user's image.
    fn plane_view(&self, plane: &VkPlane, format: vk::Format) -> Result<vk::ImageView> {
        let mut usage = vk::ImageViewUsageCreateInfo::default().usage(vk::ImageUsageFlags::STORAGE);
        // SAFETY: valid image of this device; destroyed by the caller.
        let view = unsafe {
            self.gpu().device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(plane.image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(plane.aspect)
                            .level_count(1)
                            .layer_count(1),
                    )
                    .push_next(&mut usage),
                None,
            )?
        };
        Ok(view)
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
            OutputViews::Yuv(luma, chroma) => (luma, Some(chroma)),
        };
        let first_info = storage(first);
        let second_info = second.map(storage);
        let output_set = if second.is_some() {
            self.output_yuv_set
        } else {
            self.output_set
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
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.blend_sets[i])
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .image_info(std::slice::from_ref(&accum_info)),
            );
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.blend_sets[i])
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(layer_info)),
            );
        }
        let outputs = [Some(&accum_info), Some(&first_info), second_info.as_ref()];
        for (binding, info) in outputs.into_iter().enumerate() {
            if let Some(info) = info {
                writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(output_set)
                        .dst_binding(binding as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(std::slice::from_ref(info)),
                );
            }
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
            Output::Yuv(_, luma, chroma) => (vk::Image::null(), OutputViews::Yuv(*luma, *chroma)),
        };
        self.write_descriptors(&views);
        let gpu = self.gpu().clone();
        let device = &gpu.device;
        let cmd = self.command_buffer;
        let accum = self.accum.as_ref().expect("accum").0.image;
        let (width, height) = (self.width, self.height);
        // SAFETY: all handles are valid; the previous submission has
        // completed (we wait on the fence at the end of every call).
        unsafe {
            device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
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
                let params = BlendParams {
                    offset: [layer.rect.x, layer.rect.y],
                    size: [layer.rect.width as i32, layer.rect.height as i32],
                    out_size: [width as i32, height as i32],
                    opacity: layer.style.opacity,
                    mode: layer.style.mode as i32,
                    slope: [k.slope[0], k.slope[1], k.slope[2], graded],
                    add: [k.add[0], k.add[1], k.add[2], 0.0],
                    power: [k.power[0], k.power[1], k.power[2], 0.0],
                };
                push(device, cmd, self.blend.layout, &params);
                device.cmd_dispatch(
                    cmd,
                    layer.rect.width.div_ceil(WORKGROUP_SIZE),
                    layer.rect.height.div_ceil(WORKGROUP_SIZE),
                    1,
                );
                device.cmd_pipeline_barrier2(
                    cmd,
                    &vk::DependencyInfo::default()
                        .memory_barriers(std::slice::from_ref(&accum_dependency)),
                );
            }

            if let Output::Yuv(target, ..) = &output {
                // Acquire the target from its last user. Its contents are
                // overwritten, so the old layout's data doesn't matter.
                let own = gpu.queue_family;
                // COLOR also for one multi-planar image: per-plane aspects
                // are only valid in barriers on disjoint images
                // (VUID-VkImageMemoryBarrier2-image-09242), and FFmpeg's
                // aren't.
                let acquire: Vec<_> = target
                    .images
                    .iter()
                    .map(|sync| {
                        let (src, dst) = queue_transfer(sync.queue_family, own);
                        image_barrier(sync.image, sync.layout, vk::ImageLayout::GENERAL)
                            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                            .src_access_mask(vk::AccessFlags2::from_raw(u64::from(
                                sync.access.as_raw(),
                            )))
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
                device.cmd_bind_pipeline(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    self.output_yuv.pipeline,
                );
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    self.output_yuv.layout,
                    0,
                    &[self.output_yuv_set],
                    &[],
                );
                push(
                    device,
                    cmd,
                    self.output_yuv.layout,
                    &YuvParams {
                        rows: target.matrix,
                        size: [width as i32, height as i32],
                    },
                );
                // One invocation per 2×2 block.
                device.cmd_dispatch(
                    cmd,
                    width.div_ceil(2 * WORKGROUP_SIZE),
                    height.div_ceil(2 * WORKGROUP_SIZE),
                    1,
                );
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
                let pipeline = if half_float {
                    &self.output_f16
                } else {
                    &self.output
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
                // the next user, who acquires the images itself.
                Output::Yuv(..) => {}
            }
            // The accumulator is still in GENERAL, its writes visible to
            // compute shaders (the barrier after the last layer).
            let measure = !matches!(output, Output::Yuv(..));
            if let Some(scopes) = self.scopes.as_ref().filter(|_| measure) {
                let view = self.accum.as_ref().expect("accum").0.view;
                scopes.record(device, cmd, view, width, height);
            }
            device.end_command_buffer(cmd)?;
            let (waits, signals): (Vec<_>, Vec<_>) = match &output {
                Output::Yuv(target, ..) => target
                    .images
                    .iter()
                    .map(|sync| {
                        (
                            vk::SemaphoreSubmitInfo::default()
                                .semaphore(sync.semaphore)
                                .value(sync.value)
                                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
                            vk::SemaphoreSubmitInfo::default()
                                .semaphore(sync.semaphore)
                                .value(sync.value + 1)
                                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
                        )
                    })
                    .unzip(),
                _ => (Vec::new(), Vec::new()),
            };
            let cmd_info = vk::CommandBufferSubmitInfo::default().command_buffer(cmd);
            gpu.submit(
                &vk::SubmitInfo2::default()
                    .command_buffer_infos(std::slice::from_ref(&cmd_info))
                    .wait_semaphore_infos(&waits)
                    .signal_semaphore_infos(&signals),
                self.fence,
            )?;
            for layer in &self.layers {
                if let Source::Cached(i) = layer.source {
                    self.cache[i].layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                }
            }
            // Submitted: record the state the images will be in.
            if let Output::Yuv(target, ..) = &mut output {
                for sync in &mut target.images {
                    let (_, dst) = queue_transfer(sync.queue_family, gpu.queue_family);
                    sync.layout = vk::ImageLayout::GENERAL;
                    sync.access = vk::AccessFlags::NONE;
                    sync.queue_family = dst;
                    sync.value += 1;
                }
            }
            device.wait_for_fences(&[self.fence], true, u64::MAX)?;
            device.reset_fences(&[self.fence])?;
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
        if let Some(scopes) = self.scopes.take() {
            scopes.destroy(&self.converter);
        }
        let gpu = self.gpu().clone();
        let device = &gpu.device;
        // SAFETY: no submission is pending (every call waits for completion).
        unsafe {
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.command_pool, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            for pipeline in [
                &self.blend,
                &self.output,
                &self.output_f16,
                &self.output_yuv,
            ] {
                device.destroy_pipeline(pipeline.pipeline, None);
                device.destroy_pipeline_layout(pipeline.layout, None);
                device.destroy_descriptor_set_layout(pipeline.set_layout, None);
            }
        }
    }
}

/// # Safety
/// `device` must be valid.
pub(crate) unsafe fn create_pipeline(
    device: &ash::Device,
    spirv: &[u8],
    bindings: &[vk::DescriptorSetLayoutBinding<'_>],
    push_size: usize,
) -> Result<Pipeline> {
    // SAFETY: guaranteed by the caller; objects are returned to the owner.
    unsafe {
        let set_layout = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(bindings),
            None,
        )?;
        let push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .size(push_size as u32);
        let layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(std::slice::from_ref(&set_layout))
                .push_constant_ranges(std::slice::from_ref(&push_range)),
            None,
        )?;
        let code = ash::util::read_spv(&mut std::io::Cursor::new(spirv)).expect("valid SPIR-V");
        let module = device
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(c"main");
        let pipeline = device
            .create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(stage)
                    .layout(layout)],
                None,
            )
            .map_err(|(_, err)| err)?[0];
        device.destroy_shader_module(module, None);
        Ok(Pipeline {
            set_layout,
            layout,
            pipeline,
        })
    }
}

/// # Safety
/// `cmd` must be recording with a pipeline using `layout` bound.
pub(crate) unsafe fn push<T: Copy>(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    layout: vk::PipelineLayout,
    value: &T,
) {
    // SAFETY: guaranteed by the caller; `T` is plain data.
    unsafe {
        device.cmd_push_constants(
            cmd,
            layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            std::slice::from_raw_parts((value as *const T).cast(), size_of::<T>()),
        );
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
