//! Vulkan instance, device, swapchain and metallic-roughness renderer.

use std::{collections::HashMap, ffi::CStr, ptr};

use crate::{
    render_graph::{self, Config as GraphConfig, FrameGraph, Pass},
    shadow::{self, Aabb, CASCADE_COUNT, Cascade, SHADOW_MAP_SIZES},
};
use ash::vk;
use x173ge_core::{EnvironmentId, ImageId, MaterialId, MeshId, SurfaceGeneration, SurfaceId};
#[cfg(feature = "ssr")]
use x173ge_protocol::SsrSettings;
use x173ge_protocol::{
    AlphaMode, DirectionalShadow, EnvironmentSettings, EnvironmentUpload, FrameSnapshot,
    ImageColorSpace, ImageUpload, MAX_DIRECTIONAL_LIGHTS, MAX_PUNCTUAL_LIGHTS, MaterialData,
    MaterialUpload, MeshUpload, MipmapMode, NativeSurfaceHandle, PunctualLight, RenderFeatures,
    SurfaceDescriptor, TextureFilter, TextureSampler, TextureWrap, Vertex,
};
use x173ge_renderer::{BackendError, PresentResult, RendererBackend};

/// A failure during Vulkan initialization or rendering.
#[derive(Debug, thiserror::Error)]
pub enum VulkanError {
    #[error("Vulkan loader: {0}")]
    Loader(String),
    #[error("Vulkan call failed: {0:?}")]
    Api(vk::Result),
    #[error("Vulkan call `{operation}` failed: {result:?}")]
    ApiAt {
        operation: &'static str,
        result: vk::Result,
    },
    #[error("{operation}: {source}")]
    Operation {
        operation: &'static str,
        #[source]
        source: Box<VulkanError>,
    },
    #[error("unsupported Vulkan surface: {0}")]
    Unsupported(String),
}

impl From<vk::Result> for VulkanError {
    /// Keeps the exact Vulkan result available for diagnostics.
    fn from(error: vk::Result) -> Self {
        Self::Api(error)
    }
}

fn api_call<T>(operation: &'static str, result: Result<T, vk::Result>) -> Result<T, VulkanError> {
    result.map_err(|result| VulkanError::ApiAt { operation, result })
}

fn with_context<T>(
    operation: &'static str,
    result: Result<T, VulkanError>,
) -> Result<T, VulkanError> {
    result.map_err(|source| VulkanError::Operation {
        operation,
        source: Box::new(source),
    })
}

#[cfg(windows)]
mod windows {
    use super::*;
    #[cfg(feature = "profiling")]
    use std::time::Duration;
    use std::time::Instant;
    use std::{
        ffi::CString,
        mem::size_of,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    use ash::{
        Entry,
        khr::{surface, swapchain, win32_surface},
    };
    use glam::{Mat4, Vec3};
    use x173ge_core::Extent2d;

    const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";
    const MAX_GPU_MATERIALS: u32 = 65_536;

    /// Owns one swapchain and its per-image resources.
    struct SurfaceState {
        native: NativeSurfaceHandle,
        handle: vk::SurfaceKHR,
        generation: SurfaceGeneration,
        extent: Extent2d,
        swapchain: vk::SwapchainKHR,
        vk_extent: vk::Extent2D,
        views: Vec<vk::ImageView>,
        render_pass: vk::RenderPass,
        framebuffers: Vec<vk::Framebuffer>,
        present_semaphores: Vec<vk::Semaphore>,
        scene: SceneTargets,
        exposure: ExposureTargets,
        #[cfg(feature = "ssr")]
        ssr: Option<SsrTargets>,
        #[cfg(feature = "god-rays")]
        god_rays: Option<GodRaysTargets>,
        post_descriptor_pool: vk::DescriptorPool,
        post_descriptor_set: vk::DescriptorSet,
        pipeline: vk::Pipeline,
        #[cfg(feature = "bloom")]
        bloom_pipeline: Option<vk::Pipeline>,
        #[cfg(feature = "smaa")]
        smaa_pipeline: Option<vk::Pipeline>,
        #[cfg(feature = "smaa")]
        smaa: Option<SmaaTargets>,
        #[cfg(feature = "smaa")]
        smaa_descriptor_sets: Option<[vk::DescriptorSet; 3]>,
        #[cfg(feature = "bloom")]
        bloom: Option<BloomTargets>,
        #[cfg(feature = "gtao")]
        gtao: Option<GtaoTargets>,
    }

    struct GpuAttachment {
        image: vk::Image,
        memory: GpuMemory,
        view: vk::ImageView,
    }

    #[derive(Default)]
    struct GpuMemoryStats {
        live_bytes: AtomicU64,
        peak_bytes: AtomicU64,
    }

    impl GpuMemoryStats {
        fn allocated(&self, bytes: u64) {
            let live = self.live_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
            self.peak_bytes.fetch_max(live, Ordering::Relaxed);
        }

        fn released(&self, bytes: u64) {
            let previous = self.live_bytes.fetch_sub(bytes, Ordering::Relaxed);
            assert!(previous >= bytes, "Vulkan memory counter underflow");
        }

        fn log_shutdown(&self) {
            eprintln!(
                "X173GE_GPU_MEMORY metric=x173ge_owned_vk_device_memory scope=engine_owned total_vram=false live_bytes={} peak_bytes={}",
                self.live_bytes.load(Ordering::Relaxed),
                self.peak_bytes.load(Ordering::Relaxed),
            );
        }
    }

    struct GpuMemory {
        handle: vk::DeviceMemory,
        size: u64,
        stats: Option<Arc<GpuMemoryStats>>,
    }

    impl GpuMemory {
        fn empty() -> Self {
            Self {
                handle: vk::DeviceMemory::null(),
                size: 0,
                stats: None,
            }
        }

        fn allocate(
            device: &ash::Device,
            requirements: &vk::MemoryRequirements,
            memory_type_index: u32,
            stats: &Arc<GpuMemoryStats>,
        ) -> Result<Self, VulkanError> {
            let handle = unsafe {
                device.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type_index),
                    None,
                )?
            };
            stats.allocated(requirements.size);
            Ok(Self {
                handle,
                size: requirements.size,
                stats: Some(Arc::clone(stats)),
            })
        }

        fn free(self, device: &ash::Device) {
            if self.handle != vk::DeviceMemory::null() {
                unsafe { device.free_memory(self.handle, None) };
                self.stats
                    .expect("allocated Vulkan memory has a device counter")
                    .released(self.size);
            }
        }
    }

    struct SceneTargets {
        color: GpuAttachment,
        normal_roughness: GpuAttachment,
        #[cfg(feature = "ssr")]
        reflectance: Option<GpuAttachment>,
        depth: GpuAttachment,
        framebuffer: vk::Framebuffer,
    }

    #[cfg(feature = "ssr")]
    struct SsrTargets {
        color: GpuAttachment,
        framebuffer: vk::Framebuffer,
        blend_framebuffer: vk::Framebuffer,
        descriptor_set: vk::DescriptorSet,
    }

    #[cfg(feature = "gtao")]
    struct GtaoPipelines {
        prepass_render_pass: vk::RenderPass,
        render_pass: vk::RenderPass,
        prepass: [vk::Pipeline; 4],
        horizon: vk::Pipeline,
        upsample: vk::Pipeline,
    }

    #[cfg(feature = "ssr")]
    struct SsrPipelines {
        render_pass: vk::RenderPass,
        blend_render_pass: vk::RenderPass,
        pipeline_layout: vk::PipelineLayout,
        pipeline: vk::Pipeline,
    }

    #[cfg(feature = "god-rays")]
    struct GodRaysPipelines {
        render_pass: vk::RenderPass,
        march: vk::Pipeline,
        horizontal: vk::Pipeline,
        vertical: vk::Pipeline,
        composite: vk::Pipeline,
    }

    #[cfg(feature = "god-rays")]
    struct GodRaysTargets {
        march: GpuAttachment,
        march_framebuffer: vk::Framebuffer,
        horizontal: GpuAttachment,
        horizontal_framebuffer: vk::Framebuffer,
        scattering: GpuAttachment,
        scattering_framebuffer: vk::Framebuffer,
        horizontal_set: vk::DescriptorSet,
        vertical_set: vk::DescriptorSet,
        composite_set: vk::DescriptorSet,
    }

    #[cfg(feature = "gtao")]
    struct GtaoTargets {
        prepass_framebuffer: vk::Framebuffer,
        raw: GpuAttachment,
        raw_framebuffer: vk::Framebuffer,
        full: GpuAttachment,
        full_framebuffer: vk::Framebuffer,
    }

    #[cfg(feature = "smaa")]
    struct SmaaTargets {
        post_color: GpuAttachment,
        edges: GpuAttachment,
        weights: GpuAttachment,
        post_framebuffer: vk::Framebuffer,
        edge_framebuffer: vk::Framebuffer,
        weight_framebuffer: vk::Framebuffer,
    }

    #[cfg(feature = "smaa")]
    struct SmaaLookups {
        area: GpuImage,
        search: GpuImage,
    }

    struct ExposureLevel {
        extent: vk::Extent2D,
        image: GpuAttachment,
        framebuffer: vk::Framebuffer,
        descriptor_set: vk::DescriptorSet,
    }

    struct ExposureTargets {
        pyramid: Vec<ExposureLevel>,
        history: [GpuAttachment; 2],
        history_framebuffers: [vk::Framebuffer; 2],
        adaptation_sets: [vk::DescriptorSet; 2],
        history_index: usize,
        history_valid: bool,
        last_submitted: Option<Instant>,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ExposurePush {
        first: f32,
        seconds: f32,
        key_luminance: f32,
        min_exposure: f32,
        max_exposure: f32,
        adaptation_seconds: f32,
    }

    #[cfg(feature = "bloom")]
    struct BloomPipelines {
        render_pass: vk::RenderPass,
        first_downsample: vk::Pipeline,
        downsample: vk::Pipeline,
        upsample: vk::Pipeline,
        composite: vk::Pipeline,
    }

    #[cfg(feature = "bloom")]
    struct BloomLevel {
        extent: vk::Extent2D,
        down: GpuAttachment,
        down_framebuffer: vk::Framebuffer,
        down_set: vk::DescriptorSet,
        up: Option<BloomUpsampleTarget>,
    }

    #[cfg(feature = "bloom")]
    struct BloomUpsampleTarget {
        image: GpuAttachment,
        framebuffer: vk::Framebuffer,
        descriptor_set: vk::DescriptorSet,
    }

    #[cfg(feature = "bloom")]
    struct BloomTargets {
        levels: Vec<BloomLevel>,
    }

    #[cfg(feature = "bloom")]
    impl BloomTargets {
        fn final_view(&self) -> vk::ImageView {
            self.levels[0]
                .up
                .as_ref()
                .map_or(self.levels[0].down.view, |up| up.image.view)
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PostPush {
        intensity: f32,
        radius_uv: f32,
        contrast: f32,
    }

    #[cfg(feature = "ssr")]
    #[repr(C, align(16))]
    #[derive(Clone, Copy)]
    struct SsrPush {
        ray: [f32; 2],
        max_steps: u32,
        max_roughness: f32,
        resolve: [f32; 2],
        padding: [f32; 2],
    }

    struct ShadowMap {
        depth: GpuAttachment,
        framebuffer: vk::Framebuffer,
        size: u32,
    }

    #[cfg(feature = "translucent-shadows")]
    struct MboitMap {
        moments_0123: GpuAttachment,
        moments_456: GpuAttachment,
        framebuffer: vk::Framebuffer,
        size: u32,
    }

    #[cfg(feature = "translucent-shadows")]
    struct MboitPipelines {
        render_pass: vk::RenderPass,
        format: vk::Format,
        translucent: [vk::Pipeline; 4],
    }

    #[cfg(feature = "profiling")]
    struct GpuProfile {
        pool: Option<vk::QueryPool>,
        valid_bits: u32,
        period_ns: f32,
        pending_frame: Option<u64>,
        record_submit_ms: Option<f64>,
    }

    #[cfg(feature = "profiling")]
    fn finish_gpu_profile(device: &mut DeviceState) -> Result<(), VulkanError> {
        let Some(profile) = device.profile.as_mut() else {
            return Ok(());
        };
        let Some(frame) = profile.pending_frame.take() else {
            return Ok(());
        };
        let Some(pool) = profile.pool else {
            eprintln!(
                "X173GE_GPU frame={frame} gpu_timing=unavailable record_submit_ms={:.4}",
                profile.record_submit_ms.take().unwrap_or_default()
            );
            return Ok(());
        };
        let mut times = [0_u64; 12];
        unsafe {
            device.device.get_query_pool_results(
                pool,
                0,
                &mut times,
                vk::QueryResultFlags::TYPE_64,
            )?;
        }
        let mask = if profile.valid_bits >= 64 {
            u64::MAX
        } else {
            (1_u64 << profile.valid_bits) - 1
        };
        let ms = |start: usize, end: usize| {
            (times[end].wrapping_sub(times[start]) & mask) as f64 * f64::from(profile.period_ns)
                / 1_000_000.0
        };
        eprintln!(
            "X173GE_GPU frame={frame} shadow_ms={:.4} gtao_ms={:.4} scene_ms={:.4} ssr_separate_blend_ms={:.4} god_rays_march_ms={:.4} god_rays_horizontal_ms={:.4} god_rays_vertical_ms={:.4} god_rays_composite_ms={:.4} exposure_ms={:.4} bloom_ms={:.4} post_ms={:.4} total_ms={:.4} record_submit_ms={:.4}",
            ms(0, 1),
            ms(1, 2),
            ms(2, 3),
            ms(3, 4),
            ms(4, 5),
            ms(5, 6),
            ms(6, 7),
            ms(7, 8),
            ms(8, 9),
            ms(9, 10),
            ms(10, 11),
            ms(0, 11),
            profile.record_submit_ms.take().unwrap_or_default()
        );
        Ok(())
    }

    #[cfg(feature = "profiling")]
    fn write_gpu_timestamp(
        device: &DeviceState,
        command_buffer: vk::CommandBuffer,
        query: u32,
        stage: vk::PipelineStageFlags,
    ) {
        if let Some(pool) = device.profile.as_ref().and_then(|profile| profile.pool) {
            unsafe {
                device
                    .device
                    .cmd_write_timestamp(command_buffer, stage, pool, query);
            }
        }
    }

    struct GpuBuffer {
        buffer: vk::Buffer,
        memory: GpuMemory,
        flags: vk::MemoryPropertyFlags,
    }

    struct GpuMesh {
        vertices: GpuBuffer,
        indices: GpuBuffer,
        index_count: u32,
        bounds: Aabb,
        has_tangents: bool,
    }

    struct GpuImage {
        image: vk::Image,
        memory: GpuMemory,
        linear_view: vk::ImageView,
        srgb_view: vk::ImageView,
        color_space: ImageColorSpace,
        mip_levels: u32,
        extent: Extent2d,
    }

    struct GpuMaterial {
        descriptor_set: vk::DescriptorSet,
        uniform: GpuBuffer,
    }

    #[repr(C, align(16))]
    #[derive(Clone, Copy, Default)]
    struct GpuLight {
        direction_to_light: [f32; 4],
        radiance: [f32; 4],
    }

    #[repr(C, align(16))]
    #[derive(Clone, Copy, Default)]
    struct GpuPunctualLight {
        position_range: [f32; 4],
        direction_type: [f32; 4],
        color_intensity: [f32; 4],
        cone_cosines: [f32; 4],
    }

    #[repr(C, align(16))]
    #[derive(Clone, Copy)]
    struct FrameUniform {
        view_projection: [[f32; 4]; 4],
        inverse_view_projection: [[f32; 4]; 4],
        camera_position: [f32; 4],
        ambient_diffuse: [f32; 4],
        ambient_diffuse_y: [f32; 4],
        camera_forward: [f32; 4],
        gtao_parameters: [f32; 4],
        light_count: [u32; 4],
        cascade_splits: [f32; 4],
        shadow_matrices: [[[f32; 4]; 4]; CASCADE_COUNT],
        shadow_depth: [[f32; 4]; CASCADE_COUNT],
        shadow_parameters: [f32; 4],
        lights: [GpuLight; MAX_DIRECTIONAL_LIGHTS],
        god_rays_parameters: [f32; 4],
        god_rays_counts: [u32; 4],
        punctual_lights: [GpuPunctualLight; MAX_PUNCTUAL_LIGHTS],
        punctual_count: [u32; 4],
        environment_irradiance_sh: [[f32; 4]; 9],
        environment_parameters: [f32; 4],
    }

    const _: () = {
        assert!(MAX_DIRECTIONAL_LIGHTS == 16);
        assert!(MAX_PUNCTUAL_LIGHTS == 16);
        assert!(std::mem::size_of::<GpuPunctualLight>() == 64);
        assert!(std::mem::offset_of!(FrameUniform, camera_forward) == 176);
        assert!(std::mem::offset_of!(FrameUniform, gtao_parameters) == 192);
        assert!(std::mem::offset_of!(FrameUniform, light_count) == 208);
        assert!(std::mem::offset_of!(FrameUniform, lights) == 576);
        assert!(std::mem::offset_of!(FrameUniform, god_rays_parameters) == 1088);
        assert!(std::mem::offset_of!(FrameUniform, god_rays_counts) == 1104);
        assert!(std::mem::offset_of!(FrameUniform, punctual_lights) == 1120);
        assert!(std::mem::offset_of!(FrameUniform, punctual_count) == 2144);
        assert!(std::mem::offset_of!(FrameUniform, environment_irradiance_sh) == 2160);
        assert!(std::mem::offset_of!(FrameUniform, environment_parameters) == 2304);
        assert!(std::mem::size_of::<FrameUniform>() == 2320);
    };

    impl Default for FrameUniform {
        fn default() -> Self {
            Self {
                view_projection: [[0.0; 4]; 4],
                inverse_view_projection: [[0.0; 4]; 4],
                camera_position: [0.0; 4],
                ambient_diffuse: [0.0; 4],
                ambient_diffuse_y: [0.0; 4],
                camera_forward: [0.0; 4],
                gtao_parameters: [0.0; 4],
                light_count: [0; 4],
                cascade_splits: [0.0; 4],
                shadow_matrices: [[[0.0; 4]; 4]; CASCADE_COUNT],
                shadow_depth: [[0.0; 4]; CASCADE_COUNT],
                shadow_parameters: [0.0; 4],
                lights: [GpuLight::default(); MAX_DIRECTIONAL_LIGHTS],
                god_rays_parameters: [0.0; 4],
                god_rays_counts: [0; 4],
                punctual_lights: [GpuPunctualLight::default(); MAX_PUNCTUAL_LIGHTS],
                punctual_count: [0; 4],
                environment_irradiance_sh: [[0.0; 4]; 9],
                environment_parameters: [0.0; 4],
            }
        }
    }

    #[repr(C, align(16))]
    #[derive(Clone, Copy, Default)]
    struct MaterialUniform {
        base_color_factor: [f32; 4],
        pbr: [f32; 4],
        emissive_factor: [f32; 4],
        specular_color_factor: [f32; 4],
        surface: [f32; 4],
        flags: [u32; 4],
    }

    #[repr(C, align(16))]
    #[derive(Clone, Copy)]
    struct ShadowPush {
        model: [[f32; 4]; 4],
        light_view_projection: [[f32; 4]; 4],
    }

    /// Device-global single-frame resources reused by the renderer thread.
    struct DeviceState {
        physical: vk::PhysicalDevice,
        features: RenderFeatures,
        mipmaps_supported: bool,
        queue_family: u32,
        queue: vk::Queue,
        device: ash::Device,
        depth_format: vk::Format,
        command_pool: vk::CommandPool,
        command_buffer: vk::CommandBuffer,
        fence: vk::Fence,
        image_available: vk::Semaphore,
        swapchain: swapchain::Device,
        memory: vk::PhysicalDeviceMemoryProperties,
        memory_stats: Arc<GpuMemoryStats>,
        environments: HashMap<EnvironmentId, [[f32; 3]; 9]>,
        scene_color_format: vk::Format,
        normal_roughness_format: vk::Format,
        scene_render_pass: vk::RenderPass,
        exposure_render_pass: vk::RenderPass,
        exposure_reduce_pipeline: vk::Pipeline,
        exposure_adapt_pipeline: vk::Pipeline,
        #[cfg(feature = "ssr")]
        ssr: Option<SsrPipelines>,
        #[cfg(feature = "god-rays")]
        god_rays: Option<GodRaysPipelines>,
        scene_pipelines: [vk::Pipeline; 8],
        frame_uniform: GpuBuffer,
        frame_descriptor_set_layout: vk::DescriptorSetLayout,
        material_descriptor_set_layout: vk::DescriptorSetLayout,
        post_descriptor_set_layout: vk::DescriptorSetLayout,
        #[cfg(feature = "smaa")]
        smaa_weights_descriptor_set_layout: vk::DescriptorSetLayout,
        descriptor_pool: vk::DescriptorPool,
        pipeline_layout: vk::PipelineLayout,
        post_pipeline_layout: vk::PipelineLayout,
        #[cfg(feature = "god-rays")]
        god_rays_pipeline_layout: vk::PipelineLayout,
        #[cfg(feature = "smaa")]
        smaa_weights_pipeline_layout: vk::PipelineLayout,
        #[cfg(feature = "smaa")]
        smaa_neighborhood_pipeline_layout: vk::PipelineLayout,
        frame_descriptor_set: vk::DescriptorSet,
        shadow_render_pass: vk::RenderPass,
        shadow_maps: Vec<ShadowMap>,
        shadow_diagnostics_last_items: usize,
        #[cfg(feature = "translucent-shadows")]
        mboit: Option<MboitPipelines>,
        #[cfg(feature = "translucent-shadows")]
        mboit_maps: Vec<MboitMap>,
        shadow_sampler: vk::Sampler,
        shadow_compare_sampler: vk::Sampler,
        post_sampler: vk::Sampler,
        #[cfg(feature = "smaa")]
        smaa_post_render_pass: vk::RenderPass,
        #[cfg(feature = "smaa")]
        smaa_edge_render_pass: vk::RenderPass,
        #[cfg(feature = "smaa")]
        smaa_weights_render_pass: vk::RenderPass,
        #[cfg(feature = "smaa")]
        smaa_post_pipeline: vk::Pipeline,
        #[cfg(feature = "smaa")]
        smaa_edge_pipeline: vk::Pipeline,
        #[cfg(feature = "smaa")]
        smaa_weights_pipeline: vk::Pipeline,
        #[cfg(feature = "smaa")]
        smaa_lookups: Option<SmaaLookups>,
        #[cfg(feature = "bloom")]
        bloom: Option<BloomPipelines>,
        #[cfg(feature = "gtao")]
        gtao: Option<GtaoPipelines>,
        shadow_pipelines: [vk::Pipeline; 4],
        meshes: HashMap<MeshId, GpuMesh>,
        images: HashMap<ImageId, GpuImage>,
        materials: HashMap<MaterialId, GpuMaterial>,
        samplers: HashMap<TextureSampler, vk::Sampler>,
        fallback_image: Option<GpuImage>,
        #[cfg(feature = "profiling")]
        profile: Option<GpuProfile>,
    }

    #[derive(Clone, Copy)]
    struct EnvironmentFrame {
        settings: EnvironmentSettings,
        diffuse_irradiance_sh: [[f32; 3]; 9],
    }

    /// Owns the Vulkan device and every surface resource it creates.
    pub struct VulkanRenderer {
        _entry: Entry,
        instance: ash::Instance,
        surface: surface::Instance,
        win32_surface: win32_surface::Instance,
        debug: Option<(ash::ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
        device: Option<DeviceState>,
        surfaces: HashMap<SurfaceId, SurfaceState>,
        materials: HashMap<MaterialId, MaterialData>,
        features: RenderFeatures,
    }

    impl VulkanRenderer {
        /// Creates the Vulkan instance inside the renderer thread.
        pub fn new() -> Result<Self, VulkanError> {
            let entry =
                unsafe { Entry::load() }.map_err(|error| VulkanError::Loader(error.to_string()))?;
            let layers = unsafe { entry.enumerate_instance_layer_properties()? };
            let extensions = unsafe { entry.enumerate_instance_extension_properties(None)? };
            let has_layer = layers.iter().any(|layer| unsafe {
                CStr::from_ptr(layer.layer_name.as_ptr()) == VALIDATION_LAYER
            });
            let has_debug = extensions.iter().any(|extension| unsafe {
                CStr::from_ptr(extension.extension_name.as_ptr()) == ash::ext::debug_utils::NAME
            });
            let use_validation = cfg!(debug_assertions) && has_layer && has_debug;
            #[cfg(debug_assertions)]
            eprintln!("Vulkan validation layer enabled: {use_validation}");
            let mut extension_names = vec![surface::NAME.as_ptr(), win32_surface::NAME.as_ptr()];
            if use_validation {
                extension_names.push(ash::ext::debug_utils::NAME.as_ptr());
            }
            let layer_names = use_validation.then_some([VALIDATION_LAYER.as_ptr()]);
            let app_name = CString::new("X173GE").expect("static application name");
            let app_info = vk::ApplicationInfo::default()
                .application_name(&app_name)
                .engine_name(&app_name)
                .api_version(vk::API_VERSION_1_0);
            let mut info = vk::InstanceCreateInfo::default()
                .application_info(&app_info)
                .enabled_extension_names(&extension_names);
            if let Some(layer_names) = &layer_names {
                info = info.enabled_layer_names(layer_names);
            }
            let instance = unsafe { entry.create_instance(&info, None)? };
            let debug = if use_validation {
                let loader = ash::ext::debug_utils::Instance::new(&entry, &instance);
                let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
                    .message_severity(
                        vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                            | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
                    )
                    .message_type(
                        vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                            | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                            | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
                    )
                    .pfn_user_callback(Some(debug_callback));
                match unsafe { loader.create_debug_utils_messenger(&info, None) } {
                    Ok(messenger) => Some((loader, messenger)),
                    Err(error) => {
                        unsafe { instance.destroy_instance(None) };
                        return Err(error.into());
                    }
                }
            } else {
                None
            };
            tracing::info!(validation = use_validation, "created Vulkan instance");

            Ok(Self {
                surface: surface::Instance::new(&entry, &instance),
                win32_surface: win32_surface::Instance::new(&entry, &instance),
                _entry: entry,
                instance,
                debug,
                device: None,
                surfaces: HashMap::new(),
                materials: HashMap::new(),
                features: RenderFeatures::none(),
            })
        }

        fn device_mut(&mut self) -> Result<&mut DeviceState, BackendError> {
            self.device
                .as_mut()
                .ok_or_else(|| BackendError("configure a surface before uploading assets".into()))
        }

        /// Creates a Win32 surface while the platform keeps its window alive.
        fn create_surface(
            &self,
            native: NativeSurfaceHandle,
        ) -> Result<vk::SurfaceKHR, VulkanError> {
            let NativeSurfaceHandle::Win32 { hwnd, hinstance } = native else {
                return Err(VulkanError::Unsupported(
                    "this build creates Win32 surfaces only".into(),
                ));
            };
            let info = vk::Win32SurfaceCreateInfoKHR::default()
                .hwnd(hwnd.get() as _)
                .hinstance(hinstance.get() as _);
            unsafe {
                self.win32_surface
                    .create_win32_surface(&info, None)
                    .map_err(Into::into)
            }
        }

        /// Creates the device on the first surface and checks later windows against its queue.
        fn ensure_device(&mut self, surface: vk::SurfaceKHR) -> Result<(), VulkanError> {
            if let Some(device) = &self.device {
                return if unsafe {
                    self.surface.get_physical_device_surface_support(
                        device.physical,
                        device.queue_family,
                        surface,
                    )?
                } {
                    Ok(())
                } else {
                    Err(VulkanError::Unsupported(
                        "the existing graphics queue cannot present to this window".into(),
                    ))
                };
            }

            let (physical, queue_family) = choose_physical_device(
                &self.instance,
                &self.surface,
                surface,
                cfg!(feature = "ssr") && self.features.ssr.is_some(),
                scene_fragment_shader(
                    cfg!(feature = "ssr") && self.features.ssr.is_some(),
                    cfg!(feature = "translucent-shadows") && self.features.translucent_shadows,
                )
                .1 as u32,
            )?;
            self.device = Some(create_device(
                &self.instance,
                physical,
                queue_family,
                self.features,
            )?);
            let properties = unsafe { self.instance.get_physical_device_properties(physical) };
            let device_name =
                unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }.to_string_lossy();
            eprintln!(
                "X173GE_VULKAN device={device_name} device_type_priority={} queue_family={queue_family} max_color_attachments={}",
                physical_device_priority(properties.device_type),
                properties.limits.max_color_attachments,
            );
            tracing::info!(
                device = %device_name,
                queue_family,
                "created Vulkan device"
            );
            Ok(())
        }

        /// Recreates the swapchain; after passing the old one, any failure is fatal to this backend.
        fn configure(&mut self, descriptor: SurfaceDescriptor) -> Result<(), VulkanError> {
            let previous = self.surfaces.get(&descriptor.token.id).map(|surface| {
                (
                    surface.native,
                    surface.handle,
                    surface.generation,
                    surface.swapchain,
                )
            });
            if let Some((native, _, generation, _)) = previous {
                if native != descriptor.native || descriptor.token.generation <= generation {
                    return Err(VulkanError::Unsupported(
                        "surface reconfiguration must keep its window and advance generation"
                            .into(),
                    ));
                }
            }
            let (handle, is_new) = if let Some((_, handle, _, _)) = previous {
                (handle, false)
            } else {
                (self.create_surface(descriptor.native)?, true)
            };
            if let Err(error) = self.ensure_device(handle) {
                if is_new {
                    unsafe { self.surface.destroy_surface(handle, None) };
                }
                return Err(error);
            }

            let device = self.device.as_mut().expect("ensure_device succeeded");
            if let Err(error) = unsafe { device.device.device_wait_idle() } {
                if is_new {
                    unsafe { self.surface.destroy_surface(handle, None) };
                }
                return Err(error.into());
            }
            #[cfg(feature = "smaa")]
            if self.features.smaa {
                if let Err(error) = initialize_smaa_lookups(device) {
                    if is_new {
                        unsafe { self.surface.destroy_surface(handle, None) };
                    }
                    return Err(error);
                }
            }
            let old_swapchain =
                previous.map_or(vk::SwapchainKHR::null(), |(_, _, _, swapchain)| swapchain);
            let replacement = create_swapchain(
                device,
                &self.surface,
                handle,
                descriptor.extent,
                descriptor.vsync,
                old_swapchain,
            );
            let swapchain = match replacement {
                Ok(swapchain) => swapchain,
                Err(failure) => {
                    if is_new {
                        unsafe { self.surface.destroy_surface(handle, None) };
                    } else if failure.old_swapchain_retired {
                        if let Some(old) = self.surfaces.remove(&descriptor.token.id) {
                            destroy_swapchain(device, old);
                        }
                        unsafe { self.surface.destroy_surface(handle, None) };
                    }
                    return Err(failure.error);
                }
            };

            let replacement = SurfaceState {
                native: descriptor.native,
                handle,
                generation: descriptor.token.generation,
                extent: descriptor.extent,
                swapchain: swapchain.swapchain,
                vk_extent: swapchain.extent,
                views: swapchain.views,
                render_pass: swapchain.render_pass,
                framebuffers: swapchain.framebuffers,
                present_semaphores: swapchain.present_semaphores,
                scene: swapchain.scene,
                exposure: swapchain.exposure,
                #[cfg(feature = "ssr")]
                ssr: swapchain.ssr,
                #[cfg(feature = "god-rays")]
                god_rays: swapchain.god_rays,
                post_descriptor_pool: swapchain.post_descriptor_pool,
                post_descriptor_set: swapchain.post_descriptor_set,
                pipeline: swapchain.pipeline,
                #[cfg(feature = "bloom")]
                bloom_pipeline: swapchain.bloom_pipeline,
                #[cfg(feature = "smaa")]
                smaa_pipeline: swapchain.smaa_pipeline,
                #[cfg(feature = "smaa")]
                smaa: swapchain.smaa,
                #[cfg(feature = "smaa")]
                smaa_descriptor_sets: swapchain.smaa_descriptor_sets,
                #[cfg(feature = "bloom")]
                bloom: swapchain.bloom,
                #[cfg(feature = "gtao")]
                gtao: swapchain.gtao,
            };
            if let Some(old) = self.surfaces.insert(descriptor.token.id, replacement) {
                destroy_swapchain(self.device.as_ref().expect("device exists"), old);
            }
            tracing::info!(
                surface = descriptor.token.id.get(),
                generation = descriptor.token.generation.get(),
                width = descriptor.extent.width(),
                height = descriptor.extent.height(),
                "configured Vulkan swapchain"
            );
            Ok(())
        }

        /// Draws one indexed PBR frame and presents it.
        fn present(
            &mut self,
            frame: &FrameSnapshot,
            features: RenderFeatures,
        ) -> Result<PresentResult, VulkanError> {
            let surface = self.surfaces.get(&frame.surface.id).ok_or_else(|| {
                VulkanError::Unsupported("frame references an unknown surface".into())
            })?;
            if surface.generation != frame.surface.generation || surface.extent != frame.extent {
                return Ok(PresentResult::SurfaceInvalidated);
            }
            if !self.features.supports(features) {
                return Err(VulkanError::Unsupported(
                    "frame requested a render feature outside startup capabilities".into(),
                ));
            }
            #[cfg(feature = "smaa")]
            let smaa_enabled = features.smaa;
            frame
                .validate()
                .map_err(|reason| VulkanError::Unsupported(reason.into()))?;
            let device = self
                .device
                .as_mut()
                .expect("surface configuration creates a device");
            for item in &frame.render_items {
                let mesh = device.meshes.get(&item.mesh).ok_or_else(|| {
                    VulkanError::Unsupported(format!(
                        "frame references unknown mesh {}",
                        item.mesh.get()
                    ))
                })?;
                let material = self.materials.get(&item.material).ok_or_else(|| {
                    VulkanError::Unsupported(format!(
                        "frame references unknown material {}",
                        item.material.get()
                    ))
                })?;
                if material.normal_texture.is_some() && !mesh.has_tangents {
                    return Err(VulkanError::Unsupported(format!(
                        "mesh {} needs tangents for its normal map",
                        item.mesh.get()
                    )));
                }
            }
            let shadowed_light = frame
                .directional_lights
                .iter()
                .enumerate()
                .find_map(|(index, light)| light.shadow.map(|settings| (index, settings, light)));
            let mboit_enabled =
                cfg!(feature = "translucent-shadows") && features.translucent_shadows;
            let cascades = if let Some((_, _, light)) = shadowed_light {
                shadow::fit(
                    frame.camera,
                    Vec3::from_array(light.direction_to_light),
                    &frame.render_items,
                    |mesh| device.meshes.get(&mesh).map(|mesh| mesh.bounds),
                )
                .map_err(|reason| VulkanError::Unsupported(reason.into()))?
            } else {
                Vec::new()
            };
            let has_blend = frame
                .render_items
                .iter()
                .any(|item| self.materials[&item.material].alpha_mode == AlphaMode::Blend);
            let camera_forward = if shadowed_light.is_some()
                || features.gtao
                || features.ssr.is_some()
                || features.god_rays.is_some()
                || has_blend
            {
                shadow::view_direction(frame.camera)
                    .map_err(|reason| VulkanError::Unsupported(reason.into()))?
            } else {
                Vec3::ZERO
            };
            let mut blend_items = frame
                .render_items
                .iter()
                .filter(|item| self.materials[&item.material].alpha_mode == AlphaMode::Blend)
                .collect::<Vec<_>>();
            blend_items.sort_by(|a, b| {
                let depth = |item: &&x173ge_protocol::RenderItem| {
                    let bounds = device.meshes[&item.mesh].bounds;
                    (Mat4::from_cols_array_2d(&item.model_matrix).transform_point3(bounds.center())
                        - Vec3::from_array(frame.camera.position))
                    .dot(camera_forward)
                };
                depth(b).total_cmp(&depth(a))
            });
            let surface = self.surfaces.get_mut(&frame.surface.id).ok_or_else(|| {
                VulkanError::Unsupported("frame references an unknown surface".into())
            })?;
            api_call("wait_for_fences(frame)", unsafe {
                device
                    .device
                    .wait_for_fences(&[device.fence], true, u64::MAX)
            })?;
            let exposure_seconds = surface
                .exposure
                .last_submitted
                .as_ref()
                .map_or(0.0, |last| last.elapsed().as_secs_f32());
            let exposure_write_index = if surface.exposure.history_valid {
                surface.exposure.history_index ^ 1
            } else {
                0
            };
            let exposure_view = surface.exposure.history[exposure_write_index].view;
            let environment = frame
                .environment
                .map(|settings| {
                    let diffuse_irradiance_sh =
                        *device.environments.get(&settings.id).ok_or_else(|| {
                            VulkanError::Unsupported(format!(
                                "frame references unuploaded environment {}",
                                settings.id.get()
                            ))
                        })?;
                    Ok::<_, VulkanError>(EnvironmentFrame {
                        settings,
                        diffuse_irradiance_sh,
                    })
                })
                .transpose()?;
            #[cfg(feature = "profiling")]
            finish_gpu_profile(device)?;
            let fallback_view = device
                .fallback_image
                .as_ref()
                .map_or(surface.scene.color.view, |image| image.linear_view);
            #[cfg(feature = "gtao")]
            let ao_views = if features.gtao {
                let gtao = surface.gtao.as_ref().expect("GTAO targets are configured");
                (gtao.full.view, gtao.raw.view)
            } else {
                (fallback_view, fallback_view)
            };
            #[cfg(not(feature = "gtao"))]
            let ao_views = (fallback_view, fallback_view);
            let reconstruct =
                features.gtao || features.ssr.is_some() || features.god_rays.is_some();
            let frame_views = [
                ao_views.0,
                if reconstruct {
                    surface.scene.depth.view
                } else {
                    fallback_view
                },
                if reconstruct {
                    surface.scene.normal_roughness.view
                } else {
                    fallback_view
                },
                ao_views.1,
            ];
            let frame_layouts = [
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                if reconstruct {
                    vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL
                } else {
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                },
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            ];
            write_frame_scene_images(
                &device.device,
                device.frame_descriptor_set,
                device.post_sampler,
                device.shadow_sampler,
                frame_views,
                frame_layouts,
            );
            #[cfg(feature = "ssr")]
            if device.features.ssr.is_some() {
                write_frame_reflectance(
                    &device.device,
                    device.frame_descriptor_set,
                    device.post_sampler,
                    surface
                        .scene
                        .reflectance
                        .as_ref()
                        .expect("SSR reflectance is configured")
                        .view,
                );
            }
            if !cascades.is_empty() && device.shadow_maps.len() == 1 {
                let mut maps = Vec::with_capacity(CASCADE_COUNT);
                for size in SHADOW_MAP_SIZES {
                    match create_shadow_map(
                        &device.device,
                        &device.memory,
                        &device.memory_stats,
                        device.depth_format,
                        device.shadow_render_pass,
                        size,
                    ) {
                        Ok(map) => maps.push(map),
                        Err(error) => {
                            for map in maps {
                                destroy_shadow_map(&device.device, map);
                            }
                            return Err(error);
                        }
                    }
                }
                if let Err(error) = initialize_shadow_maps(
                    &device.device,
                    device.queue,
                    device.command_buffer,
                    device.shadow_render_pass,
                    &maps,
                ) {
                    for map in maps {
                        destroy_shadow_map(&device.device, map);
                    }
                    return Err(error);
                }
                write_shadow_descriptors(
                    &device.device,
                    device.frame_descriptor_set,
                    device.shadow_sampler,
                    device.shadow_compare_sampler,
                    &maps,
                );
                let fallback = std::mem::replace(&mut device.shadow_maps, maps);
                for map in fallback {
                    destroy_shadow_map(&device.device, map);
                }
            }
            #[cfg(feature = "translucent-shadows")]
            if mboit_enabled && !cascades.is_empty() && device.mboit_maps.is_empty() {
                let mboit = device
                    .mboit
                    .as_ref()
                    .expect("MBOIT pipelines are configured");
                let mut maps = Vec::with_capacity(CASCADE_COUNT);
                for index in 0..CASCADE_COUNT {
                    match create_mboit_map(
                        device,
                        mboit.format,
                        mboit.render_pass,
                        device.shadow_maps[index].size / 2,
                    ) {
                        Ok(map) => maps.push(map),
                        Err(error) => {
                            for map in maps {
                                destroy_mboit_map(&device.device, map);
                            }
                            return Err(error);
                        }
                    }
                }
                let moments_0123 = std::array::from_fn(|index| maps[index].moments_0123.view);
                let moments_456 = std::array::from_fn(|index| maps[index].moments_456.view);
                write_mboit_descriptors(
                    &device.device,
                    device.frame_descriptor_set,
                    device.post_sampler,
                    moments_0123,
                    moments_456,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                );
                let allocated_bytes = maps
                    .iter()
                    .map(|map| map.moments_0123.memory.size + map.moments_456.memory.size)
                    .sum::<u64>();
                eprintln!(
                    "X173GE_MBOIT_MAP format={} resolution_scale=0.5 cascade_sizes={:?} vk_allocation_bytes={allocated_bytes}",
                    mboit.format.as_raw(),
                    maps.iter().map(|map| map.size).collect::<Vec<_>>(),
                );
                device.mboit_maps = maps;
            }
            let report_shadow_frame = !cascades.is_empty()
                && device.shadow_maps.len() == CASCADE_COUNT
                && (device.shadow_diagnostics_last_items == 0
                    || frame.render_items.len() >= device.shadow_diagnostics_last_items + 16)
                && frame
                    .render_items
                    .iter()
                    .any(|item| self.materials[&item.material].alpha_mode != AlphaMode::Blend);
            if report_shadow_frame {
                device.shadow_diagnostics_last_items = frame.render_items.len();
            }
            let frame_uniform = make_frame_uniform(
                frame,
                shadowed_light.map(|(index, settings, _)| (index, settings)),
                &cascades,
                camera_forward,
                features.gtao,
                features.ssr.is_some(),
                features.god_rays,
                mboit_enabled && !cascades.is_empty(),
                environment,
            )?;
            write_buffer(
                &device.device,
                &device.frame_uniform,
                bytes_of(&frame_uniform),
            )?;
            let mut graph = FrameGraph::build(GraphConfig {
                shadows: !cascades.is_empty(),
                mboit: mboit_enabled && !cascades.is_empty(),
                gtao: cfg!(feature = "gtao") && features.gtao,
                ssr: cfg!(feature = "ssr") && features.ssr.is_some(),
                god_rays: cfg!(feature = "god-rays") && features.god_rays.is_some(),
                bloom: cfg!(feature = "bloom") && features.bloom.is_some(),
                smaa: cfg!(feature = "smaa") && features.smaa,
            })
            .map_err(|error| VulkanError::Unsupported(error.to_string()))?;
            let (image_index, suboptimal) = match unsafe {
                device.swapchain.acquire_next_image(
                    surface.swapchain,
                    16_666_667,
                    device.image_available,
                    vk::Fence::null(),
                )
            } {
                Ok(result) => result,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    return Ok(PresentResult::SurfaceInvalidated);
                }
                Err(vk::Result::TIMEOUT | vk::Result::NOT_READY) => {
                    return Ok(PresentResult::NotReady);
                }
                Err(error) => {
                    return Err(VulkanError::ApiAt {
                        operation: "acquire_next_image",
                        result: error,
                    });
                }
            };

            unsafe {
                let scene_clear_color = frame.clear_color;
                macro_rules! graph_pass {
                    ($pass:ident) => {
                        graph
                            .enter(Pass::$pass)
                            .map_err(|error| VulkanError::Unsupported(error.to_string()))?
                    };
                }
                api_call(
                    "reset_fences(frame)",
                    device.device.reset_fences(&[device.fence]),
                )?;
                api_call(
                    "reset_command_buffer(frame)",
                    device.device.reset_command_buffer(
                        device.command_buffer,
                        vk::CommandBufferResetFlags::empty(),
                    ),
                )?;
                #[cfg(feature = "profiling")]
                let record_started = std::time::Instant::now();
                api_call(
                    "begin_command_buffer(frame)",
                    device.device.begin_command_buffer(
                        device.command_buffer,
                        &vk::CommandBufferBeginInfo::default(),
                    ),
                )?;
                #[cfg(feature = "profiling")]
                if let Some(pool) = device.profile.as_ref().and_then(|profile| profile.pool) {
                    device
                        .device
                        .cmd_reset_query_pool(device.command_buffer, pool, 0, 12);
                    write_gpu_timestamp(
                        device,
                        device.command_buffer,
                        0,
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                    );
                }
                if graph_pass!(Shadow) {
                    let mut shadow_draws = [0usize; CASCADE_COUNT];
                    for (cascade_index, cascade) in cascades.iter().enumerate() {
                        let map_size = device.shadow_maps[cascade_index].size;
                        let extent = vk::Extent2D {
                            width: map_size,
                            height: map_size,
                        };
                        let render_area = vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent,
                        };
                        device.device.cmd_set_viewport(
                            device.command_buffer,
                            0,
                            &[vk::Viewport::default()
                                .x(0.0)
                                .y(0.0)
                                .width(map_size as f32)
                                .height(map_size as f32)
                                .min_depth(0.0)
                                .max_depth(1.0)],
                        );
                        device
                            .device
                            .cmd_set_scissor(device.command_buffer, 0, &[render_area]);
                        device.device.cmd_begin_render_pass(
                            device.command_buffer,
                            &vk::RenderPassBeginInfo::default()
                                .render_pass(device.shadow_render_pass)
                                .framebuffer(device.shadow_maps[cascade_index].framebuffer)
                                .render_area(render_area)
                                .clear_values(&[vk::ClearValue {
                                    depth_stencil: vk::ClearDepthStencilValue {
                                        depth: 1.0,
                                        stencil: 0,
                                    },
                                }]),
                            vk::SubpassContents::INLINE,
                        );
                        shadow_draws[cascade_index] += draw_shadow_items(
                            device,
                            &self.materials,
                            frame,
                            cascade,
                            &device.shadow_pipelines,
                            ShadowDrawKind::Opaque,
                        );
                        device.device.cmd_end_render_pass(device.command_buffer);
                        #[cfg(feature = "translucent-shadows")]
                        if mboit_enabled {
                            let mboit = device
                                .mboit
                                .as_ref()
                                .expect("MBOIT pipelines are configured");
                            let size = device.mboit_maps[cascade_index].size;
                            let extent = vk::Extent2D {
                                width: size,
                                height: size,
                            };
                            let area = vk::Rect2D {
                                offset: vk::Offset2D { x: 0, y: 0 },
                                extent,
                            };
                            device.device.cmd_set_viewport(
                                device.command_buffer,
                                0,
                                &[vk::Viewport::default()
                                    .x(0.0)
                                    .y(0.0)
                                    .width(size as f32)
                                    .height(size as f32)
                                    .min_depth(0.0)
                                    .max_depth(1.0)],
                            );
                            device
                                .device
                                .cmd_set_scissor(device.command_buffer, 0, &[area]);
                            device.device.cmd_begin_render_pass(
                                device.command_buffer,
                                &vk::RenderPassBeginInfo::default()
                                    .render_pass(mboit.render_pass)
                                    .framebuffer(device.mboit_maps[cascade_index].framebuffer)
                                    .render_area(area)
                                    .clear_values(&[
                                        vk::ClearValue {
                                            color: vk::ClearColorValue { float32: [0.0; 4] },
                                        },
                                        vk::ClearValue {
                                            color: vk::ClearColorValue { float32: [0.0; 4] },
                                        },
                                    ]),
                                vk::SubpassContents::INLINE,
                            );
                            shadow_draws[cascade_index] += draw_shadow_items(
                                device,
                                &self.materials,
                                frame,
                                cascade,
                                &mboit.translucent,
                                ShadowDrawKind::MboitTranslucent,
                            );
                            device.device.cmd_end_render_pass(device.command_buffer);
                        }
                    }
                    if report_shadow_frame {
                        let (light_index, settings, light) =
                            shadowed_light.expect("shadow cascades require a shadowed light");
                        let non_blend_items = frame
                            .render_items
                            .iter()
                            .filter(|item| {
                                self.materials[&item.material].alpha_mode != AlphaMode::Blend
                            })
                            .count();
                        let splits = cascades.iter().map(|c| c.split_far).collect::<Vec<_>>();
                        let depth_ranges = cascades
                            .iter()
                            .map(|c| (c.light_near, c.light_far))
                            .collect::<Vec<_>>();
                        let half_extents =
                            cascades.iter().map(|c| c.half_extent).collect::<Vec<_>>();
                        println!(
                            "X173GE_SHADOW light_index={light_index} direction_to_light={:?} radiance={:?} angular_radius={} max_penumbra_world={} mboit={} cascades={} shadow_maps={} render_items={} non_blend_items={} blend_items={} indexed_draws_per_cascade={shadow_draws:?} split_far={splits:?} light_depth_ranges={depth_ranges:?} half_extents={half_extents:?}",
                            light.direction_to_light,
                            light.radiance,
                            settings.angular_radius_radians,
                            settings.max_penumbra_world,
                            mboit_enabled,
                            cascades.len(),
                            device.shadow_maps.len(),
                            frame.render_items.len(),
                            non_blend_items,
                            frame.render_items.len() - non_blend_items,
                        );
                    }
                }
                #[cfg(feature = "profiling")]
                write_gpu_timestamp(
                    device,
                    device.command_buffer,
                    1,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                );
                if graph_pass!(GtaoPrepass) {
                    #[cfg(feature = "gtao")]
                    {
                        let targets = surface.gtao.as_ref().expect("GTAO targets are configured");
                        let gtao = device.gtao.as_ref().expect("GTAO pipelines are configured");
                        device.device.cmd_begin_render_pass(
                            device.command_buffer,
                            &vk::RenderPassBeginInfo::default()
                                .render_pass(gtao.prepass_render_pass)
                                .framebuffer(targets.prepass_framebuffer)
                                .render_area(vk::Rect2D {
                                    offset: vk::Offset2D { x: 0, y: 0 },
                                    extent: surface.vk_extent,
                                })
                                .clear_values(&[
                                    vk::ClearValue {
                                        color: vk::ClearColorValue {
                                            float32: [0.5, 0.5, 1.0, 1.0],
                                        },
                                    },
                                    vk::ClearValue {
                                        depth_stencil: vk::ClearDepthStencilValue {
                                            depth: 0.0,
                                            stencil: 0,
                                        },
                                    },
                                ]),
                            vk::SubpassContents::INLINE,
                        );
                        device.device.cmd_set_viewport(
                            device.command_buffer,
                            0,
                            &[vk::Viewport::default()
                                .x(0.0)
                                .y(0.0)
                                .width(surface.vk_extent.width as f32)
                                .height(surface.vk_extent.height as f32)
                                .min_depth(0.0)
                                .max_depth(1.0)],
                        );
                        device.device.cmd_set_scissor(
                            device.command_buffer,
                            0,
                            &[vk::Rect2D {
                                offset: vk::Offset2D { x: 0, y: 0 },
                                extent: surface.vk_extent,
                            }],
                        );
                        device.device.cmd_bind_descriptor_sets(
                            device.command_buffer,
                            vk::PipelineBindPoint::GRAPHICS,
                            device.pipeline_layout,
                            0,
                            &[device.frame_descriptor_set],
                            &[],
                        );
                        let mut bound_pipeline = vk::Pipeline::null();
                        let mut bound_material = vk::DescriptorSet::null();
                        for item in frame.render_items.iter().filter(|item| {
                            self.materials[&item.material].alpha_mode != AlphaMode::Blend
                        }) {
                            let material = self.materials[&item.material];
                            let pipeline = gtao.prepass[pipeline_variant_index(
                                item.is_mirrored(),
                                material.double_sided,
                                false,
                            )];
                            if pipeline != bound_pipeline {
                                device.device.cmd_bind_pipeline(
                                    device.command_buffer,
                                    vk::PipelineBindPoint::GRAPHICS,
                                    pipeline,
                                );
                                bound_pipeline = pipeline;
                            }
                            let mesh = &device.meshes[&item.mesh];
                            let descriptor_set = device.materials[&item.material].descriptor_set;
                            if descriptor_set != bound_material {
                                device.device.cmd_bind_descriptor_sets(
                                    device.command_buffer,
                                    vk::PipelineBindPoint::GRAPHICS,
                                    device.pipeline_layout,
                                    1,
                                    &[descriptor_set],
                                    &[],
                                );
                                bound_material = descriptor_set;
                            }
                            device.device.cmd_push_constants(
                                device.command_buffer,
                                device.pipeline_layout,
                                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                                0,
                                bytes_of(&item.model_matrix),
                            );
                            device.device.cmd_bind_vertex_buffers(
                                device.command_buffer,
                                0,
                                &[mesh.vertices.buffer],
                                &[0],
                            );
                            device.device.cmd_bind_index_buffer(
                                device.command_buffer,
                                mesh.indices.buffer,
                                0,
                                vk::IndexType::UINT32,
                            );
                            device.device.cmd_draw_indexed(
                                device.command_buffer,
                                mesh.index_count,
                                1,
                                0,
                                0,
                                0,
                            );
                        }
                        device.device.cmd_end_render_pass(device.command_buffer);
                        if !graph_pass!(GtaoHorizon) {
                            return Err(VulkanError::Unsupported(
                                "GTAO horizon pass is missing".into(),
                            ));
                        }
                        draw_fullscreen(
                            device,
                            gtao.render_pass,
                            targets.raw_framebuffer,
                            vk::Extent2D {
                                width: surface.vk_extent.width.div_ceil(2),
                                height: surface.vk_extent.height.div_ceil(2),
                            },
                            gtao.horizon,
                            device.pipeline_layout,
                            device.frame_descriptor_set,
                            Some([1.0, 0.0, 0.0, 1.0]),
                            None,
                        );
                        if !graph_pass!(GtaoUpsample) {
                            return Err(VulkanError::Unsupported(
                                "GTAO upsample pass is missing".into(),
                            ));
                        }
                        draw_fullscreen(
                            device,
                            gtao.render_pass,
                            targets.full_framebuffer,
                            surface.vk_extent,
                            gtao.upsample,
                            device.pipeline_layout,
                            device.frame_descriptor_set,
                            Some([1.0, 0.0, 0.0, 1.0]),
                            None,
                        );
                    }
                }
                #[cfg(feature = "profiling")]
                write_gpu_timestamp(
                    device,
                    device.command_buffer,
                    2,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                );
                if !graph_pass!(Scene) {
                    return Err(VulkanError::Unsupported("scene pass is missing".into()));
                }
                let mut scene_clear_values = vec![
                    vk::ClearValue {
                        color: vk::ClearColorValue {
                            float32: scene_clear_color,
                        },
                    },
                    vk::ClearValue {
                        color: vk::ClearColorValue {
                            float32: [0.5, 0.5, 1.0, 1.0],
                        },
                    },
                ];
                #[cfg(feature = "ssr")]
                if device.features.ssr.is_some() {
                    scene_clear_values.push(vk::ClearValue {
                        color: vk::ClearColorValue { float32: [0.0; 4] },
                    });
                }
                scene_clear_values.push(vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue {
                        depth: 0.0,
                        stencil: 0,
                    },
                });
                device.device.cmd_begin_render_pass(
                    device.command_buffer,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(device.scene_render_pass)
                        .framebuffer(surface.scene.framebuffer)
                        .render_area(vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent: surface.vk_extent,
                        })
                        .clear_values(&scene_clear_values),
                    vk::SubpassContents::INLINE,
                );
                device.device.cmd_set_viewport(
                    device.command_buffer,
                    0,
                    &[vk::Viewport::default()
                        .x(0.0)
                        .y(0.0)
                        .width(surface.vk_extent.width as f32)
                        .height(surface.vk_extent.height as f32)
                        .min_depth(0.0)
                        .max_depth(1.0)],
                );
                device.device.cmd_set_scissor(
                    device.command_buffer,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: surface.vk_extent,
                    }],
                );
                #[cfg(feature = "ssr")]
                let blend_items_in_scene: &[&x173ge_protocol::RenderItem] =
                    if features.ssr.is_some() {
                        &[]
                    } else {
                        &blend_items
                    };
                #[cfg(not(feature = "ssr"))]
                let blend_items_in_scene = blend_items.as_slice();
                let draw_items = frame
                    .render_items
                    .iter()
                    .filter(|item| self.materials[&item.material].alpha_mode != AlphaMode::Blend)
                    .chain(blend_items_in_scene.iter().copied());
                draw_scene_items(device, draw_items, &self.materials);
                device.device.cmd_end_render_pass(device.command_buffer);
                #[cfg(feature = "profiling")]
                write_gpu_timestamp(
                    device,
                    device.command_buffer,
                    3,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                );
                if graph_pass!(Ssr) {
                    #[cfg(feature = "ssr")]
                    if let Some(settings) = features.ssr {
                        let targets = surface.ssr.as_ref().expect("SSR targets are configured");
                        draw_ssr(device, targets, surface.vk_extent, settings);
                        if !blend_items.is_empty() {
                            let pipelines =
                                device.ssr.as_ref().expect("SSR pipelines are configured");
                            device.device.cmd_begin_render_pass(
                                device.command_buffer,
                                &vk::RenderPassBeginInfo::default()
                                    .render_pass(pipelines.blend_render_pass)
                                    .framebuffer(targets.blend_framebuffer)
                                    .render_area(vk::Rect2D {
                                        offset: vk::Offset2D { x: 0, y: 0 },
                                        extent: surface.vk_extent,
                                    }),
                                vk::SubpassContents::INLINE,
                            );
                            device.device.cmd_set_viewport(
                                device.command_buffer,
                                0,
                                &[vk::Viewport::default()
                                    .x(0.0)
                                    .y(0.0)
                                    .width(surface.vk_extent.width as f32)
                                    .height(surface.vk_extent.height as f32)
                                    .min_depth(0.0)
                                    .max_depth(1.0)],
                            );
                            device.device.cmd_set_scissor(
                                device.command_buffer,
                                0,
                                &[vk::Rect2D {
                                    offset: vk::Offset2D { x: 0, y: 0 },
                                    extent: surface.vk_extent,
                                }],
                            );
                            draw_scene_items(device, blend_items.iter().copied(), &self.materials);
                            device.device.cmd_end_render_pass(device.command_buffer);
                        }
                    }
                }
                #[cfg(feature = "profiling")]
                write_gpu_timestamp(
                    device,
                    device.command_buffer,
                    4,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                );
                #[cfg(feature = "ssr")]
                #[allow(unused_mut)]
                let mut final_scene_view = if features.ssr.is_some() {
                    surface
                        .ssr
                        .as_ref()
                        .expect("SSR targets are configured")
                        .color
                        .view
                } else {
                    surface.scene.color.view
                };
                #[cfg(not(feature = "ssr"))]
                #[allow(unused_mut)]
                let mut final_scene_view = surface.scene.color.view;
                #[cfg(feature = "god-rays")]
                let _record_god_rays = graph_pass!(GodRaysMarch);
                #[cfg(feature = "god-rays")]
                if _record_god_rays {
                    let targets = surface
                        .god_rays
                        .as_ref()
                        .expect("God Rays targets are configured");
                    let pipelines = device
                        .god_rays
                        .as_ref()
                        .expect("God Rays pipelines are configured");
                    write_sampled_images(
                        &device.device,
                        targets.composite_set,
                        device.post_sampler,
                        &[final_scene_view, targets.scattering.view],
                    );
                    draw_fullscreen(
                        device,
                        pipelines.render_pass,
                        targets.march_framebuffer,
                        surface.vk_extent,
                        pipelines.march,
                        device.pipeline_layout,
                        device.frame_descriptor_set,
                        Some([0.0; 4]),
                        None,
                    );
                    #[cfg(feature = "profiling")]
                    write_gpu_timestamp(
                        device,
                        device.command_buffer,
                        5,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    );
                    if !graph_pass!(GodRaysHorizontal) {
                        return Err(VulkanError::Unsupported(
                            "God Rays horizontal pass is missing".into(),
                        ));
                    }
                    draw_fullscreen_sets(
                        device,
                        pipelines.render_pass,
                        targets.horizontal_framebuffer,
                        surface.vk_extent,
                        pipelines.horizontal,
                        device.god_rays_pipeline_layout,
                        &[device.frame_descriptor_set, targets.horizontal_set],
                        Some([0.0; 4]),
                        None,
                    );
                    #[cfg(feature = "profiling")]
                    write_gpu_timestamp(
                        device,
                        device.command_buffer,
                        6,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    );
                    if !graph_pass!(GodRaysVertical) {
                        return Err(VulkanError::Unsupported(
                            "God Rays vertical pass is missing".into(),
                        ));
                    }
                    draw_fullscreen_sets(
                        device,
                        pipelines.render_pass,
                        targets.scattering_framebuffer,
                        surface.vk_extent,
                        pipelines.vertical,
                        device.god_rays_pipeline_layout,
                        &[device.frame_descriptor_set, targets.vertical_set],
                        Some([0.0; 4]),
                        None,
                    );
                    final_scene_view = targets.march.view;
                    #[cfg(feature = "profiling")]
                    write_gpu_timestamp(
                        device,
                        device.command_buffer,
                        7,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    );
                    if !graph_pass!(GodRaysComposite) {
                        return Err(VulkanError::Unsupported(
                            "God Rays composite pass is missing".into(),
                        ));
                    }
                    draw_fullscreen_sets(
                        device,
                        pipelines.render_pass,
                        targets.march_framebuffer,
                        surface.vk_extent,
                        pipelines.composite,
                        device.god_rays_pipeline_layout,
                        &[device.frame_descriptor_set, targets.composite_set],
                        Some([0.0; 4]),
                        None,
                    );
                    #[cfg(feature = "profiling")]
                    write_gpu_timestamp(
                        device,
                        device.command_buffer,
                        8,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    );
                } else {
                    #[cfg(feature = "profiling")]
                    for query in 5..=8 {
                        write_gpu_timestamp(
                            device,
                            device.command_buffer,
                            query,
                            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                        );
                    }
                }
                #[cfg(not(feature = "god-rays"))]
                #[cfg(feature = "profiling")]
                for query in 5..=8 {
                    write_gpu_timestamp(
                        device,
                        device.command_buffer,
                        query,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    );
                }
                if !graph_pass!(Exposure) {
                    return Err(VulkanError::Unsupported("exposure pass is missing".into()));
                }
                let exposure = &surface.exposure;
                write_sampled_images(
                    &device.device,
                    exposure.pyramid[0].descriptor_set,
                    device.post_sampler,
                    &[final_scene_view, final_scene_view, final_scene_view],
                );
                for (index, level) in exposure.pyramid.iter().enumerate() {
                    draw_fullscreen(
                        device,
                        device.exposure_render_pass,
                        level.framebuffer,
                        level.extent,
                        device.exposure_reduce_pipeline,
                        device.post_pipeline_layout,
                        level.descriptor_set,
                        Some([0.0; 4]),
                        Some(bytes_of(&ExposurePush {
                            first: if index == 0 { 1.0 } else { 0.0 },
                            seconds: 0.0,
                            key_luminance: features.tone_mapping.key_luminance,
                            min_exposure: features.tone_mapping.min_exposure,
                            max_exposure: features.tone_mapping.max_exposure,
                            adaptation_seconds: features.tone_mapping.adaptation_seconds,
                        })),
                    );
                }
                let average_view = exposure
                    .pyramid
                    .last()
                    .expect("exposure reduction pyramid has a final level")
                    .image
                    .view;
                let previous_view = if exposure.history_valid {
                    exposure.history[exposure.history_index].view
                } else {
                    average_view
                };
                write_sampled_images(
                    &device.device,
                    exposure.adaptation_sets[exposure_write_index],
                    device.post_sampler,
                    &[average_view, previous_view, previous_view],
                );
                draw_fullscreen(
                    device,
                    device.exposure_render_pass,
                    exposure.history_framebuffers[exposure_write_index],
                    vk::Extent2D {
                        width: 1,
                        height: 1,
                    },
                    device.exposure_adapt_pipeline,
                    device.post_pipeline_layout,
                    exposure.adaptation_sets[exposure_write_index],
                    Some([0.0; 4]),
                    Some(bytes_of(&ExposurePush {
                        first: if exposure.history_valid { 0.0 } else { 1.0 },
                        seconds: exposure_seconds,
                        key_luminance: features.tone_mapping.key_luminance,
                        min_exposure: features.tone_mapping.min_exposure,
                        max_exposure: features.tone_mapping.max_exposure,
                        adaptation_seconds: features.tone_mapping.adaptation_seconds,
                    })),
                );
                #[cfg(feature = "profiling")]
                write_gpu_timestamp(
                    device,
                    device.command_buffer,
                    9,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                );
                #[cfg(feature = "bloom")]
                let post_views = {
                    let bloom_view = surface
                        .bloom
                        .as_ref()
                        .map_or(final_scene_view, BloomTargets::final_view);
                    [final_scene_view, bloom_view, exposure_view]
                };
                #[cfg(not(feature = "bloom"))]
                let post_views = [final_scene_view, final_scene_view, exposure_view];
                #[cfg(feature = "bloom")]
                let bloom_level_count = surface
                    .bloom
                    .as_ref()
                    .map_or(1, |targets| targets.levels.len().max(1))
                    as f32;
                #[cfg(feature = "bloom")]
                let (bloom_intensity, bloom_radius_uv) =
                    features.bloom.map_or((0.0, 0.0), |bloom| {
                        (bloom.intensity / bloom_level_count, bloom.radius_uv)
                    });
                #[cfg(not(feature = "bloom"))]
                let (bloom_intensity, bloom_radius_uv) = (0.0, 0.0);
                let post_constants = PostPush {
                    intensity: bloom_intensity,
                    radius_uv: bloom_radius_uv,
                    contrast: features.tone_mapping.contrast,
                };
                let post_push = Some(bytes_of(&post_constants));
                write_sampled_images(
                    &device.device,
                    surface.post_descriptor_set,
                    device.post_sampler,
                    &post_views,
                );
                if graph_pass!(BloomDown) {
                    #[cfg(feature = "bloom")]
                    if let Some(settings) = features.bloom {
                        let bloom_targets = surface
                            .bloom
                            .as_ref()
                            .expect("Bloom targets are configured");
                        let bloom = device
                            .bloom
                            .as_ref()
                            .expect("Bloom pipelines are configured");
                        for (index, level) in bloom_targets.levels.iter().enumerate() {
                            if index == 0 {
                                write_sampled_images(
                                    &device.device,
                                    level.down_set,
                                    device.post_sampler,
                                    &[final_scene_view, final_scene_view],
                                );
                            }
                            draw_fullscreen(
                                device,
                                bloom.render_pass,
                                level.down_framebuffer,
                                level.extent,
                                if index == 0 {
                                    bloom.first_downsample
                                } else {
                                    bloom.downsample
                                },
                                device.post_pipeline_layout,
                                level.down_set,
                                Some([0.0; 4]),
                                None,
                            );
                        }
                        if !graph_pass!(BloomUp) {
                            return Err(VulkanError::Unsupported(
                                "Bloom upsample pass is missing".into(),
                            ));
                        }
                        for index in (0..bloom_targets.levels.len().saturating_sub(1)).rev() {
                            let level = &bloom_targets.levels[index];
                            let up = level.up.as_ref().expect("upsample target is configured");
                            draw_fullscreen(
                                device,
                                bloom.render_pass,
                                up.framebuffer,
                                level.extent,
                                bloom.upsample,
                                device.post_pipeline_layout,
                                up.descriptor_set,
                                Some([0.0; 4]),
                                Some(bytes_of(&PostPush {
                                    intensity: 0.0,
                                    radius_uv: settings.radius_uv,
                                    contrast: features.tone_mapping.contrast,
                                })),
                            );
                        }
                        #[cfg(feature = "smaa")]
                        if smaa_enabled {
                            if !graph_pass!(BloomComposite) {
                                return Err(VulkanError::Unsupported(
                                    "Bloom composite pass is missing".into(),
                                ));
                            }
                            let smaa = surface.smaa.as_ref().expect("SMAA targets are configured");
                            draw_fullscreen(
                                device,
                                bloom.render_pass,
                                smaa.post_framebuffer,
                                surface.vk_extent,
                                bloom.composite,
                                device.post_pipeline_layout,
                                surface.post_descriptor_set,
                                Some([0.0; 4]),
                                post_push,
                            );
                        }
                    }
                }
                #[cfg(feature = "profiling")]
                write_gpu_timestamp(
                    device,
                    device.command_buffer,
                    10,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                );
                #[cfg(feature = "smaa")]
                if smaa_enabled {
                    let targets = surface.smaa.as_ref().expect("SMAA targets are configured");
                    let sets = surface
                        .smaa_descriptor_sets
                        .expect("SMAA descriptors are configured");
                    #[cfg(feature = "bloom")]
                    if features.bloom.is_none() {
                        if !graph_pass!(SmaaPost) {
                            return Err(VulkanError::Unsupported(
                                "SMAA post pass is missing".into(),
                            ));
                        }
                        draw_fullscreen(
                            device,
                            device.smaa_post_render_pass,
                            targets.post_framebuffer,
                            surface.vk_extent,
                            device.smaa_post_pipeline,
                            device.post_pipeline_layout,
                            surface.post_descriptor_set,
                            Some([0.0; 4]),
                            post_push,
                        );
                    }
                    #[cfg(not(feature = "bloom"))]
                    {
                        if !graph_pass!(SmaaPost) {
                            return Err(VulkanError::Unsupported(
                                "SMAA post pass is missing".into(),
                            ));
                        }
                        draw_fullscreen(
                            device,
                            device.smaa_post_render_pass,
                            targets.post_framebuffer,
                            surface.vk_extent,
                            device.smaa_post_pipeline,
                            device.post_pipeline_layout,
                            surface.post_descriptor_set,
                            Some([0.0; 4]),
                            post_push,
                        );
                    }
                    if !graph_pass!(SmaaEdges) {
                        return Err(VulkanError::Unsupported("SMAA edge pass is missing".into()));
                    }
                    draw_fullscreen(
                        device,
                        device.smaa_edge_render_pass,
                        targets.edge_framebuffer,
                        surface.vk_extent,
                        device.smaa_edge_pipeline,
                        device.post_pipeline_layout,
                        sets[0],
                        Some([0.0; 4]),
                        None,
                    );
                    if !graph_pass!(SmaaWeights) {
                        return Err(VulkanError::Unsupported(
                            "SMAA weights pass is missing".into(),
                        ));
                    }
                    draw_fullscreen(
                        device,
                        device.smaa_weights_render_pass,
                        targets.weight_framebuffer,
                        surface.vk_extent,
                        device.smaa_weights_pipeline,
                        device.smaa_weights_pipeline_layout,
                        sets[1],
                        Some([0.0; 4]),
                        None,
                    );
                    if !graph_pass!(Present) {
                        return Err(VulkanError::Unsupported("present pass is missing".into()));
                    }
                    draw_fullscreen(
                        device,
                        surface.render_pass,
                        surface.framebuffers[image_index as usize],
                        surface.vk_extent,
                        surface.smaa_pipeline.expect("SMAA pipeline is configured"),
                        device.smaa_neighborhood_pipeline_layout,
                        sets[2],
                        None,
                        None,
                    );
                }
                #[cfg(feature = "smaa")]
                if !smaa_enabled {
                    if !graph_pass!(Present) {
                        return Err(VulkanError::Unsupported("present pass is missing".into()));
                    }
                    draw_fullscreen(
                        device,
                        surface.render_pass,
                        surface.framebuffers[image_index as usize],
                        surface.vk_extent,
                        #[cfg(feature = "bloom")]
                        if features.bloom.is_some() {
                            surface
                                .bloom_pipeline
                                .expect("Bloom presentation pipeline is configured")
                        } else {
                            surface.pipeline
                        },
                        #[cfg(not(feature = "bloom"))]
                        surface.pipeline,
                        device.post_pipeline_layout,
                        surface.post_descriptor_set,
                        None,
                        post_push,
                    );
                }
                #[cfg(not(feature = "smaa"))]
                if !graph_pass!(Present) {
                    return Err(VulkanError::Unsupported("present pass is missing".into()));
                }
                #[cfg(not(feature = "smaa"))]
                draw_fullscreen(
                    device,
                    surface.render_pass,
                    surface.framebuffers[image_index as usize],
                    surface.vk_extent,
                    #[cfg(feature = "bloom")]
                    if features.bloom.is_some() {
                        surface
                            .bloom_pipeline
                            .expect("Bloom presentation pipeline is configured")
                    } else {
                        surface.pipeline
                    },
                    #[cfg(not(feature = "bloom"))]
                    surface.pipeline,
                    device.post_pipeline_layout,
                    surface.post_descriptor_set,
                    None,
                    post_push,
                );
                #[cfg(feature = "profiling")]
                write_gpu_timestamp(
                    device,
                    device.command_buffer,
                    11,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                );
                graph
                    .finish()
                    .map_err(|error| VulkanError::Unsupported(error.to_string()))?;
                api_call(
                    "end_command_buffer(frame)",
                    device.device.end_command_buffer(device.command_buffer),
                )?;

                let waits = [device.image_available];
                let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
                let commands = [device.command_buffer];
                let signals = [surface.present_semaphores[image_index as usize]];
                let submits = [vk::SubmitInfo::default()
                    .wait_semaphores(&waits)
                    .wait_dst_stage_mask(&stages)
                    .command_buffers(&commands)
                    .signal_semaphores(&signals)];
                api_call(
                    "queue_submit(frame)",
                    device
                        .device
                        .queue_submit(device.queue, &submits, device.fence),
                )?;
                surface.exposure.history_index = exposure_write_index;
                surface.exposure.history_valid = true;
                surface.exposure.last_submitted = Some(Instant::now());
                #[cfg(feature = "profiling")]
                if let Some(profile) = device.profile.as_mut() {
                    profile.pending_frame = Some(frame.id.get());
                    profile.record_submit_ms =
                        Some(record_started.elapsed().as_secs_f64() * 1000.0);
                }
                let swapchains = [surface.swapchain];
                let indices = [image_index];
                let presents = vk::PresentInfoKHR::default()
                    .wait_semaphores(&signals)
                    .swapchains(&swapchains)
                    .image_indices(&indices);
                match device.swapchain.queue_present(device.queue, &presents) {
                    Ok(present_suboptimal) if !suboptimal && !present_suboptimal => {
                        Ok(PresentResult::Presented)
                    }
                    Ok(_) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                        Ok(PresentResult::SurfaceInvalidated)
                    }
                    Err(error) => Err(VulkanError::ApiAt {
                        operation: "queue_present",
                        result: error,
                    }),
                }
            }
        }
    }

    impl RendererBackend for VulkanRenderer {
        fn configure_features(&mut self, features: RenderFeatures) -> Result<(), BackendError> {
            let requested = [
                ("smaa", features.smaa, cfg!(feature = "smaa")),
                ("gtao", features.gtao, cfg!(feature = "gtao")),
                ("ssr", features.ssr.is_some(), cfg!(feature = "ssr")),
                ("bloom", features.bloom.is_some(), cfg!(feature = "bloom")),
                (
                    "god-rays",
                    features.god_rays.is_some(),
                    cfg!(feature = "god-rays"),
                ),
                (
                    "translucent-shadows",
                    features.translucent_shadows,
                    cfg!(feature = "translucent-shadows"),
                ),
            ];
            if let Some((name, _, _)) = requested
                .iter()
                .find(|(_, enabled, compiled)| *enabled && !*compiled)
            {
                return Err(BackendError(format!(
                    "render effect `{name}` was requested but is not compiled; enable its Cargo feature"
                )));
            }
            if let Some(settings) = features.bloom {
                settings
                    .validate()
                    .map_err(|reason| BackendError(reason.into()))?;
            }
            if let Some(settings) = features.ssr {
                settings
                    .validate()
                    .map_err(|reason| BackendError(reason.into()))?;
            }
            if let Some(settings) = features.god_rays {
                settings
                    .validate()
                    .map_err(|reason| BackendError(reason.into()))?;
            }
            self.features = features;
            Ok(())
        }

        fn uploads_ready(&self) -> bool {
            self.device.is_some()
        }

        /// Configures a surface after the renderer service validates its generation.
        fn configure_surface(&mut self, surface: SurfaceDescriptor) -> Result<(), BackendError> {
            self.configure(surface)
                .map_err(|error| BackendError(error.to_string()))
        }

        /// Draws and presents one owned frame.
        fn render(
            &mut self,
            frame: &FrameSnapshot,
            features: RenderFeatures,
        ) -> Result<PresentResult, BackendError> {
            self.present(frame, features)
                .map_err(|error| BackendError(error.to_string()))
        }

        /// Uploads a mesh through staging buffers and atomically replaces its previous GPU copy.
        fn upload_mesh(&mut self, upload: MeshUpload) -> Result<(), BackendError> {
            let bounds = Aabb::from_vertices(upload.data.vertices())
                .ok_or_else(|| BackendError("mesh has no vertices".into()))?;
            let (vertices, indices) = {
                let device = self.device_mut()?;
                // Vertex is repr(C) and contains only initialized f32 arrays, with no padding.
                let vertex_bytes = unsafe {
                    std::slice::from_raw_parts(
                        upload.data.vertices().as_ptr().cast::<u8>(),
                        std::mem::size_of_val(upload.data.vertices()),
                    )
                };
                let index_bytes = unsafe {
                    std::slice::from_raw_parts(
                        upload.data.indices().as_ptr().cast::<u8>(),
                        std::mem::size_of_val(upload.data.indices()),
                    )
                };
                let vertices =
                    upload_buffer(device, vertex_bytes, vk::BufferUsageFlags::VERTEX_BUFFER)
                        .map_err(|error| BackendError(error.to_string()))?;
                let indices =
                    match upload_buffer(device, index_bytes, vk::BufferUsageFlags::INDEX_BUFFER) {
                        Ok(buffer) => buffer,
                        Err(error) => {
                            destroy_buffer(&device.device, vertices);
                            return Err(BackendError(error.to_string()));
                        }
                    };
                (vertices, indices)
            };
            let mesh = GpuMesh {
                vertices,
                indices,
                index_count: upload.data.indices().len() as u32,
                bounds,
                has_tangents: upload.data.has_tangents(),
            };
            if let Some(old) = self.device_mut()?.meshes.insert(upload.id, mesh) {
                let device = &self.device_mut()?.device;
                destroy_buffer(device, old.vertices);
                destroy_buffer(device, old.indices);
            }
            Ok(())
        }

        fn upload_image(&mut self, upload: ImageUpload) -> Result<(), BackendError> {
            upload
                .validate()
                .map_err(|reason| BackendError(reason.into()))?;
            let device = self.device_mut()?;
            if device.images.contains_key(&upload.id) {
                return Err(BackendError("image IDs are immutable after upload".into()));
            }
            let image = upload_rgba8_image(
                device,
                Some(upload.id),
                upload.extent,
                &upload.rgba8,
                upload.color_space,
            )
            .map_err(|error| BackendError(error.to_string()))?;
            device.images.insert(upload.id, image);
            Ok(())
        }

        fn upload_environment(&mut self, upload: EnvironmentUpload) -> Result<(), BackendError> {
            #[cfg(feature = "profiling")]
            let started = Instant::now();
            upload
                .validate()
                .map_err(|reason| BackendError(reason.into()))?;
            let device = self.device_mut()?;
            if device.environments.contains_key(&upload.id) {
                return Err(BackendError(
                    "environment IDs are immutable after upload".into(),
                ));
            }
            device
                .environments
                .insert(upload.id, upload.diffuse_irradiance_sh);
            #[cfg(feature = "profiling")]
            eprintln!(
                "X173GE_UPLOAD stage=environment id={} diffuse_sh_bytes={} total_ms={:.3}",
                upload.id.get(),
                std::mem::size_of_val(&upload.diffuse_irradiance_sh),
                started.elapsed().as_secs_f64() * 1000.0,
            );
            Ok(())
        }

        /// Creates a material descriptor set after its referenced images are ready.
        fn upload_material(&mut self, upload: MaterialUpload) -> Result<(), BackendError> {
            #[cfg(feature = "profiling")]
            let started = Instant::now();
            upload
                .data
                .validate()
                .map_err(|reason| BackendError(reason.into()))?;
            let device = self.device_mut()?;
            if !device.materials.contains_key(&upload.id)
                && device.materials.len() == MAX_GPU_MATERIALS as usize
            {
                return Err(BackendError(
                    "Vulkan material descriptor limit reached".into(),
                ));
            }
            if device.fallback_image.is_none() {
                device.fallback_image = Some(
                    upload_rgba8_image(
                        device,
                        None,
                        x173ge_core::Extent2d::new(1, 1).unwrap(),
                        &[255; 4],
                        ImageColorSpace::Srgb,
                    )
                    .map_err(|error| BackendError(error.to_string()))?,
                );
            }
            let base_color = texture_descriptor_info(device, upload.data.base_color_texture, true)
                .map_err(|error| BackendError(error.to_string()))?;
            let metallic_roughness =
                texture_descriptor_info(device, upload.data.metallic_roughness_texture, false)
                    .map_err(|error| BackendError(error.to_string()))?;
            let normal = texture_descriptor_info(device, upload.data.normal_texture, false)
                .map_err(|error| BackendError(error.to_string()))?;
            let occlusion = texture_descriptor_info(device, upload.data.occlusion_texture, false)
                .map_err(|error| BackendError(error.to_string()))?;
            let emissive = texture_descriptor_info(device, upload.data.emissive_texture, true)
                .map_err(|error| BackendError(error.to_string()))?;
            let transmission =
                texture_descriptor_info(device, upload.data.transmission_texture, false)
                    .map_err(|error| BackendError(error.to_string()))?;
            let uniform_data = make_material_uniform(upload.data);
            let uniform = create_buffer(
                &device.device,
                &device.memory,
                &device.memory_stats,
                size_of::<MaterialUniform>() as u64,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                vk::MemoryPropertyFlags::HOST_VISIBLE,
                vk::MemoryPropertyFlags::HOST_COHERENT,
            )
            .map_err(|error| BackendError(error.to_string()))?;
            if let Err(error) = write_buffer(&device.device, &uniform, bytes_of(&uniform_data)) {
                destroy_buffer(&device.device, uniform);
                return Err(BackendError(error.to_string()));
            }
            #[cfg(feature = "profiling")]
            let wait_started = Instant::now();
            if let Err(error) = unsafe {
                device
                    .device
                    .wait_for_fences(&[device.fence], true, u64::MAX)
            } {
                destroy_buffer(&device.device, uniform);
                return Err(BackendError(error.to_string()));
            }
            #[cfg(feature = "profiling")]
            let wait_ms = wait_started.elapsed().as_secs_f64() * 1000.0;
            let existing = device
                .materials
                .get(&upload.id)
                .map(|material| material.descriptor_set);
            let descriptor_set = match existing {
                Some(set) => set,
                None => match unsafe {
                    device.device.allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(device.descriptor_pool)
                            .set_layouts(&[device.material_descriptor_set_layout]),
                    )
                } {
                    Ok(sets) => sets[0],
                    Err(error) => {
                        destroy_buffer(&device.device, uniform);
                        return Err(BackendError(error.to_string()));
                    }
                },
            };
            let buffer_info = [vk::DescriptorBufferInfo::default()
                .buffer(uniform.buffer)
                .offset(0)
                .range(size_of::<MaterialUniform>() as u64)];
            let image_infos = [base_color, metallic_roughness, normal, occlusion, emissive];
            let mut writes = image_infos
                .iter()
                .enumerate()
                .map(|(binding, info)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(descriptor_set)
                        .dst_binding(binding as u32)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(std::slice::from_ref(info))
                })
                .collect::<Vec<_>>();
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set)
                    .dst_binding(5)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&buffer_info),
            );
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set)
                    .dst_binding(6)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(&transmission)),
            );
            unsafe {
                device.device.update_descriptor_sets(&writes, &[]);
            }
            let old = device.materials.insert(
                upload.id,
                GpuMaterial {
                    descriptor_set,
                    uniform,
                },
            );
            if let Some(old) = old {
                destroy_buffer(&device.device, old.uniform);
            }
            self.materials.insert(upload.id, upload.data);
            #[cfg(feature = "profiling")]
            eprintln!(
                "X173GE_UPLOAD stage=material total_ms={:.3} fence_wait_ms={wait_ms:.3}",
                started.elapsed().as_secs_f64() * 1000.0
            );
            Ok(())
        }
    }

    impl Drop for VulkanRenderer {
        /// Destroys image resources before their device, native surfaces and instance.
        fn drop(&mut self) {
            if let Some(device) = self.device.take() {
                #[cfg(feature = "profiling")]
                let mut device = device;
                unsafe {
                    if device.device.device_wait_idle().is_ok() {
                        #[cfg(feature = "profiling")]
                        if let Err(error) = finish_gpu_profile(&mut device) {
                            eprintln!("X173GE_GPU profile_error={error}");
                        }
                    }
                    #[cfg(feature = "profiling")]
                    if let Some(profile) = &device.profile {
                        if let Some(pool) = profile.pool {
                            device.device.destroy_query_pool(pool, None);
                        }
                    }
                    for (_, surface) in self.surfaces.drain() {
                        let handle = surface.handle;
                        destroy_swapchain(&device, surface);
                        self.surface.destroy_surface(handle, None);
                    }
                    for (_, material) in device.materials {
                        destroy_buffer(&device.device, material.uniform);
                    }
                    device
                        .device
                        .destroy_descriptor_pool(device.descriptor_pool, None);
                    for pipeline in device.shadow_pipelines {
                        device.device.destroy_pipeline(pipeline, None);
                    }
                    for pipeline in device.scene_pipelines {
                        device.device.destroy_pipeline(pipeline, None);
                    }
                    device
                        .device
                        .destroy_pipeline(device.exposure_reduce_pipeline, None);
                    device
                        .device
                        .destroy_pipeline(device.exposure_adapt_pipeline, None);
                    #[cfg(feature = "smaa")]
                    for pipeline in [
                        device.smaa_post_pipeline,
                        device.smaa_edge_pipeline,
                        device.smaa_weights_pipeline,
                    ] {
                        device.device.destroy_pipeline(pipeline, None);
                    }
                    #[cfg(feature = "bloom")]
                    if let Some(bloom) = device.bloom {
                        for pipeline in [
                            bloom.first_downsample,
                            bloom.downsample,
                            bloom.upsample,
                            bloom.composite,
                        ] {
                            device.device.destroy_pipeline(pipeline, None);
                        }
                        device.device.destroy_render_pass(bloom.render_pass, None);
                    }
                    #[cfg(feature = "gtao")]
                    if let Some(gtao) = device.gtao {
                        destroy_gtao_pipelines(&device.device, gtao);
                    }
                    #[cfg(feature = "ssr")]
                    if let Some(ssr) = device.ssr {
                        destroy_ssr_pipelines(&device.device, ssr);
                    }
                    #[cfg(feature = "god-rays")]
                    if let Some(pipelines) = device.god_rays {
                        destroy_god_rays_pipelines(&device.device, pipelines);
                    }
                    #[cfg(feature = "translucent-shadows")]
                    {
                        for map in device.mboit_maps {
                            destroy_mboit_map(&device.device, map);
                        }
                        if let Some(pipelines) = device.mboit {
                            destroy_mboit_pipelines(&device.device, pipelines);
                        }
                    }
                    for map in device.shadow_maps {
                        destroy_shadow_map(&device.device, map);
                    }
                    device
                        .device
                        .destroy_render_pass(device.scene_render_pass, None);
                    device
                        .device
                        .destroy_render_pass(device.exposure_render_pass, None);
                    device
                        .device
                        .destroy_render_pass(device.shadow_render_pass, None);
                    #[cfg(feature = "smaa")]
                    for render_pass in [
                        device.smaa_post_render_pass,
                        device.smaa_edge_render_pass,
                        device.smaa_weights_render_pass,
                    ] {
                        device.device.destroy_render_pass(render_pass, None);
                    }
                    device.device.destroy_sampler(device.shadow_sampler, None);
                    device
                        .device
                        .destroy_sampler(device.shadow_compare_sampler, None);
                    device.device.destroy_sampler(device.post_sampler, None);
                    for (_, mesh) in device.meshes {
                        destroy_buffer(&device.device, mesh.vertices);
                        destroy_buffer(&device.device, mesh.indices);
                    }
                    for (_, image) in device.images {
                        destroy_image(&device.device, image);
                    }
                    if let Some(image) = device.fallback_image {
                        destroy_image(&device.device, image);
                    }
                    #[cfg(feature = "smaa")]
                    if let Some(lookups) = device.smaa_lookups {
                        destroy_image(&device.device, lookups.area);
                        destroy_image(&device.device, lookups.search);
                    }
                    for (_, sampler) in device.samplers {
                        device.device.destroy_sampler(sampler, None);
                    }
                    destroy_buffer(&device.device, device.frame_uniform);
                    device
                        .device
                        .destroy_pipeline_layout(device.pipeline_layout, None);
                    device
                        .device
                        .destroy_pipeline_layout(device.post_pipeline_layout, None);
                    #[cfg(feature = "god-rays")]
                    device
                        .device
                        .destroy_pipeline_layout(device.god_rays_pipeline_layout, None);
                    #[cfg(feature = "smaa")]
                    for layout in [
                        device.smaa_weights_pipeline_layout,
                        device.smaa_neighborhood_pipeline_layout,
                    ] {
                        device.device.destroy_pipeline_layout(layout, None);
                    }
                    device
                        .device
                        .destroy_descriptor_set_layout(device.frame_descriptor_set_layout, None);
                    device
                        .device
                        .destroy_descriptor_set_layout(device.material_descriptor_set_layout, None);
                    device
                        .device
                        .destroy_descriptor_set_layout(device.post_descriptor_set_layout, None);
                    #[cfg(feature = "smaa")]
                    device.device.destroy_descriptor_set_layout(
                        device.smaa_weights_descriptor_set_layout,
                        None,
                    );
                    device
                        .device
                        .destroy_semaphore(device.image_available, None);
                    device.device.destroy_fence(device.fence, None);
                    device
                        .device
                        .destroy_command_pool(device.command_pool, None);
                    device.device.destroy_device(None);
                    device.memory_stats.log_shutdown();
                }
            } else {
                for (_, surface) in self.surfaces.drain() {
                    unsafe { self.surface.destroy_surface(surface.handle, None) };
                }
            }
            if let Some((loader, messenger)) = self.debug.take() {
                unsafe { loader.destroy_debug_utils_messenger(messenger, None) };
            }
            unsafe { self.instance.destroy_instance(None) };
        }
    }

    /// A completely constructed swapchain and its render targets.
    struct SwapchainResources {
        swapchain: vk::SwapchainKHR,
        extent: vk::Extent2D,
        views: Vec<vk::ImageView>,
        render_pass: vk::RenderPass,
        framebuffers: Vec<vk::Framebuffer>,
        present_semaphores: Vec<vk::Semaphore>,
        scene: SceneTargets,
        exposure: ExposureTargets,
        #[cfg(feature = "ssr")]
        ssr: Option<SsrTargets>,
        #[cfg(feature = "god-rays")]
        god_rays: Option<GodRaysTargets>,
        post_descriptor_pool: vk::DescriptorPool,
        post_descriptor_set: vk::DescriptorSet,
        pipeline: vk::Pipeline,
        #[cfg(feature = "bloom")]
        bloom_pipeline: Option<vk::Pipeline>,
        #[cfg(feature = "smaa")]
        smaa_pipeline: Option<vk::Pipeline>,
        #[cfg(feature = "smaa")]
        smaa: Option<SmaaTargets>,
        #[cfg(feature = "smaa")]
        smaa_descriptor_sets: Option<[vk::DescriptorSet; 3]>,
        #[cfg(feature = "bloom")]
        bloom: Option<BloomTargets>,
        #[cfg(feature = "gtao")]
        gtao: Option<GtaoTargets>,
    }

    struct SwapchainFailure {
        error: VulkanError,
        old_swapchain_retired: bool,
    }

    impl From<VulkanError> for SwapchainFailure {
        fn from(error: VulkanError) -> Self {
            Self {
                error,
                old_swapchain_retired: false,
            }
        }
    }

    impl From<vk::Result> for SwapchainFailure {
        fn from(error: vk::Result) -> Self {
            VulkanError::from(error).into()
        }
    }

    /// Creates logical-device children and frees partial state when initialization fails.
    fn create_device(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
        queue_family: u32,
        features: RenderFeatures,
    ) -> Result<DeviceState, VulkanError> {
        let depth_format = choose_depth_format(instance, physical)?;
        let scene_color_format = choose_scene_color_format(instance, physical)?;
        let normal_roughness_format = choose_normal_format(instance, physical)?;
        #[cfg(feature = "translucent-shadows")]
        let mboit_format = features
            .translucent_shadows
            .then(|| choose_mboit_format(instance, physical))
            .transpose()?;
        let mip_features = vk::FormatFeatureFlags::BLIT_SRC
            | vk::FormatFeatureFlags::BLIT_DST
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
        let mipmaps_supported = [vk::Format::R8G8B8A8_UNORM, vk::Format::R8G8B8A8_SRGB]
            .into_iter()
            .all(|format| {
                unsafe { instance.get_physical_device_format_properties(physical, format) }
                    .optimal_tiling_features
                    .contains(mip_features)
            });
        let shadow_depth_format = depth_format;
        if features.translucent_shadows {
            let required_samplers = 4 + 4 + u32::from(features.ssr.is_some()) + 4 + 8 + 6;
            let limits = unsafe { instance.get_physical_device_properties(physical) }.limits;
            if limits.max_per_stage_descriptor_samplers < required_samplers
                || limits.max_descriptor_set_samplers < required_samplers
            {
                return Err(VulkanError::Unsupported(format!(
                    "MBOIT needs {required_samplers} fragment samplers, device limits are {}/{}",
                    limits.max_per_stage_descriptor_samplers, limits.max_descriptor_set_samplers,
                )));
            }
        }
        let memory_stats = Arc::new(GpuMemoryStats::default());
        let priorities = [1.0];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let extensions = [swapchain::NAME.as_ptr()];
        let enabled_features = vk::PhysicalDeviceFeatures::default()
            .independent_blend(cfg!(feature = "ssr") && features.ssr.is_some());
        let info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&extensions)
            .enabled_features(&enabled_features);
        let device = unsafe { instance.create_device(physical, &info, None)? };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        #[cfg(feature = "profiling")]
        let timestamp_valid_bits = unsafe {
            instance.get_physical_device_queue_family_properties(physical)[queue_family as usize]
                .timestamp_valid_bits
        };
        #[cfg(feature = "profiling")]
        let timestamp_period_ns = unsafe { instance.get_physical_device_properties(physical) }
            .limits
            .timestamp_period;
        #[cfg(feature = "profiling")]
        let mut profile = None;
        let mut command_pool = vk::CommandPool::null();
        let mut fence = vk::Fence::null();
        let mut image_available = vk::Semaphore::null();
        let mut frame_uniform = GpuBuffer {
            buffer: vk::Buffer::null(),
            memory: GpuMemory::empty(),
            flags: vk::MemoryPropertyFlags::empty(),
        };
        let mut frame_descriptor_set_layout = vk::DescriptorSetLayout::null();
        let mut material_descriptor_set_layout = vk::DescriptorSetLayout::null();
        let mut post_descriptor_set_layout = vk::DescriptorSetLayout::null();
        #[cfg(feature = "smaa")]
        let mut smaa_weights_descriptor_set_layout = vk::DescriptorSetLayout::null();
        let mut descriptor_pool = vk::DescriptorPool::null();
        let mut pipeline_layout = vk::PipelineLayout::null();
        let mut post_pipeline_layout = vk::PipelineLayout::null();
        #[cfg(feature = "god-rays")]
        let mut god_rays_pipeline_layout = vk::PipelineLayout::null();
        #[cfg(feature = "smaa")]
        let mut smaa_weights_pipeline_layout = vk::PipelineLayout::null();
        #[cfg(feature = "smaa")]
        let mut smaa_neighborhood_pipeline_layout = vk::PipelineLayout::null();
        let mut scene_render_pass = vk::RenderPass::null();
        let mut exposure_render_pass = vk::RenderPass::null();
        let mut exposure_reduce_pipeline = vk::Pipeline::null();
        let mut exposure_adapt_pipeline = vk::Pipeline::null();
        #[cfg(feature = "ssr")]
        let mut ssr = None;
        #[cfg(feature = "god-rays")]
        let mut god_rays = None;
        let mut scene_pipelines = [vk::Pipeline::null(); 8];
        let mut shadow_render_pass = vk::RenderPass::null();
        #[cfg(feature = "translucent-shadows")]
        let mut mboit = None;
        #[cfg(feature = "smaa")]
        let mut smaa_post_render_pass = vk::RenderPass::null();
        #[cfg(feature = "smaa")]
        let mut smaa_edge_render_pass = vk::RenderPass::null();
        #[cfg(feature = "smaa")]
        let mut smaa_weights_render_pass = vk::RenderPass::null();
        #[cfg(feature = "smaa")]
        let mut smaa_post_pipeline = vk::Pipeline::null();
        #[cfg(feature = "smaa")]
        let mut smaa_edge_pipeline = vk::Pipeline::null();
        #[cfg(feature = "smaa")]
        let mut smaa_weights_pipeline = vk::Pipeline::null();
        #[cfg(feature = "bloom")]
        let mut bloom_render_pass = vk::RenderPass::null();
        #[cfg(feature = "bloom")]
        let mut bloom_first_downsample = vk::Pipeline::null();
        #[cfg(feature = "bloom")]
        let mut bloom_downsample = vk::Pipeline::null();
        #[cfg(feature = "bloom")]
        let mut bloom_upsample = vk::Pipeline::null();
        #[cfg(feature = "bloom")]
        let mut bloom_composite = vk::Pipeline::null();
        #[cfg(feature = "gtao")]
        let mut gtao = None;
        let mut shadow_maps = Vec::new();
        let mut shadow_sampler = vk::Sampler::null();
        let mut shadow_compare_sampler = vk::Sampler::null();
        let mut post_sampler = vk::Sampler::null();
        let mut shadow_pipelines = [vk::Pipeline::null(); 4];
        let mut frame_descriptor_set = vk::DescriptorSet::null();
        let resources = (|| {
            command_pool = unsafe {
                device.create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(queue_family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )?
            };
            let command_buffer = unsafe {
                device.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(command_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )?[0]
            };
            fence = unsafe {
                device.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )?
            };
            #[cfg(feature = "profiling")]
            {
                let pool = if timestamp_valid_bits == 0 {
                    eprintln!("X173GE_GPU gpu_timing=unavailable reason=timestamp_queries");
                    None
                } else {
                    Some(unsafe {
                        device.create_query_pool(
                            &vk::QueryPoolCreateInfo::default()
                                .query_type(vk::QueryType::TIMESTAMP)
                                .query_count(12),
                            None,
                        )?
                    })
                };
                profile = Some(GpuProfile {
                    pool,
                    valid_bits: timestamp_valid_bits,
                    period_ns: timestamp_period_ns,
                    pending_frame: None,
                    record_submit_ms: None,
                });
            }
            image_available =
                unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
            frame_uniform = create_buffer(
                &device,
                &memory,
                &memory_stats,
                size_of::<FrameUniform>() as u64,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                vk::MemoryPropertyFlags::HOST_VISIBLE,
                vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            frame_descriptor_set_layout = unsafe {
                let bindings = std::iter::once(
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(0)
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
                )
                .chain((1..=CASCADE_COUNT as u32).map(|binding| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(binding)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                }))
                .chain(
                    (5..=(8 + u32::from(features.ssr.is_some() && cfg!(feature = "ssr")))).map(
                        |binding| {
                            vk::DescriptorSetLayoutBinding::default()
                                .binding(binding)
                                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                                .descriptor_count(1)
                                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                        },
                    ),
                )
                .chain(
                    (10..=17)
                        .filter(|_| features.translucent_shadows)
                        .map(|binding| {
                            vk::DescriptorSetLayoutBinding::default()
                                .binding(binding)
                                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                                .descriptor_count(1)
                                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                        }),
                )
                .chain((18..=21).map(|binding| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(binding)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                }))
                .collect::<Vec<_>>();
                device.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )?
            };
            material_descriptor_set_layout = unsafe {
                let bindings = (0..5)
                    .map(|binding| {
                        vk::DescriptorSetLayoutBinding::default()
                            .binding(binding)
                            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                            .descriptor_count(1)
                            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                    })
                    .chain([
                        vk::DescriptorSetLayoutBinding::default()
                            .binding(5)
                            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                            .descriptor_count(1)
                            .stage_flags(vk::ShaderStageFlags::FRAGMENT),
                        vk::DescriptorSetLayoutBinding::default()
                            .binding(6)
                            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                            .descriptor_count(1)
                            .stage_flags(vk::ShaderStageFlags::FRAGMENT),
                    ])
                    .collect::<Vec<_>>();
                device.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )?
            };
            post_descriptor_set_layout = create_sampler_set_layout(&device, 3)?;
            #[cfg(feature = "smaa")]
            {
                smaa_weights_descriptor_set_layout = create_sampler_set_layout(&device, 3)?;
            }
            descriptor_pool = unsafe {
                device.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                        .max_sets(MAX_GPU_MATERIALS + 1)
                        .pool_sizes(&[
                            vk::DescriptorPoolSize::default()
                                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                                .descriptor_count(
                                    MAX_GPU_MATERIALS * 6
                                        + CASCADE_COUNT as u32
                                        + 4
                                        + u32::from(features.ssr.is_some())
                                        + 1
                                        + 8 * u32::from(features.translucent_shadows)
                                        + CASCADE_COUNT as u32,
                                ),
                            vk::DescriptorPoolSize::default()
                                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                                .descriptor_count(MAX_GPU_MATERIALS + 1),
                        ]),
                    None,
                )?
            };
            pipeline_layout = unsafe {
                device.create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&[frame_descriptor_set_layout, material_descriptor_set_layout])
                        .push_constant_ranges(&[vk::PushConstantRange::default()
                            .stage_flags(
                                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            )
                            .offset(0)
                            .size(size_of::<ShadowPush>() as u32)]),
                    None,
                )?
            };
            let post_layouts = [post_descriptor_set_layout];
            let post_push_range = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .offset(0)
                .size(size_of::<ExposurePush>() as u32)];
            let post_layout = vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&post_layouts)
                .push_constant_ranges(&post_push_range);
            post_pipeline_layout = unsafe { device.create_pipeline_layout(&post_layout, None)? };
            #[cfg(feature = "god-rays")]
            if features.god_rays.is_some() {
                god_rays_pipeline_layout = unsafe {
                    device.create_pipeline_layout(
                        &vk::PipelineLayoutCreateInfo::default().set_layouts(&[
                            frame_descriptor_set_layout,
                            post_descriptor_set_layout,
                        ]),
                        None,
                    )?
                };
                god_rays = Some(create_god_rays_pipelines(
                    &device,
                    scene_color_format,
                    pipeline_layout,
                    god_rays_pipeline_layout,
                    features.translucent_shadows,
                )?);
            }
            #[cfg(feature = "smaa")]
            {
                smaa_weights_pipeline_layout = unsafe {
                    device.create_pipeline_layout(
                        &vk::PipelineLayoutCreateInfo::default()
                            .set_layouts(&[smaa_weights_descriptor_set_layout]),
                        None,
                    )?
                };
                smaa_neighborhood_pipeline_layout = unsafe {
                    device.create_pipeline_layout(
                        &vk::PipelineLayoutCreateInfo::default()
                            .set_layouts(&[post_descriptor_set_layout]),
                        None,
                    )?
                };
            }
            scene_render_pass = create_scene_render_pass(
                &device,
                scene_color_format,
                normal_roughness_format,
                depth_format,
                features.ssr.is_some(),
            )?;
            scene_pipelines = create_scene_pipelines(
                &device,
                scene_render_pass,
                pipeline_layout,
                features.ssr.is_some(),
                features.translucent_shadows,
            )?;
            #[cfg(feature = "ssr")]
            if features.ssr.is_some() {
                ssr = Some(create_ssr_pipelines(
                    &device,
                    scene_color_format,
                    normal_roughness_format,
                    depth_format,
                    frame_descriptor_set_layout,
                    post_descriptor_set_layout,
                )?);
            }
            #[cfg(feature = "gtao")]
            if features.gtao {
                gtao = Some(create_gtao_pipelines(
                    &device,
                    normal_roughness_format,
                    depth_format,
                    pipeline_layout,
                )?);
            }
            shadow_render_pass = create_shadow_render_pass(&device, shadow_depth_format)?;
            #[cfg(feature = "translucent-shadows")]
            if let Some(format) = mboit_format {
                mboit = Some(create_mboit_pipelines(&device, format, pipeline_layout)?);
            }
            shadow_maps.push(create_shadow_map(
                &device,
                &memory,
                &memory_stats,
                shadow_depth_format,
                shadow_render_pass,
                1,
            )?);
            shadow_sampler = create_sampler(
                &device,
                TextureSampler {
                    min_filter: TextureFilter::Nearest,
                    mag_filter: TextureFilter::Nearest,
                    mipmap_mode: MipmapMode::None,
                    wrap_u: TextureWrap::ClampToEdge,
                    wrap_v: TextureWrap::ClampToEdge,
                },
            )?;
            shadow_compare_sampler = create_shadow_compare_sampler(&device)?;
            post_sampler = create_sampler(
                &device,
                TextureSampler {
                    min_filter: TextureFilter::Linear,
                    mag_filter: TextureFilter::Linear,
                    mipmap_mode: MipmapMode::None,
                    wrap_u: TextureWrap::ClampToEdge,
                    wrap_v: TextureWrap::ClampToEdge,
                },
            )?;
            exposure_render_pass = create_sampled_color_render_pass(&device, scene_color_format)?;
            exposure_reduce_pipeline = create_fullscreen_pipeline(
                &device,
                exposure_render_pass,
                post_pipeline_layout,
                include_bytes!("../shaders/exposure_reduce.frag.spv"),
            )?;
            exposure_adapt_pipeline = create_fullscreen_pipeline(
                &device,
                exposure_render_pass,
                post_pipeline_layout,
                include_bytes!("../shaders/exposure_adapt.frag.spv"),
            )?;
            #[cfg(feature = "smaa")]
            {
                smaa_post_render_pass =
                    create_sampled_color_render_pass(&device, scene_color_format)?;
                smaa_edge_render_pass =
                    create_sampled_color_render_pass(&device, normal_roughness_format)?;
                smaa_weights_render_pass =
                    create_sampled_color_render_pass(&device, normal_roughness_format)?;
                smaa_post_pipeline =
                    create_post_pipeline(&device, smaa_post_render_pass, post_pipeline_layout)?;
                smaa_edge_pipeline = create_fullscreen_pipeline(
                    &device,
                    smaa_edge_render_pass,
                    post_pipeline_layout,
                    include_bytes!("../shaders/smaa/edge.frag.spv"),
                )?;
                smaa_weights_pipeline = create_fullscreen_pipeline(
                    &device,
                    smaa_weights_render_pass,
                    smaa_weights_pipeline_layout,
                    include_bytes!("../shaders/smaa/weights.frag.spv"),
                )?;
            }
            #[cfg(feature = "bloom")]
            if features.bloom.is_some() {
                bloom_render_pass = create_sampled_color_render_pass(&device, scene_color_format)?;
                bloom_first_downsample = create_fullscreen_pipeline(
                    &device,
                    bloom_render_pass,
                    post_pipeline_layout,
                    include_bytes!("../shaders/bloom/downsample_first.frag.spv"),
                )?;
                bloom_downsample = create_fullscreen_pipeline(
                    &device,
                    bloom_render_pass,
                    post_pipeline_layout,
                    include_bytes!("../shaders/bloom/downsample.frag.spv"),
                )?;
                bloom_upsample = create_fullscreen_pipeline(
                    &device,
                    bloom_render_pass,
                    post_pipeline_layout,
                    include_bytes!("../shaders/bloom/upsample.frag.spv"),
                )?;
                bloom_composite = create_fullscreen_pipeline(
                    &device,
                    bloom_render_pass,
                    post_pipeline_layout,
                    include_bytes!("../shaders/bloom/composite.frag.spv"),
                )?;
            }
            shadow_pipelines =
                create_shadow_pipelines(&device, shadow_render_pass, pipeline_layout)?;
            frame_descriptor_set = unsafe {
                device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(descriptor_pool)
                        .set_layouts(&[frame_descriptor_set_layout]),
                )?[0]
            };
            let buffer_info = [vk::DescriptorBufferInfo::default()
                .buffer(frame_uniform.buffer)
                .offset(0)
                .range(size_of::<FrameUniform>() as u64)];
            let writes = vec![
                vk::WriteDescriptorSet::default()
                    .dst_set(frame_descriptor_set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&buffer_info),
            ];
            unsafe { device.update_descriptor_sets(&writes, &[]) };
            write_shadow_descriptors(
                &device,
                frame_descriptor_set,
                shadow_sampler,
                shadow_compare_sampler,
                &shadow_maps,
            );
            #[cfg(feature = "translucent-shadows")]
            if features.translucent_shadows {
                write_mboit_descriptors(
                    &device,
                    frame_descriptor_set,
                    post_sampler,
                    [shadow_maps[0].depth.view; CASCADE_COUNT],
                    [shadow_maps[0].depth.view; CASCADE_COUNT],
                    vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
                );
            }
            initialize_shadow_maps(
                &device,
                queue,
                command_buffer,
                shadow_render_pass,
                &shadow_maps,
            )?;
            Ok::<_, VulkanError>(command_buffer)
        })();
        let command_buffer = match resources {
            Ok(resources) => resources,
            Err(error) => {
                unsafe {
                    #[cfg(feature = "god-rays")]
                    if let Some(pipelines) = god_rays.take() {
                        destroy_god_rays_pipelines(&device, pipelines);
                    }
                    #[cfg(feature = "translucent-shadows")]
                    if let Some(pipelines) = mboit.take() {
                        destroy_mboit_pipelines(&device, pipelines);
                    }
                    #[cfg(feature = "profiling")]
                    if let Some(profile) = profile.take() {
                        if let Some(pool) = profile.pool {
                            device.destroy_query_pool(pool, None);
                        }
                    }
                    if descriptor_pool != vk::DescriptorPool::null() {
                        device.destroy_descriptor_pool(descriptor_pool, None);
                    }
                    for pipeline in shadow_pipelines.into_iter().chain(scene_pipelines) {
                        if pipeline != vk::Pipeline::null() {
                            device.destroy_pipeline(pipeline, None);
                        }
                    }
                    for pipeline in [exposure_reduce_pipeline, exposure_adapt_pipeline] {
                        if pipeline != vk::Pipeline::null() {
                            device.destroy_pipeline(pipeline, None);
                        }
                    }
                    #[cfg(feature = "smaa")]
                    for pipeline in [
                        smaa_post_pipeline,
                        smaa_edge_pipeline,
                        smaa_weights_pipeline,
                    ] {
                        if pipeline != vk::Pipeline::null() {
                            device.destroy_pipeline(pipeline, None);
                        }
                    }
                    #[cfg(feature = "bloom")]
                    for pipeline in [
                        bloom_first_downsample,
                        bloom_downsample,
                        bloom_upsample,
                        bloom_composite,
                    ] {
                        if pipeline != vk::Pipeline::null() {
                            device.destroy_pipeline(pipeline, None);
                        }
                    }
                    #[cfg(feature = "gtao")]
                    if let Some(gtao) = gtao.take() {
                        destroy_gtao_pipelines(&device, gtao);
                    }
                    #[cfg(feature = "ssr")]
                    if let Some(ssr) = ssr.take() {
                        destroy_ssr_pipelines(&device, ssr);
                    }
                    if pipeline_layout != vk::PipelineLayout::null() {
                        device.destroy_pipeline_layout(pipeline_layout, None);
                    }
                    if post_pipeline_layout != vk::PipelineLayout::null() {
                        device.destroy_pipeline_layout(post_pipeline_layout, None);
                    }
                    #[cfg(feature = "god-rays")]
                    if god_rays_pipeline_layout != vk::PipelineLayout::null() {
                        device.destroy_pipeline_layout(god_rays_pipeline_layout, None);
                    }
                    #[cfg(feature = "smaa")]
                    for layout in [
                        smaa_weights_pipeline_layout,
                        smaa_neighborhood_pipeline_layout,
                    ] {
                        if layout != vk::PipelineLayout::null() {
                            device.destroy_pipeline_layout(layout, None);
                        }
                    }
                    for map in shadow_maps {
                        destroy_shadow_map(&device, map);
                    }
                    if shadow_render_pass != vk::RenderPass::null() {
                        device.destroy_render_pass(shadow_render_pass, None);
                    }
                    if scene_render_pass != vk::RenderPass::null() {
                        device.destroy_render_pass(scene_render_pass, None);
                    }
                    if exposure_render_pass != vk::RenderPass::null() {
                        device.destroy_render_pass(exposure_render_pass, None);
                    }
                    #[cfg(feature = "smaa")]
                    for render_pass in [
                        smaa_post_render_pass,
                        smaa_edge_render_pass,
                        smaa_weights_render_pass,
                    ] {
                        if render_pass != vk::RenderPass::null() {
                            device.destroy_render_pass(render_pass, None);
                        }
                    }
                    #[cfg(feature = "bloom")]
                    if bloom_render_pass != vk::RenderPass::null() {
                        device.destroy_render_pass(bloom_render_pass, None);
                    }
                    if shadow_sampler != vk::Sampler::null() {
                        device.destroy_sampler(shadow_sampler, None);
                    }
                    if shadow_compare_sampler != vk::Sampler::null() {
                        device.destroy_sampler(shadow_compare_sampler, None);
                    }
                    if post_sampler != vk::Sampler::null() {
                        device.destroy_sampler(post_sampler, None);
                    }
                    if frame_descriptor_set_layout != vk::DescriptorSetLayout::null() {
                        device.destroy_descriptor_set_layout(frame_descriptor_set_layout, None);
                    }
                    if material_descriptor_set_layout != vk::DescriptorSetLayout::null() {
                        device.destroy_descriptor_set_layout(material_descriptor_set_layout, None);
                    }
                    if post_descriptor_set_layout != vk::DescriptorSetLayout::null() {
                        device.destroy_descriptor_set_layout(post_descriptor_set_layout, None);
                    }
                    #[cfg(feature = "smaa")]
                    if smaa_weights_descriptor_set_layout != vk::DescriptorSetLayout::null() {
                        device.destroy_descriptor_set_layout(
                            smaa_weights_descriptor_set_layout,
                            None,
                        );
                    }
                    destroy_buffer(&device, frame_uniform);
                    if image_available != vk::Semaphore::null() {
                        device.destroy_semaphore(image_available, None);
                    }
                    if fence != vk::Fence::null() {
                        device.destroy_fence(fence, None);
                    }
                    if command_pool != vk::CommandPool::null() {
                        device.destroy_command_pool(command_pool, None);
                    }
                    device.destroy_device(None);
                }
                return Err(error);
            }
        };
        let swapchain = swapchain::Device::new(instance, &device);
        Ok(DeviceState {
            physical,
            features,
            mipmaps_supported,
            queue_family,
            queue,
            device,
            depth_format,
            command_pool,
            command_buffer,
            fence,
            image_available,
            swapchain,
            memory,
            memory_stats,
            environments: HashMap::new(),
            scene_color_format,
            normal_roughness_format,
            scene_render_pass,
            exposure_render_pass,
            exposure_reduce_pipeline,
            exposure_adapt_pipeline,
            #[cfg(feature = "ssr")]
            ssr,
            #[cfg(feature = "god-rays")]
            god_rays,
            scene_pipelines,
            frame_uniform,
            frame_descriptor_set_layout,
            material_descriptor_set_layout,
            post_descriptor_set_layout,
            #[cfg(feature = "smaa")]
            smaa_weights_descriptor_set_layout,
            descriptor_pool,
            pipeline_layout,
            post_pipeline_layout,
            #[cfg(feature = "god-rays")]
            god_rays_pipeline_layout,
            #[cfg(feature = "smaa")]
            smaa_weights_pipeline_layout,
            #[cfg(feature = "smaa")]
            smaa_neighborhood_pipeline_layout,
            frame_descriptor_set,
            shadow_render_pass,
            shadow_maps,
            shadow_diagnostics_last_items: 0,
            #[cfg(feature = "translucent-shadows")]
            mboit,
            #[cfg(feature = "translucent-shadows")]
            mboit_maps: Vec::new(),
            shadow_sampler,
            shadow_compare_sampler,
            post_sampler,
            #[cfg(feature = "smaa")]
            smaa_post_render_pass,
            #[cfg(feature = "smaa")]
            smaa_edge_render_pass,
            #[cfg(feature = "smaa")]
            smaa_weights_render_pass,
            #[cfg(feature = "smaa")]
            smaa_post_pipeline,
            #[cfg(feature = "smaa")]
            smaa_edge_pipeline,
            #[cfg(feature = "smaa")]
            smaa_weights_pipeline,
            #[cfg(feature = "smaa")]
            smaa_lookups: None,
            #[cfg(feature = "bloom")]
            bloom: features.bloom.map(|_| BloomPipelines {
                render_pass: bloom_render_pass,
                first_downsample: bloom_first_downsample,
                downsample: bloom_downsample,
                upsample: bloom_upsample,
                composite: bloom_composite,
            }),
            #[cfg(feature = "gtao")]
            gtao,
            shadow_pipelines,
            meshes: HashMap::new(),
            images: HashMap::new(),
            materials: HashMap::new(),
            samplers: HashMap::new(),
            fallback_image: None,
            #[cfg(feature = "profiling")]
            profile,
        })
    }

    fn memory_type(
        memory: &vk::PhysicalDeviceMemoryProperties,
        bits: u32,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Result<(u32, vk::MemoryPropertyFlags), VulkanError> {
        for preference in [preferred, vk::MemoryPropertyFlags::empty()] {
            for index in 0..memory.memory_type_count {
                let flags = memory.memory_types[index as usize].property_flags;
                if bits & (1 << index) != 0 && flags.contains(required | preference) {
                    return Ok((index, flags));
                }
            }
        }
        Err(VulkanError::Unsupported(
            "no compatible Vulkan memory type".into(),
        ))
    }

    fn choose_depth_format(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
    ) -> Result<vk::Format, VulkanError> {
        [vk::Format::D32_SFLOAT, vk::Format::D16_UNORM]
            .into_iter()
            .find(|&format| {
                unsafe { instance.get_physical_device_format_properties(physical, format) }
                    .optimal_tiling_features
                    .contains(
                        vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT
                            | vk::FormatFeatureFlags::SAMPLED_IMAGE
                            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
                    )
            })
            .ok_or_else(|| {
                VulkanError::Unsupported(
                    "no depth format supports attachment, sampling, and linear PCF filtering"
                        .into(),
                )
            })
    }

    fn choose_scene_color_format(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
    ) -> Result<vk::Format, VulkanError> {
        [
            vk::Format::R16G16B16A16_SFLOAT,
            vk::Format::R32G32B32A32_SFLOAT,
        ]
        .into_iter()
        .find(|&format| {
            unsafe { instance.get_physical_device_format_properties(physical, format) }
                .optimal_tiling_features
                .contains(
                    vk::FormatFeatureFlags::COLOR_ATTACHMENT
                        | vk::FormatFeatureFlags::SAMPLED_IMAGE
                        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                        | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND,
                )
        })
        .ok_or_else(|| {
            VulkanError::Unsupported(
                "no sampled blend-capable HDR color attachment format is supported".into(),
            )
        })
    }

    #[cfg(feature = "translucent-shadows")]
    fn choose_mboit_format(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
    ) -> Result<vk::Format, VulkanError> {
        let format = vk::Format::R32G32B32A32_SFLOAT;
        let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
        let required = vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
        let supported = unsafe { instance.get_physical_device_format_properties(physical, format) }
            .optimal_tiling_features
            .contains(required)
            && unsafe {
                instance.get_physical_device_image_format_properties(
                    physical,
                    format,
                    vk::ImageType::TYPE_2D,
                    vk::ImageTiling::OPTIMAL,
                    usage,
                    vk::ImageCreateFlags::empty(),
                )
            }
            .is_ok_and(|properties| {
                properties.max_extent.width >= SHADOW_MAP_SIZES[CASCADE_COUNT - 1] / 2
                    && properties.max_extent.height >= SHADOW_MAP_SIZES[CASCADE_COUNT - 1] / 2
            });
        if supported {
            Ok(format)
        } else {
            Err(VulkanError::Unsupported(
                "MBOIT needs linearly filterable, blendable, sampled RGBA32F images at shadow-map resolution"
                    .into(),
            ))
        }
    }

    fn choose_normal_format(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
    ) -> Result<vk::Format, VulkanError> {
        let format = vk::Format::R8G8B8A8_UNORM;
        if unsafe { instance.get_physical_device_format_properties(physical, format) }
            .optimal_tiling_features
            .contains(
                vk::FormatFeatureFlags::COLOR_ATTACHMENT
                    | vk::FormatFeatureFlags::SAMPLED_IMAGE
                    | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            )
        {
            Ok(format)
        } else {
            Err(VulkanError::Unsupported(
                "RGBA8 normal/roughness attachment is unsupported".into(),
            ))
        }
    }

    fn create_sampler_set_layout(
        device: &ash::Device,
        count: u32,
    ) -> Result<vk::DescriptorSetLayout, VulkanError> {
        let bindings = (0..count)
            .map(|binding| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            })
            .collect::<Vec<_>>();
        unsafe {
            device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .map_err(Into::into)
        }
    }

    fn create_sampled_color_render_pass(
        device: &ash::Device,
        format: vk::Format,
    ) -> Result<vk::RenderPass, VulkanError> {
        let attachments = [vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(render_graph::SAMPLED_COLOR_LAYOUT)];
        let color = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(render_graph::COLOR_ATTACHMENT_LAYOUT)];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color)];
        let dependencies = render_graph::render_pass_dependencies(
            render_graph::sampled_scope(),
            render_graph::color_write_scope(),
            render_graph::sampled_scope(),
        );
        unsafe {
            device
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachments)
                        .subpasses(&subpasses)
                        .dependencies(&dependencies),
                    None,
                )
                .map_err(Into::into)
        }
    }

    #[cfg(feature = "gtao")]
    fn create_gtao_pipelines(
        device: &ash::Device,
        normal_format: vk::Format,
        depth_format: vk::Format,
        layout: vk::PipelineLayout,
    ) -> Result<GtaoPipelines, VulkanError> {
        let mut pipelines = GtaoPipelines {
            prepass_render_pass: vk::RenderPass::null(),
            render_pass: vk::RenderPass::null(),
            prepass: [vk::Pipeline::null(); 4],
            horizon: vk::Pipeline::null(),
            upsample: vk::Pipeline::null(),
        };
        let result = (|| {
            pipelines.prepass_render_pass =
                create_gtao_prepass_render_pass(device, normal_format, depth_format)?;
            pipelines.render_pass = create_sampled_color_render_pass(device, normal_format)?;
            for (index, pipeline) in pipelines.prepass.iter_mut().enumerate() {
                let (front_face, cull_mode) = pipeline_raster_state(index);
                *pipeline = create_graphics_pipeline(
                    device,
                    pipelines.prepass_render_pass,
                    layout,
                    front_face,
                    cull_mode,
                    false,
                    include_bytes!("../shaders/pbr.vert.spv"),
                    include_bytes!("../shaders/gtao_prepass.frag.spv"),
                    1,
                    false,
                )?;
            }
            pipelines.horizon = create_fullscreen_pipeline(
                device,
                pipelines.render_pass,
                layout,
                include_bytes!("../shaders/gtao.frag.spv"),
            )?;
            pipelines.upsample = create_fullscreen_pipeline(
                device,
                pipelines.render_pass,
                layout,
                include_bytes!("../shaders/gtao_upsample.frag.spv"),
            )?;
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_gtao_pipelines(device, pipelines);
            return Err(error);
        }
        Ok(pipelines)
    }

    #[cfg(feature = "god-rays")]
    fn create_god_rays_pipelines(
        device: &ash::Device,
        format: vk::Format,
        frame_layout: vk::PipelineLayout,
        post_layout: vk::PipelineLayout,
        mboit_enabled: bool,
    ) -> Result<GodRaysPipelines, VulkanError> {
        let mut pipelines = GodRaysPipelines {
            render_pass: vk::RenderPass::null(),
            march: vk::Pipeline::null(),
            horizontal: vk::Pipeline::null(),
            vertical: vk::Pipeline::null(),
            composite: vk::Pipeline::null(),
        };
        let result = (|| {
            pipelines.render_pass = create_sampled_color_render_pass(device, format)?;
            #[cfg(feature = "translucent-shadows")]
            let march_shader = if mboit_enabled {
                include_bytes!("../shaders/god_rays_march_mboit.frag.spv").as_slice()
            } else {
                include_bytes!("../shaders/god_rays_march.frag.spv").as_slice()
            };
            #[cfg(not(feature = "translucent-shadows"))]
            let march_shader = {
                let _ = mboit_enabled;
                include_bytes!("../shaders/god_rays_march.frag.spv").as_slice()
            };
            pipelines.march = create_fullscreen_pipeline(
                device,
                pipelines.render_pass,
                frame_layout,
                march_shader,
            )?;
            pipelines.horizontal = create_fullscreen_pipeline(
                device,
                pipelines.render_pass,
                post_layout,
                include_bytes!("../shaders/god_rays_h.frag.spv"),
            )?;
            pipelines.vertical = create_fullscreen_pipeline(
                device,
                pipelines.render_pass,
                post_layout,
                include_bytes!("../shaders/god_rays_v.frag.spv"),
            )?;
            pipelines.composite = create_fullscreen_pipeline(
                device,
                pipelines.render_pass,
                post_layout,
                include_bytes!("../shaders/god_rays_composite.frag.spv"),
            )?;
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_god_rays_pipelines(device, pipelines);
            return Err(error);
        }
        Ok(pipelines)
    }

    #[cfg(feature = "god-rays")]
    fn destroy_god_rays_pipelines(device: &ash::Device, pipelines: GodRaysPipelines) {
        unsafe {
            for pipeline in [
                pipelines.march,
                pipelines.horizontal,
                pipelines.vertical,
                pipelines.composite,
            ] {
                if pipeline != vk::Pipeline::null() {
                    device.destroy_pipeline(pipeline, None);
                }
            }
            if pipelines.render_pass != vk::RenderPass::null() {
                device.destroy_render_pass(pipelines.render_pass, None);
            }
        }
    }

    #[cfg(feature = "gtao")]
    fn destroy_gtao_pipelines(device: &ash::Device, pipelines: GtaoPipelines) {
        unsafe {
            for pipeline in pipelines
                .prepass
                .into_iter()
                .chain([pipelines.horizon, pipelines.upsample])
            {
                if pipeline != vk::Pipeline::null() {
                    device.destroy_pipeline(pipeline, None);
                }
            }
            for render_pass in [pipelines.prepass_render_pass, pipelines.render_pass] {
                if render_pass != vk::RenderPass::null() {
                    device.destroy_render_pass(render_pass, None);
                }
            }
        }
    }

    fn create_attachment(
        device: &DeviceState,
        extent: vk::Extent2D,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        aspect: vk::ImageAspectFlags,
    ) -> Result<GpuAttachment, VulkanError> {
        let image = unsafe {
            device.device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width: extent.width,
                        height: extent.height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )?
        };
        let requirements = unsafe { device.device.get_image_memory_requirements(image) };
        let (index, _) = match memory_type(
            &device.memory,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::empty(),
        ) {
            Ok(value) => value,
            Err(error) => {
                unsafe { device.device.destroy_image(image, None) };
                return Err(error);
            }
        };
        let memory =
            match GpuMemory::allocate(&device.device, &requirements, index, &device.memory_stats) {
                Ok(memory) => memory,
                Err(error) => {
                    unsafe { device.device.destroy_image(image, None) };
                    return Err(error.into());
                }
            };
        if let Err(error) = unsafe { device.device.bind_image_memory(image, memory.handle, 0) } {
            unsafe {
                device.device.destroy_image(image, None);
            }
            memory.free(&device.device);
            return Err(error.into());
        }
        let view = match unsafe {
            device.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: aspect,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        } {
            Ok(view) => view,
            Err(error) => {
                unsafe {
                    device.device.destroy_image(image, None);
                }
                memory.free(&device.device);
                return Err(error.into());
            }
        };
        Ok(GpuAttachment {
            image,
            memory,
            view,
        })
    }

    fn null_attachment() -> GpuAttachment {
        GpuAttachment {
            image: vk::Image::null(),
            memory: GpuMemory::empty(),
            view: vk::ImageView::null(),
        }
    }

    fn create_scene_targets(
        device: &DeviceState,
        extent: vk::Extent2D,
    ) -> Result<SceneTargets, VulkanError> {
        let mut scene = SceneTargets {
            color: GpuAttachment {
                image: vk::Image::null(),
                memory: GpuMemory::empty(),
                view: vk::ImageView::null(),
            },
            normal_roughness: GpuAttachment {
                image: vk::Image::null(),
                memory: GpuMemory::empty(),
                view: vk::ImageView::null(),
            },
            #[cfg(feature = "ssr")]
            reflectance: None,
            depth: GpuAttachment {
                image: vk::Image::null(),
                memory: GpuMemory::empty(),
                view: vk::ImageView::null(),
            },
            framebuffer: vk::Framebuffer::null(),
        };
        let result = (|| {
            let color_usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
            scene.color = create_attachment(
                device,
                extent,
                device.scene_color_format,
                color_usage,
                vk::ImageAspectFlags::COLOR,
            )?;
            scene.normal_roughness = create_attachment(
                device,
                extent,
                device.normal_roughness_format,
                color_usage,
                vk::ImageAspectFlags::COLOR,
            )?;
            #[cfg(feature = "ssr")]
            if device.features.ssr.is_some() {
                scene.reflectance = Some(create_attachment(
                    device,
                    extent,
                    device.normal_roughness_format,
                    color_usage,
                    vk::ImageAspectFlags::COLOR,
                )?);
            }
            scene.depth = create_attachment(
                device,
                extent,
                device.depth_format,
                vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                vk::ImageAspectFlags::DEPTH,
            )?;
            let mut attachments = vec![scene.color.view, scene.normal_roughness.view];
            #[cfg(feature = "ssr")]
            if let Some(reflectance) = &scene.reflectance {
                attachments.push(reflectance.view);
            }
            attachments.push(scene.depth.view);
            scene.framebuffer = unsafe {
                device.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(device.scene_render_pass)
                        .attachments(&attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )?
            };
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_scene_targets(&device.device, scene);
            return Err(error);
        }
        Ok(scene)
    }

    #[cfg(feature = "ssr")]
    fn create_ssr_targets(
        device: &DeviceState,
        scene: &SceneTargets,
        extent: vk::Extent2D,
    ) -> Result<SsrTargets, VulkanError> {
        let mut targets = SsrTargets {
            color: null_attachment(),
            framebuffer: vk::Framebuffer::null(),
            blend_framebuffer: vk::Framebuffer::null(),
            descriptor_set: vk::DescriptorSet::null(),
        };
        let result = (|| {
            let pipelines = device.ssr.as_ref().expect("SSR pipelines are configured");
            targets.color = create_attachment(
                device,
                extent,
                device.scene_color_format,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                vk::ImageAspectFlags::COLOR,
            )?;
            targets.framebuffer = create_color_framebuffer(
                device,
                pipelines.render_pass,
                targets.color.view,
                extent,
            )?;
            let mut blend_attachments = vec![
                targets.color.view,
                scene.normal_roughness.view,
                scene
                    .reflectance
                    .as_ref()
                    .expect("SSR reflectance attachment is configured")
                    .view,
            ];
            blend_attachments.push(scene.depth.view);
            targets.blend_framebuffer = unsafe {
                device.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(pipelines.blend_render_pass)
                        .attachments(&blend_attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )?
            };
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_ssr_targets(&device.device, targets);
            return Err(error);
        }
        Ok(targets)
    }

    #[cfg(feature = "ssr")]
    fn destroy_ssr_targets(device: &ash::Device, targets: SsrTargets) {
        unsafe {
            for framebuffer in [targets.framebuffer, targets.blend_framebuffer] {
                if framebuffer != vk::Framebuffer::null() {
                    device.destroy_framebuffer(framebuffer, None);
                }
            }
        }
        destroy_attachment(device, targets.color);
    }

    #[cfg(feature = "gtao")]
    fn create_gtao_targets(
        device: &DeviceState,
        scene: &SceneTargets,
        extent: vk::Extent2D,
    ) -> Result<GtaoTargets, VulkanError> {
        let mut targets = GtaoTargets {
            prepass_framebuffer: vk::Framebuffer::null(),
            raw: null_attachment(),
            raw_framebuffer: vk::Framebuffer::null(),
            full: null_attachment(),
            full_framebuffer: vk::Framebuffer::null(),
        };
        let result = (|| {
            let pipelines = device.gtao.as_ref().expect("GTAO pipelines are configured");
            targets.prepass_framebuffer = unsafe {
                device.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(pipelines.prepass_render_pass)
                        .attachments(&[scene.normal_roughness.view, scene.depth.view])
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )?
            };
            let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
            let half_extent = vk::Extent2D {
                width: extent.width.div_ceil(2),
                height: extent.height.div_ceil(2),
            };
            targets.raw = create_attachment(
                device,
                half_extent,
                device.normal_roughness_format,
                usage,
                vk::ImageAspectFlags::COLOR,
            )?;
            targets.raw_framebuffer = create_color_framebuffer(
                device,
                pipelines.render_pass,
                targets.raw.view,
                half_extent,
            )?;
            targets.full = create_attachment(
                device,
                extent,
                device.normal_roughness_format,
                usage,
                vk::ImageAspectFlags::COLOR,
            )?;
            targets.full_framebuffer =
                create_color_framebuffer(device, pipelines.render_pass, targets.full.view, extent)?;
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_gtao_targets(&device.device, targets);
            return Err(error);
        }
        Ok(targets)
    }

    #[cfg(feature = "god-rays")]
    fn create_god_rays_targets(
        device: &DeviceState,
        extent: vk::Extent2D,
    ) -> Result<GodRaysTargets, VulkanError> {
        let mut targets = GodRaysTargets {
            march: null_attachment(),
            march_framebuffer: vk::Framebuffer::null(),
            horizontal: null_attachment(),
            horizontal_framebuffer: vk::Framebuffer::null(),
            scattering: null_attachment(),
            scattering_framebuffer: vk::Framebuffer::null(),
            horizontal_set: vk::DescriptorSet::null(),
            vertical_set: vk::DescriptorSet::null(),
            composite_set: vk::DescriptorSet::null(),
        };
        let result = (|| {
            let pipelines = device.god_rays.as_ref().expect("God Rays are configured");
            let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
            for attachment in [
                &mut targets.march,
                &mut targets.horizontal,
                &mut targets.scattering,
            ] {
                *attachment = create_attachment(
                    device,
                    extent,
                    device.scene_color_format,
                    usage,
                    vk::ImageAspectFlags::COLOR,
                )?;
            }
            targets.march_framebuffer = create_color_framebuffer(
                device,
                pipelines.render_pass,
                targets.march.view,
                extent,
            )?;
            targets.horizontal_framebuffer = create_color_framebuffer(
                device,
                pipelines.render_pass,
                targets.horizontal.view,
                extent,
            )?;
            targets.scattering_framebuffer = create_color_framebuffer(
                device,
                pipelines.render_pass,
                targets.scattering.view,
                extent,
            )?;
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_god_rays_targets(&device.device, targets);
            return Err(error);
        }
        Ok(targets)
    }

    #[cfg(feature = "god-rays")]
    fn destroy_god_rays_targets(device: &ash::Device, targets: GodRaysTargets) {
        unsafe {
            for framebuffer in [
                targets.march_framebuffer,
                targets.horizontal_framebuffer,
                targets.scattering_framebuffer,
            ] {
                if framebuffer != vk::Framebuffer::null() {
                    device.destroy_framebuffer(framebuffer, None);
                }
            }
        }
        for attachment in [targets.march, targets.horizontal, targets.scattering] {
            destroy_attachment(device, attachment);
        }
    }

    #[cfg(feature = "smaa")]
    fn create_smaa_targets(
        device: &DeviceState,
        extent: vk::Extent2D,
    ) -> Result<SmaaTargets, VulkanError> {
        let mut targets = SmaaTargets {
            post_color: null_attachment(),
            edges: null_attachment(),
            weights: null_attachment(),
            post_framebuffer: vk::Framebuffer::null(),
            edge_framebuffer: vk::Framebuffer::null(),
            weight_framebuffer: vk::Framebuffer::null(),
        };
        let result = (|| {
            let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
            targets.post_color = create_attachment(
                device,
                extent,
                device.scene_color_format,
                usage,
                vk::ImageAspectFlags::COLOR,
            )?;
            targets.edges = create_attachment(
                device,
                extent,
                device.normal_roughness_format,
                usage,
                vk::ImageAspectFlags::COLOR,
            )?;
            targets.weights = create_attachment(
                device,
                extent,
                device.normal_roughness_format,
                usage,
                vk::ImageAspectFlags::COLOR,
            )?;
            targets.post_framebuffer = create_color_framebuffer(
                device,
                device.smaa_post_render_pass,
                targets.post_color.view,
                extent,
            )?;
            targets.edge_framebuffer = create_color_framebuffer(
                device,
                device.smaa_edge_render_pass,
                targets.edges.view,
                extent,
            )?;
            targets.weight_framebuffer = create_color_framebuffer(
                device,
                device.smaa_weights_render_pass,
                targets.weights.view,
                extent,
            )?;
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_smaa_targets(&device.device, targets);
            return Err(error);
        }
        Ok(targets)
    }

    #[cfg(feature = "bloom")]
    fn create_bloom_targets(
        device: &DeviceState,
        extent: vk::Extent2D,
    ) -> Result<BloomTargets, VulkanError> {
        let render_pass = device
            .bloom
            .as_ref()
            .expect("Bloom is configured")
            .render_pass;
        let mut targets = BloomTargets { levels: Vec::new() };
        let result = (|| {
            let mut source = extent;
            loop {
                let level_extent = vk::Extent2D {
                    width: source.width.div_ceil(2),
                    height: source.height.div_ceil(2),
                };
                targets.levels.push(BloomLevel {
                    extent: level_extent,
                    down: null_attachment(),
                    down_framebuffer: vk::Framebuffer::null(),
                    down_set: vk::DescriptorSet::null(),
                    up: None,
                });
                let level = targets.levels.last_mut().expect("level was inserted");
                let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
                level.down = create_attachment(
                    device,
                    level_extent,
                    device.scene_color_format,
                    usage,
                    vk::ImageAspectFlags::COLOR,
                )?;
                level.down_framebuffer =
                    create_color_framebuffer(device, render_pass, level.down.view, level_extent)?;
                source = level_extent;
                if source.width == 1 && source.height == 1 {
                    break;
                }
            }
            let level_count = targets.levels.len();
            for level in targets.levels.iter_mut().take(level_count - 1) {
                level.up = Some(BloomUpsampleTarget {
                    image: null_attachment(),
                    framebuffer: vk::Framebuffer::null(),
                    descriptor_set: vk::DescriptorSet::null(),
                });
                let up = level.up.as_mut().expect("target was inserted");
                up.image = create_attachment(
                    device,
                    level.extent,
                    device.scene_color_format,
                    vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                    vk::ImageAspectFlags::COLOR,
                )?;
                up.framebuffer =
                    create_color_framebuffer(device, render_pass, up.image.view, level.extent)?;
            }
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_bloom_targets(&device.device, targets);
            return Err(error);
        }
        Ok(targets)
    }

    #[cfg(feature = "bloom")]
    fn destroy_bloom_targets(device: &ash::Device, targets: BloomTargets) {
        for level in targets.levels {
            unsafe {
                if level.down_framebuffer != vk::Framebuffer::null() {
                    device.destroy_framebuffer(level.down_framebuffer, None);
                }
            }
            destroy_attachment(device, level.down);
            if let Some(up) = level.up {
                unsafe {
                    if up.framebuffer != vk::Framebuffer::null() {
                        device.destroy_framebuffer(up.framebuffer, None);
                    }
                }
                destroy_attachment(device, up.image);
            }
        }
    }

    fn create_color_framebuffer(
        device: &DeviceState,
        render_pass: vk::RenderPass,
        view: vk::ImageView,
        extent: vk::Extent2D,
    ) -> Result<vk::Framebuffer, VulkanError> {
        unsafe {
            device
                .device
                .create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(render_pass)
                        .attachments(&[view])
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )
                .map_err(Into::into)
        }
    }

    fn null_exposure_targets() -> ExposureTargets {
        ExposureTargets {
            pyramid: Vec::new(),
            history: [null_attachment(), null_attachment()],
            history_framebuffers: [vk::Framebuffer::null(); 2],
            adaptation_sets: [vk::DescriptorSet::null(); 2],
            history_index: 0,
            history_valid: false,
            last_submitted: None,
        }
    }

    fn create_exposure_targets(
        device: &DeviceState,
        extent: vk::Extent2D,
    ) -> Result<ExposureTargets, VulkanError> {
        let mut targets = null_exposure_targets();
        let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
        let result = (|| {
            let mut source = extent;
            loop {
                let level_extent = vk::Extent2D {
                    width: source.width.div_ceil(2),
                    height: source.height.div_ceil(2),
                };
                let image = create_attachment(
                    device,
                    level_extent,
                    device.scene_color_format,
                    usage,
                    vk::ImageAspectFlags::COLOR,
                )?;
                let framebuffer = match create_color_framebuffer(
                    device,
                    device.exposure_render_pass,
                    image.view,
                    level_extent,
                ) {
                    Ok(framebuffer) => framebuffer,
                    Err(error) => {
                        destroy_attachment(&device.device, image);
                        return Err(error);
                    }
                };
                targets.pyramid.push(ExposureLevel {
                    extent: level_extent,
                    image,
                    framebuffer,
                    descriptor_set: vk::DescriptorSet::null(),
                });
                source = level_extent;
                if source.width == 1 && source.height == 1 {
                    break;
                }
            }
            for index in 0..targets.history.len() {
                let image = create_attachment(
                    device,
                    vk::Extent2D {
                        width: 1,
                        height: 1,
                    },
                    device.scene_color_format,
                    usage,
                    vk::ImageAspectFlags::COLOR,
                )?;
                let framebuffer = match create_color_framebuffer(
                    device,
                    device.exposure_render_pass,
                    image.view,
                    vk::Extent2D {
                        width: 1,
                        height: 1,
                    },
                ) {
                    Ok(framebuffer) => framebuffer,
                    Err(error) => {
                        destroy_attachment(&device.device, image);
                        return Err(error);
                    }
                };
                targets.history[index] = image;
                targets.history_framebuffers[index] = framebuffer;
            }
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_exposure_targets(&device.device, targets);
            return Err(error);
        }
        Ok(targets)
    }

    fn destroy_exposure_targets(device: &ash::Device, targets: ExposureTargets) {
        unsafe {
            for framebuffer in targets
                .history_framebuffers
                .into_iter()
                .chain(targets.pyramid.iter().map(|level| level.framebuffer))
            {
                if framebuffer != vk::Framebuffer::null() {
                    device.destroy_framebuffer(framebuffer, None);
                }
            }
        }
        for level in targets.pyramid {
            destroy_attachment(device, level.image);
        }
        for image in targets.history {
            destroy_attachment(device, image);
        }
    }

    fn create_shadow_render_pass(
        device: &ash::Device,
        format: vk::Format,
    ) -> Result<vk::RenderPass, VulkanError> {
        let attachments = [vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(render_graph::SAMPLED_DEPTH_LAYOUT)];
        let depth = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(render_graph::DEPTH_ATTACHMENT_LAYOUT)];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .depth_stencil_attachment(&depth[0])];
        let dependencies = render_graph::render_pass_dependencies(
            render_graph::sampled_scope(),
            render_graph::depth_write_scope(),
            render_graph::sampled_scope(),
        );
        unsafe {
            device
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachments)
                        .subpasses(&subpasses)
                        .dependencies(&dependencies),
                    None,
                )
                .map_err(Into::into)
        }
    }

    #[cfg(feature = "translucent-shadows")]
    fn create_mboit_render_pass(
        device: &ash::Device,
        color_format: vk::Format,
    ) -> Result<vk::RenderPass, VulkanError> {
        let attachment = vk::AttachmentDescription::default()
            .format(color_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(render_graph::SAMPLED_COLOR_LAYOUT);
        let attachments = [attachment, attachment];
        let colors = [
            vk::AttachmentReference::default()
                .attachment(0)
                .layout(render_graph::COLOR_ATTACHMENT_LAYOUT),
            vk::AttachmentReference::default()
                .attachment(1)
                .layout(render_graph::COLOR_ATTACHMENT_LAYOUT),
        ];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&colors)];
        let dependencies = render_graph::render_pass_dependencies(
            render_graph::sampled_scope(),
            render_graph::color_write_scope(),
            render_graph::sampled_scope(),
        );
        unsafe {
            device
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachments)
                        .subpasses(&subpasses)
                        .dependencies(&dependencies),
                    None,
                )
                .map_err(Into::into)
        }
    }

    #[cfg(feature = "translucent-shadows")]
    fn create_mboit_map(
        device: &DeviceState,
        format: vk::Format,
        render_pass: vk::RenderPass,
        size: u32,
    ) -> Result<MboitMap, VulkanError> {
        let mut map = MboitMap {
            moments_0123: null_attachment(),
            moments_456: null_attachment(),
            framebuffer: vk::Framebuffer::null(),
            size,
        };
        let result = (|| {
            map.moments_0123 = create_attachment(
                device,
                vk::Extent2D {
                    width: size,
                    height: size,
                },
                format,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                vk::ImageAspectFlags::COLOR,
            )?;
            map.moments_456 = create_attachment(
                device,
                vk::Extent2D {
                    width: size,
                    height: size,
                },
                format,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                vk::ImageAspectFlags::COLOR,
            )?;
            map.framebuffer = unsafe {
                device.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(render_pass)
                        .attachments(&[map.moments_0123.view, map.moments_456.view])
                        .width(size)
                        .height(size)
                        .layers(1),
                    None,
                )?
            };
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_mboit_map(&device.device, map);
            return Err(error);
        }
        Ok(map)
    }

    #[cfg(feature = "translucent-shadows")]
    fn destroy_mboit_map(device: &ash::Device, map: MboitMap) {
        unsafe {
            if map.framebuffer != vk::Framebuffer::null() {
                device.destroy_framebuffer(map.framebuffer, None);
            }
        }
        destroy_attachment(device, map.moments_0123);
        destroy_attachment(device, map.moments_456);
    }

    fn create_shadow_map(
        device: &ash::Device,
        memory: &vk::PhysicalDeviceMemoryProperties,
        memory_stats: &Arc<GpuMemoryStats>,
        format: vk::Format,
        render_pass: vk::RenderPass,
        size: u32,
    ) -> Result<ShadowMap, VulkanError> {
        let mut map = ShadowMap {
            depth: null_attachment(),
            framebuffer: vk::Framebuffer::null(),
            size,
        };
        let result = (|| {
            map.depth.image = unsafe {
                device.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(format)
                        .extent(vk::Extent3D {
                            width: size,
                            height: size,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(
                            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT
                                | vk::ImageUsageFlags::SAMPLED,
                        )
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )?
            };
            let requirements = unsafe { device.get_image_memory_requirements(map.depth.image) };
            let (memory_type, _) = memory_type(
                memory,
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                vk::MemoryPropertyFlags::empty(),
            )?;
            map.depth.memory =
                GpuMemory::allocate(device, &requirements, memory_type, memory_stats)?;
            unsafe { device.bind_image_memory(map.depth.image, map.depth.memory.handle, 0)? };
            map.depth.view = unsafe {
                device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(map.depth.image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::DEPTH,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        }),
                    None,
                )?
            };
            map.framebuffer = unsafe {
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(render_pass)
                        .attachments(&[map.depth.view])
                        .width(size)
                        .height(size)
                        .layers(1),
                    None,
                )?
            };
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_shadow_map(device, map);
            return Err(error);
        }
        Ok(map)
    }

    fn create_buffer(
        device: &ash::Device,
        memory: &vk::PhysicalDeviceMemoryProperties,
        memory_stats: &Arc<GpuMemoryStats>,
        size: vk::DeviceSize,
        usage: vk::BufferUsageFlags,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Result<GpuBuffer, VulkanError> {
        let buffer = unsafe {
            device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(usage)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?
        };
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let (index, flags) =
            match memory_type(memory, requirements.memory_type_bits, required, preferred) {
                Ok(memory_type) => memory_type,
                Err(error) => {
                    unsafe { device.destroy_buffer(buffer, None) };
                    return Err(error);
                }
            };
        let allocation = GpuMemory::allocate(device, &requirements, index, memory_stats);
        let allocation = match allocation {
            Ok(memory_handle) => memory_handle,
            Err(error) => {
                unsafe { device.destroy_buffer(buffer, None) };
                return Err(error);
            }
        };
        if let Err(error) = unsafe { device.bind_buffer_memory(buffer, allocation.handle, 0) } {
            unsafe {
                device.destroy_buffer(buffer, None);
            }
            allocation.free(device);
            return Err(error.into());
        }
        Ok(GpuBuffer {
            buffer,
            memory: allocation,
            flags,
        })
    }

    fn destroy_buffer(device: &ash::Device, buffer: GpuBuffer) {
        unsafe {
            if buffer.buffer != vk::Buffer::null() {
                device.destroy_buffer(buffer.buffer, None);
            }
            buffer.memory.free(device);
        }
    }

    fn destroy_image(device: &ash::Device, image: GpuImage) {
        unsafe {
            if image.linear_view != vk::ImageView::null() {
                device.destroy_image_view(image.linear_view, None);
            }
            if image.srgb_view != vk::ImageView::null() {
                device.destroy_image_view(image.srgb_view, None);
            }
            if image.image != vk::Image::null() {
                device.destroy_image(image.image, None);
            }
            image.memory.free(device);
        }
    }

    fn bytes_of<T>(value: &T) -> &[u8] {
        // Callers use fully initialized repr(C) GPU structs without padding.
        unsafe { std::slice::from_raw_parts((value as *const T).cast(), size_of::<T>()) }
    }

    fn write_buffer(
        device: &ash::Device,
        buffer: &GpuBuffer,
        data: &[u8],
    ) -> Result<(), VulkanError> {
        let mapped = unsafe {
            device.map_memory(
                buffer.memory.handle,
                0,
                vk::WHOLE_SIZE,
                vk::MemoryMapFlags::empty(),
            )?
        };
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), mapped.cast::<u8>(), data.len()) };
        let result = if !buffer
            .flags
            .contains(vk::MemoryPropertyFlags::HOST_COHERENT)
        {
            let range = [vk::MappedMemoryRange::default()
                .memory(buffer.memory.handle)
                .offset(0)
                .size(vk::WHOLE_SIZE)];
            unsafe { device.flush_mapped_memory_ranges(&range) }.map_err(VulkanError::from)
        } else {
            Ok(())
        };
        unsafe { device.unmap_memory(buffer.memory.handle) };
        result
    }

    fn upload_buffer(
        device: &DeviceState,
        bytes: &[u8],
        usage: vk::BufferUsageFlags,
    ) -> Result<GpuBuffer, VulkanError> {
        #[cfg(feature = "profiling")]
        let started = Instant::now();
        #[cfg(feature = "profiling")]
        let mut reuse_wait = Duration::ZERO;
        #[cfg(feature = "profiling")]
        let mut completion_wait = Duration::ZERO;
        let size = bytes.len() as u64;
        let staging = create_buffer(
            &device.device,
            &device.memory,
            &device.memory_stats,
            size,
            vk::BufferUsageFlags::TRANSFER_SRC,
            vk::MemoryPropertyFlags::HOST_VISIBLE,
            vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let destination = match create_buffer(
            &device.device,
            &device.memory,
            &device.memory_stats,
            size,
            vk::BufferUsageFlags::TRANSFER_DST | usage,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::empty(),
        ) {
            Ok(buffer) => buffer,
            Err(error) => {
                destroy_buffer(&device.device, staging);
                return Err(error);
            }
        };
        let result = (|| {
            write_buffer(&device.device, &staging, bytes)?;
            unsafe {
                #[cfg(feature = "profiling")]
                let wait_started = Instant::now();
                device
                    .device
                    .wait_for_fences(&[device.fence], true, u64::MAX)?;
                #[cfg(feature = "profiling")]
                {
                    reuse_wait += wait_started.elapsed();
                }
                device.device.reset_command_buffer(
                    device.command_buffer,
                    vk::CommandBufferResetFlags::empty(),
                )?;
                device.device.begin_command_buffer(
                    device.command_buffer,
                    &vk::CommandBufferBeginInfo::default(),
                )?;
                device.device.cmd_copy_buffer(
                    device.command_buffer,
                    staging.buffer,
                    destination.buffer,
                    &[vk::BufferCopy::default().size(size)],
                );
                let access = if usage.contains(vk::BufferUsageFlags::VERTEX_BUFFER) {
                    vk::AccessFlags::VERTEX_ATTRIBUTE_READ
                } else {
                    vk::AccessFlags::INDEX_READ
                };
                let barriers = [vk::BufferMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(access)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .buffer(destination.buffer)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)];
                device.device.cmd_pipeline_barrier(
                    device.command_buffer,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::VERTEX_INPUT,
                    vk::DependencyFlags::empty(),
                    &[],
                    &barriers,
                    &[],
                );
                device.device.end_command_buffer(device.command_buffer)?;
                device.device.reset_fences(&[device.fence])?;
                device.device.queue_submit(
                    device.queue,
                    &[vk::SubmitInfo::default().command_buffers(&[device.command_buffer])],
                    device.fence,
                )?;
                #[cfg(feature = "profiling")]
                let wait_started = Instant::now();
                device
                    .device
                    .wait_for_fences(&[device.fence], true, u64::MAX)?;
                #[cfg(feature = "profiling")]
                {
                    completion_wait += wait_started.elapsed();
                }
            }
            Ok::<_, VulkanError>(())
        })();
        destroy_buffer(&device.device, staging);
        #[cfg(feature = "profiling")]
        if result.is_ok() {
            eprintln!(
                "X173GE_UPLOAD stage=buffer purpose={} bytes={} total_ms={:.3} reuse_wait_ms={:.3} completion_wait_ms={:.3}",
                if usage.contains(vk::BufferUsageFlags::VERTEX_BUFFER) {
                    "vertex"
                } else {
                    "index"
                },
                size,
                started.elapsed().as_secs_f64() * 1000.0,
                reuse_wait.as_secs_f64() * 1000.0,
                completion_wait.as_secs_f64() * 1000.0
            );
        }
        if let Err(error) = result {
            destroy_buffer(&device.device, destination);
            return Err(error);
        }
        Ok(destination)
    }

    fn create_sampler(
        device: &ash::Device,
        settings: TextureSampler,
    ) -> Result<vk::Sampler, VulkanError> {
        let filter = |filter| match filter {
            TextureFilter::Nearest => vk::Filter::NEAREST,
            TextureFilter::Linear => vk::Filter::LINEAR,
        };
        let address = |wrap| match wrap {
            TextureWrap::ClampToEdge => vk::SamplerAddressMode::CLAMP_TO_EDGE,
            TextureWrap::MirroredRepeat => vk::SamplerAddressMode::MIRRORED_REPEAT,
            TextureWrap::Repeat => vk::SamplerAddressMode::REPEAT,
        };
        let (mipmap_mode, max_lod) = match settings.mipmap_mode {
            MipmapMode::None => (vk::SamplerMipmapMode::NEAREST, 0.0),
            MipmapMode::Nearest => (vk::SamplerMipmapMode::NEAREST, 1000.0),
            MipmapMode::Linear => (vk::SamplerMipmapMode::LINEAR, 1000.0),
        };
        unsafe {
            Ok(device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(filter(settings.mag_filter))
                    .min_filter(filter(settings.min_filter))
                    .mipmap_mode(mipmap_mode)
                    .address_mode_u(address(settings.wrap_u))
                    .address_mode_v(address(settings.wrap_v))
                    .address_mode_w(vk::SamplerAddressMode::REPEAT)
                    .min_lod(0.0)
                    .max_lod(max_lod),
                None,
            )?)
        }
    }

    fn create_shadow_compare_sampler(device: &ash::Device) -> Result<vk::Sampler, VulkanError> {
        let info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_BORDER)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_BORDER)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_BORDER)
            .border_color(vk::BorderColor::FLOAT_OPAQUE_WHITE)
            .compare_enable(true)
            .compare_op(vk::CompareOp::LESS_OR_EQUAL)
            .min_lod(0.0)
            .max_lod(0.0);
        unsafe { Ok(device.create_sampler(&info, None)?) }
    }

    fn settings_needs_mips(settings: TextureSampler) -> bool {
        settings.mipmap_mode != MipmapMode::None
    }

    fn texture_descriptor_info(
        device: &mut DeviceState,
        binding: Option<x173ge_protocol::TextureBinding>,
        srgb: bool,
    ) -> Result<vk::DescriptorImageInfo, VulkanError> {
        let fallback = device
            .fallback_image
            .as_ref()
            .expect("material fallback image is initialized");
        let (view, settings) = match binding {
            Some(binding) => {
                let image = device.images.get(&binding.image).ok_or_else(|| {
                    VulkanError::Unsupported("material references an unuploaded image".into())
                })?;
                if settings_needs_mips(binding.sampler)
                    && image.mip_levels == 1
                    && image.extent.width().max(image.extent.height()) > 1
                {
                    return Err(VulkanError::Unsupported(
                        "texture sampler requires mipmaps unsupported by this device".into(),
                    ));
                }
                if image.color_space
                    != if srgb {
                        ImageColorSpace::Srgb
                    } else {
                        ImageColorSpace::Linear
                    }
                {
                    return Err(VulkanError::Unsupported(
                        "material texture color space does not match its glTF slot".into(),
                    ));
                }
                (
                    if srgb {
                        image.srgb_view
                    } else {
                        image.linear_view
                    },
                    binding.sampler,
                )
            }
            None => (
                if srgb {
                    fallback.srgb_view
                } else {
                    fallback.linear_view
                },
                TextureSampler::default(),
            ),
        };
        let sampler = match device.samplers.get(&settings) {
            Some(&sampler) => sampler,
            None => {
                let sampler = create_sampler(&device.device, settings)?;
                device.samplers.insert(settings, sampler);
                sampler
            }
        };
        Ok(vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL))
    }

    fn upload_rgba8_image(
        device: &DeviceState,
        id: Option<ImageId>,
        extent: Extent2d,
        pixels: &[u8],
        color_space: ImageColorSpace,
    ) -> Result<GpuImage, VulkanError> {
        #[cfg(feature = "profiling")]
        let started = Instant::now();
        #[cfg(feature = "profiling")]
        let mut reuse_wait = Duration::ZERO;
        #[cfg(feature = "profiling")]
        let mut completion_wait = Duration::ZERO;
        let staging = with_context(
            "create image-upload staging buffer",
            create_buffer(
                &device.device,
                &device.memory,
                &device.memory_stats,
                pixels.len() as u64,
                vk::BufferUsageFlags::TRANSFER_SRC,
                vk::MemoryPropertyFlags::HOST_VISIBLE,
                vk::MemoryPropertyFlags::HOST_COHERENT,
            ),
        )?;
        let mut image = GpuImage {
            image: vk::Image::null(),
            memory: GpuMemory::empty(),
            linear_view: vk::ImageView::null(),
            srgb_view: vk::ImageView::null(),
            color_space,
            mip_levels: if device.mipmaps_supported {
                u32::BITS - extent.width().max(extent.height()).leading_zeros()
            } else {
                1
            },
            extent,
        };
        let result = (|| {
            image.image = unsafe {
                api_call(
                    "create_image(texture upload)",
                    device.device.create_image(
                        &vk::ImageCreateInfo::default()
                            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
                            .image_type(vk::ImageType::TYPE_2D)
                            .format(match color_space {
                                ImageColorSpace::Linear => vk::Format::R8G8B8A8_UNORM,
                                ImageColorSpace::Srgb => vk::Format::R8G8B8A8_SRGB,
                            })
                            .extent(vk::Extent3D {
                                width: extent.width(),
                                height: extent.height(),
                                depth: 1,
                            })
                            .mip_levels(image.mip_levels)
                            .array_layers(1)
                            .samples(vk::SampleCountFlags::TYPE_1)
                            .tiling(vk::ImageTiling::OPTIMAL)
                            .usage(
                                vk::ImageUsageFlags::TRANSFER_DST
                                    | vk::ImageUsageFlags::SAMPLED
                                    | if image.mip_levels > 1 {
                                        vk::ImageUsageFlags::TRANSFER_SRC
                                    } else {
                                        vk::ImageUsageFlags::empty()
                                    },
                            )
                            .sharing_mode(vk::SharingMode::EXCLUSIVE)
                            .initial_layout(vk::ImageLayout::UNDEFINED),
                        None,
                    ),
                )?
            };
            let requirements = unsafe { device.device.get_image_memory_requirements(image.image) };
            let (index, _) = with_context(
                "select image-upload memory type",
                memory_type(
                    &device.memory,
                    requirements.memory_type_bits,
                    vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    vk::MemoryPropertyFlags::empty(),
                ),
            )?;
            image.memory = with_context(
                "allocate image-upload memory",
                GpuMemory::allocate(&device.device, &requirements, index, &device.memory_stats),
            )?;
            unsafe {
                api_call(
                    "bind_image_memory(texture upload)",
                    device
                        .device
                        .bind_image_memory(image.image, image.memory.handle, 0),
                )?
            };
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .base_mip_level(0)
                .level_count(image.mip_levels)
                .base_array_layer(0)
                .layer_count(1);
            let view = |format, operation| unsafe {
                api_call(
                    operation,
                    device.device.create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image.image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(format)
                            .subresource_range(range),
                        None,
                    ),
                )
            };
            image.linear_view = view(
                vk::Format::R8G8B8A8_UNORM,
                "create_image_view(texture linear)",
            )?;
            image.srgb_view = view(vk::Format::R8G8B8A8_SRGB, "create_image_view(texture sRGB)")?;
            with_context(
                "write image-upload staging buffer",
                write_buffer(&device.device, &staging, pixels),
            )?;
            unsafe {
                #[cfg(feature = "profiling")]
                let wait_started = Instant::now();
                api_call(
                    "wait_for_fences(before image upload)",
                    device
                        .device
                        .wait_for_fences(&[device.fence], true, u64::MAX),
                )?;
                #[cfg(feature = "profiling")]
                {
                    reuse_wait += wait_started.elapsed();
                }
                api_call(
                    "reset_command_buffer(image upload)",
                    device.device.reset_command_buffer(
                        device.command_buffer,
                        vk::CommandBufferResetFlags::empty(),
                    ),
                )?;
                api_call(
                    "begin_command_buffer(image upload)",
                    device.device.begin_command_buffer(
                        device.command_buffer,
                        &vk::CommandBufferBeginInfo::default(),
                    ),
                )?;
                device.device.cmd_pipeline_barrier(
                    device.command_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::empty())
                        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image.image)
                        .subresource_range(range)],
                );
                device.device.cmd_copy_buffer_to_image(
                    device.command_buffer,
                    staging.buffer,
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy::default()
                        .image_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .mip_level(0)
                                .base_array_layer(0)
                                .layer_count(1),
                        )
                        .image_extent(vk::Extent3D {
                            width: extent.width(),
                            height: extent.height(),
                            depth: 1,
                        })],
                );
                let mut mip_width = extent.width();
                let mut mip_height = extent.height();
                for level in 1..image.mip_levels {
                    let source = vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .base_mip_level(level - 1)
                        .level_count(1)
                        .base_array_layer(0)
                        .layer_count(1);
                    device.device.cmd_pipeline_barrier(
                        device.command_buffer,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[vk::ImageMemoryBarrier::default()
                            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .image(image.image)
                            .subresource_range(source)],
                    );
                    let next_width = (mip_width / 2).max(1);
                    let next_height = (mip_height / 2).max(1);
                    device.device.cmd_blit_image(
                        device.command_buffer,
                        image.image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        image.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::ImageBlit::default()
                            .src_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .mip_level(level - 1)
                                    .base_array_layer(0)
                                    .layer_count(1),
                            )
                            .src_offsets([
                                vk::Offset3D { x: 0, y: 0, z: 0 },
                                vk::Offset3D {
                                    x: mip_width as i32,
                                    y: mip_height as i32,
                                    z: 1,
                                },
                            ])
                            .dst_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .mip_level(level)
                                    .base_array_layer(0)
                                    .layer_count(1),
                            )
                            .dst_offsets([
                                vk::Offset3D { x: 0, y: 0, z: 0 },
                                vk::Offset3D {
                                    x: next_width as i32,
                                    y: next_height as i32,
                                    z: 1,
                                },
                            ])],
                        vk::Filter::LINEAR,
                    );
                    device.device.cmd_pipeline_barrier(
                        device.command_buffer,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::FRAGMENT_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[vk::ImageMemoryBarrier::default()
                            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                            .dst_access_mask(vk::AccessFlags::SHADER_READ)
                            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .image(image.image)
                            .subresource_range(source)],
                    );
                    mip_width = next_width;
                    mip_height = next_height;
                }
                let last_mip = vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(image.mip_levels - 1)
                    .level_count(1)
                    .base_array_layer(0)
                    .layer_count(1);
                device.device.cmd_pipeline_barrier(
                    device.command_buffer,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_READ)
                        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image.image)
                        .subresource_range(last_mip)],
                );
                api_call(
                    "end_command_buffer(image upload)",
                    device.device.end_command_buffer(device.command_buffer),
                )?;
                api_call(
                    "reset_fences(image upload)",
                    device.device.reset_fences(&[device.fence]),
                )?;
                api_call(
                    "queue_submit(image upload)",
                    device.device.queue_submit(
                        device.queue,
                        &[vk::SubmitInfo::default().command_buffers(&[device.command_buffer])],
                        device.fence,
                    ),
                )?;
                #[cfg(feature = "profiling")]
                let wait_started = Instant::now();
                api_call(
                    "wait_for_fences(image upload completion)",
                    device
                        .device
                        .wait_for_fences(&[device.fence], true, u64::MAX),
                )?;
                #[cfg(feature = "profiling")]
                {
                    completion_wait += wait_started.elapsed();
                }
            }
            Ok::<_, VulkanError>(())
        })();
        destroy_buffer(&device.device, staging);
        #[cfg(feature = "profiling")]
        if result.is_ok() {
            eprintln!(
                "X173GE_UPLOAD stage=image_gpu id={id:?} bytes={} width={} height={} mip_levels={} total_ms={:.3} reuse_wait_ms={:.3} completion_wait_ms={:.3}",
                pixels.len(),
                extent.width(),
                extent.height(),
                image.mip_levels,
                started.elapsed().as_secs_f64() * 1000.0,
                reuse_wait.as_secs_f64() * 1000.0,
                completion_wait.as_secs_f64() * 1000.0
            );
        }
        if let Err(error) = result {
            eprintln!(
                "X173GE_UPLOAD stage=image_gpu_failed id={id:?} width={} height={} color_space={color_space:?} error={error}",
                extent.width(),
                extent.height(),
            );
            destroy_image(&device.device, image);
            return Err(error);
        }
        Ok(image)
    }

    #[cfg(feature = "smaa")]
    fn initialize_smaa_lookups(device: &mut DeviceState) -> Result<(), VulkanError> {
        if device.smaa_lookups.is_some() {
            return Ok(());
        }
        let expand = |bytes: &[u8], channels: usize| {
            let mut rgba = vec![0; bytes.len() / channels * 4];
            for (source, pixel) in bytes.chunks_exact(channels).zip(rgba.chunks_exact_mut(4)) {
                pixel[..channels].copy_from_slice(source);
                pixel[3] = 255;
            }
            rgba
        };
        let area_bytes = include_bytes!("../assets/smaa/area_tex.rg8");
        let area_pixels = expand(area_bytes, 2);
        let area = upload_rgba8_image(
            device,
            None,
            Extent2d::new(160, 560).expect("nonzero SMAA area texture extent"),
            &area_pixels,
            ImageColorSpace::Linear,
        )?;
        let search_bytes = include_bytes!("../assets/smaa/search_tex.r8");
        let search_pixels = expand(search_bytes, 1);
        let search = match upload_rgba8_image(
            device,
            None,
            Extent2d::new(64, 16).expect("nonzero SMAA search texture extent"),
            &search_pixels,
            ImageColorSpace::Linear,
        ) {
            Ok(image) => image,
            Err(error) => {
                destroy_image(&device.device, area);
                return Err(error);
            }
        };
        device.smaa_lookups = Some(SmaaLookups { area, search });
        Ok(())
    }

    fn make_material_uniform(material: MaterialData) -> MaterialUniform {
        MaterialUniform {
            base_color_factor: material.base_color_factor,
            pbr: [
                material.metallic_factor,
                material.roughness_factor,
                material.emissive_strength,
                material.specular_factor,
            ],
            emissive_factor: [
                material.emissive_factor[0],
                material.emissive_factor[1],
                material.emissive_factor[2],
                0.0,
            ],
            specular_color_factor: [
                material.specular_color_factor[0],
                material.specular_color_factor[1],
                material.specular_color_factor[2],
                0.0,
            ],
            surface: [
                material.normal_scale,
                material.occlusion_strength,
                material.alpha_cutoff,
                material.transmission_factor,
            ],
            flags: [
                match material.alpha_mode {
                    AlphaMode::Opaque => 0,
                    AlphaMode::Mask => 1,
                    AlphaMode::Blend => 2,
                },
                u32::from(material.double_sided),
                u32::from(material.unlit),
                u32::from(material.normal_texture.is_some()),
            ],
        }
    }

    fn make_frame_uniform(
        frame: &FrameSnapshot,
        shadowed_light: Option<(usize, DirectionalShadow)>,
        cascades: &[Cascade],
        camera_forward: Vec3,
        gtao_enabled: bool,
        ssr_enabled: bool,
        god_rays: Option<x173ge_protocol::GodRaysSettings>,
        mboit_enabled: bool,
        environment: Option<EnvironmentFrame>,
    ) -> Result<FrameUniform, VulkanError> {
        let view_projection = Mat4::from_cols_array_2d(&frame.camera.view_projection);
        let inverse_view_projection = view_projection.inverse();
        if (gtao_enabled || ssr_enabled || god_rays.is_some())
            && !inverse_view_projection
                .to_cols_array()
                .iter()
                .all(|value| value.is_finite())
        {
            return Err(VulkanError::Unsupported(
                "GTAO, SSR, and God Rays require an invertible camera view-projection matrix"
                    .into(),
            ));
        }
        let (near, far, perspective) = match frame.camera.projection {
            x173ge_protocol::CameraProjection::Perspective { near, far } => (near, far, 1.0),
            x173ge_protocol::CameraProjection::Orthographic { near, far } => (near, far, 0.0),
        };
        let mut uniform = FrameUniform {
            view_projection: view_projection.to_cols_array_2d(),
            inverse_view_projection: if gtao_enabled || ssr_enabled || god_rays.is_some() {
                inverse_view_projection.to_cols_array_2d()
            } else {
                Mat4::IDENTITY.to_cols_array_2d()
            },
            camera_position: [
                frame.camera.position[0],
                frame.camera.position[1],
                frame.camera.position[2],
                1.0,
            ],
            ambient_diffuse: [
                frame.ambient_diffuse[0],
                frame.ambient_diffuse[1],
                frame.ambient_diffuse[2],
                0.0,
            ],
            ambient_diffuse_y: [
                frame.ambient_diffuse_y[0],
                frame.ambient_diffuse_y[1],
                frame.ambient_diffuse_y[2],
                0.0,
            ],
            camera_forward: [camera_forward.x, camera_forward.y, camera_forward.z, 0.0],
            gtao_parameters: [f32::from(gtao_enabled), near, far, perspective],
            god_rays_parameters: god_rays.map_or([0.0; 4], |settings| {
                [
                    settings.extinction_per_unit,
                    settings.scattering_albedo,
                    settings.anisotropy,
                    0.0,
                ]
            }),
            god_rays_counts: god_rays.map_or([0, 1, 0, 0], |settings| {
                [settings.sample_count, settings.interleave_block, 0, 0]
            }),
            light_count: [
                frame.directional_lights.len() as u32,
                shadowed_light.map_or(0, |(index, _)| index as u32),
                cascades.len() as u32,
                u32::from(shadowed_light.is_some()),
            ],
            environment_irradiance_sh: environment.map_or([[0.0; 4]; 9], |environment| {
                std::array::from_fn(|index| {
                    let [r, g, b] = environment.diffuse_irradiance_sh[index];
                    [r, g, b, 0.0]
                })
            }),
            environment_parameters: environment.map_or([0.0; 4], |environment| {
                [
                    environment.settings.intensity,
                    environment.settings.rotation_y_radians,
                    0.0,
                    1.0,
                ]
            }),
            ..FrameUniform::default()
        };
        if let Some((_, settings)) = shadowed_light {
            uniform.shadow_parameters = [
                settings.angular_radius_radians,
                settings.max_penumbra_world,
                SHADOW_MAP_SIZES[0] as f32,
                f32::from(mboit_enabled),
            ];
            for (index, cascade) in cascades.iter().enumerate().take(CASCADE_COUNT) {
                uniform.cascade_splits[index] = cascade.split_far;
                uniform.shadow_matrices[index] = cascade.view_projection.to_cols_array_2d();
                uniform.shadow_depth[index] = [
                    cascade.light_near,
                    cascade.light_far,
                    cascade.half_extent,
                    cascade.blend_start,
                ];
            }
        }
        for (target, light) in uniform.lights.iter_mut().zip(&frame.directional_lights) {
            target.direction_to_light = [
                light.direction_to_light[0],
                light.direction_to_light[1],
                light.direction_to_light[2],
                0.0,
            ];
            target.radiance = [light.radiance[0], light.radiance[1], light.radiance[2], 0.0];
        }
        uniform.punctual_count[0] = frame.punctual_lights.len() as u32;
        for (target, light) in uniform
            .punctual_lights
            .iter_mut()
            .zip(&frame.punctual_lights)
        {
            *target = match *light {
                PunctualLight::Point {
                    position,
                    color,
                    intensity,
                    range,
                } => GpuPunctualLight {
                    position_range: [position[0], position[1], position[2], range.unwrap_or(0.0)],
                    color_intensity: [color[0], color[1], color[2], intensity],
                    ..GpuPunctualLight::default()
                },
                PunctualLight::Spot {
                    position,
                    direction,
                    color,
                    intensity,
                    range,
                    inner_cone_angle,
                    outer_cone_angle,
                } => {
                    let axis = Vec3::from_array(direction);
                    let direction = (axis / axis.abs().max_element()).normalize().to_array();
                    GpuPunctualLight {
                        position_range: [
                            position[0],
                            position[1],
                            position[2],
                            range.unwrap_or(0.0),
                        ],
                        direction_type: [direction[0], direction[1], direction[2], 1.0],
                        color_intensity: [color[0], color[1], color[2], intensity],
                        cone_cosines: [inner_cone_angle.cos(), outer_cone_angle.cos(), 0.0, 0.0],
                    }
                }
            };
        }
        Ok(uniform)
    }

    /// Selects a present-capable graphics+compute queue, preferring a discrete GPU.
    fn choose_physical_device(
        instance: &ash::Instance,
        surface_loader: &surface::Instance,
        surface: vk::SurfaceKHR,
        require_independent_blend: bool,
        required_color_attachments: u32,
    ) -> Result<(vk::PhysicalDevice, u32), VulkanError> {
        let mut selected = None;
        for physical in unsafe { instance.enumerate_physical_devices()? } {
            let properties = unsafe { instance.get_physical_device_properties(physical) };
            if properties.limits.max_color_attachments < required_color_attachments {
                continue;
            }
            if require_independent_blend
                && unsafe { instance.get_physical_device_features(physical) }.independent_blend
                    == vk::FALSE
            {
                continue;
            }
            let extensions = unsafe { instance.enumerate_device_extension_properties(physical)? };
            if !extensions.iter().any(|extension| unsafe {
                CStr::from_ptr(extension.extension_name.as_ptr()) == swapchain::NAME
            }) {
                continue;
            }
            let queues = unsafe { instance.get_physical_device_queue_family_properties(physical) };
            for (index, queue) in queues.iter().enumerate() {
                if queue
                    .queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
                    && unsafe {
                        surface_loader.get_physical_device_surface_support(
                            physical,
                            index as u32,
                            surface,
                        )?
                    }
                {
                    let priority = physical_device_priority(properties.device_type);
                    if selected.is_none_or(|(_, _, best)| priority > best) {
                        selected = Some((physical, index as u32, priority));
                    }
                    break;
                }
            }
        }
        if let Some((physical, queue_family, _)) = selected {
            return Ok((physical, queue_family));
        }
        Err(VulkanError::Unsupported(format!(
            "no graphics+compute queue supports VK_KHR_swapchain presentation and the requested renderer requirements ({required_color_attachments} color attachments, independentBlend={require_independent_blend})"
        )))
    }

    fn physical_device_priority(device_type: vk::PhysicalDeviceType) -> u8 {
        match device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => 3,
            vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
            vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
            _ => 0,
        }
    }

    /// Creates the swapchain and rolls back partial resources on failure.
    fn create_swapchain(
        device: &DeviceState,
        surface_loader: &surface::Instance,
        surface: vk::SurfaceKHR,
        requested: Extent2d,
        vsync: bool,
        old_swapchain: vk::SwapchainKHR,
    ) -> Result<SwapchainResources, SwapchainFailure> {
        let capabilities = unsafe {
            surface_loader.get_physical_device_surface_capabilities(device.physical, surface)?
        };
        if !capabilities
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::COLOR_ATTACHMENT)
        {
            return Err(VulkanError::Unsupported(
                "surface images cannot be used as color attachments".into(),
            )
            .into());
        }
        let formats = unsafe {
            surface_loader.get_physical_device_surface_formats(device.physical, surface)?
        };
        let Some(format) = formats.iter().find(|format| {
            format.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                && matches!(
                    format.format,
                    vk::Format::B8G8R8A8_SRGB | vk::Format::R8G8B8A8_SRGB | vk::Format::UNDEFINED
                )
        }) else {
            return Err(VulkanError::Unsupported(
                "surface lacks an sRGB nonlinear color attachment format".into(),
            )
            .into());
        };
        let (format, color_space) = if format.format == vk::Format::UNDEFINED {
            (vk::Format::B8G8R8A8_SRGB, format.color_space)
        } else {
            (format.format, format.color_space)
        };
        let extent = if capabilities.current_extent.width != u32::MAX {
            capabilities.current_extent
        } else {
            vk::Extent2D {
                width: requested.width().clamp(
                    capabilities.min_image_extent.width,
                    capabilities.max_image_extent.width,
                ),
                height: requested.height().clamp(
                    capabilities.min_image_extent.height,
                    capabilities.max_image_extent.height,
                ),
            }
        };
        let max_images = if capabilities.max_image_count == 0 {
            u32::MAX
        } else {
            capabilities.max_image_count
        };
        let image_count = capabilities
            .min_image_count
            .saturating_add(1)
            .min(max_images);
        let present_modes = unsafe {
            surface_loader.get_physical_device_surface_present_modes(device.physical, surface)?
        };
        let (present_mode, present_mode_name) = if vsync {
            (vk::PresentModeKHR::FIFO, "FIFO")
        } else if present_modes.contains(&vk::PresentModeKHR::IMMEDIATE) {
            (vk::PresentModeKHR::IMMEDIATE, "IMMEDIATE")
        } else if present_modes.contains(&vk::PresentModeKHR::MAILBOX) {
            (vk::PresentModeKHR::MAILBOX, "MAILBOX")
        } else {
            (vk::PresentModeKHR::FIFO, "FIFO")
        };
        eprintln!("X173GE_PRESENT_MODE vsync_requested={vsync} mode={present_mode_name}");
        let alpha = [
            vk::CompositeAlphaFlagsKHR::OPAQUE,
            vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::INHERIT,
        ]
        .into_iter()
        .find(|value| capabilities.supported_composite_alpha.contains(*value))
        .ok_or_else(|| VulkanError::Unsupported("surface has no composite alpha mode".into()))?;
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(image_count)
            .image_format(format)
            .image_color_space(color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(alpha)
            .present_mode(present_mode)
            .clipped(true)
            .old_swapchain(old_swapchain);
        let swapchain = unsafe { device.swapchain.create_swapchain(&info, None)? };
        let mut views = Vec::new();
        let mut render_pass = vk::RenderPass::null();
        let mut framebuffers = Vec::new();
        let mut present_semaphores = Vec::new();
        let mut scene = SceneTargets {
            color: null_attachment(),
            normal_roughness: null_attachment(),
            #[cfg(feature = "ssr")]
            reflectance: None,
            depth: null_attachment(),
            framebuffer: vk::Framebuffer::null(),
        };
        let mut exposure = null_exposure_targets();
        #[cfg(feature = "ssr")]
        let mut ssr = None;
        #[cfg(feature = "god-rays")]
        let mut god_rays = None;
        #[cfg(feature = "smaa")]
        let mut smaa = None;
        #[cfg(feature = "bloom")]
        let mut bloom = None;
        #[cfg(feature = "gtao")]
        let mut gtao = None;
        let mut post_descriptor_pool = vk::DescriptorPool::null();
        let mut post_descriptor_set = vk::DescriptorSet::null();
        #[cfg(feature = "smaa")]
        let mut smaa_descriptor_sets = None;
        let mut pipeline = vk::Pipeline::null();
        #[cfg(feature = "bloom")]
        let mut bloom_pipeline = None;
        #[cfg(feature = "smaa")]
        let mut smaa_pipeline = None;
        let result = (|| {
            for image in unsafe { device.swapchain.get_swapchain_images(swapchain)? } {
                views.push(unsafe {
                    device.device.create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(format)
                            .subresource_range(vk::ImageSubresourceRange {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                base_mip_level: 0,
                                level_count: 1,
                                base_array_layer: 0,
                                layer_count: 1,
                            }),
                        None,
                    )?
                });
            }
            render_pass = create_present_render_pass(&device.device, format)?;
            scene = create_scene_targets(device, extent)?;
            exposure = create_exposure_targets(device, extent)?;
            #[cfg(feature = "ssr")]
            if device.features.ssr.is_some() {
                ssr = Some(create_ssr_targets(device, &scene, extent)?);
            }
            #[cfg(feature = "god-rays")]
            if device.features.god_rays.is_some() {
                god_rays = Some(create_god_rays_targets(device, extent)?);
            }
            #[cfg(feature = "gtao")]
            if device.features.gtao {
                gtao = Some(create_gtao_targets(device, &scene, extent)?);
            }
            #[cfg(feature = "smaa")]
            if device.features.smaa {
                smaa = Some(create_smaa_targets(device, extent)?);
            }
            #[cfg(feature = "bloom")]
            if device.features.bloom.is_some() {
                bloom = Some(create_bloom_targets(device, extent)?);
            }
            for &view in &views {
                framebuffers.push(unsafe {
                    device.device.create_framebuffer(
                        &vk::FramebufferCreateInfo::default()
                            .render_pass(render_pass)
                            .attachments(&[view])
                            .width(extent.width)
                            .height(extent.height)
                            .layers(1),
                        None,
                    )?
                });
                present_semaphores.push(unsafe {
                    device
                        .device
                        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?
                });
            }
            post_descriptor_pool = unsafe {
                #[cfg(feature = "bloom")]
                let bloom_sets = bloom
                    .as_ref()
                    .map_or(0, |targets| targets.levels.len() * 2 - 1);
                #[cfg(not(feature = "bloom"))]
                let bloom_sets = 0;
                let smaa_post_sets = if device.features.smaa { 2 } else { 0 };
                let smaa_weight_sets = usize::from(device.features.smaa);
                #[cfg(feature = "ssr")]
                let ssr_sets = usize::from(ssr.is_some());
                #[cfg(not(feature = "ssr"))]
                let ssr_sets = 0;
                #[cfg(feature = "god-rays")]
                let god_rays_sets = 3 * usize::from(god_rays.is_some());
                #[cfg(not(feature = "god-rays"))]
                let god_rays_sets = 0;
                let exposure_sets = exposure.pyramid.len() + 2;
                let post_sets =
                    1 + ssr_sets + god_rays_sets + smaa_post_sets + bloom_sets + exposure_sets;
                let max_sets = post_sets + smaa_weight_sets;
                let descriptor_count = 3 * post_sets + 3 * smaa_weight_sets;
                device.device.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(max_sets as u32)
                        .pool_sizes(&[vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                            .descriptor_count(descriptor_count as u32)]),
                    None,
                )?
            };
            post_descriptor_set = unsafe {
                device.device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(post_descriptor_pool)
                        .set_layouts(&[device.post_descriptor_set_layout]),
                )?[0]
            };
            #[cfg(feature = "ssr")]
            if let Some(targets) = &mut ssr {
                targets.descriptor_set = unsafe {
                    device.device.allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(post_descriptor_pool)
                            .set_layouts(&[device.post_descriptor_set_layout]),
                    )?[0]
                };
                write_sampled_images(
                    &device.device,
                    targets.descriptor_set,
                    device.post_sampler,
                    &[scene.color.view, scene.color.view],
                );
            }
            #[cfg(feature = "ssr")]
            #[allow(unused_mut)]
            let mut final_scene_view = ssr
                .as_ref()
                .map_or(scene.color.view, |targets| targets.color.view);
            #[cfg(not(feature = "ssr"))]
            #[allow(unused_mut)]
            let mut final_scene_view = scene.color.view;
            #[cfg(feature = "god-rays")]
            if let Some(targets) = &mut god_rays {
                let sets = unsafe {
                    device.device.allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(post_descriptor_pool)
                            .set_layouts(&[device.post_descriptor_set_layout; 3]),
                    )?
                };
                targets.horizontal_set = sets[0];
                targets.vertical_set = sets[1];
                targets.composite_set = sets[2];
                write_sampled_images(
                    &device.device,
                    targets.horizontal_set,
                    device.post_sampler,
                    &[targets.march.view, targets.march.view],
                );
                write_sampled_images(
                    &device.device,
                    targets.vertical_set,
                    device.post_sampler,
                    &[targets.horizontal.view, targets.horizontal.view],
                );
                write_sampled_images(
                    &device.device,
                    targets.composite_set,
                    device.post_sampler,
                    &[final_scene_view, targets.scattering.view],
                );
                final_scene_view = targets.march.view;
            }
            let exposure_sets = unsafe {
                device.device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(post_descriptor_pool)
                        .set_layouts(&vec![
                            device.post_descriptor_set_layout;
                            exposure.pyramid.len() + 2
                        ]),
                )?
            };
            for index in 0..exposure.pyramid.len() {
                let source_view = if index == 0 {
                    final_scene_view
                } else {
                    exposure.pyramid[index - 1].image.view
                };
                let target = &mut exposure.pyramid[index];
                target.descriptor_set = exposure_sets[index];
                write_sampled_images(
                    &device.device,
                    target.descriptor_set,
                    device.post_sampler,
                    &[source_view, source_view, source_view],
                );
            }
            let average_view = exposure
                .pyramid
                .last()
                .expect("exposure reduction pyramid has at least one level")
                .image
                .view;
            exposure.adaptation_sets = [
                exposure_sets[exposure.pyramid.len()],
                exposure_sets[exposure.pyramid.len() + 1],
            ];
            for set in exposure.adaptation_sets {
                write_sampled_images(
                    &device.device,
                    set,
                    device.post_sampler,
                    &[average_view, average_view, average_view],
                );
            }
            #[cfg(feature = "bloom")]
            let post_views = {
                let bloom_view = bloom
                    .as_ref()
                    .map_or(final_scene_view, BloomTargets::final_view);
                [final_scene_view, bloom_view, exposure.history[0].view]
            };
            #[cfg(not(feature = "bloom"))]
            let post_views = [final_scene_view, final_scene_view, exposure.history[0].view];
            write_sampled_images(
                &device.device,
                post_descriptor_set,
                device.post_sampler,
                &post_views,
            );
            #[cfg(feature = "smaa")]
            if device.features.smaa {
                let layouts = [
                    device.post_descriptor_set_layout,
                    device.smaa_weights_descriptor_set_layout,
                    device.post_descriptor_set_layout,
                ];
                let sets = unsafe {
                    device.device.allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(post_descriptor_pool)
                            .set_layouts(&layouts),
                    )?
                };
                let mut descriptor_sets = [vk::DescriptorSet::null(); 3];
                descriptor_sets.copy_from_slice(&sets);
                smaa_descriptor_sets = Some(descriptor_sets);
                let targets = smaa.as_ref().expect("SMAA targets were created");
                let lookups = device.smaa_lookups.as_ref().ok_or_else(|| {
                    VulkanError::Unsupported("SMAA lookup textures were not initialized".into())
                })?;
                write_sampled_images(
                    &device.device,
                    descriptor_sets[0],
                    device.post_sampler,
                    &if device.features.bloom.is_some() {
                        vec![targets.post_color.view, targets.post_color.view]
                    } else {
                        vec![targets.post_color.view]
                    },
                );
                write_sampled_images(
                    &device.device,
                    descriptor_sets[1],
                    device.post_sampler,
                    &[
                        targets.edges.view,
                        lookups.area.linear_view,
                        lookups.search.linear_view,
                    ],
                );
                write_sampled_images(
                    &device.device,
                    descriptor_sets[2],
                    device.post_sampler,
                    &[targets.post_color.view, targets.weights.view],
                );
                smaa_pipeline = Some(create_fullscreen_pipeline(
                    &device.device,
                    render_pass,
                    device.smaa_neighborhood_pipeline_layout,
                    include_bytes!("../shaders/smaa/neighborhood.frag.spv"),
                )?);
            }
            #[cfg(feature = "bloom")]
            if let Some(targets) = &mut bloom {
                let level_count = targets.levels.len();
                let layouts = vec![device.post_descriptor_set_layout; level_count * 2 - 1];
                let sets = unsafe {
                    device.device.allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(post_descriptor_pool)
                            .set_layouts(&layouts),
                    )?
                };
                for index in 0..level_count {
                    let set = sets[index];
                    let source = if index == 0 {
                        final_scene_view
                    } else {
                        targets.levels[index - 1].down.view
                    };
                    targets.levels[index].down_set = set;
                    write_sampled_images(
                        &device.device,
                        set,
                        device.post_sampler,
                        &[source, source],
                    );
                }
                for index in 0..(level_count - 1) {
                    let set = sets[level_count + index];
                    let coarse = targets.levels[index + 1]
                        .up
                        .as_ref()
                        .map_or(targets.levels[index + 1].down.view, |up| up.image.view);
                    let fine = targets.levels[index].down.view;
                    let up = targets.levels[index]
                        .up
                        .as_mut()
                        .expect("all but the coarsest level have an upsample target");
                    up.descriptor_set = set;
                    write_sampled_images(&device.device, set, device.post_sampler, &[coarse, fine]);
                }
            }
            pipeline = create_present_pipeline(
                &device.device,
                render_pass,
                device.post_pipeline_layout,
                false,
            )?;
            #[cfg(feature = "bloom")]
            if device.features.bloom.is_some() {
                bloom_pipeline = Some(create_present_pipeline(
                    &device.device,
                    render_pass,
                    device.post_pipeline_layout,
                    true,
                )?);
            }
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            unsafe {
                if pipeline != vk::Pipeline::null() {
                    device.device.destroy_pipeline(pipeline, None);
                }
                #[cfg(feature = "bloom")]
                if let Some(pipeline) = bloom_pipeline {
                    device.device.destroy_pipeline(pipeline, None);
                }
                #[cfg(feature = "smaa")]
                if let Some(pipeline) = smaa_pipeline {
                    device.device.destroy_pipeline(pipeline, None);
                }
                for framebuffer in framebuffers {
                    device.device.destroy_framebuffer(framebuffer, None);
                }
                for semaphore in present_semaphores {
                    device.device.destroy_semaphore(semaphore, None);
                }
                if render_pass != vk::RenderPass::null() {
                    device.device.destroy_render_pass(render_pass, None);
                }
                if post_descriptor_pool != vk::DescriptorPool::null() {
                    device
                        .device
                        .destroy_descriptor_pool(post_descriptor_pool, None);
                }
                #[cfg(feature = "gtao")]
                if let Some(gtao) = gtao {
                    destroy_gtao_targets(&device.device, gtao);
                }
                #[cfg(feature = "ssr")]
                if let Some(ssr) = ssr {
                    destroy_ssr_targets(&device.device, ssr);
                }
                #[cfg(feature = "god-rays")]
                if let Some(targets) = god_rays {
                    destroy_god_rays_targets(&device.device, targets);
                }
                destroy_scene_targets(&device.device, scene);
                destroy_exposure_targets(&device.device, exposure);
                #[cfg(feature = "smaa")]
                if let Some(smaa) = smaa {
                    destroy_smaa_targets(&device.device, smaa);
                }
                #[cfg(feature = "bloom")]
                if let Some(bloom) = bloom {
                    destroy_bloom_targets(&device.device, bloom);
                }
                for view in views {
                    device.device.destroy_image_view(view, None);
                }
                device.swapchain.destroy_swapchain(swapchain, None);
            }
            return Err(SwapchainFailure {
                error,
                old_swapchain_retired: old_swapchain != vk::SwapchainKHR::null(),
            });
        }
        Ok(SwapchainResources {
            swapchain,
            extent,
            views,
            render_pass,
            framebuffers,
            present_semaphores,
            scene,
            exposure,
            #[cfg(feature = "ssr")]
            ssr,
            #[cfg(feature = "god-rays")]
            god_rays,
            post_descriptor_pool,
            post_descriptor_set,
            pipeline,
            #[cfg(feature = "bloom")]
            bloom_pipeline,
            #[cfg(feature = "smaa")]
            smaa_pipeline,
            #[cfg(feature = "smaa")]
            smaa,
            #[cfg(feature = "smaa")]
            smaa_descriptor_sets,
            #[cfg(feature = "bloom")]
            bloom,
            #[cfg(feature = "gtao")]
            gtao,
        })
    }

    fn scene_render_pass_dependencies() -> [vk::SubpassDependency; 2] {
        render_graph::render_pass_dependencies(
            render_graph::previous_scene_scope(),
            render_graph::scene_attachment_scope(),
            render_graph::sampled_scope(),
        )
    }

    fn create_scene_render_pass(
        device: &ash::Device,
        color_format: vk::Format,
        normal_format: vk::Format,
        depth_format: vk::Format,
        reflectance: bool,
    ) -> Result<vk::RenderPass, VulkanError> {
        let color_attachment = |format| {
            vk::AttachmentDescription::default()
                .format(format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(render_graph::SAMPLED_COLOR_LAYOUT)
        };
        let mut attachments = vec![
            color_attachment(color_format),
            color_attachment(normal_format),
        ];
        let mut colors = vec![
            vk::AttachmentReference::default()
                .attachment(0)
                .layout(render_graph::COLOR_ATTACHMENT_LAYOUT),
            vk::AttachmentReference::default()
                .attachment(1)
                .layout(render_graph::COLOR_ATTACHMENT_LAYOUT),
        ];
        if reflectance {
            colors.push(
                vk::AttachmentReference::default()
                    .attachment(2)
                    .layout(render_graph::COLOR_ATTACHMENT_LAYOUT),
            );
            attachments.push(color_attachment(normal_format));
        }
        let depth_index = colors.len() as u32;
        attachments.push(
            vk::AttachmentDescription::default()
                .format(depth_format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(render_graph::SAMPLED_DEPTH_LAYOUT),
        );
        let depth = vk::AttachmentReference::default()
            .attachment(depth_index)
            .layout(render_graph::DEPTH_ATTACHMENT_LAYOUT);
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&colors)
            .depth_stencil_attachment(&depth)];
        let dependencies = scene_render_pass_dependencies();
        unsafe {
            device
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachments)
                        .subpasses(&subpasses)
                        .dependencies(&dependencies),
                    None,
                )
                .map_err(Into::into)
        }
    }

    #[cfg(feature = "ssr")]
    fn create_ssr_pipelines(
        device: &ash::Device,
        color_format: vk::Format,
        normal_format: vk::Format,
        depth_format: vk::Format,
        frame_layout: vk::DescriptorSetLayout,
        color_layout: vk::DescriptorSetLayout,
    ) -> Result<SsrPipelines, VulkanError> {
        let mut pipelines = SsrPipelines {
            render_pass: vk::RenderPass::null(),
            blend_render_pass: vk::RenderPass::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
        };
        let result = (|| {
            pipelines.render_pass = create_sampled_color_render_pass(device, color_format)?;
            pipelines.blend_render_pass =
                create_blend_render_pass(device, color_format, normal_format, depth_format)?;
            pipelines.pipeline_layout = unsafe {
                device.create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&[frame_layout, color_layout])
                        .push_constant_ranges(&[vk::PushConstantRange::default()
                            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                            .offset(0)
                            .size(size_of::<SsrPush>() as u32)]),
                    None,
                )?
            };
            pipelines.pipeline = create_fullscreen_pipeline(
                device,
                pipelines.render_pass,
                pipelines.pipeline_layout,
                include_bytes!("../shaders/ssr.frag.spv"),
            )?;
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_ssr_pipelines(device, pipelines);
            return Err(error);
        }
        Ok(pipelines)
    }

    #[cfg(feature = "ssr")]
    fn create_blend_render_pass(
        device: &ash::Device,
        color_format: vk::Format,
        normal_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<vk::RenderPass, VulkanError> {
        let loaded_color = |format| {
            vk::AttachmentDescription::default()
                .format(format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(render_graph::SAMPLED_COLOR_LAYOUT)
                .final_layout(render_graph::SAMPLED_COLOR_LAYOUT)
        };
        let mut attachments = vec![
            loaded_color(color_format),
            loaded_color(normal_format),
            loaded_color(normal_format),
        ];
        let depth_index = attachments.len() as u32;
        attachments.push(
            vk::AttachmentDescription::default()
                .format(depth_format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(render_graph::SAMPLED_DEPTH_LAYOUT)
                .final_layout(render_graph::SAMPLED_DEPTH_LAYOUT),
        );
        let colors = (0..depth_index)
            .map(|attachment| {
                vk::AttachmentReference::default()
                    .attachment(attachment)
                    .layout(render_graph::COLOR_ATTACHMENT_LAYOUT)
            })
            .collect::<Vec<_>>();
        let depth = vk::AttachmentReference::default()
            .attachment(depth_index)
            .layout(render_graph::DEPTH_ATTACHMENT_LAYOUT);
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&colors)
            .depth_stencil_attachment(&depth)];
        let dependencies = scene_render_pass_dependencies();
        unsafe {
            device
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachments)
                        .subpasses(&subpasses)
                        .dependencies(&dependencies),
                    None,
                )
                .map_err(Into::into)
        }
    }

    #[cfg(feature = "ssr")]
    fn destroy_ssr_pipelines(device: &ash::Device, pipelines: SsrPipelines) {
        unsafe {
            if pipelines.pipeline != vk::Pipeline::null() {
                device.destroy_pipeline(pipelines.pipeline, None);
            }
            if pipelines.pipeline_layout != vk::PipelineLayout::null() {
                device.destroy_pipeline_layout(pipelines.pipeline_layout, None);
            }
            for render_pass in [pipelines.render_pass, pipelines.blend_render_pass] {
                if render_pass != vk::RenderPass::null() {
                    device.destroy_render_pass(render_pass, None);
                }
            }
        }
    }

    #[cfg(feature = "gtao")]
    fn create_gtao_prepass_render_pass(
        device: &ash::Device,
        normal_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<vk::RenderPass, VulkanError> {
        let attachments = [
            vk::AttachmentDescription::default()
                .format(normal_format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(render_graph::SAMPLED_COLOR_LAYOUT),
            vk::AttachmentDescription::default()
                .format(depth_format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(render_graph::SAMPLED_DEPTH_LAYOUT),
        ];
        let color = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(render_graph::COLOR_ATTACHMENT_LAYOUT)];
        let depth = vk::AttachmentReference::default()
            .attachment(1)
            .layout(render_graph::DEPTH_ATTACHMENT_LAYOUT);
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color)
            .depth_stencil_attachment(&depth)];
        let dependencies = render_graph::render_pass_dependencies(
            render_graph::sampled_scope(),
            render_graph::color_depth_write_scope(),
            render_graph::sampled_scope(),
        );
        unsafe {
            device
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachments)
                        .subpasses(&subpasses)
                        .dependencies(&dependencies),
                    None,
                )
                .map_err(Into::into)
        }
    }

    fn create_present_render_pass(
        device: &ash::Device,
        format: vk::Format,
    ) -> Result<vk::RenderPass, VulkanError> {
        let attachments = [vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(render_graph::PRESENT_LAYOUT)];
        let color = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(render_graph::COLOR_ATTACHMENT_LAYOUT)];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color)];
        unsafe {
            device
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachments)
                        .subpasses(&subpasses),
                    None,
                )
                .map_err(Into::into)
        }
    }

    fn shader_module(device: &ash::Device, bytes: &[u8]) -> Result<vk::ShaderModule, VulkanError> {
        if bytes.len() % size_of::<u32>() != 0 {
            return Err(VulkanError::Unsupported(
                "invalid embedded SPIR-V length".into(),
            ));
        }
        let words = bytes
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().expect("four-byte word")))
            .collect::<Vec<_>>();
        unsafe {
            device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .map_err(Into::into)
        }
    }

    fn create_graphics_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
        front_face: vk::FrontFace,
        cull_mode: vk::CullModeFlags,
        blend_enabled: bool,
        vertex_spirv: &[u8],
        fragment_spirv: &[u8],
        color_attachment_count: usize,
        preserve_auxiliary: bool,
    ) -> Result<vk::Pipeline, VulkanError> {
        let vertex = shader_module(device, vertex_spirv)?;
        let fragment = match shader_module(device, fragment_spirv) {
            Ok(module) => module,
            Err(error) => {
                unsafe { device.destroy_shader_module(vertex, None) };
                return Err(error);
            }
        };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment)
                .name(c"main"),
        ];
        let bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Vertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, normal) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, uv_0) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(3)
                .binding(0)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, color_0) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(4)
                .binding(0)
                .format(vk::Format::R8G8B8A8_SNORM)
                .offset(std::mem::offset_of!(Vertex, tangent) as u32),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(&attributes);
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(cull_mode)
            .front_face(front_face)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let depth = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(!blend_enabled)
            .depth_compare_op(vk::CompareOp::GREATER);
        let color_blend = (0..color_attachment_count)
            .map(|index| {
                let auxiliary = preserve_auxiliary && blend_enabled && index > 0;
                let mut state = vk::PipelineColorBlendAttachmentState::default()
                    .color_write_mask(vk::ColorComponentFlags::RGBA);
                if auxiliary {
                    state = state.color_write_mask(vk::ColorComponentFlags::empty());
                }
                if blend_enabled && !auxiliary {
                    state = state
                        .blend_enable(true)
                        .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                        .color_blend_op(vk::BlendOp::ADD)
                        .src_alpha_blend_factor(vk::BlendFactor::ONE)
                        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                        .alpha_blend_op(vk::BlendOp::ADD);
                }
                state
            })
            .collect::<Vec<_>>();
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_blend);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0)];
        let result =
            unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &info, None) };
        unsafe {
            device.destroy_shader_module(vertex, None);
            device.destroy_shader_module(fragment, None);
        }
        match result {
            Ok(mut pipelines) => Ok(pipelines.remove(0)),
            Err((pipelines, error)) => {
                unsafe {
                    for pipeline in pipelines {
                        if pipeline != vk::Pipeline::null() {
                            device.destroy_pipeline(pipeline, None);
                        }
                    }
                }
                Err(error.into())
            }
        }
    }

    fn pipeline_raster_state(index: usize) -> (vk::FrontFace, vk::CullModeFlags) {
        // The 3D vertex shaders flip clip Y; mirrored transforms reverse winding once more.
        let front_face = if index & 1 == 0 {
            vk::FrontFace::COUNTER_CLOCKWISE
        } else {
            vk::FrontFace::CLOCKWISE
        };
        let cull_mode = if index & 2 == 0 {
            vk::CullModeFlags::BACK
        } else {
            vk::CullModeFlags::NONE
        };
        (front_face, cull_mode)
    }

    fn pipeline_variant_index(mirrored: bool, double_sided: bool, blend: bool) -> usize {
        mirrored as usize | ((double_sided as usize) << 1) | ((blend as usize) << 2)
    }

    fn scene_fragment_shader(reflectance: bool, mboit: bool) -> (&'static [u8], usize) {
        #[cfg(feature = "translucent-shadows")]
        if mboit {
            if reflectance {
                return (include_bytes!("../shaders/pbr_ssr_mboit.frag.spv"), 3);
            }
            return (include_bytes!("../shaders/pbr_mboit.frag.spv"), 2);
        }
        #[cfg(not(feature = "translucent-shadows"))]
        let _ = mboit;

        #[cfg(feature = "ssr")]
        if reflectance {
            return (include_bytes!("../shaders/pbr_ssr.frag.spv"), 3);
        }
        #[cfg(not(feature = "ssr"))]
        let _ = reflectance;

        (include_bytes!("../shaders/pbr.frag.spv"), 2)
    }

    fn create_scene_pipelines(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
        reflectance: bool,
        mboit_enabled: bool,
    ) -> Result<[vk::Pipeline; 8], VulkanError> {
        let (fragment, color_attachment_count) = scene_fragment_shader(reflectance, mboit_enabled);
        let vertex = include_bytes!("../shaders/pbr.vert.spv").as_slice();
        let mut pipelines = [vk::Pipeline::null(); 8];
        for (index, pipeline) in pipelines.iter_mut().enumerate() {
            let (front_face, cull_mode) = pipeline_raster_state(index);
            *pipeline = match create_graphics_pipeline(
                device,
                render_pass,
                layout,
                front_face,
                cull_mode,
                index & 4 != 0,
                vertex,
                fragment,
                color_attachment_count,
                reflectance,
            ) {
                Ok(pipeline) => pipeline,
                Err(error) => {
                    for pipeline in pipelines {
                        if pipeline != vk::Pipeline::null() {
                            unsafe { device.destroy_pipeline(pipeline, None) };
                        }
                    }
                    return Err(error);
                }
            };
        }
        Ok(pipelines)
    }

    fn create_shadow_pipelines(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
    ) -> Result<[vk::Pipeline; 4], VulkanError> {
        let mut pipelines = [vk::Pipeline::null(); 4];
        for (index, pipeline) in pipelines.iter_mut().enumerate() {
            let (front_face, cull_mode) = pipeline_raster_state(index);
            *pipeline =
                match create_shadow_pipeline(device, render_pass, layout, front_face, cull_mode) {
                    Ok(pipeline) => pipeline,
                    Err(error) => {
                        for pipeline in pipelines {
                            if pipeline != vk::Pipeline::null() {
                                unsafe { device.destroy_pipeline(pipeline, None) };
                            }
                        }
                        return Err(error);
                    }
                };
        }
        Ok(pipelines)
    }

    #[derive(Clone, Copy)]
    enum ShadowDrawKind {
        Opaque,
        #[cfg(feature = "translucent-shadows")]
        MboitTranslucent,
    }

    fn draw_shadow_items(
        device: &DeviceState,
        materials: &HashMap<MaterialId, MaterialData>,
        frame: &FrameSnapshot,
        cascade: &Cascade,
        pipelines: &[vk::Pipeline; 4],
        kind: ShadowDrawKind,
    ) -> usize {
        let cmd = device.command_buffer;
        unsafe {
            device.device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                device.pipeline_layout,
                0,
                &[device.frame_descriptor_set],
                &[],
            );
        }
        let mut bound_pipeline = vk::Pipeline::null();
        let mut bound_material = vk::DescriptorSet::null();
        let mut draw_count = 0;
        for &item_index in &cascade.caster_indices {
            let item = &frame.render_items[item_index];
            let material = materials[&item.material];
            let draw = match kind {
                ShadowDrawKind::Opaque => material.alpha_mode != AlphaMode::Blend,
                #[cfg(feature = "translucent-shadows")]
                ShadowDrawKind::MboitTranslucent => material.alpha_mode == AlphaMode::Blend,
            };
            if !draw {
                continue;
            }
            let pipeline =
                pipelines[pipeline_variant_index(item.is_mirrored(), material.double_sided, false)];
            if pipeline != bound_pipeline {
                unsafe {
                    device
                        .device
                        .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline)
                };
                bound_pipeline = pipeline;
            }
            let mesh = &device.meshes[&item.mesh];
            unsafe {
                device.device.cmd_push_constants(
                    cmd,
                    device.pipeline_layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    bytes_of(&ShadowPush {
                        model: item.model_matrix,
                        light_view_projection: cascade.view_projection.to_cols_array_2d(),
                    }),
                );
                device
                    .device
                    .cmd_bind_vertex_buffers(cmd, 0, &[mesh.vertices.buffer], &[0]);
                device.device.cmd_bind_index_buffer(
                    cmd,
                    mesh.indices.buffer,
                    0,
                    vk::IndexType::UINT32,
                );
            }
            let descriptor_set = device.materials[&item.material].descriptor_set;
            if descriptor_set != bound_material {
                unsafe {
                    device.device.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        device.pipeline_layout,
                        1,
                        &[descriptor_set],
                        &[],
                    )
                };
                bound_material = descriptor_set;
            }
            unsafe {
                device
                    .device
                    .cmd_draw_indexed(cmd, mesh.index_count, 1, 0, 0, 0)
            };
            draw_count += 1;
        }
        draw_count
    }

    #[cfg(feature = "translucent-shadows")]
    fn create_mboit_pipelines(
        device: &ash::Device,
        color_format: vk::Format,
        layout: vk::PipelineLayout,
    ) -> Result<MboitPipelines, VulkanError> {
        let mut pipelines = MboitPipelines {
            render_pass: vk::RenderPass::null(),
            format: color_format,
            translucent: [vk::Pipeline::null(); 4],
        };
        let result = (|| {
            pipelines.render_pass = create_mboit_render_pass(device, color_format)?;
            for index in 0..4 {
                let (front_face, cull_mode) = pipeline_raster_state(index);
                pipelines.translucent[index] = create_mboit_pipeline(
                    device,
                    pipelines.render_pass,
                    layout,
                    front_face,
                    cull_mode,
                    include_bytes!("../shaders/mboit_shadow.frag.spv"),
                )?;
            }
            Ok::<_, VulkanError>(())
        })();
        if let Err(error) = result {
            destroy_mboit_pipelines(device, pipelines);
            return Err(error);
        }
        Ok(pipelines)
    }

    #[cfg(feature = "translucent-shadows")]
    fn create_mboit_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
        front_face: vk::FrontFace,
        cull_mode: vk::CullModeFlags,
        fragment_spirv: &[u8],
    ) -> Result<vk::Pipeline, VulkanError> {
        let vertex = shader_module(device, include_bytes!("../shaders/mboit_shadow.vert.spv"))?;
        let fragment = match shader_module(device, fragment_spirv) {
            Ok(module) => module,
            Err(error) => {
                unsafe { device.destroy_shader_module(vertex, None) };
                return Err(error);
            }
        };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment)
                .name(c"main"),
        ];
        let bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Vertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, uv_0) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(3)
                .binding(0)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, color_0) as u32),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(&attributes);
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(cull_mode)
            .front_face(front_face)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(
                vk::ColorComponentFlags::R
                    | vk::ColorComponentFlags::G
                    | vk::ColorComponentFlags::B
                    | vk::ColorComponentFlags::A,
            );
        let attachments = [attachment; 2];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .render_pass(render_pass);
        let result =
            unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) };
        unsafe {
            device.destroy_shader_module(vertex, None);
            device.destroy_shader_module(fragment, None);
        }
        match result {
            Ok(mut pipelines) => Ok(pipelines.remove(0)),
            Err((pipelines, error)) => {
                unsafe {
                    for pipeline in pipelines {
                        if pipeline != vk::Pipeline::null() {
                            device.destroy_pipeline(pipeline, None);
                        }
                    }
                }
                Err(error.into())
            }
        }
    }

    #[cfg(feature = "translucent-shadows")]
    fn destroy_mboit_pipelines(device: &ash::Device, pipelines: MboitPipelines) {
        unsafe {
            for pipeline in pipelines.translucent {
                if pipeline != vk::Pipeline::null() {
                    device.destroy_pipeline(pipeline, None);
                }
            }
            if pipelines.render_pass != vk::RenderPass::null() {
                device.destroy_render_pass(pipelines.render_pass, None);
            }
        }
    }

    fn create_post_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
    ) -> Result<vk::Pipeline, VulkanError> {
        create_fullscreen_pipeline(
            device,
            render_pass,
            layout,
            include_bytes!("../shaders/post.frag.spv"),
        )
    }

    fn create_present_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
        bloom: bool,
    ) -> Result<vk::Pipeline, VulkanError> {
        #[cfg(feature = "bloom")]
        if bloom {
            return create_fullscreen_pipeline(
                device,
                render_pass,
                layout,
                include_bytes!("../shaders/bloom/composite.frag.spv"),
            );
        }
        let _ = bloom;
        create_post_pipeline(device, render_pass, layout)
    }

    fn create_fullscreen_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
        fragment_spirv: &[u8],
    ) -> Result<vk::Pipeline, VulkanError> {
        let vertex = shader_module(device, include_bytes!("../shaders/post.vert.spv"))?;
        let fragment = match shader_module(device, fragment_spirv) {
            Ok(module) => module,
            Err(error) => {
                unsafe { device.destroy_shader_module(vertex, None) };
                return Err(error);
            }
        };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment)
                .name(c"main"),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let color_blend = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(false)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_blend);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0)];
        let result =
            unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &info, None) };
        unsafe {
            device.destroy_shader_module(vertex, None);
            device.destroy_shader_module(fragment, None);
        }
        match result {
            Ok(mut pipelines) => Ok(pipelines.remove(0)),
            Err((pipelines, error)) => {
                unsafe {
                    for pipeline in pipelines {
                        if pipeline != vk::Pipeline::null() {
                            device.destroy_pipeline(pipeline, None);
                        }
                    }
                }
                Err(error.into())
            }
        }
    }

    fn create_shadow_pipeline(
        device: &ash::Device,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
        front_face: vk::FrontFace,
        cull_mode: vk::CullModeFlags,
    ) -> Result<vk::Pipeline, VulkanError> {
        let vertex = shader_module(device, include_bytes!("../shaders/shadow.vert.spv"))?;
        let fragment = match shader_module(device, include_bytes!("../shaders/shadow.frag.spv")) {
            Ok(module) => module,
            Err(error) => {
                unsafe { device.destroy_shader_module(vertex, None) };
                return Err(error);
            }
        };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment)
                .name(c"main"),
        ];
        let bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Vertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, uv_0) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(3)
                .binding(0)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, color_0) as u32),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(&attributes);
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(cull_mode)
            .front_face(front_face)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let depth = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS);
        let blend = vk::PipelineColorBlendStateCreateInfo::default();
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0)];
        let result =
            unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &info, None) };
        unsafe {
            device.destroy_shader_module(vertex, None);
            device.destroy_shader_module(fragment, None);
        }
        match result {
            Ok(mut pipelines) => Ok(pipelines.remove(0)),
            Err((pipelines, error)) => {
                unsafe {
                    for pipeline in pipelines {
                        if pipeline != vk::Pipeline::null() {
                            device.destroy_pipeline(pipeline, None);
                        }
                    }
                }
                Err(error.into())
            }
        }
    }

    fn destroy_attachment(device: &ash::Device, attachment: GpuAttachment) {
        unsafe {
            if attachment.view != vk::ImageView::null() {
                device.destroy_image_view(attachment.view, None);
            }
            if attachment.image != vk::Image::null() {
                device.destroy_image(attachment.image, None);
            }
            attachment.memory.free(device);
        }
    }

    fn destroy_scene_targets(device: &ash::Device, scene: SceneTargets) {
        unsafe {
            if scene.framebuffer != vk::Framebuffer::null() {
                device.destroy_framebuffer(scene.framebuffer, None);
            }
        }
        destroy_attachment(device, scene.color);
        destroy_attachment(device, scene.normal_roughness);
        #[cfg(feature = "ssr")]
        if let Some(reflectance) = scene.reflectance {
            destroy_attachment(device, reflectance);
        }
        destroy_attachment(device, scene.depth);
    }

    #[cfg(feature = "gtao")]
    fn destroy_gtao_targets(device: &ash::Device, targets: GtaoTargets) {
        unsafe {
            for framebuffer in [
                targets.prepass_framebuffer,
                targets.raw_framebuffer,
                targets.full_framebuffer,
            ] {
                if framebuffer != vk::Framebuffer::null() {
                    device.destroy_framebuffer(framebuffer, None);
                }
            }
        }
        destroy_attachment(device, targets.raw);
        destroy_attachment(device, targets.full);
    }

    #[cfg(feature = "smaa")]
    fn destroy_smaa_targets(device: &ash::Device, targets: SmaaTargets) {
        unsafe {
            for framebuffer in [
                targets.post_framebuffer,
                targets.edge_framebuffer,
                targets.weight_framebuffer,
            ] {
                if framebuffer != vk::Framebuffer::null() {
                    device.destroy_framebuffer(framebuffer, None);
                }
            }
        }
        destroy_attachment(device, targets.post_color);
        destroy_attachment(device, targets.edges);
        destroy_attachment(device, targets.weights);
    }

    fn draw_scene_items<'a>(
        device: &DeviceState,
        items: impl Iterator<Item = &'a x173ge_protocol::RenderItem>,
        materials: &HashMap<MaterialId, MaterialData>,
    ) {
        let mut bound_pipeline = vk::Pipeline::null();
        let mut bound_descriptor_set = vk::DescriptorSet::null();
        for item in items {
            let material = &materials[&item.material];
            let pipeline = device.scene_pipelines[pipeline_variant_index(
                item.is_mirrored(),
                material.double_sided,
                material.alpha_mode == AlphaMode::Blend,
            )];
            if pipeline != bound_pipeline {
                unsafe {
                    device.device.cmd_bind_pipeline(
                        device.command_buffer,
                        vk::PipelineBindPoint::GRAPHICS,
                        pipeline,
                    );
                }
                bound_pipeline = pipeline;
            }
            let mesh = &device.meshes[&item.mesh];
            let descriptor_set = device.materials[&item.material].descriptor_set;
            if descriptor_set != bound_descriptor_set {
                unsafe {
                    device.device.cmd_bind_descriptor_sets(
                        device.command_buffer,
                        vk::PipelineBindPoint::GRAPHICS,
                        device.pipeline_layout,
                        0,
                        &[device.frame_descriptor_set, descriptor_set],
                        &[],
                    );
                }
                bound_descriptor_set = descriptor_set;
            }
            unsafe {
                device.device.cmd_push_constants(
                    device.command_buffer,
                    device.pipeline_layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    bytes_of(&item.model_matrix),
                );
                device.device.cmd_bind_vertex_buffers(
                    device.command_buffer,
                    0,
                    &[mesh.vertices.buffer],
                    &[0],
                );
                device.device.cmd_bind_index_buffer(
                    device.command_buffer,
                    mesh.indices.buffer,
                    0,
                    vk::IndexType::UINT32,
                );
                device
                    .device
                    .cmd_draw_indexed(device.command_buffer, mesh.index_count, 1, 0, 0, 0);
            }
        }
    }

    #[cfg(feature = "ssr")]
    fn draw_ssr(
        device: &DeviceState,
        targets: &SsrTargets,
        extent: vk::Extent2D,
        settings: SsrSettings,
    ) {
        let pipelines = device.ssr.as_ref().expect("SSR pipelines are configured");
        let push = SsrPush {
            ray: [settings.max_distance, settings.thickness],
            max_steps: settings.max_steps,
            max_roughness: settings.max_roughness,
            resolve: [settings.intensity, settings.edge_fade_pixels],
            padding: [0.0; 2],
        };
        unsafe {
            device.device.cmd_begin_render_pass(
                device.command_buffer,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(pipelines.render_pass)
                    .framebuffer(targets.framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    })
                    .clear_values(&[vk::ClearValue {
                        color: vk::ClearColorValue { float32: [0.0; 4] },
                    }]),
                vk::SubpassContents::INLINE,
            );
            device.device.cmd_set_viewport(
                device.command_buffer,
                0,
                &[vk::Viewport::default()
                    .x(0.0)
                    .y(0.0)
                    .width(extent.width as f32)
                    .height(extent.height as f32)
                    .min_depth(0.0)
                    .max_depth(1.0)],
            );
            device.device.cmd_set_scissor(
                device.command_buffer,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );
            device.device.cmd_bind_pipeline(
                device.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                pipelines.pipeline,
            );
            device.device.cmd_bind_descriptor_sets(
                device.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                pipelines.pipeline_layout,
                0,
                &[device.frame_descriptor_set, targets.descriptor_set],
                &[],
            );
            device.device.cmd_push_constants(
                device.command_buffer,
                pipelines.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                bytes_of(&push),
            );
            device.device.cmd_draw(device.command_buffer, 3, 1, 0, 0);
            device.device.cmd_end_render_pass(device.command_buffer);
        }
    }

    fn draw_fullscreen(
        device: &DeviceState,
        render_pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
        pipeline: vk::Pipeline,
        layout: vk::PipelineLayout,
        descriptor_set: vk::DescriptorSet,
        clear: Option<[f32; 4]>,
        push: Option<&[u8]>,
    ) {
        draw_fullscreen_sets(
            device,
            render_pass,
            framebuffer,
            extent,
            pipeline,
            layout,
            &[descriptor_set],
            clear,
            push,
        );
    }

    fn draw_fullscreen_sets(
        device: &DeviceState,
        render_pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
        pipeline: vk::Pipeline,
        layout: vk::PipelineLayout,
        descriptor_sets: &[vk::DescriptorSet],
        clear: Option<[f32; 4]>,
        push: Option<&[u8]>,
    ) {
        let clear_values = clear.map(|float32| {
            [vk::ClearValue {
                color: vk::ClearColorValue { float32 },
            }]
        });
        draw_fullscreen_sets_with_clears(
            device,
            render_pass,
            framebuffer,
            extent,
            pipeline,
            layout,
            descriptor_sets,
            clear_values.as_ref().map_or(&[], |values| values),
            push,
        );
    }

    fn draw_fullscreen_sets_with_clears(
        device: &DeviceState,
        render_pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
        pipeline: vk::Pipeline,
        layout: vk::PipelineLayout,
        descriptor_sets: &[vk::DescriptorSet],
        clear_values: &[vk::ClearValue],
        push: Option<&[u8]>,
    ) {
        let mut begin = vk::RenderPassBeginInfo::default()
            .render_pass(render_pass)
            .framebuffer(framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            });
        if !clear_values.is_empty() {
            begin = begin.clear_values(clear_values);
        }
        unsafe {
            let cmd = device.command_buffer;
            device
                .device
                .cmd_begin_render_pass(cmd, &begin, vk::SubpassContents::INLINE);
            device
                .device
                .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                descriptor_sets,
                &[],
            );
            if let Some(push) = push {
                device.device.cmd_push_constants(
                    cmd,
                    layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push,
                );
            }
            device.device.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport::default()
                    .x(0.0)
                    .y(0.0)
                    .width(extent.width as f32)
                    .height(extent.height as f32)
                    .min_depth(0.0)
                    .max_depth(1.0)],
            );
            device.device.cmd_set_scissor(
                cmd,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );
            device.device.cmd_draw(cmd, 3, 1, 0, 0);
            device.device.cmd_end_render_pass(cmd);
        }
    }

    fn write_sampled_images(
        device: &ash::Device,
        set: vk::DescriptorSet,
        sampler: vk::Sampler,
        views: &[vk::ImageView],
    ) {
        let infos = views
            .iter()
            .map(|&view| {
                vk::DescriptorImageInfo::default()
                    .sampler(sampler)
                    .image_view(view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            })
            .collect::<Vec<_>>();
        let writes = infos
            .iter()
            .enumerate()
            .map(|(binding, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(binding as u32)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(info))
            })
            .collect::<Vec<_>>();
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    fn write_shadow_descriptors(
        device: &ash::Device,
        set: vk::DescriptorSet,
        raw_sampler: vk::Sampler,
        compare_sampler: vk::Sampler,
        maps: &[ShadowMap],
    ) {
        debug_assert!(maps.len() == 1 || maps.len() == CASCADE_COUNT);
        let infos = (0..CASCADE_COUNT)
            .map(|index| {
                let map = &maps[if maps.len() == 1 { 0 } else { index }];
                vk::DescriptorImageInfo::default()
                    .sampler(raw_sampler)
                    .image_view(map.depth.view)
                    .image_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL)
            })
            .collect::<Vec<_>>();
        let compare_infos = (0..CASCADE_COUNT)
            .map(|index| {
                let map = &maps[if maps.len() == 1 { 0 } else { index }];
                vk::DescriptorImageInfo::default()
                    .sampler(compare_sampler)
                    .image_view(map.depth.view)
                    .image_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL)
            })
            .collect::<Vec<_>>();
        let writes = infos
            .iter()
            .enumerate()
            .map(|(index, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(index as u32 + 1)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(info))
            })
            .chain(compare_infos.iter().enumerate().map(|(index, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(index as u32 + 18)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(info))
            }))
            .collect::<Vec<_>>();
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    #[cfg(feature = "translucent-shadows")]
    fn write_mboit_descriptors(
        device: &ash::Device,
        set: vk::DescriptorSet,
        sampler: vk::Sampler,
        moments_0123: [vk::ImageView; CASCADE_COUNT],
        moments_456: [vk::ImageView; CASCADE_COUNT],
        layout: vk::ImageLayout,
    ) {
        let infos = (0..CASCADE_COUNT)
            .flat_map(|index| [moments_0123[index], moments_456[index]])
            .map(|view| {
                vk::DescriptorImageInfo::default()
                    .sampler(sampler)
                    .image_view(view)
                    .image_layout(layout)
            })
            .collect::<Vec<_>>();
        let writes = infos
            .iter()
            .enumerate()
            .map(|(index, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(index as u32 + 10)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(info))
            })
            .collect::<Vec<_>>();
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    fn write_frame_scene_images(
        device: &ash::Device,
        set: vk::DescriptorSet,
        color_sampler: vk::Sampler,
        nearest_sampler: vk::Sampler,
        views: [vk::ImageView; 4],
        layouts: [vk::ImageLayout; 4],
    ) {
        let infos = views
            .into_iter()
            .zip(layouts)
            .enumerate()
            .map(|(index, (view, layout))| {
                vk::DescriptorImageInfo::default()
                    .sampler(if index == 0 {
                        color_sampler
                    } else {
                        nearest_sampler
                    })
                    .image_view(view)
                    .image_layout(layout)
            })
            .collect::<Vec<_>>();
        let writes = infos
            .iter()
            .enumerate()
            .map(|(index, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(index as u32 + 5)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(info))
            })
            .collect::<Vec<_>>();
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    #[cfg(feature = "ssr")]
    fn write_frame_reflectance(
        device: &ash::Device,
        set: vk::DescriptorSet,
        sampler: vk::Sampler,
        view: vk::ImageView,
    ) {
        let info = vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(9)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&info));
        unsafe { device.update_descriptor_sets(&[write], &[]) };
    }

    fn initialize_shadow_maps(
        device: &ash::Device,
        queue: vk::Queue,
        command_buffer: vk::CommandBuffer,
        render_pass: vk::RenderPass,
        maps: &[ShadowMap],
    ) -> Result<(), VulkanError> {
        unsafe {
            device.reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            for map in maps {
                device.cmd_begin_render_pass(
                    command_buffer,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(render_pass)
                        .framebuffer(map.framebuffer)
                        .render_area(vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent: vk::Extent2D {
                                width: map.size,
                                height: map.size,
                            },
                        })
                        .clear_values(&[vk::ClearValue {
                            depth_stencil: vk::ClearDepthStencilValue {
                                depth: 1.0,
                                stencil: 0,
                            },
                        }]),
                    vk::SubpassContents::INLINE,
                );
                device.cmd_end_render_pass(command_buffer);
            }
            device.end_command_buffer(command_buffer)?;
            device.queue_submit(
                queue,
                &[vk::SubmitInfo::default().command_buffers(&[command_buffer])],
                vk::Fence::null(),
            )?;
            device.queue_wait_idle(queue)?;
        }
        Ok(())
    }

    fn destroy_shadow_map(device: &ash::Device, map: ShadowMap) {
        unsafe {
            if map.framebuffer != vk::Framebuffer::null() {
                device.destroy_framebuffer(map.framebuffer, None);
            }
            destroy_attachment(device, map.depth);
        }
    }

    /// Destroys swapchain children before the swapchain handle.
    fn destroy_swapchain(device: &DeviceState, surface: SurfaceState) {
        unsafe {
            device.device.destroy_pipeline(surface.pipeline, None);
            #[cfg(feature = "bloom")]
            if let Some(pipeline) = surface.bloom_pipeline {
                device.device.destroy_pipeline(pipeline, None);
            }
            #[cfg(feature = "smaa")]
            if let Some(pipeline) = surface.smaa_pipeline {
                device.device.destroy_pipeline(pipeline, None);
            }
            for framebuffer in surface.framebuffers {
                device.device.destroy_framebuffer(framebuffer, None);
            }
            for semaphore in surface.present_semaphores {
                device.device.destroy_semaphore(semaphore, None);
            }
            device
                .device
                .destroy_descriptor_pool(surface.post_descriptor_pool, None);
        }
        #[cfg(feature = "gtao")]
        if let Some(gtao) = surface.gtao {
            destroy_gtao_targets(&device.device, gtao);
        }
        #[cfg(feature = "ssr")]
        if let Some(ssr) = surface.ssr {
            destroy_ssr_targets(&device.device, ssr);
        }
        #[cfg(feature = "god-rays")]
        if let Some(targets) = surface.god_rays {
            destroy_god_rays_targets(&device.device, targets);
        }
        destroy_scene_targets(&device.device, surface.scene);
        destroy_exposure_targets(&device.device, surface.exposure);
        #[cfg(feature = "smaa")]
        if let Some(smaa) = surface.smaa {
            destroy_smaa_targets(&device.device, smaa);
        }
        #[cfg(feature = "bloom")]
        if let Some(bloom) = surface.bloom {
            destroy_bloom_targets(&device.device, bloom);
        }
        unsafe {
            device.device.destroy_render_pass(surface.render_pass, None);
            for view in surface.views {
                device.device.destroy_image_view(view, None);
            }
            device.swapchain.destroy_swapchain(surface.swapchain, None);
        }
    }

    /// Sends validation warnings and errors to the configured tracing subscriber.
    unsafe extern "system" fn debug_callback(
        severity: vk::DebugUtilsMessageSeverityFlagsEXT,
        kind: vk::DebugUtilsMessageTypeFlagsEXT,
        data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
        _: *mut std::ffi::c_void,
    ) -> vk::Bool32 {
        let message = if data.is_null() || unsafe { (*data).p_message.is_null() } {
            "<empty Vulkan validation message>".into()
        } else {
            unsafe { CStr::from_ptr((*data).p_message) }.to_string_lossy()
        };
        tracing::warn!(
            target: "x173ge::vulkan",
            severity = severity.as_raw(),
            kind = kind.as_raw(),
            message = %message
        );
        #[cfg(debug_assertions)]
        eprintln!("Vulkan validation warning/error: {message}");
        vk::FALSE
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use x173ge_protocol::{CameraData, CameraProjection, GodRaysSettings, SurfaceToken};

        #[test]
        fn gpu_memory_stats_track_live_bytes_and_lifetime_peak() {
            let stats = GpuMemoryStats::default();
            stats.allocated(64);
            stats.allocated(128);
            stats.released(64);

            assert_eq!(stats.live_bytes.load(Ordering::Relaxed), 128);
            assert_eq!(stats.peak_bytes.load(Ordering::Relaxed), 192);

            stats.released(128);
            assert_eq!(stats.live_bytes.load(Ordering::Relaxed), 0);
            assert_eq!(stats.peak_bytes.load(Ordering::Relaxed), 192);
        }

        #[test]
        fn vulkan_api_errors_include_the_failing_operation() {
            let error = api_call::<()>("queue_submit(frame)", Err(vk::Result::ERROR_DEVICE_LOST))
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "Vulkan call `queue_submit(frame)` failed: ERROR_DEVICE_LOST"
            );
        }

        #[test]
        fn device_selection_prefers_discrete_over_integrated_gpu() {
            assert!(
                physical_device_priority(vk::PhysicalDeviceType::DISCRETE_GPU)
                    > physical_device_priority(vk::PhysicalDeviceType::INTEGRATED_GPU)
            );
            assert!(
                physical_device_priority(vk::PhysicalDeviceType::INTEGRATED_GPU)
                    > physical_device_priority(vk::PhysicalDeviceType::CPU)
            );
        }

        #[test]
        fn god_rays_frame_uniform_keeps_the_inverse_camera_transform_and_settings() {
            let view_projection = Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0));
            let frame = FrameSnapshot {
                id: x173ge_core::FrameId::new(1).unwrap(),
                surface: SurfaceToken {
                    id: SurfaceId::new(1).unwrap(),
                    generation: SurfaceGeneration::new(1).unwrap(),
                },
                extent: Extent2d::new(1, 1).unwrap(),
                clear_color: [0.0; 4],
                ambient_diffuse: [0.0; 3],
                ambient_diffuse_y: [0.0; 3],
                environment: None,
                camera: CameraData {
                    view_projection: view_projection.to_cols_array_2d(),
                    position: [0.0; 3],
                    projection: CameraProjection::Orthographic {
                        near: 0.1,
                        far: 100.0,
                    },
                },
                directional_lights: Vec::new(),
                punctual_lights: Vec::new(),
                render_items: Vec::new(),
            };
            let settings = GodRaysSettings::default();
            let uniform = make_frame_uniform(
                &frame,
                None,
                &[],
                Vec3::Z,
                false,
                false,
                Some(settings),
                false,
                None,
            )
            .unwrap();

            assert_eq!(
                uniform.inverse_view_projection,
                view_projection.inverse().to_cols_array_2d()
            );
            assert_eq!(
                uniform.god_rays_parameters[..3],
                [
                    settings.extinction_per_unit,
                    settings.scattering_albedo,
                    settings.anisotropy,
                ]
            );
            assert_eq!(
                uniform.god_rays_counts[..2],
                [settings.sample_count, settings.interleave_block]
            );
        }

        #[test]
        fn punctual_lights_are_normalized_and_packed_for_the_shader_layout() {
            let frame = FrameSnapshot {
                id: x173ge_core::FrameId::new(1).unwrap(),
                surface: SurfaceToken {
                    id: SurfaceId::new(1).unwrap(),
                    generation: SurfaceGeneration::new(1).unwrap(),
                },
                extent: Extent2d::new(1, 1).unwrap(),
                clear_color: [0.0; 4],
                ambient_diffuse: [1.0; 3],
                ambient_diffuse_y: [0.2, -0.3, 0.4],
                environment: None,
                camera: CameraData::default(),
                directional_lights: Vec::new(),
                punctual_lights: vec![
                    PunctualLight::Point {
                        position: [1.0, 2.0, 3.0],
                        color: [0.2, 0.4, 0.6],
                        intensity: 8.0,
                        range: Some(12.0),
                    },
                    PunctualLight::Spot {
                        position: [-1.0, 0.0, 2.0],
                        direction: [0.0, 0.0, -2.0],
                        color: [1.0; 3],
                        intensity: 4.0,
                        range: None,
                        inner_cone_angle: 0.2,
                        outer_cone_angle: 0.6,
                    },
                ],
                render_items: Vec::new(),
            };
            let uniform =
                make_frame_uniform(&frame, None, &[], Vec3::Z, false, false, None, false, None)
                    .unwrap();

            assert_eq!(uniform.ambient_diffuse, [1.0, 1.0, 1.0, 0.0]);
            assert_eq!(uniform.ambient_diffuse_y, [0.2, -0.3, 0.4, 0.0]);
            assert_eq!(uniform.punctual_count[0], 2);
            assert_eq!(
                uniform.punctual_lights[0].position_range,
                [1.0, 2.0, 3.0, 12.0]
            );
            assert_eq!(
                uniform.punctual_lights[0].color_intensity,
                [0.2, 0.4, 0.6, 8.0]
            );
            assert_eq!(
                uniform.punctual_lights[1].direction_type,
                [0.0, 0.0, -1.0, 1.0]
            );
            assert_eq!(
                uniform.punctual_lights[1].cone_cosines[..2],
                [0.2_f32.cos(), 0.6_f32.cos()]
            );
        }
    }
}

#[cfg(windows)]
pub use windows::VulkanRenderer;

#[cfg(not(windows))]
/// Placeholder for targets whose native Vulkan surface adapter is not implemented yet.
pub struct VulkanRenderer;

#[cfg(not(windows))]
impl VulkanRenderer {
    /// Reports that the current build does not implement native surfaces yet.
    pub fn new() -> Result<Self, VulkanError> {
        Err(VulkanError::Unsupported(
            "native Vulkan surfaces are implemented for Windows first".into(),
        ))
    }
}

#[cfg(not(windows))]
impl RendererBackend for VulkanRenderer {
    /// Rejects unsupported native surface configuration.
    fn configure_surface(&mut self, _: SurfaceDescriptor) -> Result<(), BackendError> {
        Err(BackendError(
            "native Vulkan surfaces are not implemented on this target".into(),
        ))
    }

    /// Rejects frame submission on unsupported targets.
    fn render(
        &mut self,
        _: &FrameSnapshot,
        _: RenderFeatures,
    ) -> Result<PresentResult, BackendError> {
        Err(BackendError(
            "native Vulkan surfaces are not implemented on this target".into(),
        ))
    }
}
