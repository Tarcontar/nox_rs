//! Video scopes: waveform, RGB parade, vectorscope and histogram of the
//! composited picture, measured after the output transform (what an export
//! contains).
//!
//! After Aras Pranckevičius's GPU scopes for Blender ("This many points is
//! surely out of scope", 2025): drawing a point per pixel with blending
//! stalls where points pile up (most pixels sit near the vectorscope's
//! centre), so a compute pass counts pixels into bins with atomics
//! (`scope_count.comp`) and a second pass turns the counts into images
//! (`scope_resolve.comp`), brightness growing with the logarithm of the
//! count. Both run in the compositor's submission, on the accumulator it
//! just blended; the small images are read back for the GUI, which draws
//! the graticules.

use ash::vk;
use gpu_allocator::MemoryLocation;

use crate::compositor::{Pipeline, create_pipeline, push};
use crate::converter::{GpuBuffer, GpuImage, color_layers, image_barrier};
use crate::scope_layout::*;
use crate::{Converter, Image, Result};

const COUNT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scope_count.comp.spv"));
const RESOLVE_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scope_resolve.comp.spv"));
const WORKGROUP_SIZE: u32 = 16;

/// The scope images of one picture: RGBA8 with straight alpha, to be drawn
/// over a dark background with a graticule.
#[derive(Debug, Clone)]
pub struct ScopeImages {
    /// Luma by column, 100% at the top (`WAVEFORM_WIDTH` × `LEVELS`).
    pub waveform: Image,
    /// R, G and B waveforms side by side (`PARADE_WIDTH` × `LEVELS`).
    pub parade: Image,
    /// Cb to the right, Cr up, ±0.5 across the image (`VECTOR_SIZE`²).
    pub vectorscope: Image,
    /// R, G and B level counts as overlaid bars (`HISTOGRAM_BINS` ×
    /// `HISTOGRAM_HEIGHT`).
    pub histogram: Image,
}

impl ScopeImages {
    /// Approximate memory use.
    pub fn bytes(&self) -> usize {
        [
            &self.waveform,
            &self.parade,
            &self.vectorscope,
            &self.histogram,
        ]
        .iter()
        .map(|i| i.data.len())
        .sum()
    }
}

/// Scope modes of `scope_resolve.comp`, in the order of `Scopes::images`.
const MODES: [(u32, u32); 4] = [
    (WAVEFORM_WIDTH, LEVELS),
    (PARADE_WIDTH, LEVELS),
    (VECTOR_SIZE, VECTOR_SIZE),
    (HISTOGRAM_BINS, HISTOGRAM_HEIGHT),
];

#[repr(C)]
#[derive(Clone, Copy)]
struct CountParams {
    size: [i32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResolveParams {
    mode: i32,
    full: f32,
}

pub(crate) struct Scopes {
    count: Pipeline,
    resolve: Pipeline,
    descriptor_pool: vk::DescriptorPool,
    count_set: vk::DescriptorSet,
    resolve_sets: Vec<vk::DescriptorSet>,
    counts: GpuBuffer,
    images: Vec<GpuImage>,
    readback: GpuBuffer,
}

impl Scopes {
    pub fn new(converter: &Converter) -> Result<Self> {
        let device = &converter.gpu().device;
        let binding = |binding, ty| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(ty)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let bindings = [
            binding(0, vk::DescriptorType::STORAGE_IMAGE),
            binding(1, vk::DescriptorType::STORAGE_BUFFER),
        ];
        // SAFETY: plain object creation on a valid device; everything is
        // destroyed in `destroy`.
        unsafe {
            let count = create_pipeline(device, COUNT_SHADER, &bindings, size_of::<CountParams>())?;
            let resolve = create_pipeline(
                device,
                RESOLVE_SHADER,
                &bindings,
                size_of::<ResolveParams>(),
            )?;
            let sets = 1 + MODES.len() as u32;
            let pool_sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: sets,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_BUFFER,
                    descriptor_count: sets,
                },
            ];
            let descriptor_pool = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(sets)
                    .pool_sizes(&pool_sizes),
                None,
            )?;
            let count_set = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(std::slice::from_ref(&count.set_layout)),
            )?[0];
            let layouts = vec![resolve.set_layout; MODES.len()];
            let resolve_sets = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&layouts),
            )?;
            let counts = converter.create_buffer(
                u64::from(COUNT_LEN) * 4,
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                MemoryLocation::GpuOnly,
            )?;
            let images = MODES
                .iter()
                .map(|&(width, height)| {
                    converter.create_image(
                        vk::Format::R8G8B8A8_UNORM,
                        width,
                        height,
                        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            let readback_size: u64 = MODES.iter().map(|&(w, h)| u64::from(w * h * 4)).sum();
            let readback = converter.create_buffer(
                readback_size,
                vk::BufferUsageFlags::TRANSFER_DST,
                MemoryLocation::GpuToCpu,
            )?;

            // The counts and images never change; only the count pass's
            // input (the accumulator) does, see `record`.
            let counts_info = vk::DescriptorBufferInfo::default()
                .buffer(counts.buffer)
                .range(vk::WHOLE_SIZE);
            let image_infos: Vec<_> = images
                .iter()
                .map(|image| {
                    vk::DescriptorImageInfo::default()
                        .image_view(image.view)
                        .image_layout(vk::ImageLayout::GENERAL)
                })
                .collect();
            let mut writes = vec![
                vk::WriteDescriptorSet::default()
                    .dst_set(count_set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&counts_info)),
            ];
            for (set, info) in resolve_sets.iter().zip(&image_infos) {
                writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(*set)
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(std::slice::from_ref(info)),
                );
                writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(*set)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(std::slice::from_ref(&counts_info)),
                );
            }
            device.update_descriptor_sets(&writes, &[]);

            Ok(Self {
                count,
                resolve,
                descriptor_pool,
                count_set,
                resolve_sets,
                counts,
                images,
                readback,
            })
        }
    }

    /// Records the scopes of `accum` (`width` × `height`, GENERAL layout,
    /// its writes visible to compute shaders) and the copy to the readback
    /// buffer; `read` returns them once the submission has completed.
    ///
    /// # Safety
    /// `cmd` must be recording, and no submission using these resources
    /// may be pending.
    pub unsafe fn record(
        &self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        accum: vk::ImageView,
        width: u32,
        height: u32,
    ) {
        let accum_info = vk::DescriptorImageInfo::default()
            .image_view(accum)
            .image_layout(vk::ImageLayout::GENERAL);
        let compute_rw =
            vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE;
        // SAFETY: guaranteed by the caller; the count set isn't bound in
        // any pending command buffer.
        unsafe {
            device.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(self.count_set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .image_info(std::slice::from_ref(&accum_info))],
                &[],
            );

            // Zero the counts; the images are written whole, their old
            // contents don't matter.
            device.cmd_fill_buffer(cmd, self.counts.buffer, 0, vk::WHOLE_SIZE, 0);
            let cleared = vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(compute_rw);
            let to_general: Vec<_> = self
                .images
                .iter()
                .map(|image| {
                    image_barrier(
                        image.image,
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::GENERAL,
                    )
                    .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                })
                .collect();
            device.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default()
                    .memory_barriers(std::slice::from_ref(&cleared))
                    .image_memory_barriers(&to_general),
            );

            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.count.pipeline);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.count.layout,
                0,
                &[self.count_set],
                &[],
            );
            push(
                device,
                cmd,
                self.count.layout,
                &CountParams {
                    size: [width as i32, height as i32],
                },
            );
            device.cmd_dispatch(
                cmd,
                width.div_ceil(WORKGROUP_SIZE),
                height.div_ceil(WORKGROUP_SIZE),
                1,
            );
            let counted = vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(compute_rw)
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_READ);
            device.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().memory_barriers(std::slice::from_ref(&counted)),
            );

            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.resolve.pipeline);
            for (mode, (&(w, h), set)) in MODES.iter().zip(&self.resolve_sets).enumerate() {
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    self.resolve.layout,
                    0,
                    &[*set],
                    &[],
                );
                push(
                    device,
                    cmd,
                    self.resolve.layout,
                    &ResolveParams {
                        mode: mode as i32,
                        full: full_brightness(mode, width, height),
                    },
                );
                device.cmd_dispatch(
                    cmd,
                    w.div_ceil(WORKGROUP_SIZE),
                    h.div_ceil(WORKGROUP_SIZE),
                    1,
                );
            }

            let to_copy: Vec<_> = self
                .images
                .iter()
                .map(|image| {
                    image_barrier(
                        image.image,
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    )
                    .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                })
                .collect();
            device.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&to_copy),
            );
            let mut offset = 0;
            for (image, &(w, h)) in self.images.iter().zip(&MODES) {
                device.cmd_copy_image_to_buffer(
                    cmd,
                    image.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    self.readback.buffer,
                    &[vk::BufferImageCopy::default()
                        .buffer_offset(offset)
                        .image_subresource(color_layers())
                        .image_extent(vk::Extent3D {
                            width: w,
                            height: h,
                            depth: 1,
                        })],
                );
                offset += u64::from(w * h * 4);
            }
            let to_host = vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ);
            device.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().memory_barriers(std::slice::from_ref(&to_host)),
            );
        }
    }

    /// The images recorded by `record`, after its submission completed.
    pub fn read(&self) -> ScopeImages {
        let data = self
            .readback
            .allocation
            .as_ref()
            .and_then(|a| a.mapped_slice())
            .expect("mapped scope readback");
        let mut offset = 0;
        let mut images = MODES.iter().map(|&(width, height)| {
            let len = (width * height * 4) as usize;
            let image = Image {
                width,
                height,
                stride: width as usize * 4,
                data: data[offset..offset + len].to_vec(),
            };
            offset += len;
            image
        });
        let mut next = || images.next().expect("four scopes");
        ScopeImages {
            waveform: next(),
            parade: next(),
            vectorscope: next(),
            histogram: next(),
        }
    }

    pub fn destroy(self, converter: &Converter) {
        let device = &converter.gpu().device;
        // SAFETY: no submission is pending (every call waits for completion).
        unsafe {
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            for pipeline in [&self.count, &self.resolve] {
                device.destroy_pipeline(pipeline.pipeline, None);
                device.destroy_pipeline_layout(pipeline.layout, None);
                device.destroy_descriptor_set_layout(pipeline.set_layout, None);
            }
        }
        for image in self.images {
            converter.destroy_image(image);
        }
        converter.destroy_buffer(self.counts);
        converter.destroy_buffer(self.readback);
    }
}

/// The count that `scope_resolve.comp` shows at full brightness: a level
/// bin holding about a sixteenth of its column's pixels (waveform,
/// parade), or a small share of the picture (vectorscope).
fn full_brightness(mode: usize, width: u32, height: u32) -> f32 {
    let (width, height) = (width as f32, height as f32);
    let per_column = |columns: u32| height * (width / columns as f32).max(1.0);
    match mode {
        0 => (per_column(WAVEFORM_WIDTH) / 16.0).max(1.0),
        1 => (per_column(PARADE_WIDTH / 3) / 16.0).max(1.0),
        2 => (width * height / 1024.0).max(1.0),
        _ => 1.0,
    }
}
