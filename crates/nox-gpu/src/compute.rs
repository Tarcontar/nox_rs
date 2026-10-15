//! General-purpose compute on Nox's device, for other crates (nox-ai's
//! inference): storage buffers, compute kernels over them, and recordings
//! of dispatches that run again and again on Nox's queue (with FFmpeg's
//! queue lock, like everything else here).
//!
//! Every dispatch in a recording is followed by a compute-to-compute
//! memory barrier, so each sees the writes of those before it.

use ash::vk;
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme};
use std::sync::Arc;

use crate::vk_util::{OneShot, Pipeline, buffer_write, create_pipeline, image_write};
use crate::{Error, GpuDevice, Result};

/// Where a buffer's memory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryUse {
    /// On the GPU only: what kernels read and write.
    Device,
    /// Written by the CPU, read by the GPU.
    Upload,
    /// Written by the GPU, read by the CPU.
    Download,
}

/// A storage buffer on the device.
pub struct Buffer {
    gpu: Arc<GpuDevice>,
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    size: u64,
}

impl Buffer {
    /// A buffer of `size` bytes (at least 4).
    pub fn new(gpu: &Arc<GpuDevice>, size: u64, memory: MemoryUse) -> Result<Self> {
        let size = size.max(4);
        let location = match memory {
            MemoryUse::Device => MemoryLocation::GpuOnly,
            MemoryUse::Upload => MemoryLocation::CpuToGpu,
            MemoryUse::Download => MemoryLocation::GpuToCpu,
        };
        let device = &gpu.device;
        // SAFETY: valid create info; the buffer is bound to fresh memory and
        // destroyed in Drop.
        unsafe {
            let buffer = device.create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(
                    vk::BufferUsageFlags::STORAGE_BUFFER
                        | vk::BufferUsageFlags::TRANSFER_SRC
                        | vk::BufferUsageFlags::TRANSFER_DST,
                ),
                None,
            )?;
            let allocation = match gpu.allocator().allocate(&AllocationCreateDesc {
                name: "compute buffer",
                requirements: device.get_buffer_memory_requirements(buffer),
                location,
                linear: true,
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            }) {
                Ok(allocation) => allocation,
                Err(err) => {
                    device.destroy_buffer(buffer, None);
                    return Err(err.into());
                }
            };
            if let Err(err) =
                device.bind_buffer_memory(buffer, allocation.memory(), allocation.offset())
            {
                device.destroy_buffer(buffer, None);
                let _ = gpu.allocator().free(allocation);
                return Err(err.into());
            }
            Ok(Self {
                gpu: gpu.clone(),
                buffer,
                allocation: Some(allocation),
                size,
            })
        }
    }

    pub fn handle(&self) -> vk::Buffer {
        self.buffer
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Copies `data` to the start of an `Upload` buffer.
    pub fn write<T: bytemuck::Pod>(&mut self, data: &[T]) -> Result<()> {
        let bytes = bytemuck::cast_slice(data);
        let mapped = self
            .allocation
            .as_mut()
            .and_then(|a| a.mapped_slice_mut())
            .ok_or(Error::Unsupported(
                "writing a buffer the CPU can't see".into(),
            ))?;
        mapped[..bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    /// The contents of a `Download` buffer, after the GPU wrote them.
    pub fn read<T: bytemuck::Pod>(&self) -> Result<Vec<T>> {
        let mapped = self
            .allocation
            .as_ref()
            .and_then(|a| a.mapped_slice())
            .ok_or(Error::Unsupported(
                "reading a buffer the CPU can't see".into(),
            ))?;
        let mut out = vec![T::zeroed(); mapped.len() / size_of::<T>()];
        let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut out);
        let len = bytes.len();
        bytes.copy_from_slice(&mapped[..len]);
        Ok(out)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: the owner makes sure no submission using the buffer is
        // pending (recordings wait for completion).
        unsafe { self.gpu.device.destroy_buffer(self.buffer, None) };
        if let Some(allocation) = self.allocation.take() {
            let _ = self.gpu.allocator().free(allocation);
        }
    }
}

/// What a kernel binds at each binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    /// A storage buffer.
    Buffer,
    /// A storage image in GENERAL layout (e.g. a compositor layer).
    Image,
}

impl BindingKind {
    fn descriptor_type(self) -> vk::DescriptorType {
        match self {
            Self::Buffer => vk::DescriptorType::STORAGE_BUFFER,
            Self::Image => vk::DescriptorType::STORAGE_IMAGE,
        }
    }
}

/// What a dispatch binds, in binding order.
#[derive(Clone, Copy)]
pub enum Binding<'a> {
    Buffer(BufferSlice<'a>),
    /// A storage image view; the image must be in GENERAL layout, its
    /// earlier writes visible to compute shaders.
    Image(vk::ImageView),
}

impl<'a> From<BufferSlice<'a>> for Binding<'a> {
    fn from(slice: BufferSlice<'a>) -> Self {
        Self::Buffer(slice)
    }
}

impl<'a> From<&'a Buffer> for Binding<'a> {
    fn from(buffer: &'a Buffer) -> Self {
        Self::Buffer(buffer.into())
    }
}

/// A compute shader (entry point `main`, any workgroup size) over
/// storage buffers and images at bindings 0.., with push constants.
pub struct Kernel {
    gpu: Arc<GpuDevice>,
    pipeline: Pipeline,
    bindings: Vec<BindingKind>,
}

impl Kernel {
    pub fn new(
        gpu: &Arc<GpuDevice>,
        spirv: &[u8],
        bindings: &[BindingKind],
        push_size: usize,
    ) -> Result<Self> {
        let layout: Vec<_> = bindings
            .iter()
            .enumerate()
            .map(|(binding, kind)| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding as u32)
                    .descriptor_type(kind.descriptor_type())
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
            })
            .collect();
        // SAFETY: a valid device; destroyed in Drop.
        let pipeline = unsafe { create_pipeline(&gpu.device, spirv, &layout, push_size)? };
        Ok(Self {
            gpu: gpu.clone(),
            pipeline,
            bindings: bindings.to_vec(),
        })
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        // SAFETY: recordings using the kernel wait for completion and must
        // be dropped first (they borrow nothing, so the owner orders it).
        unsafe { self.pipeline.destroy(&self.gpu.device) };
    }
}

/// A part of a buffer bound to a kernel.
#[derive(Clone, Copy)]
pub struct BufferSlice<'a> {
    pub buffer: &'a Buffer,
    /// In bytes; a multiple of the device's storage buffer alignment.
    pub offset: u64,
    /// In bytes; `vk::WHOLE_SIZE` for the rest of the buffer.
    pub range: u64,
}

impl<'a> From<&'a Buffer> for BufferSlice<'a> {
    fn from(buffer: &'a Buffer) -> Self {
        Self {
            buffer,
            offset: 0,
            range: vk::WHOLE_SIZE,
        }
    }
}

/// Dispatches recorded once and run many times. The buffers and kernels
/// it uses must outlive it.
pub struct Recording {
    gpu: Arc<GpuDevice>,
    one_shot: OneShot,
    pool: vk::DescriptorPool,
    sets: u32,
    max_sets: u32,
    recording: bool,
}

impl Recording {
    /// Room for `max_dispatches` dispatches of up to `max_bindings`
    /// bindings each. Commands see the writes of earlier submissions.
    pub fn new(gpu: &Arc<GpuDevice>, max_dispatches: u32, max_bindings: u32) -> Result<Self> {
        let device = &gpu.device;
        // SAFETY: a valid device; destroyed in Drop.
        unsafe {
            let one_shot = OneShot::new(device, gpu.queue_family)?;
            let pool = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(max_dispatches.max(1))
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::STORAGE_BUFFER,
                            descriptor_count: (max_dispatches * max_bindings).max(1),
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::STORAGE_IMAGE,
                            descriptor_count: (max_dispatches * max_bindings).max(1),
                        },
                    ]),
                None,
            )?;
            one_shot.begin_reusable(device)?;
            let recording = Self {
                gpu: gpu.clone(),
                one_shot,
                pool,
                sets: 0,
                max_sets: max_dispatches.max(1),
                recording: true,
            };
            recording.barrier();
            Ok(recording)
        }
    }

    /// Records `kernel` over `bindings` (in binding order) with push
    /// constants `push_constants` (e.g. `bytemuck::bytes_of` of the
    /// shader's block), in `groups` workgroups, then a barrier.
    pub fn dispatch(
        &mut self,
        kernel: &Kernel,
        bindings: &[Binding<'_>],
        push_constants: &[u8],
        groups: [u32; 3],
    ) -> Result<()> {
        assert!(self.recording, "recording already finished");
        assert_eq!(
            bindings.len(),
            kernel.bindings.len(),
            "bindings for the kernel"
        );
        if self.sets >= self.max_sets {
            return Err(Error::Unsupported(
                "more dispatches than the recording has room for".into(),
            ));
        }
        let device = &self.gpu.device;
        // SAFETY: valid handles; the set is new and only used here.
        unsafe {
            let set = crate::vk_util::alloc_set(device, self.pool, kernel.pipeline.set_layout)?;
            self.sets += 1;
            let buffer_infos: Vec<_> = bindings
                .iter()
                .map(|b| match b {
                    Binding::Buffer(b) => vk::DescriptorBufferInfo::default()
                        .buffer(b.buffer.buffer)
                        .offset(b.offset)
                        .range(b.range),
                    Binding::Image(_) => vk::DescriptorBufferInfo::default(),
                })
                .collect();
            let image_infos: Vec<_> = bindings
                .iter()
                .map(|b| match b {
                    Binding::Image(view) => vk::DescriptorImageInfo::default()
                        .image_view(*view)
                        .image_layout(vk::ImageLayout::GENERAL),
                    Binding::Buffer(_) => vk::DescriptorImageInfo::default(),
                })
                .collect();
            let writes: Vec<_> = bindings
                .iter()
                .zip(&kernel.bindings)
                .enumerate()
                .map(|(i, (binding, kind))| match (binding, kind) {
                    (Binding::Buffer(_), BindingKind::Buffer) => {
                        buffer_write(set, i as u32, &buffer_infos[i])
                    }
                    (Binding::Image(_), BindingKind::Image) => image_write(
                        set,
                        i as u32,
                        vk::DescriptorType::STORAGE_IMAGE,
                        &image_infos[i],
                    ),
                    _ => panic!("binding {i} is a {kind:?} in the kernel"),
                })
                .collect();
            device.update_descriptor_sets(&writes, &[]);
            let cmd = self.one_shot.cmd;
            device.cmd_bind_pipeline(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                kernel.pipeline.pipeline,
            );
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                kernel.pipeline.layout,
                0,
                &[set],
                &[],
            );
            if !push_constants.is_empty() {
                device.cmd_push_constants(
                    cmd,
                    kernel.pipeline.layout,
                    vk::ShaderStageFlags::COMPUTE,
                    0,
                    push_constants,
                );
            }
            device.cmd_dispatch(cmd, groups[0], groups[1], groups[2]);
            self.barrier();
        }
        Ok(())
    }

    /// Records copying `bytes` from `src` to `dst` (offsets in bytes), then
    /// a barrier.
    pub fn copy(&mut self, src: BufferSlice<'_>, dst: BufferSlice<'_>, bytes: u64) {
        assert!(self.recording, "recording already finished");
        // SAFETY: valid buffers with TRANSFER usage.
        unsafe {
            self.gpu.device.cmd_copy_buffer(
                self.one_shot.cmd,
                src.buffer.buffer,
                dst.buffer.buffer,
                &[vk::BufferCopy {
                    src_offset: src.offset,
                    dst_offset: dst.offset,
                    size: bytes,
                }],
            );
            self.barrier();
        }
    }

    /// Makes compute and transfer writes visible to later compute and
    /// transfer commands (and to the host after completion).
    unsafe fn barrier(&self) {
        let stages = vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::COPY;
        let barrier = vk::MemoryBarrier2::default()
            .src_stage_mask(stages)
            .src_access_mask(
                vk::AccessFlags2::SHADER_STORAGE_WRITE | vk::AccessFlags2::TRANSFER_WRITE,
            )
            .dst_stage_mask(stages | vk::PipelineStageFlags2::HOST)
            .dst_access_mask(
                vk::AccessFlags2::SHADER_STORAGE_READ
                    | vk::AccessFlags2::SHADER_STORAGE_WRITE
                    | vk::AccessFlags2::TRANSFER_READ
                    | vk::AccessFlags2::TRANSFER_WRITE
                    | vk::AccessFlags2::HOST_READ,
            );
        // SAFETY: recording.
        unsafe {
            self.gpu.device.cmd_pipeline_barrier2(
                self.one_shot.cmd,
                &vk::DependencyInfo::default().memory_barriers(std::slice::from_ref(&barrier)),
            );
        }
    }

    /// Runs the recorded dispatches and waits until they are done. The
    /// first run ends the recording.
    pub fn run(&mut self) -> Result<()> {
        // SAFETY: the command buffer holds valid commands over buffers and
        // kernels its owner keeps alive; earlier runs have completed.
        unsafe {
            if self.recording {
                self.one_shot.end(&self.gpu.device)?;
                self.recording = false;
            }
            self.one_shot.resubmit_and_wait(&self.gpu, &[], &[])
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let device = &self.gpu.device;
        // SAFETY: every run waited for completion.
        unsafe {
            if self.recording {
                let _ = self.one_shot.end(device);
            }
            self.one_shot.destroy(device);
            device.destroy_descriptor_pool(self.pool, None);
        }
    }
}
