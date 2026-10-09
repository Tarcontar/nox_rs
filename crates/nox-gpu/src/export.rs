//! Zero-copy frame export as DMA-BUF (Linux).
//!
//! Output images are allocated with a DRM format modifier the consumer (GTK)
//! can import, in exportable memory. A `DmabufFrame` hands out a file
//! descriptor for one image; dropping the frame (when GTK releases its
//! texture) returns the image to the pool for reuse.

use ash::vk;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex, Weak};

use crate::{Error, GpuDevice, Result};

/// DRM fourcc `AB24` (DRM_FORMAT_ABGR8888): bytes R, G, B, A in memory,
/// matching `VK_FORMAT_R8G8B8A8_UNORM`.
pub const FOURCC_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
pub(crate) const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

const EXTENSIONS: [&std::ffi::CStr; 3] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
];

/// A converted frame in an exported image, ready to be wrapped by the
/// consumer (e.g. `GdkDmabufTexture`). The GPU has finished writing it.
pub struct DmabufFrame {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
    image: Option<ExportImage>,
    pool: Weak<Mutex<Vec<ExportImage>>>,
}

impl std::fmt::Debug for DmabufFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "DmabufFrame({}x{}, modifier {:#x})",
            self.width, self.height, self.modifier
        )
    }
}

impl Drop for DmabufFrame {
    fn drop(&mut self) {
        if let (Some(image), Some(pool)) = (self.image.take(), self.pool.upgrade()) {
            pool.lock().unwrap_or_else(|e| e.into_inner()).push(image);
        }
    }
}

pub(crate) struct ExportImage {
    gpu: Arc<GpuDevice>,
    pub image: vk::Image,
    pub view: vk::ImageView,
    memory: vk::DeviceMemory,
    pub width: u32,
    pub height: u32,
    modifier: u64,
    offset: u32,
    stride: u32,
}

impl Drop for ExportImage {
    fn drop(&mut self) {
        let device = &self.gpu.device;
        // SAFETY: the image is idle (we wait for every submission) and no
        // longer shared (the consumer released it).
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// Pool of exportable images of one size.
pub(crate) struct ExportPool {
    gpu: Arc<GpuDevice>,
    modifiers: Vec<u64>,
    free: Arc<Mutex<Vec<ExportImage>>>,
    external_memory_fd: ash::khr::external_memory_fd::Device,
    drm_modifier: ash::ext::image_drm_format_modifier::Device,
}

impl ExportPool {
    /// `importable` lists the modifiers the consumer accepts for AB24.
    pub fn new(gpu: Arc<GpuDevice>, importable: &[u64]) -> Result<Self> {
        if let Some(missing) = EXTENSIONS.iter().find(|e| !gpu.has_extension(e)) {
            return Err(Error::Unsupported(format!(
                "device lacks {}",
                missing.to_string_lossy()
            )));
        }
        let modifiers: Vec<u64> = exportable_modifiers(&gpu)
            .into_iter()
            .filter(|m| importable.contains(m))
            .collect();
        if modifiers.is_empty() {
            return Err(Error::Unsupported(
                "no DRM modifier both exportable and importable".into(),
            ));
        }
        tracing::info!("DMA-BUF export with modifiers {modifiers:x?}");
        let external_memory_fd =
            ash::khr::external_memory_fd::Device::new(&gpu.instance, &gpu.device);
        let drm_modifier =
            ash::ext::image_drm_format_modifier::Device::new(&gpu.instance, &gpu.device);
        Ok(Self {
            gpu,
            modifiers,
            free: Arc::default(),
            external_memory_fd,
            drm_modifier,
        })
    }

    /// A free image of the given size (allocating one if needed).
    pub fn acquire(&self, width: u32, height: u32) -> Result<ExportImage> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        // Images of other sizes are no longer useful.
        free.retain(|image| (image.width, image.height) == (width, height));
        match free.pop() {
            Some(image) => Ok(image),
            None => {
                drop(free);
                self.create(width, height)
            }
        }
    }

    /// Exports `image` (whose rendering has completed) as a frame.
    pub fn export(&self, image: ExportImage) -> Result<DmabufFrame> {
        // SAFETY: the memory was allocated with DMA-BUF export enabled.
        let fd = unsafe {
            self.external_memory_fd.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(image.memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
            )?
        };
        Ok(DmabufFrame {
            // SAFETY: vkGetMemoryFdKHR returns a new fd owned by the caller.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            width: image.width,
            height: image.height,
            fourcc: FOURCC_ABGR8888,
            modifier: image.modifier,
            offset: image.offset,
            stride: image.stride,
            image: Some(image),
            pool: Arc::downgrade(&self.free),
        })
    }

    fn create(&self, width: u32, height: u32) -> Result<ExportImage> {
        let device = &self.gpu.device;
        let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&self.modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // SAFETY: valid create infos; objects are owned by the returned
        // ExportImage and destroyed in its Drop (or below on error).
        unsafe {
            let image = device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(FORMAT)
                    .extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(vk::ImageUsageFlags::STORAGE)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .push_next(&mut modifier_list)
                    .push_next(&mut external),
                None,
            )?;
            let requirements = device.get_image_memory_requirements(image);
            let Some(memory_type) = self.gpu.memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            ) else {
                device.destroy_image(image, None);
                return Err(Error::Unsupported(
                    "no device-local memory for export".into(),
                ));
            };
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let memory = match device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type)
                    .push_next(&mut export)
                    .push_next(&mut dedicated),
                None,
            ) {
                Ok(memory) => memory,
                Err(err) => {
                    device.destroy_image(image, None);
                    return Err(err.into());
                }
            };
            // From here on ExportImage's Drop cleans up.
            let mut export_image = ExportImage {
                gpu: self.gpu.clone(),
                image,
                view: vk::ImageView::null(),
                memory,
                width,
                height,
                modifier: 0,
                offset: 0,
                stride: 0,
            };
            device.bind_image_memory(image, memory, 0)?;
            export_image.view = device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(FORMAT)
                    .subresource_range(crate::converter::color_range()),
                None,
            )?;

            let mut properties = vk::ImageDrmFormatModifierPropertiesEXT::default();
            self.drm_modifier
                .get_image_drm_format_modifier_properties(image, &mut properties)?;
            let layout = device.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default()
                    .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            );
            export_image.modifier = properties.drm_format_modifier;
            export_image.offset = layout.offset as u32;
            export_image.stride = layout.row_pitch as u32;
            tracing::debug!(
                "export image {width}x{height}: modifier {:#x}, stride {}",
                export_image.modifier,
                export_image.stride
            );
            Ok(export_image)
        }
    }
}

/// Single-plane modifiers for RGBA8 that support storage writes and DMA-BUF
/// export.
fn exportable_modifiers(gpu: &GpuDevice) -> Vec<u64> {
    let instance = &gpu.instance;
    // SAFETY: standard two-call enumeration on a valid physical device.
    let properties = unsafe {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format_properties = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(
            gpu.physical_device,
            FORMAT,
            &mut format_properties,
        );
        let mut properties = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut properties);
        let mut format_properties = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(
            gpu.physical_device,
            FORMAT,
            &mut format_properties,
        );
        properties
    };
    properties
        .iter()
        .filter(|p| {
            p.drm_format_modifier_plane_count == 1
                && p.drm_format_modifier_tiling_features
                    .contains(vk::FormatFeatureFlags::STORAGE_IMAGE)
                && is_exportable(gpu, p.drm_format_modifier)
        })
        .map(|p| p.drm_format_modifier)
        .collect()
}

fn is_exportable(gpu: &GpuDevice, modifier: u64) -> bool {
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(FORMAT)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::STORAGE)
        .push_next(&mut modifier_info)
        .push_next(&mut external_info);
    let mut external_properties = vk::ExternalImageFormatProperties::default();
    let mut properties = vk::ImageFormatProperties2::default().push_next(&mut external_properties);
    // SAFETY: valid query structures.
    let supported = unsafe {
        gpu.instance
            .get_physical_device_image_format_properties2(
                gpu.physical_device,
                &info,
                &mut properties,
            )
            .is_ok()
    };
    supported
        && external_properties
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
}
