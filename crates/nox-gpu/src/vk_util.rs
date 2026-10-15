//! Vulkan helpers shared by the converter, compositor and scopes: compute
//! pipelines, descriptor writes, and one-shot submissions that wait for
//! completion.

use ash::vk;

use crate::converter::ImageSync;
use crate::{Error, GpuDevice, Result};

/// A compute pipeline with one descriptor set and push constants.
pub(crate) struct Pipeline {
    pub set_layout: vk::DescriptorSetLayout,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

impl Pipeline {
    /// # Safety
    /// No submission using the pipeline may be pending.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
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
        let pipeline = compute_pipeline(device, spirv, layout)?;
        Ok(Pipeline {
            set_layout,
            layout,
            pipeline,
        })
    }
}

/// A compute pipeline of `spirv` (entry point `main`) with `layout`, e.g.
/// a variant of a shader sharing another pipeline's layout.
///
/// # Safety
/// `device` and `layout` must be valid.
pub(crate) unsafe fn compute_pipeline(
    device: &ash::Device,
    spirv: &[u8],
    layout: vk::PipelineLayout,
) -> Result<vk::Pipeline> {
    let code = ash::util::read_spv(&mut std::io::Cursor::new(spirv)).expect("valid SPIR-V");
    // SAFETY: guaranteed by the caller.
    unsafe {
        let module = device
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(c"main");
        let pipeline = device.create_compute_pipelines(
            vk::PipelineCache::null(),
            &[vk::ComputePipelineCreateInfo::default()
                .stage(stage)
                .layout(layout)],
            None,
        );
        device.destroy_shader_module(module, None);
        Ok(pipeline.map_err(|(_, err)| err)?[0])
    }
}

/// # Safety
/// `cmd` must be recording with a pipeline using `layout` bound.
pub(crate) unsafe fn push<T: bytemuck::Pod>(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    layout: vk::PipelineLayout,
    value: &T,
) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        device.cmd_push_constants(
            cmd,
            layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            bytemuck::bytes_of(value),
        );
    }
}

/// One descriptor set of `layout` from `pool`.
///
/// # Safety
/// `device`, `pool` and `layout` must be valid.
pub(crate) unsafe fn alloc_set(
    device: &ash::Device,
    pool: vk::DescriptorPool,
    layout: vk::DescriptorSetLayout,
) -> Result<vk::DescriptorSet> {
    // SAFETY: guaranteed by the caller.
    let sets = unsafe {
        device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(std::slice::from_ref(&layout)),
        )?
    };
    Ok(sets[0])
}

/// Writes image `info` to `binding` of `set`.
pub(crate) fn image_write<'a>(
    set: vk::DescriptorSet,
    binding: u32,
    ty: vk::DescriptorType,
    info: &'a vk::DescriptorImageInfo,
) -> vk::WriteDescriptorSet<'a> {
    vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(binding)
        .descriptor_type(ty)
        .image_info(std::slice::from_ref(info))
}

/// Writes storage buffer `info` to `binding` of `set`.
pub(crate) fn buffer_write<'a>(
    set: vk::DescriptorSet,
    binding: u32,
    info: &'a vk::DescriptorBufferInfo,
) -> vk::WriteDescriptorSet<'a> {
    vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(binding)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .buffer_info(std::slice::from_ref(info))
}

/// A command buffer recorded, submitted and waited for again and again,
/// one submission at a time.
pub(crate) struct OneShot {
    pool: vk::CommandPool,
    pub cmd: vk::CommandBuffer,
    fence: vk::Fence,
}

impl OneShot {
    /// # Safety
    /// `device` must be valid.
    pub unsafe fn new(device: &ash::Device, queue_family: u32) -> Result<Self> {
        // SAFETY: guaranteed by the caller; destroyed in `destroy`.
        unsafe {
            let pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let cmd = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
            Ok(Self { pool, cmd, fence })
        }
    }

    /// Starts recording.
    ///
    /// # Safety
    /// The previous submission must have completed (`submit_and_wait`
    /// returned).
    pub unsafe fn begin(&self, device: &ash::Device) -> Result<()> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            device.begin_command_buffer(
                self.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
        }
        Ok(())
    }

    /// Ends recording, submits waiting on `waits` and signalling `signals`,
    /// and waits until the GPU has finished.
    ///
    /// # Safety
    /// The command buffer must be recording (`begin`) with valid commands.
    pub unsafe fn submit_and_wait(
        &self,
        gpu: &GpuDevice,
        waits: &[vk::SemaphoreSubmitInfo<'_>],
        signals: &[vk::SemaphoreSubmitInfo<'_>],
    ) -> Result<()> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            gpu.device.end_command_buffer(self.cmd)?;
            self.resubmit_and_wait(gpu, waits, signals)
        }
    }

    /// Starts recording commands to be submitted again and again
    /// (`end`, then `resubmit_and_wait` each time).
    ///
    /// # Safety
    /// No submission may be pending.
    pub unsafe fn begin_reusable(&self, device: &ash::Device) -> Result<()> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            device.begin_command_buffer(self.cmd, &vk::CommandBufferBeginInfo::default())?;
        }
        Ok(())
    }

    /// Ends recording.
    ///
    /// # Safety
    /// The command buffer must be recording valid commands.
    pub unsafe fn end(&self, device: &ash::Device) -> Result<()> {
        // SAFETY: guaranteed by the caller.
        unsafe { device.end_command_buffer(self.cmd)? };
        Ok(())
    }

    /// Submits the recorded commands as they are and waits until the GPU
    /// has finished.
    ///
    /// # Safety
    /// The command buffer must be recorded and ended, and nothing it uses
    /// may have been destroyed.
    pub unsafe fn resubmit_and_wait(
        &self,
        gpu: &GpuDevice,
        waits: &[vk::SemaphoreSubmitInfo<'_>],
        signals: &[vk::SemaphoreSubmitInfo<'_>],
    ) -> Result<()> {
        let device = &gpu.device;
        // SAFETY: guaranteed by the caller.
        unsafe {
            let cmd_info = vk::CommandBufferSubmitInfo::default().command_buffer(self.cmd);
            gpu.submit(
                &vk::SubmitInfo2::default()
                    .command_buffer_infos(std::slice::from_ref(&cmd_info))
                    .wait_semaphore_infos(waits)
                    .signal_semaphore_infos(signals),
                self.fence,
            )?;
            device.wait_for_fences(&[self.fence], true, u64::MAX)?;
            device.reset_fences(&[self.fence])?;
        }
        Ok(())
    }

    /// # Safety
    /// No submission may be pending.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.pool, None);
        }
    }
}

/// Timeline-semaphore operations for images shared with another user
/// (FFmpeg): wait for each image's current value at `wait_stage`, signal
/// the next one when done.
pub(crate) fn semaphore_infos(
    images: &[ImageSync],
    wait_stage: vk::PipelineStageFlags2,
) -> (
    Vec<vk::SemaphoreSubmitInfo<'static>>,
    Vec<vk::SemaphoreSubmitInfo<'static>>,
) {
    images
        .iter()
        .map(|sync| {
            (
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(sync.semaphore)
                    .value(sync.value)
                    .stage_mask(wait_stage),
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(sync.semaphore)
                    .value(sync.value + 1)
                    .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
            )
        })
        .unzip()
}

/// Device-local memory for one image or buffer (`dedicated`), exportable
/// as `handle_types`. `purpose` says what for, in the error when there's no
/// such memory.
///
/// # Safety
/// `requirements` must be those of the resource in `dedicated`.
pub(crate) unsafe fn alloc_exportable(
    gpu: &GpuDevice,
    requirements: vk::MemoryRequirements,
    handle_types: vk::ExternalMemoryHandleTypeFlags,
    mut dedicated: vk::MemoryDedicatedAllocateInfo<'_>,
    purpose: &str,
) -> Result<vk::DeviceMemory> {
    let memory_type = gpu
        .memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| Error::Unsupported(format!("no device-local memory {purpose}")))?;
    let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(handle_types);
    // SAFETY: guaranteed by the caller.
    let memory = unsafe {
        gpu.device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type)
                .push_next(&mut export)
                .push_next(&mut dedicated),
            None,
        )?
    };
    Ok(memory)
}
