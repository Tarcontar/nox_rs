//! Zero-copy access to frames decoded with Vulkan Video.
//!
//! A decoded `AV_PIX_FMT_VULKAN` frame carries an `AVVkFrame`: FFmpeg's
//! images plus their synchronization state. We describe its planes for the
//! converter and follow FFmpeg's protocol: lock the frame, wait on each
//! image's timeline semaphore at `sem_value`, signal `sem_value + 1`, record
//! the new layout/access/queue family, unlock.

use ffmpeg::{ffi, format, frame};
use nox_gpu::{ImageSync, Input, Layout, VkFrame, VkPlane, vk};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::frames::{Classified, classify};

/// Calls `f` with the best GPU input for `frame`: its own Vulkan images when
/// possible (no copies), otherwise a CPU view (downloading hardware frames).
pub fn with_gpu_input<R>(
    frame: &frame::Video,
    f: impl FnOnce(Input<'_, '_>) -> R,
) -> crate::Result<R> {
    if frame.format() == format::Pixel::VULKAN {
        // SAFETY: `frame` is a valid Vulkan frame.
        match unsafe { Prepared::new(frame) } {
            Ok(prepared) => {
                log_path_once(None);
                // SAFETY: `prepared` was built from `frame`, which outlives the call.
                return Ok(unsafe { prepared.run(f) });
            }
            Err(reason) => log_path_once(Some(reason)),
        }
    }
    crate::with_cpu_frame(frame, |cpu| f(Input::Cpu(cpu)))
}

/// Logs (once per process and outcome) whether decoded frames are used
/// in place or downloaded.
fn log_path_once(fallback: Option<&'static str>) {
    static ZERO_COPY: AtomicBool = AtomicBool::new(false);
    static DOWNLOAD: AtomicBool = AtomicBool::new(false);
    match fallback {
        None if !ZERO_COPY.swap(true, Ordering::Relaxed) => {
            tracing::info!("using Vulkan-decoded frames in place (zero copy)")
        }
        Some(reason) if !DOWNLOAD.swap(true, Ordering::Relaxed) => {
            tracing::info!("downloading Vulkan-decoded frames: {reason}")
        }
        _ => {}
    }
}

/// Everything needed to describe the frame, gathered before locking it.
struct Prepared {
    frames_ctx: *mut ffi::AVHWFramesContext,
    vk_frames_ctx: *const ffi::AVVulkanFramesContext,
    vk_frame: *mut ffi::AVVkFrame,
    template: VkFrame,
}

impl Prepared {
    /// # Safety
    /// `frame` must be a valid `AV_PIX_FMT_VULKAN` frame.
    unsafe fn new(frame: &frame::Video) -> Result<Self, &'static str> {
        // SAFETY: guaranteed by the caller; FFmpeg keeps these alive as long
        // as the frame.
        unsafe {
            let av = frame.as_ptr();
            let frames_ref = (*av).hw_frames_ctx;
            if frames_ref.is_null() {
                return Err("frame has no hardware frames context");
            }
            let frames_ctx = (*frames_ref).data.cast::<ffi::AVHWFramesContext>();
            let vk_frames_ctx = (*frames_ctx)
                .hwctx
                .cast::<ffi::AVVulkanFramesContext>()
                .cast_const();
            if (*vk_frames_ctx).img_flags & vk::ImageCreateFlags::MUTABLE_FORMAT.as_raw() == 0 {
                return Err(
                    "decoder images are not created with MUTABLE_FORMAT (no per-plane views)",
                );
            }
            let sw_format = format::Pixel::from((*frames_ctx).sw_format);
            let Classified {
                layout,
                swap_chroma,
                matrix,
                chroma_offset,
                planes,
            } = classify(sw_format, frame).ok_or("unsupported decoder output format")?;
            if layout == Layout::Packed {
                return Err("packed decoder output format");
            }
            let vk_frame = (*av).data[0].cast::<ffi::AVVkFrame>();
            let images = &(*vk_frame).img;
            // One multi-planar image, or one image per plane.
            let multi_planar = images[1] == 0;
            let (extent_w, extent_h) = ((*frames_ctx).width as u32, (*frames_ctx).height as u32);
            const PLANE_ASPECTS: [vk::ImageAspectFlags; 3] = [
                vk::ImageAspectFlags::PLANE_0,
                vk::ImageAspectFlags::PLANE_1,
                vk::ImageAspectFlags::PLANE_2,
            ];
            let planes = planes
                .iter()
                .enumerate()
                .map(|(i, &(format, log2_w, log2_h))| VkPlane {
                    image: vk::Handle::from_raw(if multi_planar { images[0] } else { images[i] }),
                    aspect: if multi_planar {
                        PLANE_ASPECTS[i]
                    } else {
                        vk::ImageAspectFlags::COLOR
                    },
                    format,
                    width: extent_w.div_ceil(1 << log2_w),
                    height: extent_h.div_ceil(1 << log2_h),
                })
                .collect();
            let template = VkFrame {
                width: frame.width(),
                height: frame.height(),
                layout,
                swap_chroma,
                matrix,
                chroma_offset,
                planes,
                images: Vec::new(),
            };
            Ok(Self {
                frames_ctx,
                vk_frames_ctx,
                vk_frame,
                template,
            })
        }
    }

    fn image_count(&self) -> usize {
        // SAFETY: valid while the frame is.
        let images = unsafe { &(*self.vk_frame).img };
        if images[1] == 0 {
            1
        } else {
            self.template.planes.len()
        }
    }

    /// Locks the frame, runs `f` on it and writes back the new state.
    ///
    /// # Safety
    /// The frame `self` was built from must still be alive.
    unsafe fn run<R>(mut self, f: impl FnOnce(Input<'_, '_>) -> R) -> R {
        let count = self.image_count();
        // SAFETY: guaranteed by the caller; lock/unlock bracket all access to
        // the AVVkFrame state, as FFmpeg requires.
        unsafe {
            let ctx = &*self.vk_frames_ctx;
            if let Some(lock) = ctx.lock_frame {
                lock(self.frames_ctx, self.vk_frame);
            }
            let vkf = &mut *self.vk_frame;
            self.template.images = (0..count)
                .map(|i| ImageSync {
                    image: vk::Handle::from_raw(vkf.img[i]),
                    layout: vk::ImageLayout::from_raw(vkf.layout[i]),
                    access: vk::AccessFlags::from_raw(vkf.access[i] as u32),
                    queue_family: vkf.queue_family[i],
                    semaphore: vk::Handle::from_raw(vkf.sem[i]),
                    value: vkf.sem_value[i],
                })
                .collect();
            let result = f(Input::Vulkan(&mut self.template));
            for (i, sync) in self.template.images.iter().enumerate() {
                vkf.layout[i] = sync.layout.as_raw();
                vkf.access[i] = sync.access.as_raw() as i32;
                vkf.queue_family[i] = sync.queue_family;
                vkf.sem_value[i] = sync.value;
            }
            if let Some(unlock) = ctx.unlock_frame {
                unlock(self.frames_ctx, self.vk_frame);
            }
            result
        }
    }
}
