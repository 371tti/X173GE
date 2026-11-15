//! Owned data exchanged between game, platform and renderer threads.

use std::{
    num::{NonZeroIsize, NonZeroU32, NonZeroUsize},
    sync::Arc,
};

use x173ge_core::{
    EnvironmentId, Extent2d, FrameId, ImageId, MaterialId, MeshId, SurfaceGeneration, SurfaceId,
};

/// Maximum queued payload for one mesh upload (64 MiB).
pub const MAX_MESH_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Maximum decoded pixel payload for one uploaded glTF image (64 MiB).
pub const MAX_IMAGE_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Maximum draw references retained by one owned frame snapshot.
pub const MAX_RENDER_ITEMS: usize = 65_536;
/// Maximum directional lights retained in one owned frame snapshot.
pub const MAX_DIRECTIONAL_LIGHTS: usize = 16;
/// Maximum point and spot lights retained in one owned frame snapshot.
pub const MAX_PUNCTUAL_LIGHTS: usize = 16;

/// Converts a smooth linear sky-to-ground radiance gradient into diffuse irradiance coefficients.
/// The returned pair is `(constant, world_y)` for `E(n) = constant + world_y * n.y`.
pub fn vertical_environment_irradiance(
    sky_radiance: [f32; 3],
    ground_radiance: [f32; 3],
) -> Result<([f32; 3], [f32; 3]), &'static str> {
    if sky_radiance
        .iter()
        .chain(&ground_radiance)
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err("environment radiance must be finite and non-negative");
    }
    let base = std::array::from_fn(|i| {
        (std::f64::consts::PI * (f64::from(sky_radiance[i]) + f64::from(ground_radiance[i])) * 0.5)
            as f32
    });
    let vertical = std::array::from_fn(|i| {
        (std::f64::consts::PI / 3.0 * (f64::from(sky_radiance[i]) - f64::from(ground_radiance[i])))
            as f32
    });
    if base.iter().chain(&vertical).any(|value| !value.is_finite()) {
        return Err("environment irradiance exceeds the finite f32 range");
    }
    Ok((base, vertical))
}

/// Native window identity kept alive by the platform until renderer shutdown.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativeSurfaceHandle {
    /// HWND and HINSTANCE values for the Win32 Vulkan surface extension.
    Win32 {
        hwnd: NonZeroIsize,
        hinstance: NonZeroIsize,
    },
    /// Xlib display pointer and window ID reserved for the Linux adapter.
    X11 {
        display: NonZeroUsize,
        window: NonZeroU32,
    },
    /// Wayland display and surface pointers reserved for the Linux adapter.
    Wayland {
        display: NonZeroUsize,
        surface: NonZeroUsize,
    },
}

/// A surface version. The platform must retain the native window while it is used.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SurfaceToken {
    pub id: SurfaceId,
    pub generation: SurfaceGeneration,
}

/// Renderer configuration for a native surface.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SurfaceDescriptor {
    pub token: SurfaceToken,
    pub extent: Extent2d,
    pub native: NativeSurfaceHandle,
    /// Allows immediate presentation when the platform supports it.
    pub vsync: bool,
}

/// Active render effects; changes are sent through the renderer command queue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderFeatures {
    pub smaa: bool,
    pub gtao: bool,
    pub ssr: Option<SsrSettings>,
    pub bloom: Option<BloomSettings>,
    pub god_rays: Option<GodRaysSettings>,
    pub translucent_shadows: bool,
    pub tone_mapping: ToneMappingSettings,
}

impl Default for RenderFeatures {
    fn default() -> Self {
        Self::none()
    }
}

/// Runtime controls for automatic exposure and the display-linear contrast curve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToneMappingSettings {
    /// Contrast around the linear output of an 18% grey value.
    pub contrast: f32,
    /// Scene log-average luminance mapped toward the 18% key.
    pub key_luminance: f32,
    /// Lower bound for the automatic exposure gain.
    pub min_exposure: f32,
    /// Upper bound for the automatic exposure gain.
    pub max_exposure: f32,
    /// Temporal adaptation time constant in seconds.
    pub adaptation_seconds: f32,
}

impl Default for ToneMappingSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl ToneMappingSettings {
    pub const DEFAULT: Self = Self {
        contrast: 1.0,
        key_luminance: 0.1,
        min_exposure: 0.21,
        max_exposure: 16.0,
        adaptation_seconds: 1.0,
    };

    pub fn validate(self) -> Result<(), &'static str> {
        if !self.contrast.is_finite() || !(0.5..=1.5).contains(&self.contrast) {
            return Err("tone-mapping contrast must be finite and in 0.5..=1.5");
        }
        if !self.key_luminance.is_finite() || !(0.01..=1.0).contains(&self.key_luminance) {
            return Err("exposure key luminance must be finite and in 0.01..=1");
        }
        if !self.min_exposure.is_finite() || !(0.01..=1.0).contains(&self.min_exposure) {
            return Err("minimum exposure must be finite and in 0.01..=1");
        }
        if !self.max_exposure.is_finite() || !(1.0..=16.0).contains(&self.max_exposure) {
            return Err("maximum exposure must be finite and in 1..=16");
        }
        if !self.adaptation_seconds.is_finite() || !(0.05..=4.0).contains(&self.adaptation_seconds)
        {
            return Err("exposure adaptation time must be finite and in 0.05..=4 seconds");
        }
        Ok(())
    }
}

/// Runtime controls for homogeneous single-scattering volumetric lighting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GodRaysSettings {
    /// Medium extinction per world unit.
    pub extinction_per_unit: f32,
    /// Fraction of extinction that scatters light.
    pub scattering_albedo: f32,
    /// Henyey-Greenstein anisotropy in [-0.95, 0.95].
    pub anisotropy: f32,
    /// Total integration terms distributed across an interleave block.
    pub sample_count: u32,
    /// Width and height of each interleave block in pixels.
    pub interleave_block: u32,
}

impl Default for GodRaysSettings {
    /// Supplies a bounded initial preset for scene-based calibration.
    fn default() -> Self {
        Self::high_quality()
    }
}

impl GodRaysSettings {
    pub const fn high_quality() -> Self {
        Self {
            extinction_per_unit: 0.0006,
            scattering_albedo: 0.6,
            anisotropy: 0.5,
            sample_count: 128,
            interleave_block: 2,
        }
    }

    pub fn validate(self) -> Result<(), &'static str> {
        if !self.extinction_per_unit.is_finite() || self.extinction_per_unit < 0.0 {
            return Err("god rays extinction must be finite and non-negative");
        }
        if !self.scattering_albedo.is_finite() || !(0.0..=1.0).contains(&self.scattering_albedo) {
            return Err("god rays scattering albedo must be finite and in 0..=1");
        }
        if !self.anisotropy.is_finite() || !(-0.95..=0.95).contains(&self.anisotropy) {
            return Err("god rays anisotropy must be finite and in -0.95..=0.95");
        }
        if !(1..=4096).contains(&self.sample_count) {
            return Err("god rays sample count must be in 1..=4096");
        }
        if !(1..=8).contains(&self.interleave_block)
            || self.sample_count % self.interleave_block.pow(2) != 0
        {
            return Err("god rays sample count must divide evenly across a 1..=8 pixel block");
        }
        Ok(())
    }
}

/// Runtime controls for single-layer screen-space reflection tracing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SsrSettings {
    /// Maximum reflected-ray length in world units.
    pub max_distance: f32,
    /// Depth-buffer interval thickness in camera-forward world units.
    pub thickness: f32,
    /// Maximum screen-space DDA steps per pixel.
    pub max_steps: u32,
    /// Skip surfaces rougher than this value.
    pub max_roughness: f32,
    /// Scales the Fresnel-weighted reflected HDR radiance.
    pub intensity: f32,
    /// Fade confidence within this many pixels of the viewport edge; zero disables the fade.
    pub edge_fade_pixels: f32,
}

impl Default for SsrSettings {
    /// Supplies a bounded starting preset for scene-based calibration.
    fn default() -> Self {
        Self::high_quality()
    }
}

impl SsrSettings {
    pub const fn high_quality() -> Self {
        Self {
            max_distance: 15.0,
            thickness: 0.1,
            max_steps: 192,
            max_roughness: 0.85,
            intensity: 1.0,
            edge_fade_pixels: 16.0,
        }
    }

    pub fn validate(self) -> Result<(), &'static str> {
        if !self.max_distance.is_finite() || self.max_distance <= 0.0 {
            return Err("SSR max distance must be finite and positive");
        }
        if !self.thickness.is_finite() || self.thickness < 0.0 {
            return Err("SSR thickness must be finite and non-negative");
        }
        if !(1..=4096).contains(&self.max_steps) {
            return Err("SSR max steps must be in 1..=4096");
        }
        if !self.max_roughness.is_finite() || !(0.0..=1.0).contains(&self.max_roughness) {
            return Err("SSR max roughness must be finite and in 0..=1");
        }
        if !self.intensity.is_finite() || self.intensity < 0.0 {
            return Err("SSR intensity must be finite and non-negative");
        }
        if !self.edge_fade_pixels.is_finite() || self.edge_fade_pixels < 0.0 {
            return Err("SSR edge fade must be finite and non-negative");
        }
        Ok(())
    }
}

/// Runtime controls for the linear HDR bloom pyramid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BloomSettings {
    /// Overall gain after normalizing the additive reconstruction by pyramid level count.
    pub intensity: f32,
    /// Upsample radius in normalized UV coordinates.
    pub radius_uv: f32,
}

impl Default for BloomSettings {
    /// Supplies an initial low-gain preset pending scene-based visual calibration.
    fn default() -> Self {
        Self {
            intensity: 0.04,
            radius_uv: 0.003,
        }
    }
}

impl BloomSettings {
    /// Rejects values that would invert or invalidate the HDR composite.
    pub fn validate(self) -> Result<(), &'static str> {
        if !self.intensity.is_finite() || self.intensity < 0.0 {
            return Err("bloom intensity must be finite and non-negative");
        }
        if !self.radius_uv.is_finite() || !(0.0..=0.5).contains(&self.radius_uv) {
            return Err("bloom radius must be finite and in 0..=0.5 UV");
        }
        Ok(())
    }
}

impl RenderFeatures {
    /// Standard effects enabled by default in the regular build.
    pub const fn standard() -> Self {
        Self {
            smaa: true,
            ..Self::none()
        }
    }

    /// All compatible effects in the optional `high-quality` bundle; MBOIT stays opt-in because of its memory and reconstruction cost.
    pub const fn high_quality() -> Self {
        Self {
            smaa: true,
            gtao: true,
            ssr: Some(SsrSettings::high_quality()),
            bloom: Some(BloomSettings {
                intensity: 0.04,
                radius_uv: 0.003,
            }),
            god_rays: Some(GodRaysSettings::high_quality()),
            translucent_shadows: false,
            tone_mapping: ToneMappingSettings::DEFAULT,
        }
    }

    /// No optional screen-space or post-processing effects.
    pub const fn none() -> Self {
        Self {
            smaa: false,
            gtao: false,
            ssr: None,
            bloom: None,
            god_rays: None,
            translucent_shadows: false,
            tone_mapping: ToneMappingSettings::DEFAULT,
        }
    }

    pub const fn any(self) -> bool {
        self.smaa
            || self.gtao
            || self.ssr.is_some()
            || self.bloom.is_some()
            || self.god_rays.is_some()
            || self.translucent_shadows
    }

    /// Validates active settings before they cross the renderer boundary.
    pub fn validate(self) -> Result<(), &'static str> {
        self.tone_mapping.validate()?;
        if let Some(settings) = self.ssr {
            settings.validate()?;
        }
        if let Some(settings) = self.bloom {
            settings.validate()?;
        }
        if let Some(settings) = self.god_rays {
            settings.validate()?;
        }
        Ok(())
    }

    /// Returns whether this startup allocation can run the requested frame settings.
    pub const fn supports(self, requested: Self) -> bool {
        (!requested.smaa || self.smaa)
            && (!requested.gtao || self.gtao)
            && (requested.ssr.is_none() || self.ssr.is_some())
            && (requested.bloom.is_none() || self.bloom.is_some())
            && (requested.god_rays.is_none() || self.god_rays.is_some())
            && (!requested.translucent_shadows || self.translucent_shadows)
    }
}

/// One mesh/material instance with a column-major affine model matrix.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderItem {
    pub mesh: MeshId,
    pub material: MaterialId,
    pub model_matrix: [[f32; 4]; 4],
}

impl RenderItem {
    /// Returns whether the affine model transform reverses triangle winding.
    pub fn is_mirrored(&self) -> bool {
        model_determinant(self.model_matrix) < 0.0
    }
}

/// Column-major view-projection matrix and world-space camera position.
///
/// Matrices use Vulkan's reverse-Z 0..=1 clip-depth range (near=1, far=0) and
/// conventional +Y-up clip coordinates; the backend performs the framebuffer Y flip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraData {
    pub view_projection: [[f32; 4]; 4],
    pub position: [f32; 3],
    pub projection: CameraProjection,
}

/// Camera depth range and projection family needed for frustum operations.
/// The view-projection matrix must use reverse-Z depth: near maps to 1 and far to 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CameraProjection {
    Perspective { near: f32, far: f32 },
    Orthographic { near: f32, far: f32 },
}

impl CameraProjection {
    fn depth_range(self) -> (f32, f32) {
        match self {
            Self::Perspective { near, far } | Self::Orthographic { near, far } => (near, far),
        }
    }
}

impl Default for CameraData {
    fn default() -> Self {
        Self {
            view_projection: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            position: [0.0; 3],
            projection: CameraProjection::Perspective {
                near: 0.1,
                far: 100.0,
            },
        }
    }
}

/// Directional shadow controls; zero angular radius produces hard shadows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectionalShadow {
    pub angular_radius_radians: f32,
    pub max_penumbra_world: f32,
}

/// A directional light with linear RGB radiance and a direction toward its source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectionalLight {
    pub direction_to_light: [f32; 3],
    pub radiance: [f32; 3],
    pub shadow: Option<DirectionalShadow>,
}

/// A point or spot light; color is linear RGB and intensity is in candela.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PunctualLight {
    Point {
        position: [f32; 3],
        color: [f32; 3],
        intensity: f32,
        range: Option<f32>,
    },
    Spot {
        position: [f32; 3],
        /// Emission direction, matching the light node's local `-Z` axis.
        direction: [f32; 3],
        color: [f32; 3],
        intensity: f32,
        range: Option<f32>,
        inner_cone_angle: f32,
        outer_cone_angle: f32,
    },
}

/// Interleaved position, unit normal, UV0, color, and compact tangent data for PBR.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv_0: [f32; 2],
    pub color_0: [f32; 4],
    /// MikkTSpace tangent XYZ and handedness, signed normalized 8-bit values.
    pub tangent: [i8; 4],
}

/// CPU-side indexed triangle data; the renderer owns its GPU copy after upload.
#[derive(Clone, Debug, PartialEq)]
pub struct MeshData {
    vertices: Arc<[Vertex]>,
    indices: Arc<[u32]>,
}

impl MeshData {
    /// Creates a bounded indexed triangle mesh and checks every index and float.
    pub fn new(vertices: Vec<Vertex>, indices: Vec<u32>) -> Result<Self, &'static str> {
        if vertices.is_empty() || indices.is_empty() {
            return Err("mesh has no vertices or indices");
        }
        if indices.len() % 3 != 0 {
            return Err("triangle index count is not divisible by three");
        }
        if indices
            .iter()
            .any(|&index| index as usize >= vertices.len())
        {
            return Err("mesh index is outside the vertex buffer");
        }
        if vertices.iter().any(|vertex| {
            vertex
                .position
                .iter()
                .chain(&vertex.normal)
                .chain(&vertex.uv_0)
                .chain(&vertex.color_0)
                .any(|value| !value.is_finite())
                || {
                    let length_squared =
                        vertex.normal.iter().map(|value| value * value).sum::<f32>();
                    !length_squared.is_finite() || (length_squared - 1.0).abs() > 1.0e-3
                }
                || (vertex.tangent != [0; 4]
                    && (vertex.tangent[3].unsigned_abs() != 127
                        || (((vertex.tangent[0] as f32).powi(2)
                            + (vertex.tangent[1] as f32).powi(2)
                            + (vertex.tangent[2] as f32).powi(2))
                            / 127.0_f32.powi(2)
                            - 1.0)
                            .abs()
                            > 0.05))
        }) {
            return Err("mesh contains invalid vertex data or tangent encoding");
        }
        let byte_len = vertices
            .len()
            .saturating_mul(std::mem::size_of::<Vertex>())
            .saturating_add(indices.len().saturating_mul(std::mem::size_of::<u32>()));
        if byte_len > MAX_MESH_UPLOAD_BYTES {
            return Err("mesh exceeds the 64 MiB upload limit");
        }
        Ok(Self {
            vertices: vertices.into(),
            indices: indices.into(),
        })
    }

    /// Returns the interleaved vertex buffer.
    pub fn vertices(&self) -> &[Vertex] {
        &self.vertices
    }

    /// Returns the triangle index buffer.
    pub fn indices(&self) -> &[u32] {
        &self.indices
    }

    /// Returns the payload size used for bounded upload accounting.
    pub fn byte_len(&self) -> usize {
        self.vertices
            .len()
            .saturating_mul(std::mem::size_of::<Vertex>())
            .saturating_add(
                self.indices
                    .len()
                    .saturating_mul(std::mem::size_of::<u32>()),
            )
    }

    /// Returns whether every indexed vertex has a packed tangent frame.
    pub fn has_tangents(&self) -> bool {
        self.indices
            .iter()
            .all(|&index| self.vertices[index as usize].tangent[3] != 0)
    }
}

/// Sampling state retained from a glTF texture sampler.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TextureSampler {
    pub min_filter: TextureFilter,
    pub mag_filter: TextureFilter,
    pub mipmap_mode: MipmapMode,
    pub wrap_u: TextureWrap,
    pub wrap_v: TextureWrap,
}

impl Default for TextureSampler {
    fn default() -> Self {
        Self {
            min_filter: TextureFilter::Linear,
            mag_filter: TextureFilter::Linear,
            mipmap_mode: MipmapMode::Linear,
            wrap_u: TextureWrap::Repeat,
            wrap_v: TextureWrap::Repeat,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TextureFilter {
    Nearest,
    Linear,
}

/// Mipmap filtering, or `None` when the sampler must use level zero only.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MipmapMode {
    None,
    Nearest,
    Linear,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TextureWrap {
    ClampToEdge,
    MirroredRepeat,
    Repeat,
}

/// One glTF image and sampler binding used by a material texture slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TextureBinding {
    pub image: ImageId,
    pub sampler: TextureSampler,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AlphaMode {
    #[default]
    Opaque,
    Mask,
    Blend,
}

/// glTF material factors and texture slots, including transmission inputs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaterialData {
    pub base_color_factor: [f32; 4],
    pub metallic_factor: f32,
    pub roughness_factor: f32,
    pub emissive_factor: [f32; 3],
    pub emissive_strength: f32,
    pub alpha_mode: AlphaMode,
    pub alpha_cutoff: f32,
    pub transmission_factor: f32,
    pub double_sided: bool,
    pub unlit: bool,
    pub normal_scale: f32,
    pub occlusion_strength: f32,
    pub specular_factor: f32,
    pub specular_color_factor: [f32; 3],
    pub base_color_texture: Option<TextureBinding>,
    pub metallic_roughness_texture: Option<TextureBinding>,
    pub normal_texture: Option<TextureBinding>,
    pub occlusion_texture: Option<TextureBinding>,
    pub emissive_texture: Option<TextureBinding>,
    pub transmission_texture: Option<TextureBinding>,
}

impl Default for MaterialData {
    /// glTF's default white, fully metallic, fully rough material.
    fn default() -> Self {
        Self {
            base_color_factor: [1.0; 4],
            metallic_factor: 1.0,
            roughness_factor: 1.0,
            emissive_factor: [0.0; 3],
            emissive_strength: 1.0,
            alpha_mode: AlphaMode::Opaque,
            alpha_cutoff: 0.5,
            transmission_factor: 0.0,
            double_sided: false,
            unlit: false,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
            specular_factor: 1.0,
            specular_color_factor: [1.0; 3],
            base_color_texture: None,
            metallic_roughness_texture: None,
            normal_texture: None,
            occlusion_texture: None,
            emissive_texture: None,
            transmission_texture: None,
        }
    }
}

/// Decoded RGBA8 pixels uploaded once and shared by material texture slots.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageUpload {
    pub id: ImageId,
    pub extent: Extent2d,
    pub color_space: ImageColorSpace,
    pub rgba8: Arc<[u8]>,
}

/// Precomputed linear-RGB diffuse irradiance for one environment.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvironmentUpload {
    pub id: EnvironmentId,
    /// Cosine-convolved diffuse irradiance SH in asset-local Y-up coordinates, linear RGB.
    /// Coefficients use this real basis order:
    /// `[0.282095, 0.488603y, 0.488603z, 0.488603x, 1.092548xy, 1.092548yz,
    /// 0.315392(3y²-1), 1.092548xz, 0.546274(x²-z²)]`.
    pub diffuse_irradiance_sh: [[f32; 3]; 9],
}

impl EnvironmentUpload {
    /// Validates the preconvolved diffuse irradiance coefficients.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .diffuse_irradiance_sh
            .iter()
            .flatten()
            .any(|value| !value.is_finite())
        {
            return Err("diffuse irradiance coefficients must be finite");
        }
        Ok(())
    }
}

/// Transfer function used to filter decoded pixels while building image mips.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ImageColorSpace {
    #[default]
    Linear,
    Srgb,
}

impl ImageUpload {
    /// Validates the tightly packed RGBA8 payload and its per-image budget.
    pub fn validate(&self) -> Result<(), &'static str> {
        let byte_len = (self.extent.width() as usize)
            .checked_mul(self.extent.height() as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("image dimensions overflow")?;
        if byte_len > MAX_IMAGE_UPLOAD_BYTES {
            return Err("image exceeds the 64 MiB upload limit");
        }
        if self.rgba8.len() != byte_len {
            return Err("RGBA8 byte count does not match image extent");
        }
        Ok(())
    }
}

impl MaterialData {
    /// Checks the glTF metallic-roughness factor ranges and finite emission values.
    pub fn validate(self) -> Result<(), &'static str> {
        if self
            .base_color_factor
            .iter()
            .chain(&self.emissive_factor)
            .chain(&self.specular_color_factor)
            .chain([
                &self.metallic_factor,
                &self.roughness_factor,
                &self.emissive_strength,
                &self.alpha_cutoff,
                &self.transmission_factor,
                &self.normal_scale,
                &self.occlusion_strength,
                &self.specular_factor,
            ])
            .any(|value| !value.is_finite())
        {
            return Err("material contains non-finite values");
        }
        if self
            .base_color_factor
            .iter()
            .chain([
                &self.metallic_factor,
                &self.roughness_factor,
                &self.transmission_factor,
            ])
            .any(|value| !(0.0..=1.0).contains(value))
        {
            return Err(
                "base color, metallic, roughness, and transmission factors must be in 0..=1",
            );
        }
        if self
            .emissive_factor
            .iter()
            .any(|value| !(0.0..=1.0).contains(value))
            || self.emissive_strength < 0.0
        {
            return Err("emissive factor must be in 0..=1 and strength non-negative");
        }
        if self.alpha_cutoff < 0.0
            || !(0.0..=1.0).contains(&self.occlusion_strength)
            || !(0.0..=1.0).contains(&self.specular_factor)
            || self.specular_color_factor.iter().any(|value| *value < 0.0)
        {
            return Err("alpha cutoff, occlusion, or specular parameters are out of range");
        }
        Ok(())
    }
}

/// One renderer-owned mesh upload request.
#[derive(Clone, Debug, PartialEq)]
pub struct MeshUpload {
    pub id: MeshId,
    pub data: MeshData,
}

/// One renderer-owned material upload request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaterialUpload {
    pub id: MaterialId,
    pub data: MaterialData,
}

/// Frame-level controls for an immutable renderer-owned environment map.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvironmentSettings {
    pub id: EnvironmentId,
    pub intensity: f32,
    /// Active right-handed yaw about world +Y. Positive angles rotate local +X toward world -Z;
    /// sample both the radiance map and diffuse SH with the inverse world-direction rotation.
    pub rotation_y_radians: f32,
}

/// Complete, renderer-independent data for one frame.
///
/// `clear_color` is linear RGBA in 0..=1 and is converted by an sRGB surface.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSnapshot {
    pub id: FrameId,
    pub surface: SurfaceToken,
    pub extent: Extent2d,
    pub clear_color: [f32; 4],
    /// Constant term of diffuse irradiance for a vertical sky/ground environment, in linear RGB.
    pub ambient_diffuse: [f32; 3],
    /// First-order world-Y coefficient. Together, these fields must reconstruct nonnegative sky and ground radiance.
    pub ambient_diffuse_y: [f32; 3],
    pub environment: Option<EnvironmentSettings>,
    pub camera: CameraData,
    pub directional_lights: Vec<DirectionalLight>,
    pub punctual_lights: Vec<PunctualLight>,
    pub render_items: Vec<RenderItem>,
}

impl FrameSnapshot {
    /// Checks camera, light count, and finite scene data before queue submission.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .clear_color
            .iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            || self
                .camera
                .position
                .iter()
                .chain(self.camera.view_projection.iter().flatten())
                .any(|value| !value.is_finite())
        {
            return Err("frame contains invalid camera or linear clear values");
        }
        if self
            .ambient_diffuse
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
            || self
                .ambient_diffuse_y
                .iter()
                .any(|value| !value.is_finite())
        {
            return Err("ambient diffuse irradiance coefficients must be finite");
        }
        if (0..3).any(|i| {
            let diffuse = f64::from(self.ambient_diffuse[i]);
            let vertical = 1.5 * f64::from(self.ambient_diffuse_y[i].abs());
            vertical > diffuse + 2.0 * f64::from(f32::EPSILON) * diffuse.max(vertical)
        }) {
            return Err("ambient diffuse coefficients imply negative environment radiance");
        }
        if self.environment.is_some_and(|environment| {
            !environment.intensity.is_finite()
                || environment.intensity < 0.0
                || !environment.rotation_y_radians.is_finite()
        }) {
            return Err("environment settings must be finite and intensity non-negative");
        }
        let (near, far) = self.camera.projection.depth_range();
        if !near.is_finite() || !far.is_finite() || near <= 0.0 || far <= near {
            return Err("camera projection requires finite 0 < near < far");
        }
        if self.directional_lights.len() > MAX_DIRECTIONAL_LIGHTS {
            return Err("frame exceeds the 16 directional light limit");
        }
        if self.punctual_lights.len() > MAX_PUNCTUAL_LIGHTS {
            return Err("frame exceeds the 16 point and spot light limit");
        }
        if self
            .directional_lights
            .iter()
            .filter(|light| light.shadow.is_some())
            .count()
            > 1
        {
            return Err("only one shadowed directional light is currently supported");
        }
        if self.render_items.len() > MAX_RENDER_ITEMS {
            return Err("frame exceeds the 65,536 render item limit");
        }
        for item in &self.render_items {
            if !valid_model_matrix(item.model_matrix) {
                return Err("frame contains an invalid render transform");
            }
        }
        if self.directional_lights.iter().any(|light| {
            light
                .direction_to_light
                .iter()
                .chain(&light.radiance)
                .any(|value| !value.is_finite())
                || light.direction_to_light.iter().all(|value| *value == 0.0)
                || light.radiance.iter().any(|value| *value < 0.0)
                || light.shadow.is_some_and(|shadow| {
                    !shadow.angular_radius_radians.is_finite()
                        || !(0.0..0.5).contains(&shadow.angular_radius_radians)
                        || !shadow.max_penumbra_world.is_finite()
                        || shadow.max_penumbra_world < 0.0
                })
        }) {
            return Err("frame contains an invalid directional light");
        }
        if self.punctual_lights.iter().any(|light| match light {
            PunctualLight::Point {
                position,
                color,
                intensity,
                range,
            } => {
                position.iter().any(|value| !value.is_finite())
                    || color
                        .iter()
                        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
                    || !intensity.is_finite()
                    || *intensity < 0.0
                    || range.is_some_and(|range| !range.is_finite() || range <= 0.0)
            }
            PunctualLight::Spot {
                position,
                direction,
                color,
                intensity,
                range,
                inner_cone_angle,
                outer_cone_angle,
            } => {
                position
                    .iter()
                    .chain(direction)
                    .any(|value| !value.is_finite())
                    || direction.iter().all(|value| *value == 0.0)
                    || color
                        .iter()
                        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
                    || !intensity.is_finite()
                    || *intensity < 0.0
                    || range.is_some_and(|range| !range.is_finite() || range <= 0.0)
                    || !inner_cone_angle.is_finite()
                    || !outer_cone_angle.is_finite()
                    || *inner_cone_angle < 0.0
                    || *outer_cone_angle <= *inner_cone_angle
                    || *outer_cone_angle > std::f32::consts::FRAC_PI_2
            }
        }) {
            return Err("frame contains an invalid point or spot light");
        }
        Ok(())
    }
}

fn valid_model_matrix(matrix: [[f32; 4]; 4]) -> bool {
    if matrix.iter().flatten().any(|value| !value.is_finite())
        || matrix[0][3] != 0.0
        || matrix[1][3] != 0.0
        || matrix[2][3] != 0.0
        || matrix[3][3] != 1.0
    {
        return false;
    }
    let determinant = model_determinant(matrix);
    determinant.is_finite() && determinant != 0.0
}

fn model_determinant(matrix: [[f32; 4]; 4]) -> f32 {
    let [x, y, z, _] = matrix;
    x[0] * (y[1] * z[2] - y[2] * z[1]) - y[0] * (x[1] * z[2] - x[2] * z[1])
        + z[0] * (x[1] * y[2] - x[2] * y[1])
}

/// A command sent to the dedicated renderer thread.
#[derive(Clone, Debug, PartialEq)]
pub enum RendererCommand {
    ConfigureSurface(SurfaceDescriptor),
    UploadMesh(MeshUpload),
    UploadImage(ImageUpload),
    UploadEnvironment(EnvironmentUpload),
    UploadMaterial(MaterialUpload),
    /// Applies new active effects after all earlier renderer commands.
    SetRenderFeatures(RenderFeatures),
    SubmitFrame(FrameSnapshot),
    Shutdown,
}

/// Why the renderer dropped a command or failed a backend operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RendererFault {
    /// Runtime requested an effect that is unavailable or invalid for this backend build.
    Configuration(String),
    StaleSurface {
        received: SurfaceGeneration,
        current: SurfaceGeneration,
    },
    UnconfiguredSurface(SurfaceId),
    SurfaceRequiresReconfigure(SurfaceId),
    SurfaceNotReady,
    /// The frame arrived before the corresponding upload command completed.
    MissingMesh(MeshId),
    /// The material referenced an image whose upload has not completed.
    MissingImage(ImageId),
    /// The frame referenced an environment whose upload has not completed.
    MissingEnvironment(EnvironmentId),
    /// The frame arrived before the corresponding upload command completed.
    MissingMaterial(MaterialId),
    Backend(String),
}

/// An event returned to the game/platform thread.
#[derive(Clone, Debug, PartialEq)]
pub enum RendererEvent {
    Ready,
    SurfaceConfigured(SurfaceToken),
    /// Confirms that a queued render feature change has been applied.
    RenderFeaturesChanged(RenderFeatures),
    /// Rejects a queued render feature change without stopping the renderer.
    RenderFeaturesRejected {
        reason: String,
    },
    /// Confirms the renderer has installed this mesh ID.
    MeshUploaded(MeshId),
    /// Confirms the renderer has installed this image ID.
    ImageUploaded(ImageId),
    /// Confirms the renderer has installed this environment's diffuse SH.
    EnvironmentUploaded(EnvironmentId),
    /// Confirms the renderer has installed this material ID.
    MaterialUploaded(MaterialId),
    SurfaceInvalidated {
        frame: FrameId,
        surface: SurfaceToken,
    },
    FramePresented(FrameId),
    FrameDropped {
        frame: FrameId,
        fault: RendererFault,
    },
    Failed(RendererFault),
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_environment_irradiance_matches_diffuse_convolution() {
        let (constant_environment, no_gradient) =
            vertical_environment_irradiance([1.0; 3], [1.0; 3]).unwrap();
        assert!(
            constant_environment
                .iter()
                .all(|value| (*value - std::f32::consts::PI).abs() < 1e-6)
        );
        assert!(no_gradient.iter().all(|value| value.abs() < 1e-6));

        let (constant, vertical) = vertical_environment_irradiance([1.0; 3], [0.0; 3]).unwrap();
        let pi = std::f32::consts::PI;
        assert!(
            constant
                .iter()
                .all(|value| (*value - pi * 0.5).abs() < 1e-6)
        );
        assert!(
            vertical
                .iter()
                .all(|value| (*value - pi / 3.0).abs() < 1e-6)
        );
        assert!((constant[0] + vertical[0] - pi * 5.0 / 6.0).abs() < 1e-6);
        assert!((constant[0] - vertical[0] - pi / 6.0).abs() < 1e-6);
        assert_eq!(
            vertical_environment_irradiance([f32::NAN, 0.0, 0.0], [0.0; 3]),
            Err("environment radiance must be finite and non-negative")
        );
        assert_eq!(
            vertical_environment_irradiance([0.0; 3], [-0.1, 0.0, 0.0]),
            Err("environment radiance must be finite and non-negative")
        );
        assert_eq!(
            vertical_environment_irradiance([f32::MAX; 3], [f32::MAX; 3]),
            Err("environment irradiance exceeds the finite f32 range")
        );
    }

    #[test]
    fn high_quality_preset_keeps_mboit_opt_in() {
        let features = RenderFeatures::high_quality();
        assert!(features.ssr.is_some());
        assert!(!features.translucent_shadows);
    }

    fn vertex(position: [f32; 3]) -> Vertex {
        Vertex {
            position,
            normal: [0.0, 0.0, 1.0],
            uv_0: [0.0; 2],
            color_0: [1.0; 4],
            tangent: [0; 4],
        }
    }

    #[test]
    fn mesh_data_checks_triangle_indices_and_finite_vertices() {
        let mesh = MeshData::new(
            vec![
                vertex([0.0, 0.0, 0.0]),
                vertex([1.0, 0.0, 0.0]),
                vertex([0.0, 1.0, 0.0]),
            ],
            vec![0, 1, 2],
        )
        .unwrap();
        assert_eq!(mesh.indices(), [0, 1, 2]);
        assert!(MeshData::new(vec![vertex([0.0; 3]); 3], vec![0, 1]).is_err());
        assert!(MeshData::new(vec![vertex([0.0; 3]); 3], vec![0, 1, 3]).is_err());
        assert!(MeshData::new(vec![Vertex::default(); 3], vec![0, 1, 2]).is_err());
        assert!(MeshData::new(vec![vertex([f32::NAN, 0.0, 0.0]); 3], vec![0, 1, 2]).is_err());
    }

    #[test]
    fn mesh_tangents_use_zero_sentinel_or_unit_snorm_frame() {
        let mut vertices = vec![
            vertex([0.0; 3]),
            vertex([1.0, 0.0, 0.0]),
            vertex([0.0, 1.0, 0.0]),
        ];
        assert!(
            !MeshData::new(vertices.clone(), vec![0, 1, 2])
                .unwrap()
                .has_tangents()
        );
        for vertex in &mut vertices {
            vertex.tangent = [127, 0, 0, -127];
        }
        assert!(
            MeshData::new(vertices.clone(), vec![0, 1, 2])
                .unwrap()
                .has_tangents()
        );
        vertices[0].tangent = [127, 0, 0, 0];
        assert!(MeshData::new(vertices, vec![0, 1, 2]).is_err());
    }

    #[test]
    fn material_data_checks_gltf_factor_ranges() {
        assert!(MaterialData::default().validate().is_ok());
        let mut material = MaterialData::default();
        material.roughness_factor = 1.1;
        assert!(material.validate().is_err());
        material.roughness_factor = 1.0;
        material.normal_scale = -1.0;
        assert!(material.validate().is_ok());
        material.normal_scale = f32::NAN;
        assert!(material.validate().is_err());
        material.normal_scale = 1.0;
        material.transmission_factor = 1.0;
        assert!(material.validate().is_ok());
        material.transmission_factor = f32::NAN;
        assert!(material.validate().is_err());
        material.transmission_factor = 1.01;
        assert!(material.validate().is_err());
    }

    #[test]
    fn bloom_settings_reject_non_finite_or_negative_values() {
        assert!(BloomSettings::default().validate().is_ok());
        assert!(
            BloomSettings {
                intensity: f32::NAN,
                radius_uv: 0.003
            }
            .validate()
            .is_err()
        );
        assert!(
            BloomSettings {
                intensity: -1.0,
                radius_uv: 0.003
            }
            .validate()
            .is_err()
        );
        assert!(
            BloomSettings {
                intensity: 0.04,
                radius_uv: f32::INFINITY
            }
            .validate()
            .is_err()
        );
        assert!(
            BloomSettings {
                intensity: 0.04,
                radius_uv: 0.6
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn god_rays_settings_bound_scattering_and_interleave_work() {
        assert!(GodRaysSettings::default().validate().is_ok());
        assert!(
            GodRaysSettings {
                extinction_per_unit: f32::NAN,
                ..GodRaysSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            GodRaysSettings {
                scattering_albedo: 1.1,
                ..GodRaysSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            GodRaysSettings {
                anisotropy: 0.96,
                ..GodRaysSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            GodRaysSettings {
                sample_count: 0,
                ..GodRaysSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            GodRaysSettings {
                sample_count: 63,
                ..GodRaysSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            GodRaysSettings {
                interleave_block: 9,
                ..GodRaysSettings::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn ssr_settings_bound_ray_work_and_reject_invalid_values() {
        assert!(SsrSettings::default().validate().is_ok());
        assert!(
            SsrSettings {
                max_distance: f32::INFINITY,
                ..SsrSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            SsrSettings {
                thickness: -0.1,
                ..SsrSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            SsrSettings {
                max_steps: 0,
                ..SsrSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            SsrSettings {
                max_roughness: 1.1,
                ..SsrSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            SsrSettings {
                edge_fade_pixels: f32::NAN,
                ..SsrSettings::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn image_upload_checks_rgba8_extent_and_budget() {
        let image = ImageUpload {
            id: ImageId::new(1).unwrap(),
            extent: Extent2d::new(1, 1).unwrap(),
            color_space: ImageColorSpace::Linear,
            rgba8: Arc::from([255, 255, 255, 255]),
        };
        assert!(image.validate().is_ok());
        assert_eq!(
            ImageUpload {
                rgba8: Arc::from([0, 0, 0]),
                ..image.clone()
            }
            .validate(),
            Err("RGBA8 byte count does not match image extent")
        );
        assert_eq!(
            ImageUpload {
                extent: Extent2d::new(4097, 4096).unwrap(),
                ..image
            }
            .validate(),
            Err("image exceeds the 64 MiB upload limit")
        );
    }

    #[test]
    fn environment_upload_checks_finite_diffuse_coefficients() {
        let upload = EnvironmentUpload {
            id: EnvironmentId::new(1).unwrap(),
            diffuse_irradiance_sh: [[0.0; 3]; 9],
        };
        assert!(upload.validate().is_ok());
        assert!(
            EnvironmentUpload {
                diffuse_irradiance_sh: [[f32::NAN, 0.0, 0.0]; 9],
                ..upload.clone()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn frame_validation_rejects_invalid_render_transforms_and_oversized_snapshots() {
        let item = RenderItem {
            mesh: MeshId::new(1).unwrap(),
            material: MaterialId::new(1).unwrap(),
            model_matrix: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
        };
        let mut frame = FrameSnapshot {
            id: FrameId::new(1).unwrap(),
            surface: SurfaceToken {
                id: SurfaceId::new(1).unwrap(),
                generation: SurfaceGeneration::new(1).unwrap(),
            },
            extent: Extent2d::new(64, 64).unwrap(),
            clear_color: [0.0; 4],
            ambient_diffuse: [0.0; 3],
            ambient_diffuse_y: [0.0; 3],
            environment: None,
            camera: CameraData::default(),
            directional_lights: Vec::new(),
            punctual_lights: Vec::new(),
            render_items: vec![item],
        };
        assert!(frame.validate().is_ok());

        frame.environment = Some(EnvironmentSettings {
            id: EnvironmentId::new(1).unwrap(),
            intensity: f32::NAN,
            rotation_y_radians: 0.0,
        });
        assert_eq!(
            frame.validate(),
            Err("environment settings must be finite and intensity non-negative")
        );
        frame.environment = Some(EnvironmentSettings {
            id: EnvironmentId::new(1).unwrap(),
            intensity: 1.0,
            rotation_y_radians: 0.0,
        });
        assert!(frame.validate().is_ok());
        frame.environment = None;

        frame.ambient_diffuse = [1.0; 3];
        frame.ambient_diffuse_y = [1.1; 3];
        assert_eq!(
            frame.validate(),
            Err("ambient diffuse coefficients imply negative environment radiance")
        );
        frame.ambient_diffuse_y = [0.8; 3];
        assert_eq!(
            frame.validate(),
            Err("ambient diffuse coefficients imply negative environment radiance")
        );
        frame.ambient_diffuse_y = [0.5; 3];
        assert!(frame.validate().is_ok());
        let (constant, vertical) =
            vertical_environment_irradiance([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]).unwrap();
        frame.ambient_diffuse = constant;
        frame.ambient_diffuse_y = vertical;
        assert!(frame.validate().is_ok());
        frame.ambient_diffuse = [0.0; 3];
        frame.ambient_diffuse_y = [0.0; 3];

        frame.render_items.push(item);
        assert_eq!(
            frame.validate(),
            Err("render item IDs must be unique within a frame")
        );
        frame.render_items.pop();

        frame.render_items[0].model_matrix[3][0] = f32::NAN;
        assert_eq!(
            frame.validate(),
            Err("frame contains an invalid render transform")
        );
        frame.render_items[0].model_matrix = [[0.0; 4]; 4];
        assert!(frame.validate().is_err());
        frame.render_items[0].model_matrix = [
            [-1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        assert!(frame.validate().is_ok());
        frame.render_items = vec![item; MAX_RENDER_ITEMS + 1];
        assert_eq!(
            frame.validate(),
            Err("frame exceeds the 65,536 render item limit")
        );
    }

    #[test]
    fn frame_validation_checks_shadow_camera_and_source_bounds() {
        let mut frame = FrameSnapshot {
            id: FrameId::new(1).unwrap(),
            surface: SurfaceToken {
                id: SurfaceId::new(1).unwrap(),
                generation: SurfaceGeneration::new(1).unwrap(),
            },
            extent: Extent2d::new(64, 64).unwrap(),
            clear_color: [0.0; 4],
            ambient_diffuse: [0.0; 3],
            ambient_diffuse_y: [0.0; 3],
            environment: None,
            camera: CameraData::default(),
            directional_lights: Vec::new(),
            punctual_lights: Vec::new(),
            render_items: Vec::new(),
        };
        frame.camera.projection = CameraProjection::Perspective {
            near: 0.0,
            far: 10.0,
        };
        assert_eq!(
            frame.validate(),
            Err("camera projection requires finite 0 < near < far")
        );

        frame.camera = CameraData::default();
        frame.directional_lights.push(DirectionalLight {
            direction_to_light: [0.0, 0.0, 1.0],
            radiance: [1.0; 3],
            shadow: Some(DirectionalShadow {
                angular_radius_radians: 0.5,
                max_penumbra_world: 1.0,
            }),
        });
        assert!(frame.validate().is_err());
    }

    #[test]
    fn punctual_light_validation_checks_parameters_and_limit() {
        let mut frame = FrameSnapshot {
            id: FrameId::new(1).unwrap(),
            surface: SurfaceToken {
                id: SurfaceId::new(1).unwrap(),
                generation: SurfaceGeneration::new(1).unwrap(),
            },
            extent: Extent2d::new(64, 64).unwrap(),
            clear_color: [0.0; 4],
            ambient_diffuse: [0.0; 3],
            ambient_diffuse_y: [0.0; 3],
            environment: None,
            camera: CameraData::default(),
            directional_lights: Vec::new(),
            punctual_lights: vec![
                PunctualLight::Spot {
                    position: [0.0, 1.0, 2.0],
                    direction: [0.0, 0.0, -1.0],
                    color: [1.0, 0.8, 0.6],
                    intensity: 100.0,
                    range: Some(5.0),
                    inner_cone_angle: 0.1,
                    outer_cone_angle: 0.5,
                };
                MAX_PUNCTUAL_LIGHTS
            ],
            render_items: Vec::new(),
        };
        assert!(frame.validate().is_ok());
        frame.punctual_lights.push(frame.punctual_lights[0]);
        assert_eq!(
            frame.validate(),
            Err("frame exceeds the 16 point and spot light limit")
        );
        frame.punctual_lights.pop();
        frame.punctual_lights[0] = PunctualLight::Point {
            position: [0.0; 3],
            color: [1.0; 3],
            intensity: 1.0,
            range: Some(0.0),
        };
        assert_eq!(
            frame.validate(),
            Err("frame contains an invalid point or spot light")
        );
    }
}
