//! What the driver compiled the kernels to: registers, shared memory and
//! the other statistics `VK_KHR_pipeline_executable_properties` reports,
//! per SPIR-V file (e.g. `target/release/build/nox-ai-*/out/*.spv`).
//!
//!     shader_stats [--constants A,B,...] FILE.spv...
//!
//! `--constants` sets specialization constants 0, 1, ... (32-bit unsigned),
//! e.g. a convolution's geometry for `conv_gemm*.spv` (`KernelKind::constants`).
//!
//! Nox's device comes from FFmpeg, which doesn't enable that extension, so
//! this creates a device of its own on the first GPU with the features the
//! kernels use (16-bit storage and arithmetic, the Vulkan memory model,
//! cooperative matrices). Kernels binding storage images are skipped.

use ash::vk;
use std::ffi::CStr;

fn main() {
    let mut files: Vec<String> = std::env::args().skip(1).collect();
    let mut constants: Vec<u32> = Vec::new();
    if files.first().map(String::as_str) == Some("--constants") {
        constants = files[1]
            .split(',')
            .map(|c| c.parse().expect("a constant"))
            .collect();
        files.drain(..2);
    }
    let entries: Vec<_> = (0..constants.len() as u32)
        .map(|i| vk::SpecializationMapEntry {
            constant_id: i,
            offset: 4 * i,
            size: 4,
        })
        .collect();
    let bytes: Vec<u8> = constants.iter().flat_map(|c| c.to_ne_bytes()).collect();
    let specialization = vk::SpecializationInfo::default()
        .map_entries(&entries)
        .data(&bytes);
    // SAFETY: plain Vulkan setup; everything is destroyed with the process.
    unsafe {
        let entry = ash::Entry::load().expect("Vulkan loader");
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let instance = entry
            .create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
            .expect("instance");
        let physical = instance.enumerate_physical_devices().expect("GPUs")[0];
        let properties = instance.get_physical_device_properties(physical);
        println!(
            "{}",
            CStr::from_ptr(properties.device_name.as_ptr()).to_string_lossy()
        );
        let family = instance
            .get_physical_device_queue_family_properties(physical)
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .expect("compute queue") as u32;
        let mut executable = vk::PhysicalDevicePipelineExecutablePropertiesFeaturesKHR::default()
            .pipeline_executable_info(true);
        let mut cooperative =
            vk::PhysicalDeviceCooperativeMatrixFeaturesKHR::default().cooperative_matrix(true);
        let mut v11 =
            vk::PhysicalDeviceVulkan11Features::default().storage_buffer16_bit_access(true);
        let mut v12 = vk::PhysicalDeviceVulkan12Features::default()
            .shader_float16(true)
            .vulkan_memory_model(true);
        let mut v13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
        // NVIDIA's cooperative matrices 2 where the GPU has them (all its
        // features: workgroup scope, tensor addressing, ...).
        let supported = instance
            .enumerate_device_extension_properties(physical)
            .expect("extensions");
        let cm2 = supported
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(c"VK_NV_cooperative_matrix2"));
        // Its feature structure (newer than ash), raw: sType, 4 bytes of
        // padding, pNext, then 7 booleans, all on.
        let mut cm2_features = [0u32; 11];
        cm2_features[0] = 1_000_593_000;
        for flag in &mut cm2_features[4..11] {
            *flag = 1;
        }
        let mut extensions = vec![
            ash::khr::pipeline_executable_properties::NAME.as_ptr(),
            ash::khr::cooperative_matrix::NAME.as_ptr(),
        ];
        if cm2 {
            extensions.push(c"VK_NV_cooperative_matrix2".as_ptr());
        }
        let priorities = [1.0];
        let queue = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priorities);
        let mut info = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue))
            .enabled_extension_names(&extensions)
            .push_next(&mut executable)
            .push_next(&mut cooperative)
            .push_next(&mut v11)
            .push_next(&mut v12)
            .push_next(&mut v13);
        if cm2 {
            // At the head of the chain: its pNext (words 2..4) takes the rest.
            let rest = info.p_next as u64;
            cm2_features[2] = rest as u32;
            cm2_features[3] = (rest >> 32) as u32;
            info.p_next = cm2_features.as_ptr().cast();
        }
        let device = instance
            .create_device(physical, &info, None)
            .expect("device");
        let stats = ash::khr::pipeline_executable_properties::Device::new(&instance, &device);
        // Enough storage buffers and push constants for every kernel.
        let bindings: Vec<_> = (0..8)
            .map(|b| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
            })
            .collect();
        let set_layout = device
            .create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
            .expect("set layout");
        let push = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .size(128);
        let layout = device
            .create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(std::slice::from_ref(&set_layout))
                    .push_constant_ranges(std::slice::from_ref(&push)),
                None,
            )
            .expect("layout");
        for file in &files {
            let bytes = std::fs::read(file).expect("SPIR-V file");
            let code = ash::util::read_spv(&mut std::io::Cursor::new(bytes)).expect("SPIR-V");
            let module = device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
                .expect("module");
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main")
                .specialization_info(&specialization);
            let created = device.create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(stage)
                    .layout(layout)
                    .flags(vk::PipelineCreateFlags::CAPTURE_STATISTICS_KHR)],
                None,
            );
            let name = std::path::Path::new(file)
                .file_name()
                .map_or(file.clone(), |n| n.to_string_lossy().into_owned());
            let pipeline = match created {
                Ok(p) => p[0],
                Err((_, err)) => {
                    println!("{name}: skipped ({err})");
                    continue;
                }
            };
            let info = vk::PipelineExecutableInfoKHR::default()
                .pipeline(pipeline)
                .executable_index(0);
            let values: Vec<String> = stats
                .get_pipeline_executable_statistics(&info)
                .expect("statistics")
                .iter()
                .map(|s| {
                    let label = s.name_as_c_str().expect("name").to_string_lossy();
                    let value = match s.format {
                        vk::PipelineExecutableStatisticFormatKHR::BOOL32 => s.value.b32.to_string(),
                        vk::PipelineExecutableStatisticFormatKHR::INT64 => s.value.i64.to_string(),
                        vk::PipelineExecutableStatisticFormatKHR::UINT64 => s.value.u64.to_string(),
                        _ => format!("{:.1}", s.value.f64),
                    };
                    format!("{label}={value}")
                })
                .collect();
            println!("{name}: {}", values.join(", "));
            device.destroy_pipeline(pipeline, None);
            device.destroy_shader_module(module, None);
        }
    }
}
