use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroU16,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use x173ge::platform::winit::{
    event::MouseButton,
    keyboard::KeyCode,
    window::{CursorGrabMode, Window},
};
use x173ge::{
    FramePacing, GameDefinition,
    core::{
        EnvironmentId, Extent2d, ImageId, Mat4, MaterialId, MeshId, Vec3, perspective_rh_reverse_z,
    },
    ecs::{Workload, World},
    platform::{PlatformEvent, WindowConfig, winit::dpi::PhysicalSize},
    protocol::{
        BloomSettings, CameraData, CameraProjection, DirectionalLight, DirectionalShadow,
        EnvironmentSettings, EnvironmentUpload, FrameSnapshot, GodRaysSettings, ImageUpload,
        MAX_DIRECTIONAL_LIGHTS, MAX_PUNCTUAL_LIGHTS, MaterialUpload, MeshData, MeshUpload,
        PunctualLight, RenderFeatures, RenderItem, RendererCommand, RendererEvent, SsrSettings,
        ToneMappingSettings, vertical_environment_irradiance,
    },
    renderer::{RendererHandle, SendError},
    run,
    scene::{ExtractFrame, RenderExtract},
};

const CAMERA_MIN_RADIUS: f32 = 0.01;
const CAMERA_SPEED_PER_RADIUS: f32 = 0.45;
const CAMERA_MIN_SPEED_MULTIPLIER: f32 = 0.0001;
const CAMERA_NEAR_PER_RADIUS: f32 = 0.001;
const CAMERA_NEAR_MAX: f32 = 0.001;
const CAMERA_FAR_MARGIN_PER_RADIUS: f32 = 0.15;
const CAMERA_LOOK_SENSITIVITY: f32 = 0.0022;
const CAMERA_PITCH_LIMIT: f32 = 1.35;
const DEFAULT_ENVIRONMENT_INTENSITY: f32 = 0.1;
const DEFAULT_GLOBAL_LIGHT_SCALE: f32 = 10.0;
const DEFAULT_POINT_LIGHT_INTENSITY: f32 = 30.0;
const DEFAULT_SPOT_LIGHT_INTENSITY: f32 = 60.0;
const DEFAULT_CAMERA_POSITION: [f32; 3] = [1.2659273, -6.2382364, 11.584539];
const DEFAULT_CAMERA_SPEED_MULTIPLIER: f32 = 0.044;
const DEFAULT_CAMERA_YAW_DEGREES: f32 = -467.52;
const DEFAULT_CAMERA_PITCH_DEGREES: f32 = -15.08;
const DEFAULT_SHADOW_ANGULAR_RADIUS_DEGREES: f32 = 0.5;
const DEFAULT_SHADOW_MAX_PENUMBRA_WORLD: f32 = 0.2;

#[cfg(feature = "profiling")]
use x173ge::core::FrameId;

struct PbrSmoke {
    pending_uploads: VecDeque<PendingUpload>,
    renderer: Option<RendererHandle>,
    ready_meshes: HashSet<MeshId>,
    ready_materials: HashSet<MaterialId>,
    environment_id: Option<EnvironmentId>,
    environment_ready: bool,
    draw_items: Vec<RenderItem>,
    directional_lights: Vec<DirectionalLight>,
    global_light_intensity: f32,
    punctual_lights: Vec<PunctualLight>,
    point_light_enabled: bool,
    point_light_intensity: f32,
    spot_light_enabled: bool,
    spot_light_intensity: f32,
    ambient_diffuse: [f32; 3],
    ambient_diffuse_y: [f32; 3],
    environment_enabled: bool,
    environment_intensity: f32,
    environment_rotation: f32,
    shadow_settings: DirectionalShadow,
    tone_mapping: ToneMappingSettings,
    ssr_settings: SsrSettings,
    bloom_settings: BloomSettings,
    god_rays_settings: GodRaysSettings,
    frame_pacing: FramePacing,
    shadows: bool,
    frame_limit: Option<u32>,
    warmup_frames: u32,
    ready_frames: u32,
    measured_frames: u32,
    #[cfg(feature = "profiling")]
    profiled_presented_frame: Option<FrameId>,
    #[cfg(feature = "profiling")]
    measured_started_at: Option<Instant>,
    asset_upload_started: Option<Instant>,
    upload_error: Option<String>,
    presented: u32,
    presented_fps: f32,
    fps_sample_frames: u32,
    fps_sample_started: Instant,
    ready_frames_after_resize: u32,
    resize_event_seen: bool,
    resized: bool,
    mboit: bool,
    smaa: bool,
    gtao: bool,
    ssr: bool,
    bloom: bool,
    god_rays: bool,
    interactive: bool,
    tuning_index: usize,
    pending_render_features: Option<RenderFeatures>,
    window: Option<Arc<Window>>,
    camera: FreeCamera,
    mouse_delta: [f64; 2],
    cursor_captured: bool,
}

struct FreeCamera {
    position: Vec3,
    yaw: f32,
    pitch: f32,
    scene_radius: f32,
    speed_multiplier: f32,
    r1_benchmark_view: bool,
}

impl FreeCamera {
    fn new(scene_radius: f32) -> Self {
        let scene_radius = scene_radius.max(CAMERA_MIN_RADIUS);
        Self {
            position: Vec3::from_array(DEFAULT_CAMERA_POSITION),
            yaw: DEFAULT_CAMERA_YAW_DEGREES.to_radians(),
            pitch: DEFAULT_CAMERA_PITCH_DEGREES.to_radians(),
            scene_radius,
            speed_multiplier: DEFAULT_CAMERA_SPEED_MULTIPLIER,
            r1_benchmark_view: false,
        }
    }

    fn r1_benchmark_view(scene_radius: f32) -> Self {
        let radius = scene_radius.max(1.0);
        Self {
            position: Vec3::new(0.0, radius * 0.24, radius * 2.45),
            yaw: -std::f32::consts::FRAC_PI_2,
            pitch: -0.24_f32.atan2(2.45),
            scene_radius: radius,
            speed_multiplier: (radius / 28.0).clamp(0.15, 40.0),
            r1_benchmark_view: true,
        }
    }

    fn fov_y(&self) -> f32 {
        if self.r1_benchmark_view { 60.0 } else { 50.0 }
    }

    fn direction(&self) -> Vec3 {
        Vec3::new(
            self.yaw.cos() * self.pitch.cos(),
            self.pitch.sin(),
            self.yaw.sin() * self.pitch.cos(),
        )
        .normalize()
    }

    fn update(
        &mut self,
        input: &x173ge::platform::InputState,
        mouse: [f64; 2],
        dt: Duration,
        captured: bool,
    ) {
        if captured {
            self.rotate(mouse);
        }
        self.speed_multiplier = scroll_speed(self.speed_multiplier, input.wheel_delta());
        if !captured {
            return;
        }
        let down = |key| input.key_down(key);
        let axis = |positive, negative| positive as u8 as f32 - negative as u8 as f32;
        let forward = axis(down(KeyCode::KeyW), down(KeyCode::KeyS));
        let strafe = axis(
            down(KeyCode::KeyD) || down(KeyCode::ArrowRight),
            down(KeyCode::KeyA) || down(KeyCode::ArrowLeft),
        );
        let vertical = axis(
            down(KeyCode::Space) || down(KeyCode::KeyE),
            down(KeyCode::KeyQ) || down(KeyCode::ShiftLeft),
        );
        let direction = movement_direction(self.direction(), forward, strafe, vertical);
        let sprint = if down(KeyCode::ControlLeft) {
            1.65
        } else {
            1.0
        };
        self.position += direction
            * (self.scene_radius
                * CAMERA_SPEED_PER_RADIUS
                * self.speed_multiplier
                * sprint
                * dt.as_secs_f32());
    }

    fn rotate(&mut self, mouse: [f64; 2]) {
        self.yaw += mouse[0] as f32 * CAMERA_LOOK_SENSITIVITY;
        self.pitch = (self.pitch - mouse[1] as f32 * CAMERA_LOOK_SENSITIVITY)
            .clamp(-CAMERA_PITCH_LIMIT, CAMERA_PITCH_LIMIT);
    }
}

fn movement_direction(
    forward: Vec3,
    forward_axis: f32,
    strafe_axis: f32,
    vertical_axis: f32,
) -> Vec3 {
    let forward = Vec3::new(forward.x, 0.0, forward.z).normalize_or_zero();
    let right = forward.cross(Vec3::Y).normalize_or_zero();
    let direction = forward * forward_axis + right * strafe_axis + Vec3::Y * vertical_axis;
    direction.normalize_or_zero()
}

fn scroll_speed(speed: f32, wheel_delta: f64) -> f32 {
    (speed * 1.5_f32.powf(wheel_delta.clamp(-4.0, 4.0) as f32))
        .clamp(CAMERA_MIN_SPEED_MULTIPLIER, 20.0)
}

#[derive(Clone, Copy, Debug)]
enum Control {
    Contrast,
    ExposureKey,
    ExposureMinimum,
    ExposureMaximum,
    ExposureAdaptation,
    BloomIntensity,
    BloomRadius,
    SsrDistance,
    SsrThickness,
    SsrSteps,
    SsrRoughness,
    SsrIntensity,
    SsrEdgeFade,
    GodRaysExtinction,
    GodRaysAlbedo,
    GodRaysAnisotropy,
    GodRaysSamples,
    GodRaysInterleave,
    ShadowAngularRadius,
    ShadowPenumbra,
    EnvironmentIntensity,
    EnvironmentRotation,
    DirectionalLight,
    PointLight,
    SpotLight,
    MaxFps,
    CameraSpeed,
}

impl Control {
    const ALL: [Self; 27] = [
        Self::Contrast,
        Self::ExposureKey,
        Self::ExposureMinimum,
        Self::ExposureMaximum,
        Self::ExposureAdaptation,
        Self::BloomIntensity,
        Self::BloomRadius,
        Self::SsrDistance,
        Self::SsrThickness,
        Self::SsrSteps,
        Self::SsrRoughness,
        Self::SsrIntensity,
        Self::SsrEdgeFade,
        Self::GodRaysExtinction,
        Self::GodRaysAlbedo,
        Self::GodRaysAnisotropy,
        Self::GodRaysSamples,
        Self::GodRaysInterleave,
        Self::ShadowAngularRadius,
        Self::ShadowPenumbra,
        Self::EnvironmentIntensity,
        Self::EnvironmentRotation,
        Self::DirectionalLight,
        Self::PointLight,
        Self::SpotLight,
        Self::MaxFps,
        Self::CameraSpeed,
    ];

    fn available(self) -> bool {
        match self {
            Self::BloomIntensity | Self::BloomRadius => cfg!(feature = "bloom"),
            Self::SsrDistance
            | Self::SsrThickness
            | Self::SsrSteps
            | Self::SsrRoughness
            | Self::SsrIntensity
            | Self::SsrEdgeFade => cfg!(feature = "ssr"),
            Self::GodRaysExtinction
            | Self::GodRaysAlbedo
            | Self::GodRaysAnisotropy
            | Self::GodRaysSamples
            | Self::GodRaysInterleave => cfg!(feature = "god-rays"),
            _ => true,
        }
    }

    fn controls() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().filter(|control| control.available())
    }

    fn count() -> usize {
        Self::controls().count()
    }

    fn name(self) -> &'static str {
        match self {
            Self::Contrast => "contrast",
            Self::ExposureKey => "exposure key",
            Self::ExposureMinimum => "exposure min",
            Self::ExposureMaximum => "exposure max",
            Self::ExposureAdaptation => "exposure delay",
            Self::BloomIntensity => "bloom intensity",
            Self::BloomRadius => "bloom radius",
            Self::SsrDistance => "SSR distance",
            Self::SsrThickness => "SSR thickness",
            Self::SsrSteps => "SSR steps",
            Self::SsrRoughness => "SSR roughness",
            Self::SsrIntensity => "SSR intensity",
            Self::SsrEdgeFade => "SSR edge fade",
            Self::GodRaysExtinction => "fog density",
            Self::GodRaysAlbedo => "fog scattering",
            Self::GodRaysAnisotropy => "fog phase",
            Self::GodRaysSamples => "fog samples",
            Self::GodRaysInterleave => "fog block",
            Self::ShadowAngularRadius => "shadow angular radius",
            Self::ShadowPenumbra => "shadow penumbra",
            Self::EnvironmentIntensity => "environment intensity",
            Self::EnvironmentRotation => "environment rotation",
            Self::DirectionalLight => "directional light",
            Self::PointLight => "point light",
            Self::SpotLight => "spot light",
            Self::MaxFps => "max FPS",
            Self::CameraSpeed => "camera speed",
        }
    }

    fn value(self, app: &PbrSmoke) -> f32 {
        match self {
            Self::Contrast => app.tone_mapping.contrast,
            Self::ExposureKey => app.tone_mapping.key_luminance,
            Self::ExposureMinimum => app.tone_mapping.min_exposure,
            Self::ExposureMaximum => app.tone_mapping.max_exposure,
            Self::ExposureAdaptation => app.tone_mapping.adaptation_seconds,
            Self::BloomIntensity => app.bloom_settings.intensity,
            Self::BloomRadius => app.bloom_settings.radius_uv,
            Self::SsrDistance => app.ssr_settings.max_distance,
            Self::SsrThickness => app.ssr_settings.thickness,
            Self::SsrSteps => app.ssr_settings.max_steps as f32,
            Self::SsrRoughness => app.ssr_settings.max_roughness,
            Self::SsrIntensity => app.ssr_settings.intensity,
            Self::SsrEdgeFade => app.ssr_settings.edge_fade_pixels,
            Self::GodRaysExtinction => app.god_rays_settings.extinction_per_unit,
            Self::GodRaysAlbedo => app.god_rays_settings.scattering_albedo,
            Self::GodRaysAnisotropy => app.god_rays_settings.anisotropy,
            Self::GodRaysSamples => app.god_rays_settings.sample_count as f32,
            Self::GodRaysInterleave => app.god_rays_settings.interleave_block as f32,
            Self::ShadowAngularRadius => app.shadow_settings.angular_radius_radians.to_degrees(),
            Self::ShadowPenumbra => app.shadow_settings.max_penumbra_world,
            Self::EnvironmentIntensity => app.environment_intensity,
            Self::EnvironmentRotation => app.environment_rotation.to_degrees(),
            Self::DirectionalLight => app.global_light_intensity,
            Self::PointLight => app.point_light_intensity,
            Self::SpotLight => app.spot_light_intensity,
            Self::MaxFps => f32::from(app.frame_pacing.max_fps.get()),
            Self::CameraSpeed => app.camera.speed_multiplier,
        }
    }

    fn adjust(self, app: &mut PbrSmoke, direction: f32) -> bool {
        let step = |value: f32, amount: f32, min: f32, max: f32| {
            (value + direction * amount).clamp(min, max)
        };
        match self {
            Self::Contrast => {
                app.tone_mapping.contrast = step(app.tone_mapping.contrast, 0.05, 0.5, 1.5);
                true
            }
            Self::ExposureKey => {
                app.tone_mapping.key_luminance =
                    step(app.tone_mapping.key_luminance, 0.01, 0.01, 1.0);
                true
            }
            Self::ExposureMinimum => {
                app.tone_mapping.min_exposure = step(
                    app.tone_mapping.min_exposure,
                    0.05,
                    0.01,
                    app.tone_mapping.max_exposure.min(1.0),
                );
                true
            }
            Self::ExposureMaximum => {
                app.tone_mapping.max_exposure =
                    step(app.tone_mapping.max_exposure, 0.25, 1.0, 16.0);
                true
            }
            Self::ExposureAdaptation => {
                app.tone_mapping.adaptation_seconds =
                    step(app.tone_mapping.adaptation_seconds, 0.05, 0.05, 4.0);
                true
            }
            Self::BloomIntensity => {
                app.bloom_settings.intensity = step(app.bloom_settings.intensity, 0.01, 0.0, 1.0);
                app.bloom
            }
            Self::BloomRadius => {
                app.bloom_settings.radius_uv = step(app.bloom_settings.radius_uv, 0.001, 0.0, 0.5);
                app.bloom
            }
            Self::SsrDistance => {
                app.ssr_settings.max_distance =
                    step(app.ssr_settings.max_distance, 1.0, 0.1, 500.0);
                app.ssr
            }
            Self::SsrThickness => {
                app.ssr_settings.thickness = step(app.ssr_settings.thickness, 0.01, 0.0, 10.0);
                app.ssr
            }
            Self::SsrSteps => {
                app.ssr_settings.max_steps = (app.ssr_settings.max_steps as i32
                    + (direction * 16.0) as i32)
                    .clamp(1, 4096) as u32;
                app.ssr
            }
            Self::SsrRoughness => {
                app.ssr_settings.max_roughness =
                    step(app.ssr_settings.max_roughness, 0.05, 0.0, 1.0);
                app.ssr
            }
            Self::SsrIntensity => {
                app.ssr_settings.intensity = step(app.ssr_settings.intensity, 0.05, 0.0, 8.0);
                app.ssr
            }
            Self::SsrEdgeFade => {
                app.ssr_settings.edge_fade_pixels =
                    step(app.ssr_settings.edge_fade_pixels, 1.0, 0.0, 256.0);
                app.ssr
            }
            Self::GodRaysExtinction => {
                app.god_rays_settings.extinction_per_unit =
                    step(app.god_rays_settings.extinction_per_unit, 0.0001, 0.0, 0.1);
                app.god_rays
            }
            Self::GodRaysAlbedo => {
                app.god_rays_settings.scattering_albedo =
                    step(app.god_rays_settings.scattering_albedo, 0.05, 0.0, 1.0);
                app.god_rays
            }
            Self::GodRaysAnisotropy => {
                app.god_rays_settings.anisotropy =
                    step(app.god_rays_settings.anisotropy, 0.05, -0.95, 0.95);
                app.god_rays
            }
            Self::GodRaysSamples => {
                let block_samples = app.god_rays_settings.interleave_block.pow(2);
                app.god_rays_settings.sample_count = (app.god_rays_settings.sample_count as i32
                    + direction as i32 * block_samples as i32)
                    .clamp(block_samples as i32, 4096)
                    as u32;
                app.god_rays
            }
            Self::GodRaysInterleave => {
                const BLOCKS: [u32; 4] = [1, 2, 4, 8];
                let index = BLOCKS
                    .iter()
                    .position(|&block| block == app.god_rays_settings.interleave_block)
                    .unwrap_or(2) as i32;
                let index = (index + direction as i32).clamp(0, 3) as usize;
                let block = BLOCKS[index];
                let terms = block.pow(2);
                app.god_rays_settings.sample_count = app
                    .god_rays_settings
                    .sample_count
                    .div_ceil(terms)
                    .saturating_mul(terms)
                    .min(4096);
                app.god_rays_settings.interleave_block = block;
                app.god_rays
            }
            Self::ShadowAngularRadius => {
                app.shadow_settings.angular_radius_radians = step(
                    app.shadow_settings.angular_radius_radians,
                    0.1_f32.to_radians(),
                    0.0,
                    0.49,
                );
                false
            }
            Self::ShadowPenumbra => {
                app.shadow_settings.max_penumbra_world =
                    step(app.shadow_settings.max_penumbra_world, 0.05, 0.0, 100.0);
                false
            }
            Self::EnvironmentIntensity => {
                app.environment_intensity = step(app.environment_intensity, 0.1, 0.0, 16.0);
                false
            }
            Self::EnvironmentRotation => {
                app.environment_rotation = (app.environment_rotation
                    + direction * 5.0_f32.to_radians())
                .rem_euclid(std::f32::consts::TAU);
                false
            }
            Self::DirectionalLight => {
                app.global_light_intensity = step(app.global_light_intensity, 0.25, 0.0, 64.0);
                false
            }
            Self::PointLight => {
                app.point_light_intensity = step(app.point_light_intensity, 5.0, 0.0, 10_000.0);
                false
            }
            Self::SpotLight => {
                app.spot_light_intensity = step(app.spot_light_intensity, 5.0, 0.0, 10_000.0);
                false
            }
            Self::MaxFps => {
                let fps = (i32::from(app.frame_pacing.max_fps.get()) + (direction * 15.0) as i32)
                    .clamp(1, 1000) as u16;
                app.frame_pacing.max_fps = NonZeroU16::new(fps).expect("minimum FPS is nonzero");
                false
            }
            Self::CameraSpeed => {
                app.camera.speed_multiplier = step(
                    app.camera.speed_multiplier,
                    0.1,
                    CAMERA_MIN_SPEED_MULTIPLIER,
                    20.0,
                );
                false
            }
        }
    }

    fn reset(self, app: &mut PbrSmoke) -> bool {
        match self {
            Self::Contrast => {
                app.tone_mapping.contrast = ToneMappingSettings::default().contrast;
                true
            }
            Self::ExposureKey => {
                app.tone_mapping.key_luminance = ToneMappingSettings::default().key_luminance;
                true
            }
            Self::ExposureMinimum => {
                app.tone_mapping.min_exposure = ToneMappingSettings::default().min_exposure;
                true
            }
            Self::ExposureMaximum => {
                app.tone_mapping.max_exposure = ToneMappingSettings::default().max_exposure;
                true
            }
            Self::ExposureAdaptation => {
                app.tone_mapping.adaptation_seconds =
                    ToneMappingSettings::default().adaptation_seconds;
                true
            }
            Self::BloomIntensity => {
                app.bloom_settings.intensity = BloomSettings::default().intensity;
                app.bloom
            }
            Self::BloomRadius => {
                app.bloom_settings.radius_uv = BloomSettings::default().radius_uv;
                app.bloom
            }
            Self::SsrDistance => {
                app.ssr_settings.max_distance = SsrSettings::default().max_distance;
                app.ssr
            }
            Self::SsrThickness => {
                app.ssr_settings.thickness = SsrSettings::default().thickness;
                app.ssr
            }
            Self::SsrSteps => {
                app.ssr_settings.max_steps = SsrSettings::default().max_steps;
                app.ssr
            }
            Self::SsrRoughness => {
                app.ssr_settings.max_roughness = SsrSettings::default().max_roughness;
                app.ssr
            }
            Self::SsrIntensity => {
                app.ssr_settings.intensity = SsrSettings::default().intensity;
                app.ssr
            }
            Self::SsrEdgeFade => {
                app.ssr_settings.edge_fade_pixels = SsrSettings::default().edge_fade_pixels;
                app.ssr
            }
            Self::GodRaysExtinction => {
                app.god_rays_settings.extinction_per_unit =
                    GodRaysSettings::default().extinction_per_unit;
                app.god_rays
            }
            Self::GodRaysAlbedo => {
                app.god_rays_settings.scattering_albedo =
                    GodRaysSettings::default().scattering_albedo;
                app.god_rays
            }
            Self::GodRaysAnisotropy => {
                app.god_rays_settings.anisotropy = GodRaysSettings::default().anisotropy;
                app.god_rays
            }
            Self::GodRaysSamples => {
                app.god_rays_settings.sample_count = GodRaysSettings::default().sample_count;
                app.god_rays
            }
            Self::GodRaysInterleave => {
                app.god_rays_settings.interleave_block =
                    GodRaysSettings::default().interleave_block;
                app.god_rays
            }
            Self::ShadowAngularRadius => {
                app.shadow_settings.angular_radius_radians =
                    DEFAULT_SHADOW_ANGULAR_RADIUS_DEGREES.to_radians();
                false
            }
            Self::ShadowPenumbra => {
                app.shadow_settings.max_penumbra_world = DEFAULT_SHADOW_MAX_PENUMBRA_WORLD;
                false
            }
            Self::EnvironmentIntensity => {
                app.environment_intensity = DEFAULT_ENVIRONMENT_INTENSITY;
                false
            }
            Self::EnvironmentRotation => {
                app.environment_rotation = 0.0;
                false
            }
            Self::DirectionalLight => {
                app.global_light_intensity = DEFAULT_GLOBAL_LIGHT_SCALE;
                false
            }
            Self::PointLight => {
                app.point_light_intensity = DEFAULT_POINT_LIGHT_INTENSITY;
                false
            }
            Self::SpotLight => {
                app.spot_light_intensity = DEFAULT_SPOT_LIGHT_INTENSITY;
                false
            }
            Self::MaxFps => {
                app.frame_pacing.max_fps = FramePacing::default().max_fps;
                false
            }
            Self::CameraSpeed => {
                app.camera.speed_multiplier = DEFAULT_CAMERA_SPEED_MULTIPLIER;
                false
            }
        }
    }
}

fn setting_value(value: f32) -> String {
    if value.abs() < 0.01 {
        format!("{value:.4}")
    } else if value.abs() < 1.0 {
        format!("{value:.3}")
    } else {
        format!("{value:.2}")
    }
}

fn viewer_title(
    features: RenderFeatures,
    frame_pacing: FramePacing,
    presented_fps: f32,
    shadows: bool,
    mboit: bool,
    speed_multiplier: f32,
    global_light_intensity: f32,
    environment: bool,
    point_light: bool,
    spot_light: bool,
    control: Control,
    control_index: usize,
    value: f32,
) -> String {
    format!(
        "X173GE [{}/{}] {}={} | FPS {:.1} cap {} VSync F12:{} | AO F2:{} SSR F3:{} Bloom F4:{} Rays F5:{} SMAA F6:{} Shadow F7:{} MBOIT F8:{} Point F9:{} Spot F10:{} HDRI F11:{} | light {:.2}x speed {:.2}x | Tab select Up/Down adjust Home reset",
        control_index + 1,
        Control::count(),
        control.name(),
        setting_value(value),
        presented_fps,
        frame_pacing.max_fps,
        if frame_pacing.vsync { "ON" } else { "OFF" },
        if features.gtao { "ON" } else { "OFF" },
        if features.ssr.is_some() { "ON" } else { "OFF" },
        if features.bloom.is_some() {
            "ON"
        } else {
            "OFF"
        },
        if features.god_rays.is_some() {
            "ON"
        } else {
            "OFF"
        },
        if features.smaa { "ON" } else { "OFF" },
        if shadows { "ON" } else { "OFF" },
        if mboit { "ON" } else { "OFF" },
        if point_light { "ON" } else { "OFF" },
        if spot_light { "ON" } else { "OFF" },
        if environment { "ON" } else { "OFF" },
        global_light_intensity,
        speed_multiplier,
    )
}

fn camera_clip_range(
    position: Vec3,
    scene_radius: f32,
    direction: Vec3,
    r1_benchmark_view: bool,
) -> (f32, f32) {
    let scene_radius = scene_radius.max(if r1_benchmark_view {
        1.0
    } else {
        CAMERA_MIN_RADIUS
    });
    if r1_benchmark_view {
        let near = (scene_radius * 0.00035).clamp(0.06, 0.16);
        let far = ((-position).dot(direction) + scene_radius * 1.35 + 32.0).clamp(32.0, 5000.0);
        return (near, far.max(near + 1.0));
    }
    let near = (scene_radius * CAMERA_NEAR_PER_RADIUS).min(CAMERA_NEAR_MAX);
    let far = position.length() + scene_radius * (1.0 + CAMERA_FAR_MARGIN_PER_RADIUS);
    (near, far.max(near * 2.0))
}

#[cfg(test)]
mod camera_tests {
    use super::*;

    #[test]
    fn camera_starts_at_saved_viewpoint() {
        let camera = FreeCamera::new(1.0);
        assert_eq!(camera.position, Vec3::from_array(DEFAULT_CAMERA_POSITION));
        assert!((camera.yaw.to_degrees() - DEFAULT_CAMERA_YAW_DEGREES).abs() < 1e-4);
        assert!((camera.pitch.to_degrees() - DEFAULT_CAMERA_PITCH_DEGREES).abs() < 1e-4);
        assert_eq!(camera.speed_multiplier, DEFAULT_CAMERA_SPEED_MULTIPLIER);
    }

    #[test]
    fn diagonal_motion_is_normalized_and_pitch_stays_usable() {
        let direction = movement_direction(-Vec3::Z, 1.0, 1.0, 0.0);
        assert!((direction.length() - 1.0).abs() < 1e-6);
        let uphill_look = Vec3::new(0.7, 0.7, -0.1).normalize();
        assert_eq!(movement_direction(uphill_look, 1.0, 0.0, 0.0).y, 0.0);
        let mut camera = FreeCamera::new(1.0);
        camera.rotate([0.0, -100_000.0]);
        assert!(camera.pitch < 1.56);
    }

    #[test]
    fn wheel_speed_is_bounded_in_both_directions() {
        assert_eq!(scroll_speed(20.0, 4.0), 20.0);
        assert_eq!(
            scroll_speed(CAMERA_MIN_SPEED_MULTIPLIER, -4.0),
            CAMERA_MIN_SPEED_MULTIPLIER
        );
        let minimum = (0..64).fold(1.0, |speed, _| scroll_speed(speed, -1.0));
        assert_eq!(minimum, CAMERA_MIN_SPEED_MULTIPLIER);
        assert!(CAMERA_SPEED_PER_RADIUS * 310.426 * minimum / 60.0 < CAMERA_NEAR_MAX);
    }

    #[test]
    fn clip_range_tracks_model_scale_and_camera_distance() {
        let camera = FreeCamera::new(2.0);
        let (near, far) = camera_clip_range(
            camera.position,
            camera.scene_radius,
            camera.direction(),
            false,
        );
        assert!((near - 0.001).abs() < 1e-6);
        assert!(far > camera.position.length() + camera.scene_radius);
        assert!(
            (camera_clip_range(Vec3::new(0.0, 0.0, 900.0), 310.426, -Vec3::Z, false,).0 - 0.001)
                .abs()
                < 1e-6
        );
        let projection = perspective_rh_reverse_z(50_f32.to_radians(), 16.0 / 9.0, near, far);
        let near_depth = projection.project_point3(Vec3::new(0.0, 0.0, -near)).z;
        let far_depth = projection.project_point3(Vec3::new(0.0, 0.0, -far)).z;
        assert!((near_depth - 1.0).abs() < 1e-5);
        assert!(far_depth.abs() < 1e-5);
    }

    #[test]
    fn r1_benchmark_view_matches_reference_framing_and_clip_rules() {
        let camera = FreeCamera::r1_benchmark_view(2.0);
        assert_eq!(camera.position, Vec3::new(0.0, 0.48, 4.9));
        assert!((camera.fov_y() - 60.0).abs() < f32::EPSILON);
        let (near, far) = camera_clip_range(
            camera.position,
            camera.scene_radius,
            camera.direction(),
            true,
        );
        assert!((near - 0.06).abs() < f32::EPSILON);
        assert!((far - (camera.position.length() + 2.0 * 1.35 + 32.0)).abs() < 1e-5);
    }

    #[test]
    fn builtin_environment_path_resolves_from_the_crate_root() {
        assert!(default_environment_path().is_file());
    }
}

struct DrawAsset {
    mesh_id: MeshId,
    material_id: MaterialId,
    model_matrix: [[f32; 4]; 4],
    mesh: MeshData,
}

fn review_items(assets: &[DrawAsset]) -> (Vec<RenderItem>, f32) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for asset in assets {
        let transform = Mat4::from_cols_array_2d(&asset.model_matrix);
        for vertex in asset.mesh.vertices() {
            let point = transform.transform_point3(Vec3::from_array(vertex.position));
            min = min.min(point);
            max = max.max(point);
        }
    }
    let center = (min + max) * 0.5;
    let radius = ((max - min) * 0.5).length().max(CAMERA_MIN_RADIUS);
    let recenter = Mat4::from_translation(-center);
    let items = assets
        .iter()
        .map(|asset| RenderItem {
            mesh: asset.mesh_id,
            material: asset.material_id,
            model_matrix: (recenter * Mat4::from_cols_array_2d(&asset.model_matrix))
                .to_cols_array_2d(),
        })
        .collect();
    (items, radius)
}

fn default_environment_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/assets/aloe_farm_shade_house_1k.hdr")
}

enum PendingUpload {
    ImageSource(x173ge::assets::GltfImage, ImageId),
    Image(ImageUpload),
    Environment(EnvironmentUpload),
    Mesh(MeshUpload),
    Material(MaterialUpload),
}

enum UploadError {
    Send(SendError),
    Decode(x173ge::assets::ImportError),
}

impl PendingUpload {
    fn send(self, renderer: &RendererHandle) -> Result<(), UploadError> {
        match self {
            Self::ImageSource(image, id) => {
                #[cfg(feature = "profiling")]
                let started = Instant::now();
                let image = image.upload(id).map_err(UploadError::Decode)?;
                #[cfg(feature = "profiling")]
                eprintln!(
                    "X173GE_UPLOAD stage=image_decode bytes={} elapsed_ms={:.3}",
                    image.rgba8.len(),
                    started.elapsed().as_secs_f64() * 1000.0
                );
                renderer.try_upload_image(image).map_err(UploadError::Send)
            }
            Self::Image(image) => renderer.try_upload_image(image).map_err(UploadError::Send),
            Self::Environment(environment) => renderer
                .try_upload_environment(environment)
                .map_err(UploadError::Send),
            Self::Mesh(mesh) => renderer.try_upload_mesh(mesh).map_err(UploadError::Send),
            Self::Material(material) => renderer
                .try_upload_material(material)
                .map_err(UploadError::Send),
        }
    }
}

impl From<RendererCommand> for PendingUpload {
    fn from(command: RendererCommand) -> Self {
        match command {
            RendererCommand::UploadImage(image) => Self::Image(image),
            RendererCommand::UploadEnvironment(environment) => Self::Environment(environment),
            RendererCommand::UploadMesh(mesh) => Self::Mesh(mesh),
            RendererCommand::UploadMaterial(material) => Self::Material(material),
            _ => unreachable!("only asset upload commands are retried"),
        }
    }
}

impl PbrSmoke {
    fn active_render_features(&self) -> RenderFeatures {
        RenderFeatures {
            smaa: self.smaa,
            gtao: self.gtao,
            ssr: self.ssr.then_some(self.ssr_settings),
            bloom: self.bloom.then_some(self.bloom_settings),
            god_rays: self.god_rays.then_some(self.god_rays_settings),
            translucent_shadows: self.mboit,
            tone_mapping: self.tone_mapping,
        }
    }

    fn current_control(&self) -> Control {
        Control::controls()
            .nth(self.tuning_index)
            .expect("selected control is within the available control list")
    }

    fn update_title(&self) {
        if let Some(window) = &self.window {
            let control = self.current_control();
            window.set_title(&viewer_title(
                self.active_render_features(),
                self.frame_pacing,
                self.presented_fps,
                self.shadows,
                self.mboit,
                self.camera.speed_multiplier,
                self.global_light_intensity,
                self.environment_enabled,
                self.point_light_enabled,
                self.spot_light_enabled,
                control,
                self.tuning_index,
                control.value(self),
            ));
        }
    }

    fn record_presented_frame(&mut self) {
        self.fps_sample_frames = self.fps_sample_frames.saturating_add(1);
        let elapsed = self.fps_sample_started.elapsed();
        if elapsed >= Duration::from_millis(500) {
            self.presented_fps = self.fps_sample_frames as f32 / elapsed.as_secs_f32();
            self.fps_sample_frames = 0;
            self.fps_sample_started = Instant::now();
            if self.interactive {
                self.update_title();
            }
        }
    }

    fn pump_render_features(&mut self) {
        let (Some(features), Some(renderer)) =
            (self.pending_render_features, self.renderer.clone())
        else {
            return;
        };
        match renderer.try_set_render_features(features) {
            Ok(()) => self.pending_render_features = None,
            Err(SendError::Full(RendererCommand::SetRenderFeatures(features))) => {
                self.pending_render_features = Some(features);
            }
            Err(SendError::Stopped(_)) => {
                self.pending_render_features = None;
                eprintln!("X173GE_RENDER_FEATURES queue_stopped=true");
            }
            Err(error) => {
                self.pending_render_features = None;
                eprintln!("X173GE_RENDER_FEATURES queue_error={error:?}");
            }
        }
    }

    fn assets_ready(&self) -> bool {
        self.pending_uploads.is_empty()
            && (self.environment_id.is_none() || self.environment_ready)
            && self.draw_items.iter().all(|item| {
                self.ready_meshes.contains(&item.mesh)
                    && self.ready_materials.contains(&item.material)
            })
    }

    fn pump_uploads(&mut self) {
        let Some(renderer) = self.renderer.clone() else {
            return;
        };
        while let Some(upload) = self.pending_uploads.pop_front() {
            match upload.send(&renderer) {
                Ok(()) => {}
                Err(UploadError::Send(SendError::Full(command) | SendError::NotReady(command))) => {
                    self.pending_uploads.push_front(command.into());
                    break;
                }
                Err(UploadError::Send(SendError::Stopped(_))) => {
                    self.upload_error = Some("renderer stopped during asset upload".into());
                    break;
                }
                Err(UploadError::Send(SendError::ShutdownManaged(_))) => {
                    unreachable!("asset uploads cannot shut down the renderer")
                }
                Err(UploadError::Decode(error)) => {
                    self.upload_error = Some(error.to_string());
                    break;
                }
            }
        }
    }
}

impl Drop for PbrSmoke {
    fn drop(&mut self) {
        if !self.interactive {
            return;
        }
        eprintln!(
            "X173GE_SETTINGS_FINAL effects: smaa={} gtao={} ssr_enabled={} ssr={:?} bloom_enabled={} bloom={:?} god_rays_enabled={} god_rays={:?} shadows={} mboit={}",
            self.smaa,
            self.gtao,
            self.ssr,
            self.ssr_settings,
            self.bloom,
            self.bloom_settings,
            self.god_rays,
            self.god_rays_settings,
            self.shadows,
            self.mboit,
        );
        eprintln!(
            "X173GE_SETTINGS_FINAL lighting: global_directional_scale={} point_enabled={} point_intensity={} spot_enabled={} spot_intensity={} hdr_available={} hdr_diffuse_enabled={} hdr_diffuse_intensity={} hdr_rotation_degrees={} shadow={:?}",
            setting_value(self.global_light_intensity),
            self.point_light_enabled,
            setting_value(self.point_light_intensity),
            self.spot_light_enabled,
            setting_value(self.spot_light_intensity),
            self.environment_id.is_some(),
            self.environment_enabled,
            setting_value(self.environment_intensity),
            setting_value(self.environment_rotation.to_degrees()),
            self.shadow_settings,
        );
        eprintln!(
            "X173GE_SETTINGS_FINAL tone_mapping={:?} max_fps={} vsync={} camera_speed={} camera_position={:?} camera_yaw_degrees={} camera_pitch_degrees={} selected={}:{}",
            self.tone_mapping,
            self.frame_pacing.max_fps,
            self.frame_pacing.vsync,
            setting_value(self.camera.speed_multiplier),
            self.camera.position.to_array(),
            setting_value(self.camera.yaw.to_degrees()),
            setting_value(self.camera.pitch.to_degrees()),
            self.current_control().name(),
            setting_value(self.current_control().value(self)),
        );
    }
}

impl RenderExtract<World> for PbrSmoke {
    type Error = String;

    fn extract(&self, _: &World, frame: ExtractFrame) -> Result<FrameSnapshot, Self::Error> {
        if let Some(error) = &self.upload_error {
            return Err(error.clone());
        }
        let position = self.camera.position;
        let direction = self.camera.direction();
        let (near, far) = camera_clip_range(
            position,
            self.camera.scene_radius,
            direction,
            self.camera.r1_benchmark_view,
        );
        let view = Mat4::look_at_rh(position, position + direction, Vec3::Y);
        let projection = perspective_rh_reverse_z(
            self.camera.fov_y().to_radians(),
            frame.extent.width() as f32 / frame.extent.height() as f32,
            near,
            far,
        );
        let mut punctual_lights = self.punctual_lights.clone();
        if self.point_light_enabled {
            punctual_lights.push(PunctualLight::Point {
                position: [0.5, 0.5, 1.0],
                color: [0.55, 0.7, 1.0],
                intensity: self.point_light_intensity,
                range: Some(4.0),
            });
        }
        if self.spot_light_enabled {
            punctual_lights.push(PunctualLight::Spot {
                position: [0.2, 0.6, 1.5],
                direction: [-0.128, -0.384, -0.916],
                color: [1.0, 0.75, 0.45],
                intensity: self.spot_light_intensity,
                range: Some(5.0),
                inner_cone_angle: 0.15,
                outer_cone_angle: 0.55,
            });
        }
        Ok(FrameSnapshot {
            id: frame.id,
            surface: frame.surface,
            extent: frame.extent,
            clear_color: [0.025, 0.045, 0.09, 1.0],
            ambient_diffuse: self.ambient_diffuse,
            ambient_diffuse_y: self.ambient_diffuse_y,
            environment: (self.environment_ready && self.environment_enabled).then(|| {
                EnvironmentSettings {
                    id: self.environment_id.expect("ready environment has an ID"),
                    intensity: self.environment_intensity,
                    rotation_y_radians: self.environment_rotation,
                }
            }),
            camera: CameraData {
                view_projection: (projection * view).to_cols_array_2d(),
                position: position.to_array(),
                projection: CameraProjection::Perspective { near, far },
            },
            directional_lights: self
                .directional_lights
                .iter()
                .copied()
                .enumerate()
                .map(|(index, mut light)| {
                    light.shadow = (index == 0 && self.shadows).then_some(self.shadow_settings);
                    light.radiance = light
                        .radiance
                        .map(|radiance| radiance * self.global_light_intensity);
                    light
                })
                .collect(),
            punctual_lights,
            render_items: self
                .draw_items
                .iter()
                .filter(|item| {
                    self.ready_meshes.contains(&item.mesh)
                        && self.ready_materials.contains(&item.material)
                })
                .copied()
                .collect(),
        })
    }
}

impl GameDefinition for PbrSmoke {
    /// Allocates the configured effects plus every compiled option for interactive toggles.
    fn render_features(&self) -> RenderFeatures {
        if !self.interactive {
            return self.active_render_features();
        }
        RenderFeatures {
            smaa: cfg!(feature = "smaa"),
            gtao: cfg!(feature = "gtao"),
            ssr: cfg!(feature = "ssr").then_some(self.ssr_settings),
            bloom: cfg!(feature = "bloom").then_some(self.bloom_settings),
            god_rays: cfg!(feature = "god-rays").then_some(self.god_rays_settings),
            translucent_shadows: cfg!(feature = "translucent-shadows"),
            tone_mapping: self.tone_mapping,
        }
    }

    fn initial_render_features(&self) -> RenderFeatures {
        self.active_render_features()
    }

    fn frame_pacing(&self) -> FramePacing {
        self.frame_pacing
    }

    fn can_submit_frame(&self) -> bool {
        self.pending_render_features.is_none()
    }

    fn workload(&mut self) -> Workload {
        Workload::new("pbr-smoke")
    }

    fn on_window(&mut self, window: &Arc<Window>) {
        self.window = Some(Arc::clone(window));
        self.capture_cursor(window, true);
    }

    fn on_renderer_ready(&mut self, renderer: &RendererHandle) {
        self.renderer = Some(renderer.clone());
        self.asset_upload_started = Some(Instant::now());
        self.pump_render_features();
        self.pump_uploads();
    }

    fn on_event(&mut self, _: &World, event: PlatformEvent, _: &x173ge::platform::InputState) {
        if event
            == (PlatformEvent::Resized {
                width: 800,
                height: 600,
            })
        {
            self.resize_event_seen = true;
        }
        match event {
            PlatformEvent::MouseMotion { delta } if self.cursor_captured => {
                self.mouse_delta[0] += delta[0];
                self.mouse_delta[1] += delta[1];
            }
            PlatformEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            } => self.capture_cursor_from_window(true),
            PlatformEvent::Key {
                code: KeyCode::Escape,
                pressed: true,
                repeat: false,
            } => self.capture_cursor_from_window(false),
            PlatformEvent::Focused(false) => self.capture_cursor_from_window(false),
            _ => {}
        }
    }

    fn before_fixed_update(
        &mut self,
        _: &World,
        time: x173ge::FixedTime,
        input: &x173ge::platform::InputState,
    ) {
        self.camera
            .update(input, self.mouse_delta, time.delta(), self.cursor_captured);
        let increase = input.key_pressed(KeyCode::PageUp) || input.key_pressed(KeyCode::NumpadAdd);
        let decrease =
            input.key_pressed(KeyCode::PageDown) || input.key_pressed(KeyCode::NumpadSubtract);
        let light_delta = if increase {
            0.25
        } else if decrease {
            -0.25
        } else {
            0.0
        };
        let mut feature_changed = false;
        let mut title_changed = light_delta != 0.0 || input.wheel_delta() != 0.0;
        if self.interactive {
            if input.key_pressed(KeyCode::Tab) {
                self.tuning_index = (self.tuning_index + 1) % Control::count();
                title_changed = true;
            }
            if input.key_pressed(KeyCode::Backspace) {
                let count = Control::count();
                self.tuning_index = (self.tuning_index + count - 1) % count;
                title_changed = true;
            }
            let adjust = if input.key_pressed(KeyCode::ArrowDown) {
                -1.0
            } else if input.key_pressed(KeyCode::ArrowUp) {
                1.0
            } else {
                0.0
            };
            if adjust != 0.0 {
                let control = self.current_control();
                feature_changed |= control.adjust(self, adjust);
                eprintln!(
                    "X173GE_SETTING name={} value={}",
                    control.name(),
                    setting_value(control.value(self))
                );
                title_changed = true;
            }
            if input.key_pressed(KeyCode::Home) {
                let control = self.current_control();
                feature_changed |= control.reset(self);
                eprintln!(
                    "X173GE_SETTING_RESET name={} value={}",
                    control.name(),
                    setting_value(control.value(self))
                );
                title_changed = true;
            }
            for (key, name, compiled, enabled) in [
                (KeyCode::F2, "GTAO", cfg!(feature = "gtao"), &mut self.gtao),
                (KeyCode::F3, "SSR", cfg!(feature = "ssr"), &mut self.ssr),
                (
                    KeyCode::F4,
                    "Bloom",
                    cfg!(feature = "bloom"),
                    &mut self.bloom,
                ),
                (
                    KeyCode::F5,
                    "God Rays",
                    cfg!(feature = "god-rays"),
                    &mut self.god_rays,
                ),
                (KeyCode::F6, "SMAA", cfg!(feature = "smaa"), &mut self.smaa),
            ] {
                if input.key_pressed(key) {
                    if compiled {
                        *enabled = !*enabled;
                        eprintln!("X173GE_RENDER_FEATURE name={name} enabled={}", *enabled);
                        feature_changed = true;
                    } else {
                        eprintln!("X173GE_RENDER_FEATURE name={name} available=false");
                    }
                }
            }
            if input.key_pressed(KeyCode::F7) {
                self.shadows = !self.shadows;
                if !self.shadows {
                    self.mboit = false;
                }
                eprintln!(
                    "X173GE_RENDER_FEATURE name=shadows enabled={} mboit={}",
                    self.shadows, self.mboit
                );
                feature_changed = true;
            }
            if input.key_pressed(KeyCode::F8) {
                if cfg!(feature = "translucent-shadows") {
                    self.mboit = !self.mboit;
                    self.shadows |= self.mboit;
                    eprintln!(
                        "X173GE_RENDER_FEATURE name=MBOIT enabled={} shadows={}",
                        self.mboit, self.shadows
                    );
                    feature_changed = true;
                } else {
                    eprintln!("X173GE_RENDER_FEATURE name=MBOIT available=false");
                }
            }
            if input.key_pressed(KeyCode::F9) {
                if self.point_light_enabled {
                    self.point_light_enabled = false;
                } else if self.punctual_lights.len() + usize::from(self.spot_light_enabled)
                    < MAX_PUNCTUAL_LIGHTS
                {
                    self.point_light_enabled = true;
                } else {
                    eprintln!("X173GE_LIGHT name=point available=false reason=light-limit");
                }
                eprintln!(
                    "X173GE_LIGHT name=point enabled={} intensity={}",
                    self.point_light_enabled, self.point_light_intensity
                );
                title_changed = true;
            }
            if input.key_pressed(KeyCode::F10) {
                if self.spot_light_enabled {
                    self.spot_light_enabled = false;
                } else if self.punctual_lights.len() + usize::from(self.point_light_enabled)
                    < MAX_PUNCTUAL_LIGHTS
                {
                    self.spot_light_enabled = true;
                } else {
                    eprintln!("X173GE_LIGHT name=spot available=false reason=light-limit");
                }
                eprintln!(
                    "X173GE_LIGHT name=spot enabled={} intensity={}",
                    self.spot_light_enabled, self.spot_light_intensity
                );
                title_changed = true;
            }
            if input.key_pressed(KeyCode::F11) {
                if self.environment_id.is_some() {
                    self.environment_enabled = !self.environment_enabled;
                    eprintln!(
                        "X173GE_ENVIRONMENT mode=diffuse_irradiance enabled={} intensity={} rotation_degrees={}",
                        self.environment_enabled,
                        setting_value(self.environment_intensity),
                        setting_value(self.environment_rotation.to_degrees())
                    );
                } else {
                    eprintln!("X173GE_ENVIRONMENT available=false reason=not-loaded");
                }
                title_changed = true;
            }
            if input.key_pressed(KeyCode::F12) {
                self.frame_pacing.vsync = !self.frame_pacing.vsync;
                eprintln!(
                    "X173GE_FRAME_PACING max_fps={} vsync={}",
                    self.frame_pacing.max_fps, self.frame_pacing.vsync
                );
                title_changed = true;
            }
            if input.key_pressed(KeyCode::F7) || input.key_pressed(KeyCode::F8) {
                title_changed = true;
            }
            if [
                KeyCode::F2,
                KeyCode::F3,
                KeyCode::F4,
                KeyCode::F5,
                KeyCode::F6,
            ]
            .into_iter()
            .any(|key| input.key_pressed(key))
            {
                title_changed = true;
            }
        }
        if feature_changed {
            self.pending_render_features = Some(self.active_render_features());
        }
        self.pump_render_features();
        if light_delta != 0.0 {
            self.global_light_intensity =
                (self.global_light_intensity + light_delta).clamp(0.0, 64.0);
            eprintln!(
                "X173GE_LIGHT global_directional_scale={:.2}",
                self.global_light_intensity
            );
        }
        if feature_changed || light_delta != 0.0 {
            title_changed = true;
        }
        if title_changed {
            self.update_title();
        }
        self.mouse_delta = [0.0; 2];
    }

    fn on_renderer_event(&mut self, event: &RendererEvent) {
        match event {
            RendererEvent::RenderFeaturesChanged(features) => {
                eprintln!("X173GE_RENDER_FEATURES_APPLIED features={features:?}");
            }
            RendererEvent::RenderFeaturesRejected { reason } => {
                eprintln!("X173GE_RENDER_FEATURES_REJECTED reason={reason}");
            }
            RendererEvent::MeshUploaded(id) => {
                self.ready_meshes.insert(*id);
            }
            RendererEvent::MaterialUploaded(id) => {
                self.ready_materials.insert(*id);
            }
            RendererEvent::EnvironmentUploaded(id) if self.environment_id == Some(*id) => {
                self.environment_ready = true;
                eprintln!("X173GE_ENVIRONMENT id={} ready=true", id.get());
            }
            RendererEvent::FramePresented(id) => {
                #[cfg(feature = "profiling")]
                {
                    self.profiled_presented_frame = None;
                }
                self.presented = self.presented.saturating_add(1);
                self.record_presented_frame();
                let assets_ready = self.assets_ready();
                if self.resized && assets_ready {
                    self.ready_frames_after_resize =
                        self.ready_frames_after_resize.saturating_add(1);
                    if self.ready_frames_after_resize == 12 {
                        eprintln!("X173GE_RUN post_resize_ready_frames=12");
                    }
                }
                #[cfg(feature = "profiling")]
                if self.frame_limit.is_none() && assets_ready {
                    self.profiled_presented_frame = Some(*id);
                }
                if let Some(limit) = self.frame_limit.filter(|_| assets_ready) {
                    self.ready_frames = self.ready_frames.saturating_add(1);
                    if self.ready_frames > self.warmup_frames && self.measured_frames < limit {
                        self.measured_frames = self.measured_frames.saturating_add(1);
                        #[cfg(feature = "profiling")]
                        {
                            self.profiled_presented_frame = Some(*id);
                        }
                        if self.measured_frames == 1 {
                            #[cfg(feature = "profiling")]
                            {
                                self.measured_started_at = Some(Instant::now());
                            }
                            eprintln!("X173GE_RUN first_measured_frame_id={}", id.get());
                        }
                        if self.measured_frames == limit {
                            #[cfg(feature = "profiling")]
                            if let Some(started) = self.measured_started_at.take() {
                                let elapsed = started.elapsed().as_secs_f64();
                                let intervals = self.measured_frames.saturating_sub(1);
                                let fps = if elapsed > 0.0 {
                                    f64::from(intervals) / elapsed
                                } else {
                                    0.0
                                };
                                eprintln!(
                                    "X173GE_RUN measured_present_event_ms={:.3} presented_interframe_fps={fps:.2}",
                                    elapsed * 1000.0
                                );
                            }
                            eprintln!("X173GE_RUN last_measured_frame_id={}", id.get());
                        }
                    }
                }
                if self.frame_limit.is_none() && !self.interactive && self.presented == 2 {
                    if let Some(window) = &self.window {
                        let _ = window.request_inner_size(PhysicalSize::new(800, 600));
                    }
                }
            }
            RendererEvent::SurfaceConfigured(token)
                if token.generation.get() > 1 && self.resize_event_seen =>
            {
                self.resized = true;
                eprintln!("X173GE_RUN resize_generation={}", token.generation.get());
            }
            _ => {}
        }
        if self.asset_upload_started.is_some() && self.assets_ready() {
            let elapsed = self
                .asset_upload_started
                .take()
                .expect("checked above")
                .elapsed()
                .as_secs_f64()
                * 1000.0;
            eprintln!("X173GE_ASSET upload_ms={elapsed:.3}");
        }
        self.pump_uploads();
    }

    fn should_exit(&self) -> bool {
        self.frame_limit.map_or(
            !self.interactive && self.assets_ready() && self.ready_frames_after_resize >= 12,
            |limit| self.measured_frames >= limit,
        )
    }

    #[cfg(feature = "profiling")]
    fn profile_cpu_frame(&self, frame: FrameId) -> bool {
        self.profiled_presented_frame == Some(frame)
    }
}

impl PbrSmoke {
    fn capture_cursor_from_window(&mut self, capture: bool) {
        if let Some(window) = self.window.clone() {
            self.capture_cursor(&window, capture);
        }
    }

    fn capture_cursor(&mut self, window: &Window, capture: bool) {
        if capture {
            self.cursor_captured = window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
                .is_ok();
            window.set_cursor_visible(!self.cursor_captured);
            self.mouse_delta = [0.0; 2];
            if !self.cursor_captured {
                eprintln!("X173GE_INPUT cursor capture failed; left-click to retry");
            }
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.cursor_captured = false;
            self.mouse_delta = [0.0; 2];
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let has_flag = |flag: &str| args.iter().any(|argument| argument == flag);
    let shadows = !has_flag("--no-shadow");
    let r1_benchmark_view = has_flag("--r1-benchmark-camera");
    let request_mboit = has_flag("--mboit");
    let mboit = cfg!(feature = "translucent-shadows") && shadows && !has_flag("--no-mboit");
    let test_point =
        !has_flag("--no-point-light") && (has_flag("--point-light") || !r1_benchmark_view);
    let test_spot =
        !has_flag("--no-spot-light") && (has_flag("--spot-light") || !r1_benchmark_view);
    if args
        .last()
        .is_some_and(|argument| argument == "--global-light-intensity")
    {
        return Err("--global-light-intensity requires a value from 0 to 64".into());
    }
    let global_light_intensity = args
        .windows(2)
        .find(|pair| pair[0] == "--global-light-intensity")
        .map(|pair| pair[1].parse::<f32>())
        .transpose()?
        .unwrap_or(DEFAULT_GLOBAL_LIGHT_SCALE);
    if !global_light_intensity.is_finite() || !(0.0..=64.0).contains(&global_light_intensity) {
        return Err("--global-light-intensity must be finite and between 0 and 64".into());
    }
    for (feature, flag, compiled) in [
        ("gtao", "--gtao", cfg!(feature = "gtao")),
        ("ssr", "--ssr", cfg!(feature = "ssr")),
        ("bloom", "--bloom", cfg!(feature = "bloom")),
        ("god-rays", "--god-rays", cfg!(feature = "god-rays")),
    ] {
        if !compiled && args.iter().any(|argument| argument == flag) {
            return Err(format!("{flag} requires the `{feature}` Cargo feature").into());
        }
    }
    if request_mboit && !cfg!(feature = "translucent-shadows") {
        return Err("--mboit requires the `translucent-shadows` Cargo feature".into());
    }
    if request_mboit && !shadows {
        return Err("--mboit requires shadows; remove `--no-shadow`".into());
    }
    let smaa = cfg!(feature = "smaa") && !has_flag("--no-smaa");
    let gtao = cfg!(feature = "gtao") && !has_flag("--no-gtao");
    let ssr = cfg!(feature = "ssr") && !has_flag("--no-ssr");
    let bloom = cfg!(feature = "bloom") && !has_flag("--no-bloom");
    let god_rays = cfg!(feature = "god-rays") && !has_flag("--no-god-rays");
    let environment_enabled = !has_flag("--no-environment");
    let max_fps = args
        .windows(2)
        .find(|pair| pair[0] == "--max-fps")
        .map(|pair| pair[1].parse::<u16>())
        .transpose()?
        .unwrap_or(60);
    if !(1..=1000).contains(&max_fps) {
        return Err("--max-fps must be between 1 and 1000".into());
    }
    let frame_pacing = FramePacing::new(
        NonZeroU16::new(max_fps).expect("max_fps was checked as nonzero"),
        !has_flag("--no-vsync"),
    );
    let frame_limit = args
        .windows(2)
        .find(|pair| pair[0] == "--frames")
        .map(|pair| pair[1].parse::<u32>())
        .transpose()?;
    if frame_limit == Some(0) {
        return Err("--frames must be greater than zero".into());
    }
    let warmup_frames = args
        .windows(2)
        .find(|pair| pair[0] == "--warmup")
        .map(|pair| pair[1].parse::<u32>())
        .transpose()?
        .unwrap_or(0);
    if frame_limit.is_none() && warmup_frames != 0 {
        return Err("--warmup requires --frames".into());
    }
    let interactive = args.iter().any(|argument| argument == "--interactive");
    eprintln!(
        "X173GE_RUN target_frames={} warmup_frames={warmup_frames} extent=1280x720 max_fps={} vsync={} shadows={shadows} global_light_intensity={global_light_intensity:.2} mboit={mboit} point_light={test_point} spot_light={test_spot} smaa={smaa} gtao={gtao} ssr={ssr} bloom={bloom} god_rays={god_rays} environment={environment_enabled} environment_intensity={DEFAULT_ENVIRONMENT_INTENSITY:.2} r1_benchmark_view={r1_benchmark_view} interactive={interactive}",
        frame_limit.map_or_else(|| "continuous".to_owned(), |frames| frames.to_string()),
        frame_pacing.max_fps,
        frame_pacing.vsync,
    );
    let asset_path = args
        .windows(2)
        .find(|pair| pair[0] == "--asset")
        .map(|pair| PathBuf::from(&pair[1]))
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rebuild1/assets/model.glb")
        });
    let environment_path = if !environment_enabled {
        None
    } else {
        Some(
            args.windows(2)
                .find(|pair| pair[0] == "--environment")
                .map_or_else(
                    || default_environment_path(),
                    |pair| PathBuf::from(&pair[1]),
                ),
        )
    };
    if environment_path.is_none() && args.windows(2).any(|pair| pair[0] == "--environment") {
        return Err("--environment cannot be combined with --no-environment".into());
    }
    let environment_id = EnvironmentId::new(1).unwrap();
    let environment_upload = environment_path
        .as_deref()
        .map(|path| {
            let started = Instant::now();
            let environment = x173ge::assets::import_environment(path)?;
            eprintln!(
                "X173GE_ENVIRONMENT path={} bytes={} extent={}x{} import_ms={:.3}",
                path.display(),
                std::fs::metadata(path)?.len(),
                environment.extent.width(),
                environment.extent.height(),
                started.elapsed().as_secs_f64() * 1000.0
            );
            Ok::<_, Box<dyn std::error::Error>>(environment.upload(environment_id))
        })
        .transpose()?;
    let asset_started = Instant::now();
    let mut asset = x173ge::assets::import(&asset_path)?;
    eprintln!(
        "X173GE_ASSET path={} bytes={} import_ms={:.3}",
        asset_path.display(),
        std::fs::metadata(&asset_path)?.len(),
        asset_started.elapsed().as_secs_f64() * 1000.0
    );
    let scene_primitives = asset.scene_primitives()?;
    let scene_lights = asset.scene_lights()?;
    let images = std::mem::take(&mut asset.images);
    drop(asset);
    let expected_rgba8_bytes: u64 = images
        .iter()
        .map(|image| u64::from(image.extent.width()) * u64::from(image.extent.height()) * 4)
        .sum();
    eprintln!(
        "X173GE_ASSET image_count={} expected_rgba8_bytes={expected_rgba8_bytes}",
        images.len()
    );
    let image_ids: HashMap<_, _> = images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            (
                (image.image_index, image.color_space),
                ImageId::new(index as u64 + 1).unwrap(),
            )
        })
        .collect();
    let mut material_ids = HashMap::new();
    let mut material_uploads = Vec::new();
    let mut assets = Vec::new();
    for (index, primitive) in scene_primitives.into_iter().enumerate() {
        let material = primitive.material.renderer_data(&image_ids)?;
        let material_id = match material_ids.get(&primitive.material_index).copied() {
            Some(id) => id,
            None => {
                let id = MaterialId::new(material_ids.len() as u64 + 1).unwrap();
                material_ids.insert(primitive.material_index, id);
                material_uploads.push(MaterialUpload { id, data: material });
                id
            }
        };
        assets.push(DrawAsset {
            mesh_id: MeshId::new(index as u64 + 1).unwrap(),
            material_id,
            model_matrix: primitive.model_transform,
            mesh: primitive.mesh,
        });
    }
    if assets.is_empty() {
        return Err("asset has no drawable scene instances".into());
    }
    let mut directional_lights = Vec::new();
    let mut punctual_lights = Vec::new();
    for light in scene_lights {
        match light {
            x173ge::assets::GltfSceneLight::Directional { light, .. } => {
                directional_lights.push(light)
            }
            x173ge::assets::GltfSceneLight::Punctual { light, .. } => punctual_lights.push(light),
        }
    }
    let shadow_settings = DirectionalShadow {
        angular_radius_radians: DEFAULT_SHADOW_ANGULAR_RADIUS_DEGREES.to_radians(),
        max_penumbra_world: DEFAULT_SHADOW_MAX_PENUMBRA_WORLD,
    };
    let shadow = shadows.then_some(shadow_settings);
    if directional_lights.is_empty() {
        directional_lights.push(DirectionalLight {
            direction_to_light: [-0.45, 1.0, -0.25],
            radiance: [3.0, 3.0, 3.0],
            shadow,
        });
    } else {
        directional_lights[0].shadow = shadow;
    }
    if directional_lights.len() > MAX_DIRECTIONAL_LIGHTS
        || punctual_lights.len() + usize::from(test_point) + usize::from(test_spot)
            > MAX_PUNCTUAL_LIGHTS
    {
        return Err("selected scene and review lights exceed the per-frame light limits".into());
    }
    eprintln!(
        "X173GE_ASSET directional_lights={} punctual_lights={}",
        directional_lights.len(),
        punctual_lights.len() + usize::from(test_point) + usize::from(test_spot)
    );
    let (draw_items, scene_radius) = review_items(&assets);
    let camera = if r1_benchmark_view {
        FreeCamera::r1_benchmark_view(scene_radius)
    } else {
        FreeCamera::new(scene_radius)
    };
    let direction = camera.direction();
    let (near, far) = camera_clip_range(
        camera.position,
        camera.scene_radius,
        direction,
        camera.r1_benchmark_view,
    );
    eprintln!(
        "X173GE_CAMERA scene_radius={scene_radius:.3} start_position=({:.3},{:.3},{:.3}) direction=({:.4},{:.4},{:.4}) fov_y_degrees={:.1} base_speed={:.3} near={near:.4} far={far:.3}",
        camera.position.x,
        camera.position.y,
        camera.position.z,
        direction.x,
        direction.y,
        direction.z,
        camera.fov_y(),
        scene_radius * CAMERA_SPEED_PER_RADIUS,
    );
    let mut pending_uploads = VecDeque::new();
    pending_uploads.extend(environment_upload.map(PendingUpload::Environment));
    pending_uploads.extend(images.into_iter().map(|image| {
        let id = image_ids[&(image.image_index, image.color_space)];
        PendingUpload::ImageSource(image, id)
    }));
    pending_uploads.extend(assets.into_iter().map(|asset| {
        PendingUpload::Mesh(MeshUpload {
            id: asset.mesh_id,
            data: asset.mesh,
        })
    }));
    pending_uploads.extend(material_uploads.into_iter().map(PendingUpload::Material));
    let (ambient_diffuse, ambient_diffuse_y) =
        vertical_environment_irradiance([0.72, 0.80, 0.90], [0.36, 0.33, 0.29])
            .expect("valid default environment radiance");
    let tone_mapping = ToneMappingSettings::default();
    let ssr_settings = SsrSettings::default();
    let bloom_settings = BloomSettings::default();
    let god_rays_settings = GodRaysSettings::default();
    let environment_enabled = environment_path.is_some();
    let title_features = RenderFeatures {
        smaa,
        gtao,
        ssr: ssr.then_some(ssr_settings),
        bloom: bloom.then_some(bloom_settings),
        god_rays: god_rays.then_some(god_rays_settings),
        translucent_shadows: mboit,
        tone_mapping,
    };
    if interactive {
        eprintln!(
            "X173GE_CONTROLS Tab/Backspace=select Up/Down=adjust Home=reset PgUp/PgDn=directional-light wheel=camera-speed F2=GTAO F3=SSR F4=Bloom F5=GodRays F6=SMAA F7=shadows F8=MBOIT F9=point F10=spot F11=diffuse-HDRI F12=VSync"
        );
        eprintln!(
            "X173GE_SETTINGS {}",
            Control::controls()
                .map(Control::name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    run(
        WindowConfig::new(
            viewer_title(
                title_features,
                frame_pacing,
                0.0,
                shadows,
                mboit,
                1.0,
                global_light_intensity,
                environment_enabled,
                test_point,
                test_spot,
                Control::Contrast,
                0,
                1.0,
            ),
            Extent2d::new(1280, 720).unwrap(),
        ),
        PbrSmoke {
            pending_uploads,
            renderer: None,
            ready_meshes: HashSet::new(),
            ready_materials: HashSet::new(),
            environment_id: environment_path.map(|_| environment_id),
            environment_ready: false,
            environment_enabled,
            draw_items,
            directional_lights,
            global_light_intensity,
            punctual_lights,
            point_light_enabled: test_point,
            point_light_intensity: DEFAULT_POINT_LIGHT_INTENSITY,
            spot_light_enabled: test_spot,
            spot_light_intensity: DEFAULT_SPOT_LIGHT_INTENSITY,
            ambient_diffuse,
            ambient_diffuse_y,
            environment_intensity: DEFAULT_ENVIRONMENT_INTENSITY,
            environment_rotation: 0.0,
            shadow_settings,
            tone_mapping,
            ssr_settings,
            bloom_settings,
            god_rays_settings,
            frame_pacing,
            shadows,
            frame_limit,
            warmup_frames,
            ready_frames: 0,
            measured_frames: 0,
            #[cfg(feature = "profiling")]
            profiled_presented_frame: None,
            #[cfg(feature = "profiling")]
            measured_started_at: None,
            asset_upload_started: None,
            upload_error: None,
            presented: 0,
            presented_fps: 0.0,
            fps_sample_frames: 0,
            fps_sample_started: Instant::now(),
            ready_frames_after_resize: 0,
            resize_event_seen: false,
            resized: false,
            mboit,
            smaa,
            gtao,
            ssr,
            bloom,
            god_rays,
            interactive,
            tuning_index: Control::controls()
                .position(|control| matches!(control, Control::SsrSteps))
                .unwrap_or(0),
            pending_render_features: None,
            window: None,
            camera,
            mouse_delta: [0.0; 2],
            cursor_captured: false,
        },
    )?;
    Ok(())
}
