//! CUDA on the GPU Nox renders with, both ways, through Vulkan buffers
//! exported (opaque file descriptors or NT handles) and imported into
//! FFmpeg's CUDA context as device pointers (`Slot`):
//!
//! - encoding (NVENC): the compositor writes frames into the buffers, which
//!   go to the encoder as CUDA frames. NVENC registers each pointer once,
//!   so the buffers are a stable pool that grows to however many frames the
//!   encoder holds at once (`CudaSink`).
//! - decoding (NVDEC): each decoded CUDA frame is copied on the GPU into a
//!   buffer, which the converter reads (`DecodeBuffers`, one copy).
//!
//! libcuda is loaded at run time, so Nox runs without it.

use ffmpeg::{ffi, format, frame};
use libloading::Library;
use nox_gpu::{
    BufferTarget, ExportedMemory, GpuDevice, GpuTarget, LinearFormat, LinearLayout, MemoryHandle,
    SharedBuffer,
};
use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock};

use super::{GpuSink, Render, ycbcr_matrix};
use crate::{Error, Result};

/// Frames the encoder may hold at once; NVENC registers at most 64
/// resources.
const MAX_FRAMES: usize = 32;
/// Row alignment, as FFmpeg's CUDA frames use.
const PITCH_ALIGNMENT: u32 = 256;

type CuResult = i32;
type CuDevice = i32;
type CuContext = *mut c_void;
type CuDevicePtr = u64;
type CuExternalMemory = *mut c_void;
type CuStream = *mut c_void;

/// `CU_MEMORYTYPE_DEVICE`.
const CU_MEMORYTYPE_DEVICE: u32 = 2;

/// `CUDA_MEMCPY2D`.
#[repr(C)]
#[derive(Default)]
struct Memcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: usize,
    src_device: CuDevicePtr,
    src_array: usize,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: usize,
    dst_device: CuDevicePtr,
    dst_array: usize,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

#[cfg(unix)]
const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: u32 = 1;
#[cfg(windows)]
const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32: u32 = 2;
/// The memory is a dedicated allocation (as `SharedBuffer`s are).
const CUDA_EXTERNAL_MEMORY_DEDICATED: u32 = 1;

#[repr(C)]
union ExternalMemoryHandle {
    fd: i32,
    win32: [*const c_void; 2],
}

/// `CUDA_EXTERNAL_MEMORY_HANDLE_DESC`.
#[repr(C)]
struct ExternalMemoryHandleDesc {
    kind: u32,
    handle: ExternalMemoryHandle,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}

/// `CUDA_EXTERNAL_MEMORY_BUFFER_DESC`.
#[repr(C)]
struct ExternalMemoryBufferDesc {
    offset: u64,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}

/// The CUDA driver library.
const LIBRARY: &str = if cfg!(windows) {
    "nvcuda.dll"
} else {
    "libcuda.so.1"
};

/// The CUDA driver API functions Nox uses.
struct Cuda {
    init: unsafe extern "C" fn(u32) -> CuResult,
    device_get_count: unsafe extern "C" fn(*mut i32) -> CuResult,
    device_get: unsafe extern "C" fn(*mut CuDevice, i32) -> CuResult,
    device_get_uuid: unsafe extern "C" fn(*mut [u8; 16], CuDevice) -> CuResult,
    ctx_push: unsafe extern "C" fn(CuContext) -> CuResult,
    ctx_pop: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    import_external_memory:
        unsafe extern "C" fn(*mut CuExternalMemory, *const ExternalMemoryHandleDesc) -> CuResult,
    external_memory_get_mapped_buffer: unsafe extern "C" fn(
        *mut CuDevicePtr,
        CuExternalMemory,
        *const ExternalMemoryBufferDesc,
    ) -> CuResult,
    mem_free: unsafe extern "C" fn(CuDevicePtr) -> CuResult,
    memcpy_2d_async: unsafe extern "C" fn(*const Memcpy2D, CuStream) -> CuResult,
    stream_synchronize: unsafe extern "C" fn(CuStream) -> CuResult,
    destroy_external_memory: unsafe extern "C" fn(CuExternalMemory) -> CuResult,
    _library: Library,
}

impl Cuda {
    /// The driver, loaded once.
    fn get() -> Result<&'static Cuda> {
        static CUDA: OnceLock<std::result::Result<Cuda, String>> = OnceLock::new();
        CUDA.get_or_init(Self::load)
            .as_ref()
            .map_err(|err| Error::Encode(err.clone()))
    }

    fn load() -> std::result::Result<Cuda, String> {
        // SAFETY: libcuda's initializers have no preconditions; the symbols
        // have the signatures of the CUDA driver API (cuda.h).
        unsafe {
            let library = Library::new(LIBRARY).map_err(|err| format!("no CUDA driver ({err})"))?;
            macro_rules! symbol {
                ($name:literal) => {
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|err| format!("CUDA driver lacks {}: {err}", $name))?
                };
            }
            let cuda = Cuda {
                init: symbol!("cuInit"),
                device_get_count: symbol!("cuDeviceGetCount"),
                device_get: symbol!("cuDeviceGet"),
                device_get_uuid: symbol!("cuDeviceGetUuid"),
                ctx_push: symbol!("cuCtxPushCurrent_v2"),
                ctx_pop: symbol!("cuCtxPopCurrent_v2"),
                import_external_memory: symbol!("cuImportExternalMemory"),
                external_memory_get_mapped_buffer: symbol!("cuExternalMemoryGetMappedBuffer"),
                mem_free: symbol!("cuMemFree_v2"),
                memcpy_2d_async: symbol!("cuMemcpy2DAsync_v2"),
                stream_synchronize: symbol!("cuStreamSynchronize"),
                destroy_external_memory: symbol!("cuDestroyExternalMemory"),
                _library: library,
            };
            check((cuda.init)(0), "cuInit").map_err(|err| err.to_string())?;
            Ok(cuda)
        }
    }

    /// The CUDA ordinal of the GPU with this UUID.
    fn device_with_uuid(&self, uuid: [u8; 16]) -> Result<i32> {
        let mut count = 0;
        // SAFETY: valid out-pointers.
        unsafe {
            check((self.device_get_count)(&mut count), "cuDeviceGetCount")?;
            for ordinal in 0..count {
                let mut device = 0;
                let mut id = [0u8; 16];
                check((self.device_get)(&mut device, ordinal), "cuDeviceGet")?;
                check((self.device_get_uuid)(&mut id, device), "cuDeviceGetUuid")?;
                if id == uuid {
                    return Ok(ordinal);
                }
            }
        }
        Err(Error::Encode(
            "the Vulkan device isn't a CUDA device".into(),
        ))
    }
}

fn check(result: CuResult, call: &str) -> Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(Error::Encode(format!(
            "{call} failed (CUDA error {result})"
        )))
    }
}

/// FFmpeg's CUDA device: a reference to keep its context alive.
pub(crate) struct CudaDevice {
    buffer: *mut ffi::AVBufferRef,
    context: CuContext,
    /// FFmpeg's stream (NVDEC's), or null for the default one.
    stream: CuStream,
}

// SAFETY: AVBufferRef reference counting is atomic; CUDA contexts may be
// made current on any thread.
unsafe impl Send for CudaDevice {}
unsafe impl Sync for CudaDevice {}

impl CudaDevice {
    /// FFmpeg's CUDA device on the same GPU as `gpu` (matched by UUID).
    pub(crate) fn for_gpu(gpu: &GpuDevice) -> Result<Self> {
        let cuda = Cuda::get()?;
        Self::new(cuda.device_with_uuid(gpu.uuid())?)
    }

    fn new(ordinal: i32) -> Result<Self> {
        let name = CString::new(ordinal.to_string()).expect("no NUL");
        let mut buffer = ptr::null_mut();
        // SAFETY: valid out-pointer and arguments.
        let ret = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut buffer,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                name.as_ptr(),
                ptr::null_mut(),
                0,
            )
        };
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret).into());
        }
        // SAFETY: a CUDA device context.
        let (context, stream) = unsafe {
            let device = (*buffer).data.cast::<ffi::AVHWDeviceContext>();
            let cuda = &*(*device).hwctx.cast::<ffi::AVCUDADeviceContext>();
            (cuda.cuda_ctx, cuda.stream)
        };
        Ok(Self {
            buffer,
            context: context.cast(),
            stream: stream.cast(),
        })
    }

    /// FFmpeg's device context (borrowed), for decoders.
    pub(crate) fn as_ptr(&self) -> *mut ffi::AVBufferRef {
        self.buffer
    }

    /// Runs `f` with the context current on this thread.
    fn with<T>(&self, cuda: &Cuda, f: impl FnOnce() -> Result<T>) -> Result<T> {
        // SAFETY: a valid context, popped again below.
        unsafe { check((cuda.ctx_push)(self.context), "cuCtxPushCurrent")? };
        let result = f();
        let mut popped = ptr::null_mut();
        // SAFETY: pops what we pushed.
        unsafe { (cuda.ctx_pop)(&mut popped) };
        result
    }
}

impl Drop for CudaDevice {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { ffi::av_buffer_unref(&mut self.buffer) };
    }
}

/// A frame's memory, in Vulkan and in CUDA.
struct Slot {
    buffer: SharedBuffer,
    memory: CuExternalMemory,
    pointer: CuDevicePtr,
    device: Arc<CudaDevice>,
}

// SAFETY: the CUDA handles are plain values, used under the context.
unsafe impl Send for Slot {}
unsafe impl Sync for Slot {}

impl Slot {
    fn new(gpu: &Arc<GpuDevice>, device: &Arc<CudaDevice>, size: u64) -> Result<Self> {
        let cuda = Cuda::get()?;
        let handle = MemoryHandle::native()
            .ok_or_else(|| Error::Encode("no memory sharing with CUDA here".into()))?;
        let buffer = SharedBuffer::new(gpu, size, handle)?;
        let exported = buffer.export()?;
        device.with(cuda, || {
            let (kind, handle) = match &exported {
                // CUDA takes the descriptor on success (closed below if not).
                #[cfg(unix)]
                ExportedMemory::Fd(fd) => (
                    CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
                    ExternalMemoryHandle {
                        fd: std::os::fd::AsRawFd::as_raw_fd(fd),
                    },
                ),
                // CUDA doesn't take NT handles: ours is closed when
                // `exported` drops.
                #[cfg(windows)]
                ExportedMemory::Win32(owned) => (
                    CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32,
                    ExternalMemoryHandle {
                        win32: [
                            std::os::windows::io::AsRawHandle::as_raw_handle(owned).cast_const(),
                            ptr::null(),
                        ],
                    },
                ),
            };
            let desc = ExternalMemoryHandleDesc {
                kind,
                handle,
                size: buffer.size(),
                flags: CUDA_EXTERNAL_MEMORY_DEDICATED,
                reserved: [0; 16],
            };
            let mut memory = ptr::null_mut();
            // SAFETY: a valid descriptor of live memory.
            let imported = unsafe { (cuda.import_external_memory)(&mut memory, &desc) };
            check(imported, "cuImportExternalMemory")?;
            // Imported: a descriptor is CUDA's now.
            #[cfg(unix)]
            {
                let ExportedMemory::Fd(fd) = exported;
                std::mem::forget(fd);
            }
            let mut pointer = 0;
            let mapping = ExternalMemoryBufferDesc {
                offset: 0,
                size: buffer.size(),
                flags: 0,
                reserved: [0; 16],
            };
            // SAFETY: imported memory of at least `size` bytes.
            let mapped =
                unsafe { (cuda.external_memory_get_mapped_buffer)(&mut pointer, memory, &mapping) };
            if mapped != 0 {
                // SAFETY: imported above, unused.
                unsafe { (cuda.destroy_external_memory)(memory) };
                check(mapped, "cuExternalMemoryGetMappedBuffer")?;
            }
            Ok(Self {
                buffer,
                memory,
                pointer,
                device: device.clone(),
            })
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let Ok(cuda) = Cuda::get() else { return };
        // The encoder released the frame, so nothing reads the memory.
        let _ = self.device.with(cuda, || {
            // SAFETY: our mapping and import, freed once.
            unsafe {
                (cuda.mem_free)(self.pointer);
                (cuda.destroy_external_memory)(self.memory);
            }
            Ok(())
        });
    }
}

/// Buffers that decoded CUDA frames are copied into for the converter,
/// reused (a decoder's frames all have one size).
#[derive(Default)]
pub(crate) struct DecodeBuffers(Mutex<Vec<Slot>>);

/// Free buffers kept for reuse.
const KEPT_DECODE_BUFFERS: usize = 4;

/// A decoded CUDA frame copied into a shared buffer; the buffer goes back
/// to the pool when this drops.
pub(crate) struct CopiedFrame<'a> {
    pool: &'a DecodeBuffers,
    slot: Option<Slot>,
    pub layout: LinearLayout,
}

impl CopiedFrame<'_> {
    pub fn buffer(&self) -> &SharedBuffer {
        &self.slot.as_ref().expect("slot").buffer
    }
}

impl Drop for CopiedFrame<'_> {
    fn drop(&mut self) {
        let mut free = self.pool.0.lock().expect("pool lock");
        if free.len() < KEPT_DECODE_BUFFERS {
            free.push(self.slot.take().expect("slot"));
        }
    }
}

impl DecodeBuffers {
    /// Copies the planes of the decoded CUDA `frame` (2 planes, `format`)
    /// into a buffer of the pool, on the GPU, and waits for the copy.
    ///
    /// # Safety
    /// `frame` must be a CUDA frame of `device` with two planes in `format`.
    pub(crate) unsafe fn copy<'a>(
        &'a self,
        gpu: &Arc<GpuDevice>,
        device: &Arc<CudaDevice>,
        frame: &frame::Video,
        format: LinearFormat,
    ) -> Result<CopiedFrame<'a>> {
        let cuda = Cuda::get()?;
        let (width, height) = (frame.width(), frame.height());
        let layout = LinearLayout::packed(format, width, height, PITCH_ALIGNMENT);
        let reused = {
            let mut free = self.0.lock().expect("pool lock");
            free.iter()
                .position(|slot| slot.buffer.size() >= layout.size())
                .map(|i| free.swap_remove(i))
        };
        let slot = match reused {
            Some(slot) => slot,
            None => Slot::new(gpu, device, layout.size())?,
        };
        let copied = CopiedFrame {
            pool: self,
            slot: Some(slot),
            layout,
        };
        let pointer = copied.slot.as_ref().expect("slot").pointer;
        let bytes = format.bytes_per_sample() as usize;
        // Chroma rows hold Cb and Cr for every other column.
        let row_bytes = width.next_multiple_of(2) as usize * bytes;
        device.with(cuda, || {
            for (plane, rows, offset) in [
                (0, height, layout.luma_offset),
                (1, format.chroma_rows(height), layout.chroma_offset),
            ] {
                // SAFETY: guaranteed by the caller.
                let (source, pitch) = unsafe {
                    let av = frame.as_ptr();
                    ((*av).data[plane] as CuDevicePtr, (*av).linesize[plane])
                };
                let copy = Memcpy2D {
                    src_memory_type: CU_MEMORYTYPE_DEVICE,
                    src_device: source,
                    src_pitch: pitch.max(0) as usize,
                    dst_memory_type: CU_MEMORYTYPE_DEVICE,
                    dst_device: pointer + offset,
                    dst_pitch: layout.pitch as usize,
                    width_in_bytes: row_bytes.min(layout.pitch as usize),
                    height: rows as usize,
                    ..Memcpy2D::default()
                };
                // SAFETY: both regions lie in live device memory; FFmpeg's
                // stream orders the copy after NVDEC's output.
                check(
                    unsafe { (cuda.memcpy_2d_async)(&copy, device.stream) },
                    "cuMemcpy2DAsync",
                )?;
            }
            // SAFETY: a valid stream of the current context.
            check(
                unsafe { (cuda.stream_synchronize)(device.stream) },
                "cuStreamSynchronize",
            )
        })?;
        Ok(copied)
    }
}

/// Frames' memory, shared with the frames FFmpeg holds.
struct Pool {
    slots: Mutex<Vec<Arc<Slot>>>,
    /// Indices of slots no frame uses.
    free: Mutex<Vec<usize>>,
}

/// Returns a slot to its pool when FFmpeg releases the frame.
struct Release {
    pool: Arc<Pool>,
    index: usize,
}

unsafe extern "C" fn release(opaque: *mut c_void, _data: *mut u8) {
    // SAFETY: created by `CudaSink::next` for this buffer, freed once.
    let release = unsafe { Box::from_raw(opaque.cast::<Release>()) };
    release
        .pool
        .free
        .lock()
        .expect("pool lock")
        .push(release.index);
}

/// NV12 (P010 with `deep`) CUDA frames whose memory the compositor writes.
pub(crate) struct CudaSink {
    gpu: Arc<GpuDevice>,
    device: Arc<CudaDevice>,
    frames: *mut ffi::AVBufferRef,
    layout: LinearLayout,
    matrix: nox_color::Matrix3x4,
    pool: Arc<Pool>,
}

impl CudaSink {
    pub fn new(
        gpu: &Arc<GpuDevice>,
        device: Arc<CudaDevice>,
        width: u32,
        height: u32,
        deep: bool,
    ) -> Result<Self> {
        // SAFETY: a valid CUDA device context; fields are set before init.
        let frames = unsafe {
            let mut frames = ffi::av_hwframe_ctx_alloc(device.buffer);
            if frames.is_null() {
                return Err(Error::Encode("cannot allocate CUDA frames".into()));
            }
            let ctx = (*frames).data.cast::<ffi::AVHWFramesContext>();
            (*ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
            (*ctx).sw_format = if deep {
                ffi::AVPixelFormat::AV_PIX_FMT_P010LE
            } else {
                ffi::AVPixelFormat::AV_PIX_FMT_NV12
            };
            (*ctx).width = width as i32;
            (*ctx).height = height as i32;
            let ret = ffi::av_hwframe_ctx_init(frames);
            if ret < 0 {
                ffi::av_buffer_unref(&mut frames);
                return Err(ffmpeg::Error::from(ret).into());
            }
            frames
        };
        let format = if deep {
            LinearFormat::P010
        } else {
            LinearFormat::Nv12
        };
        let frames = Self {
            gpu: gpu.clone(),
            device,
            frames,
            layout: LinearLayout::packed(format, width, height, PITCH_ALIGNMENT),
            matrix: ycbcr_matrix(deep),
            pool: Arc::new(Pool {
                slots: Mutex::new(Vec::new()),
                free: Mutex::new(Vec::new()),
            }),
        };
        // Fail now rather than at the first frame if sharing doesn't work.
        let slot = Slot::new(&frames.gpu, &frames.device, frames.layout.size())?;
        frames
            .pool
            .slots
            .lock()
            .expect("pool lock")
            .push(Arc::new(slot));
        frames.pool.free.lock().expect("pool lock").push(0);
        Ok(frames)
    }

    /// A free slot, or a new one while the encoder holds all others.
    fn take_slot(&self) -> Result<(usize, Arc<Slot>)> {
        if let Some(index) = self.pool.free.lock().expect("pool lock").pop() {
            let slot = self.pool.slots.lock().expect("pool lock")[index].clone();
            return Ok((index, slot));
        }
        let mut slots = self.pool.slots.lock().expect("pool lock");
        if slots.len() >= MAX_FRAMES {
            return Err(Error::Encode(format!(
                "the encoder holds more than {MAX_FRAMES} frames"
            )));
        }
        let slot = Arc::new(Slot::new(&self.gpu, &self.device, self.layout.size())?);
        slots.push(slot.clone());
        Ok((slots.len() - 1, slot))
    }
}

impl Drop for CudaSink {
    fn drop(&mut self) {
        // SAFETY: we own one reference. Frames FFmpeg still holds keep the
        // pool (and the CUDA device) alive.
        unsafe { ffi::av_buffer_unref(&mut self.frames) };
    }
}

impl GpuSink for CudaSink {
    fn api(&self) -> &'static str {
        "CUDA"
    }

    fn pixel_format(&self) -> format::Pixel {
        format::Pixel::CUDA
    }

    fn frames_context(&self) -> *mut ffi::AVBufferRef {
        self.frames
    }

    fn device_context(&self) -> *mut ffi::AVBufferRef {
        self.device.buffer
    }

    fn next(&mut self, render: Render<'_>) -> Result<frame::Video> {
        let (index, slot) = self.take_slot()?;
        let give_back = || self.pool.free.lock().expect("pool lock").push(index);
        // The compositor has finished writing when this returns.
        if let Err(err) = render(GpuTarget::Buffer(BufferTarget {
            buffer: &slot.buffer,
            layout: self.layout,
            matrix: self.matrix,
        })) {
            give_back();
            return Err(err.into());
        }
        let layout = &self.layout;
        let mut frame = frame::Video::empty();
        // SAFETY: the frame describes the slot's memory; its buffer returns
        // the slot to the pool when FFmpeg frees it.
        unsafe {
            let f = frame.as_mut_ptr();
            let base = slot.pointer as *mut u8;
            let opaque = Box::into_raw(Box::new(Release {
                pool: self.pool.clone(),
                index,
            }));
            (*f).buf[0] = ffi::av_buffer_create(
                base,
                layout.size() as usize,
                Some(release),
                opaque.cast(),
                0,
            );
            if (*f).buf[0].is_null() {
                drop(Box::from_raw(opaque));
                give_back();
                return Err(Error::Encode("cannot allocate a frame".into()));
            }
            (*f).format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA as i32;
            (*f).width = layout.width as i32;
            (*f).height = layout.height as i32;
            (*f).data[0] = base.add(layout.luma_offset as usize);
            (*f).data[1] = base.add(layout.chroma_offset as usize);
            (*f).linesize[0] = layout.pitch as i32;
            (*f).linesize[1] = layout.pitch as i32;
            (*f).hw_frames_ctx = ffi::av_buffer_ref(self.frames);
        }
        Ok(frame)
    }
}
