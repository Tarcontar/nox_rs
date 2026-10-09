//! Converts decoded frames to RGBA on the GPU (`shaders/ycbcr_to_rgb.comp`).
//!
//! Frames in system memory are uploaded plane by plane and converted by a
//! compute shader, either into an image that is read back (`convert`) or into
//! an exportable image handed out as DMA-BUF (`convert_to_dmabuf`). Work is
//! synchronous: both return when the GPU is done. Resources are cached and
//! only recreated when the frame geometry or format changes.

use ash::vk;
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme};
use nox_color::Matrix3x4;
use std::sync::Arc;

use crate::export::{DmabufFrame, ExportPool};
use crate::{Error, GpuDevice, Image, Result};

const SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ycbcr_to_rgb.comp.spv"));
const WORKGROUP_SIZE: u32 = 16;
const MAX_PLANES: usize = 3;

/// Texel format of one uploaded plane. 16-bit formats hold 9–16 bit samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaneFormat {
    R8,
    R16,
    Rg8,
    Rg16,
    Rgba8,
    Rgba16,
}

impl PlaneFormat {
    fn vk(self) -> vk::Format {
        match self {
            Self::R8 => vk::Format::R8_UNORM,
            Self::R16 => vk::Format::R16_UNORM,
            Self::Rg8 => vk::Format::R8G8_UNORM,
            Self::Rg16 => vk::Format::R16G16_UNORM,
            Self::Rgba8 => vk::Format::R8G8B8A8_UNORM,
            Self::Rgba16 => vk::Format::R16G16B16A16_UNORM,
        }
    }

    pub fn bytes_per_texel(self) -> usize {
        match self {
            Self::R8 => 1,
            Self::R16 | Self::Rg8 => 2,
            Self::Rg16 | Self::Rgba8 => 4,
            Self::Rgba16 => 8,
        }
    }
}

/// How samples are spread over planes; matches the shader's `layout_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layout {
    /// Three planes: Y, Cb, Cr (or G, B, R).
    ThreePlane = 0,
    /// Luma plane plus one interleaved chroma plane (NV12, P010, ...).
    TwoPlane = 1,
    /// A single RGBA plane.
    Packed = 2,
    /// Luma only.
    OnePlane = 3,
}

impl Layout {
    fn plane_count(self) -> usize {
        match self {
            Self::ThreePlane => 3,
            Self::TwoPlane => 2,
            Self::Packed | Self::OnePlane => 1,
        }
    }
}

pub struct Plane<'a> {
    pub format: PlaneFormat,
    pub width: u32,
    pub height: u32,
    /// Row `y` starts at `data[y * stride]`.
    pub data: &'a [u8],
    pub stride: usize,
}

/// A decoded frame in system memory, described for the GPU.
pub struct CpuFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub layout: Layout,
    /// Chroma plane stores Cr before Cb (NV21).
    pub swap_chroma: bool,
    /// Maps normalized texel values to R'G'B'; see `nox_color::ycbcr`.
    pub matrix: Matrix3x4,
    /// Where chroma samples sit relative to center-sited, in luma pixels
    /// (e.g. `(0.5, 0.0)` for left-sited 4:2:0, the MPEG-2/H.264 default).
    pub chroma_offset: [f32; 2],
    pub planes: Vec<Plane<'a>>,
}

/// A plane of a frame that already lives in a Vulkan image on this device
/// (e.g. decoded by Vulkan Video). Viewed with `format`, which must be
/// compatible with the plane's format; the image needs
/// `VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT` if they differ.
pub struct VkPlane {
    pub image: vk::Image,
    /// `PLANE_n` for a plane of a multi-planar image, `COLOR` otherwise.
    pub aspect: vk::ImageAspectFlags,
    pub format: PlaneFormat,
    /// Extent of the plane in the image (may exceed the picture).
    pub width: u32,
    pub height: u32,
}

/// Synchronization state of an image shared with another user (FFmpeg).
/// The converter waits on `semaphore` reaching `value`, signals `value + 1`
/// and updates the other fields to the state it leaves the image in.
pub struct ImageSync {
    pub image: vk::Image,
    pub layout: vk::ImageLayout,
    pub access: vk::AccessFlags,
    pub queue_family: u32,
    /// Timeline semaphore.
    pub semaphore: vk::Semaphore,
    pub value: u64,
}

/// A frame whose planes are Vulkan images, converted without copies.
pub struct VkFrame {
    /// Picture size.
    pub width: u32,
    pub height: u32,
    pub layout: Layout,
    pub swap_chroma: bool,
    pub matrix: Matrix3x4,
    pub chroma_offset: [f32; 2],
    pub planes: Vec<VkPlane>,
    /// One entry per distinct image used by `planes`.
    pub images: Vec<ImageSync>,
}

/// What `Converter` converts.
pub enum Input<'f, 'a> {
    Cpu(&'f CpuFrame<'a>),
    Vulkan(&'f mut VkFrame),
}

impl<'f, 'a> From<&'f CpuFrame<'a>> for Input<'f, 'a> {
    fn from(frame: &'f CpuFrame<'a>) -> Self {
        Self::Cpu(frame)
    }
}

impl<'f> From<&'f mut VkFrame> for Input<'f, '_> {
    fn from(frame: &'f mut VkFrame) -> Self {
        Self::Vulkan(frame)
    }
}

/// Shader parameters common to both inputs.
struct Params {
    width: u32,
    height: u32,
    /// Size of plane 0 as stored (padded for decoded images).
    extent: (u32, u32),
    layout: Layout,
    swap_chroma: bool,
    matrix: Matrix3x4,
    chroma_offset: [f32; 2],
}

impl Input<'_, '_> {
    fn params(&self) -> Params {
        match self {
            Input::Cpu(f) => Params {
                width: f.width,
                height: f.height,
                extent: (f.width, f.height),
                layout: f.layout,
                swap_chroma: f.swap_chroma,
                matrix: f.matrix,
                chroma_offset: f.chroma_offset,
            },
            Input::Vulkan(f) => Params {
                width: f.width,
                height: f.height,
                extent: (f.planes[0].width, f.planes[0].height),
                layout: f.layout,
                swap_chroma: f.swap_chroma,
                matrix: f.matrix,
                chroma_offset: f.chroma_offset,
            },
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PushConstants {
    rows: Matrix3x4,
    chroma_offset: [f32; 2],
    uv_scale: [f32; 2],
    size: [i32; 2],
    layout_mode: i32,
    swap_chroma: i32,
}

struct GpuImage {
    image: vk::Image,
    view: vk::ImageView,
    allocation: Option<Allocation>,
}

struct GpuBuffer {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
}

/// Uploaded planes, keyed by their formats and sizes.
struct Inputs {
    shape: Vec<(PlaneFormat, u32, u32)>,
    planes: Vec<GpuImage>,
    staging: Vec<GpuBuffer>,
}

/// Output image plus host-visible copy for readback, keyed by size.
struct Readback {
    width: u32,
    height: u32,
    image: GpuImage,
    buffer: GpuBuffer,
}

/// Where the shader writes.
enum Target<'a> {
    Readback(&'a Readback),
    /// An exported image, handed to an external consumer afterwards.
    Export(vk::Image, vk::ImageView),
}

pub struct Converter {
    gpu: Arc<GpuDevice>,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    sampler: vk::Sampler,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    inputs: Option<Inputs>,
    readback: Option<Readback>,
    export: Option<ExportPool>,
}

impl Converter {
    pub fn new(gpu: Arc<GpuDevice>) -> Result<Self> {
        let device = &gpu.device;
        // SAFETY: plain object creation on a valid device; everything is
        // destroyed in Drop.
        unsafe {
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                plane_binding(1),
                plane_binding(2),
                plane_binding(3),
            ];
            let set_layout = device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )?;
            let push_range = vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(size_of::<PushConstants>() as u32);
            let pipeline_layout = device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(std::slice::from_ref(&set_layout))
                    .push_constant_ranges(std::slice::from_ref(&push_range)),
                None,
            )?;

            let code =
                ash::util::read_spv(&mut std::io::Cursor::new(SHADER)).expect("valid SPIR-V");
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
                        .layout(pipeline_layout)],
                    None,
                )
                .map_err(|(_, err)| err)?[0];
            device.destroy_shader_module(module, None);

            let pool_sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: 1,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: MAX_PLANES as u32,
                },
            ];
            let descriptor_pool = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )?;
            let descriptor_set = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(std::slice::from_ref(&set_layout)),
            )?[0];

            let sampler = device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
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
                gpu,
                set_layout,
                pipeline_layout,
                pipeline,
                descriptor_pool,
                descriptor_set,
                sampler,
                command_pool,
                command_buffer,
                fence,
                inputs: None,
                readback: None,
                export: None,
            })
        }
    }

    pub fn gpu(&self) -> &Arc<GpuDevice> {
        &self.gpu
    }

    /// Enables `convert_to_dmabuf`. `importable_modifiers` are the DRM
    /// modifiers the consumer can import for `FOURCC_ABGR8888`.
    pub fn enable_dmabuf(&mut self, importable_modifiers: &[u64]) -> Result<()> {
        self.export = Some(ExportPool::new(self.gpu.clone(), importable_modifiers)?);
        Ok(())
    }

    pub fn dmabuf_enabled(&self) -> bool {
        self.export.is_some()
    }

    /// Converts `frame` to RGBA8 at `width` × `height` (scaling bilinearly if
    /// that differs from the frame size) and reads the result back.
    pub fn convert<'f, 'a: 'f>(
        &mut self,
        input: impl Into<Input<'f, 'a>>,
        width: u32,
        height: u32,
    ) -> Result<Image> {
        let mut input = input.into();
        validate(&input, width, height)?;
        if self
            .readback
            .as_ref()
            .is_none_or(|r| (r.width, r.height) != (width, height))
        {
            self.destroy_readback();
            self.readback = Some(self.create_readback(width, height)?);
        }
        let readback = self.readback.take().expect("readback");
        let result = self.run(&mut input, width, height, Target::Readback(&readback));
        let image = result.map(|()| {
            let stride = width as usize * 4;
            let data = readback
                .buffer
                .allocation
                .as_ref()
                .and_then(|a| a.mapped_slice())
                .expect("mapped readback")[..stride * height as usize]
                .to_vec();
            Image {
                width,
                height,
                stride,
                data,
            }
        });
        self.readback = Some(readback);
        image
    }

    /// Like `convert`, but renders into an exported image and returns it as
    /// DMA-BUF, without copying to system memory.
    pub fn convert_to_dmabuf<'f, 'a: 'f>(
        &mut self,
        input: impl Into<Input<'f, 'a>>,
        width: u32,
        height: u32,
    ) -> Result<DmabufFrame> {
        let mut input = input.into();
        validate(&input, width, height)?;
        let pool = self
            .export
            .as_ref()
            .ok_or(Error::Unsupported("DMA-BUF export not enabled".into()))?;
        let image = pool.acquire(width, height)?;
        self.run(
            &mut input,
            width,
            height,
            Target::Export(image.image, image.view),
        )?;
        self.export.as_ref().expect("export pool").export(image)
    }

    /// Runs the shader on `input` into `target` and waits for completion.
    fn run(
        &mut self,
        input: &mut Input<'_, '_>,
        width: u32,
        height: u32,
        target: Target<'_>,
    ) -> Result<()> {
        let params = input.params();
        // Plane views: our upload images, or temporary views of external images.
        let (plane_views, temporary_views) = match input {
            Input::Cpu(frame) => {
                self.upload(frame)?;
                let views = self
                    .inputs
                    .as_ref()
                    .expect("inputs")
                    .planes
                    .iter()
                    .map(|p| p.view)
                    .collect();
                (views, Vec::new())
            }
            Input::Vulkan(frame) => {
                let views = self.plane_views(frame)?;
                (views.clone(), views)
            }
        };
        let result = self.record_and_submit(input, &params, &plane_views, width, height, &target);
        for view in temporary_views {
            // SAFETY: the submission has completed (or failed); views are unused.
            unsafe { self.gpu.device.destroy_image_view(view, None) };
        }
        result
    }

    /// Copies a CPU frame into the (cached) staging buffers.
    fn upload(&mut self, frame: &CpuFrame<'_>) -> Result<()> {
        let shape: Vec<_> = frame
            .planes
            .iter()
            .map(|p| (p.format, p.width, p.height))
            .collect();
        if self.inputs.as_ref().is_none_or(|i| i.shape != shape) {
            self.destroy_inputs();
            self.inputs = Some(self.create_inputs(shape)?);
        }
        let inputs = self.inputs.as_mut().expect("inputs");
        for (plane, staging) in frame.planes.iter().zip(&mut inputs.staging) {
            let dst = staging
                .allocation
                .as_mut()
                .and_then(|a| a.mapped_slice_mut())
                .expect("mapped staging");
            let row = plane.width as usize * plane.format.bytes_per_texel();
            for (y, dst_row) in dst
                .chunks_exact_mut(row)
                .take(plane.height as usize)
                .enumerate()
            {
                dst_row.copy_from_slice(&plane.data[y * plane.stride..][..row]);
            }
        }
        Ok(())
    }

    fn plane_views(&self, frame: &VkFrame) -> Result<Vec<vk::ImageView>> {
        let mut views = Vec::new();
        for plane in &frame.planes {
            let mut usage =
                vk::ImageViewUsageCreateInfo::default().usage(vk::ImageUsageFlags::SAMPLED);
            // SAFETY: valid image of this device; destroyed after use by `run`.
            let view = unsafe {
                self.gpu.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(plane.image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(plane.format.vk())
                        .subresource_range(
                            vk::ImageSubresourceRange::default()
                                .aspect_mask(plane.aspect)
                                .level_count(1)
                                .layer_count(1),
                        )
                        .push_next(&mut usage),
                    None,
                )
            };
            match view {
                Ok(view) => views.push(view),
                Err(err) => {
                    for view in views {
                        // SAFETY: just created, unused.
                        unsafe { self.gpu.device.destroy_image_view(view, None) };
                    }
                    return Err(err.into());
                }
            }
        }
        Ok(views)
    }

    fn record_and_submit(
        &self,
        input: &mut Input<'_, '_>,
        params: &Params,
        plane_views: &[vk::ImageView],
        width: u32,
        height: u32,
        target: &Target<'_>,
    ) -> Result<()> {
        let (out_image, out_view) = match target {
            Target::Readback(r) => (r.image.image, r.image.view),
            Target::Export(image, view) => (*image, *view),
        };
        self.write_descriptors(plane_views, out_view);

        let device = &self.gpu.device;
        let cmd = self.command_buffer;
        let own_family = self.gpu.queue_family;
        // SAFETY: all handles are valid; the previous submission has completed
        // (we wait on the fence at the end of every call).
        unsafe {
            device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // The output is fully overwritten, so its old contents (and, for
            // exported images, the consumer's last use) don't matter.
            let mut barriers = vec![
                image_barrier(
                    out_image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                )
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE),
            ];
            match &*input {
                Input::Cpu(frame) => {
                    let inputs = self.inputs.as_ref().expect("inputs");
                    let to_transfer: Vec<_> = inputs
                        .planes
                        .iter()
                        .map(|p| {
                            image_barrier(
                                p.image,
                                vk::ImageLayout::UNDEFINED,
                                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            )
                            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        })
                        .collect();
                    device.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&to_transfer),
                    );
                    for ((plane, image), staging) in
                        frame.planes.iter().zip(&inputs.planes).zip(&inputs.staging)
                    {
                        let region = vk::BufferImageCopy::default()
                            .image_subresource(color_layers())
                            .image_extent(vk::Extent3D {
                                width: plane.width,
                                height: plane.height,
                                depth: 1,
                            });
                        device.cmd_copy_buffer_to_image(
                            cmd,
                            staging.buffer,
                            image.image,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[region],
                        );
                    }
                    barriers.extend(inputs.planes.iter().map(|p| {
                        image_barrier(
                            p.image,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        )
                        .src_stage_mask(vk::PipelineStageFlags2::COPY)
                        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                    }));
                }
                Input::Vulkan(frame) => {
                    // Acquire from the decoder: whatever it did last, at its
                    // recorded layout (the semaphore wait orders execution).
                    barriers.extend(frame.images.iter().map(|sync| {
                        let (src_family, dst_family) =
                            queue_transfer(sync.queue_family, own_family);
                        image_barrier(
                            sync.image,
                            sync.layout,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        )
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .src_access_mask(vk::AccessFlags2::from_raw(u64::from(
                            sync.access.as_raw(),
                        )))
                        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                        .src_queue_family_index(src_family)
                        .dst_queue_family_index(dst_family)
                    }));
                }
            }
            device.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&barriers),
            );

            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[self.descriptor_set],
                &[],
            );
            let (extent_w, extent_h) = (params.extent.0 as f32, params.extent.1 as f32);
            let push = PushConstants {
                rows: params.matrix,
                chroma_offset: [
                    params.chroma_offset[0] / extent_w,
                    params.chroma_offset[1] / extent_h,
                ],
                uv_scale: [
                    params.width as f32 / extent_w,
                    params.height as f32 / extent_h,
                ],
                size: [width as i32, height as i32],
                layout_mode: params.layout as i32,
                swap_chroma: params.swap_chroma.into(),
            };
            device.cmd_push_constants(
                cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                std::slice::from_raw_parts((&raw const push).cast(), size_of::<PushConstants>()),
            );
            device.cmd_dispatch(
                cmd,
                width.div_ceil(WORKGROUP_SIZE),
                height.div_ceil(WORKGROUP_SIZE),
                1,
            );

            match target {
                Target::Readback(readback) => record_readback(device, cmd, readback, width, height),
                Target::Export(image, _) => {
                    // Release to the external consumer.
                    let release =
                        image_barrier(*image, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL)
                            .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                            .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                            .src_queue_family_index(own_family)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL);
                    device.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default()
                            .image_memory_barriers(std::slice::from_ref(&release)),
                    );
                }
            }
            device.end_command_buffer(cmd)?;

            // External images: wait for the decoder, then signal the next value.
            let (waits, signals): (Vec<_>, Vec<_>) = match &*input {
                Input::Cpu(_) => (Vec::new(), Vec::new()),
                Input::Vulkan(frame) => frame
                    .images
                    .iter()
                    .map(|sync| {
                        (
                            vk::SemaphoreSubmitInfo::default()
                                .semaphore(sync.semaphore)
                                .value(sync.value)
                                .stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER),
                            vk::SemaphoreSubmitInfo::default()
                                .semaphore(sync.semaphore)
                                .value(sync.value + 1)
                                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
                        )
                    })
                    .unzip(),
            };
            let cmd_info = vk::CommandBufferSubmitInfo::default().command_buffer(cmd);
            self.gpu.submit(
                &vk::SubmitInfo2::default()
                    .command_buffer_infos(std::slice::from_ref(&cmd_info))
                    .wait_semaphore_infos(&waits)
                    .signal_semaphore_infos(&signals),
                self.fence,
            )?;
            // Submitted: the images will be in our state; record it for the
            // next user.
            if let Input::Vulkan(frame) = input {
                for sync in &mut frame.images {
                    let (_, dst_family) = queue_transfer(sync.queue_family, own_family);
                    sync.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                    sync.access = vk::AccessFlags::SHADER_READ;
                    sync.queue_family = dst_family;
                    sync.value += 1;
                }
            }
            device.wait_for_fences(&[self.fence], true, u64::MAX)?;
            device.reset_fences(&[self.fence])?;
        }
        Ok(())
    }

    fn create_inputs(&self, shape: Vec<(PlaneFormat, u32, u32)>) -> Result<Inputs> {
        let mut planes = Vec::new();
        let mut staging = Vec::new();
        for &(format, width, height) in &shape {
            planes.push(self.create_image(
                format.vk(),
                width,
                height,
                vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            )?);
            let size = width as u64 * height as u64 * format.bytes_per_texel() as u64;
            staging.push(self.create_buffer(
                size,
                vk::BufferUsageFlags::TRANSFER_SRC,
                MemoryLocation::CpuToGpu,
            )?);
        }
        tracing::debug!("converter inputs: {shape:?}");
        Ok(Inputs {
            shape,
            planes,
            staging,
        })
    }

    fn create_readback(&self, width: u32, height: u32) -> Result<Readback> {
        Ok(Readback {
            width,
            height,
            image: self.create_image(
                vk::Format::R8G8B8A8_UNORM,
                width,
                height,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
            )?,
            buffer: self.create_buffer(
                width as u64 * height as u64 * 4,
                vk::BufferUsageFlags::TRANSFER_DST,
                MemoryLocation::GpuToCpu,
            )?,
        })
    }

    fn create_image(
        &self,
        format: vk::Format,
        width: u32,
        height: u32,
        usage: vk::ImageUsageFlags,
    ) -> Result<GpuImage> {
        let device = &self.gpu.device;
        // SAFETY: valid create infos; the image is bound to fresh memory.
        unsafe {
            let image = device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )?;
            let allocation = self.gpu.allocator().allocate(&AllocationCreateDesc {
                name: "converter image",
                requirements: device.get_image_memory_requirements(image),
                location: MemoryLocation::GpuOnly,
                linear: false,
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            })?;
            device.bind_image_memory(image, allocation.memory(), allocation.offset())?;
            let view = device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(color_range()),
                None,
            )?;
            Ok(GpuImage {
                image,
                view,
                allocation: Some(allocation),
            })
        }
    }

    fn create_buffer(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        location: MemoryLocation,
    ) -> Result<GpuBuffer> {
        let device = &self.gpu.device;
        // SAFETY: valid create info; the buffer is bound to fresh memory.
        unsafe {
            let buffer = device.create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(usage),
                None,
            )?;
            let allocation = self.gpu.allocator().allocate(&AllocationCreateDesc {
                name: "converter buffer",
                requirements: device.get_buffer_memory_requirements(buffer),
                location,
                linear: true,
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            })?;
            device.bind_buffer_memory(buffer, allocation.memory(), allocation.offset())?;
            Ok(GpuBuffer {
                buffer,
                allocation: Some(allocation),
            })
        }
    }

    fn write_descriptors(&self, plane_views: &[vk::ImageView], output: vk::ImageView) {
        let output = vk::DescriptorImageInfo::default()
            .image_view(output)
            .image_layout(vk::ImageLayout::GENERAL);
        // Unused plane bindings must still be valid; point them at plane 0.
        let planes: Vec<_> = (0..MAX_PLANES)
            .map(|i| {
                vk::DescriptorImageInfo::default()
                    .sampler(self.sampler)
                    .image_view(*plane_views.get(i).unwrap_or(&plane_views[0]))
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            })
            .collect();
        let mut writes = vec![
            vk::WriteDescriptorSet::default()
                .dst_set(self.descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(std::slice::from_ref(&output)),
        ];
        for (i, info) in planes.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.descriptor_set)
                    .dst_binding(i as u32 + 1)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(info)),
            );
        }
        // SAFETY: the set is not in use (no pending submission).
        unsafe { self.gpu.device.update_descriptor_sets(&writes, &[]) };
    }

    fn destroy_image(&self, mut image: GpuImage) {
        // SAFETY: the GPU is idle with respect to this image.
        unsafe {
            self.gpu.device.destroy_image_view(image.view, None);
            self.gpu.device.destroy_image(image.image, None);
        }
        if let Some(a) = image.allocation.take() {
            let _ = self.gpu.allocator().free(a);
        }
    }

    fn destroy_buffer(&self, mut buffer: GpuBuffer) {
        // SAFETY: the GPU is idle with respect to this buffer.
        unsafe { self.gpu.device.destroy_buffer(buffer.buffer, None) };
        if let Some(a) = buffer.allocation.take() {
            let _ = self.gpu.allocator().free(a);
        }
    }

    fn destroy_inputs(&mut self) {
        if let Some(inputs) = self.inputs.take() {
            inputs
                .planes
                .into_iter()
                .for_each(|i| self.destroy_image(i));
            inputs
                .staging
                .into_iter()
                .for_each(|b| self.destroy_buffer(b));
        }
    }

    fn destroy_readback(&mut self) {
        if let Some(readback) = self.readback.take() {
            self.destroy_image(readback.image);
            self.destroy_buffer(readback.buffer);
        }
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        self.destroy_inputs();
        self.destroy_readback();
        // Exported frames still held by consumers keep their images alive.
        self.export = None;
        let device = &self.gpu.device;
        // SAFETY: no submission is pending (every call waits for completion).
        unsafe {
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.command_pool, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

/// Copies the output image into the host-visible readback buffer.
///
/// # Safety
/// `cmd` must be recording; the output must be in GENERAL layout after the
/// compute dispatch.
unsafe fn record_readback(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    readback: &Readback,
    width: u32,
    height: u32,
) {
    let to_copy = image_barrier(
        readback.image.image,
        vk::ImageLayout::GENERAL,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
    )
    .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
    .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ);
    let to_host = vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::HOST)
        .dst_access_mask(vk::AccessFlags2::HOST_READ);
    // SAFETY: guaranteed by the caller.
    unsafe {
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(std::slice::from_ref(&to_copy)),
        );
        device.cmd_copy_image_to_buffer(
            cmd,
            readback.image.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.buffer.buffer,
            &[vk::BufferImageCopy::default()
                .image_subresource(color_layers())
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })],
        );
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().memory_barriers(std::slice::from_ref(&to_host)),
        );
    }
}

fn validate(input: &Input<'_, '_>, width: u32, height: u32) -> Result<()> {
    let params = input.params();
    if params.width == 0 || params.height == 0 || width == 0 || height == 0 {
        return Err(Error::InvalidFrame("empty frame or output"));
    }
    let plane_count = match input {
        Input::Cpu(frame) => frame.planes.len(),
        Input::Vulkan(frame) => frame.planes.len(),
    };
    if plane_count != params.layout.plane_count() {
        return Err(Error::InvalidFrame("plane count does not match layout"));
    }
    let Input::Cpu(frame) = input else {
        return Ok(());
    };
    for plane in &frame.planes {
        let row = plane.width as usize * plane.format.bytes_per_texel();
        let needed = plane.stride * (plane.height as usize).saturating_sub(1) + row;
        if plane.stride < row || plane.data.len() < needed {
            return Err(Error::InvalidFrame("plane data too small"));
        }
    }
    Ok(())
}

fn plane_binding(binding: u32) -> vk::DescriptorSetLayoutBinding<'static> {
    vk::DescriptorSetLayoutBinding::default()
        .binding(binding)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
}

pub(crate) fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

/// Queue family indices for acquiring an image last used on `from`.
/// Images FFmpeg shares across queue families report IGNORED.
fn queue_transfer(from: u32, own: u32) -> (u32, u32) {
    if from == vk::QUEUE_FAMILY_IGNORED || from == own {
        (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED)
    } else {
        (from, own)
    }
}

fn image_barrier(
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .image(image)
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .subresource_range(color_range())
}
