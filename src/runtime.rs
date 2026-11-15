//! The small integration loop joining input, fixed updates and rendering.

#[cfg(feature = "profiling")]
use std::collections::HashMap;
use std::{
    fmt::Display,
    io,
    num::NonZeroU16,
    time::{Duration, Instant},
};

use x173ge_core::{Extent2d, FrameId, SurfaceGeneration, SurfaceId};
use x173ge_ecs::{Workload, World};
use x173ge_platform::winit::window::Window;
use x173ge_platform::{
    InputState, PlatformError, PlatformEvent, SurfaceError, WindowApplication, WindowConfig,
    describe_surface, drawable_extent, run_windowed_with,
};
use x173ge_protocol::{
    RenderFeatures, RendererCommand, RendererEvent, RendererFault, SurfaceDescriptor, SurfaceToken,
};
use x173ge_renderer::{BackendError, RendererBackend, RendererHandle, RendererService, SendError};
use x173ge_scene::{ExtractFrame, RenderExtract};

use crate::{FixedTime, Game, GameError};

#[cfg(feature = "profiling")]
const MAX_PROFILE_SAMPLES: usize = 8192;
const MAX_RENDERER_EVENTS_PER_POLL: usize = 64;

/// Main-loop frame cap and swapchain synchronization policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FramePacing {
    /// Maximum number of frames extracted and submitted each second.
    pub max_fps: NonZeroU16,
    /// Uses FIFO presentation when enabled; otherwise prefers immediate presentation.
    pub vsync: bool,
}

impl FramePacing {
    /// Creates a frame pacing policy with a nonzero maximum frame rate.
    pub const fn new(max_fps: NonZeroU16, vsync: bool) -> Self {
        Self { max_fps, vsync }
    }

    fn interval(self) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.max_fps.get()))
    }
}

impl Default for FramePacing {
    fn default() -> Self {
        Self::new(NonZeroU16::new(60).expect("60 is nonzero"), true)
    }
}

#[cfg(feature = "profiling")]
#[derive(Default)]
struct FrameProfileCounts {
    attempted: u64,
    enqueued: u64,
    presented: u64,
    dropped: u64,
    invalidated: u64,
}

#[cfg(feature = "profiling")]
impl FrameProfileCounts {
    fn observe(&mut self, event: &RendererEvent) {
        let count = match event {
            RendererEvent::FramePresented(_) => &mut self.presented,
            RendererEvent::FrameDropped { .. } => &mut self.dropped,
            RendererEvent::SurfaceInvalidated { .. } => &mut self.invalidated,
            _ => return,
        };
        *count = count.saturating_add(1);
    }
}

/// Application hooks around Shipyard's fixed workload and render extraction.
pub trait GameDefinition: RenderExtract<World> {
    /// Registers the systems run for each fixed simulation tick.
    fn workload(&mut self) -> Workload;

    /// Declares startup resource capabilities for effects changed by renderer queue commands.
    fn render_features(&self) -> RenderFeatures {
        compiled_render_features()
    }

    /// Selects initial active effects; later changes use `RendererHandle::try_set_render_features`.
    fn initial_render_features(&self) -> RenderFeatures {
        self.render_features()
    }

    /// Sets the frame submission cap and vertical-sync policy; sampled after each update.
    fn frame_pacing(&self) -> FramePacing {
        FramePacing::default()
    }

    /// Initializes components and resources after the world is created.
    fn setup(&mut self, _world: &World) {}

    /// Observes the initialized native window before Vulkan surface creation.
    fn on_window(&mut self, _window: &std::sync::Arc<Window>) {}

    /// Receives platform input after `InputState` has been updated.
    fn on_event(&mut self, _world: &World, _event: PlatformEvent, _input: &InputState) {}

    /// Runs game-owned work immediately before the registered ECS workload.
    fn before_fixed_update(&mut self, _world: &World, _time: FixedTime, _input: &InputState) {}

    /// Pauses frame extraction while a renderer command needed by the next frame is still pending.
    fn can_submit_frame(&self) -> bool {
        true
    }

    /// Observes renderer lifecycle and frame completion events.
    fn on_renderer_event(&mut self, _event: &RendererEvent) {}

    /// Queues initial assets after the first native surface is configured.
    fn on_renderer_ready(&mut self, _renderer: &RendererHandle) {}

    /// Requests an orderly event-loop and renderer shutdown.
    fn should_exit(&self) -> bool {
        false
    }

    /// Selects presented frames included in CPU profile summaries.
    #[cfg(feature = "profiling")]
    fn profile_cpu_frame(&self, _frame: FrameId) -> bool {
        true
    }
}

fn compiled_render_features() -> RenderFeatures {
    RenderFeatures {
        smaa: cfg!(feature = "smaa"),
        ..RenderFeatures::none()
    }
}

/// Failure while starting or running the integrated game loop.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[error(transparent)]
    Game(#[from] GameError),
    #[error(transparent)]
    Surface(#[from] SurfaceError),
    #[error("renderer service could not start: {0}")]
    RendererStart(#[source] io::Error),
    #[error("renderer failed: {0}")]
    Renderer(String),
    #[error("renderer effect configuration failed: {0}")]
    RendererConfiguration(String),
    #[error("render extraction failed: {0}")]
    Extract(String),
    #[error("render extraction returned a frame with mismatched id, surface or extent")]
    SnapshotMismatch,
    #[error("render extraction returned invalid frame data: {0}")]
    SnapshotInvalid(String),
    #[error("frame id space is exhausted")]
    FrameIdExhausted,
    #[error("surface generation space is exhausted")]
    SurfaceGenerationExhausted,
}

/// Runs a game with the Vulkan backend selected by the default feature.
#[cfg(feature = "vulkan")]
pub fn run<G>(window: WindowConfig, game: G) -> Result<(), EngineError>
where
    G: GameDefinition + 'static,
    G::Error: Display,
{
    run_with_backend(window, game, || {
        x173ge_renderer_vulkan::VulkanRenderer::new()
            .map(|renderer| Box::new(renderer) as Box<dyn RendererBackend>)
            .map_err(|error| BackendError(error.to_string()))
    })
}

/// Runs a game with an explicitly selected renderer backend.
pub fn run_with_backend<G>(
    window: WindowConfig,
    mut definition: G,
    factory: impl FnOnce() -> Result<Box<dyn RendererBackend>, BackendError> + Send + 'static,
) -> Result<(), EngineError>
where
    G: GameDefinition + 'static,
    G::Error: Display,
{
    let game = Game::new(definition.workload())?;
    definition.setup(game.world());
    let frame_pacing = definition.frame_pacing();
    let runtime = EngineRuntime {
        definition,
        game,
        factory: Some(factory),
        renderer: None,
        surface_id: SurfaceId::new(1).expect("nonzero surface id"),
        generation: SurfaceGeneration::new(1).expect("nonzero generation"),
        extent: None,
        configured: None,
        pending_surface: None,
        suspended: false,
        renderer_initialized: false,
        frame_pacing,
        next_frame: 1,
        last_update: Instant::now(),
        error: None,
        #[cfg(feature = "profiling")]
        cpu_profiles: Vec::with_capacity(MAX_PROFILE_SAMPLES),
        #[cfg(feature = "profiling")]
        cpu_profiles_pending: HashMap::new(),
        #[cfg(feature = "profiling")]
        queue_full_dropped: 0,
        #[cfg(feature = "profiling")]
        profile_counts: FrameProfileCounts::default(),
    };
    let error = run_windowed_with(window, runtime, |runtime| runtime.error.take())?;
    error.map_or(Ok(()), Err)
}

struct EngineRuntime<G, F> {
    definition: G,
    game: Game,
    factory: Option<F>,
    renderer: Option<RendererService>,
    surface_id: SurfaceId,
    generation: SurfaceGeneration,
    extent: Option<Extent2d>,
    configured: Option<SurfaceToken>,
    pending_surface: Option<SurfaceDescriptor>,
    suspended: bool,
    renderer_initialized: bool,
    frame_pacing: FramePacing,
    next_frame: u64,
    last_update: Instant,
    error: Option<EngineError>,
    #[cfg(feature = "profiling")]
    cpu_profiles: Vec<[f64; 3]>,
    #[cfg(feature = "profiling")]
    cpu_profiles_pending: HashMap<FrameId, [f64; 3]>,
    #[cfg(feature = "profiling")]
    queue_full_dropped: u64,
    #[cfg(feature = "profiling")]
    profile_counts: FrameProfileCounts,
}

impl<G, F> EngineRuntime<G, F>
where
    G: GameDefinition,
    G::Error: Display,
    F: FnOnce() -> Result<Box<dyn RendererBackend>, BackendError> + Send + 'static,
{
    fn configure(
        &mut self,
        window: &x173ge_platform::winit::window::Window,
        extent: Extent2d,
        force: bool,
    ) {
        if self.extent == Some(extent) && !force {
            return;
        }
        if self.extent.is_some() || force {
            let Some(generation) = self
                .generation
                .get()
                .checked_add(1)
                .and_then(SurfaceGeneration::new)
            else {
                self.error = Some(EngineError::SurfaceGenerationExhausted);
                return;
            };
            self.generation = generation;
        }
        match describe_surface(
            window,
            self.surface_id,
            self.generation,
            extent,
            self.frame_pacing.vsync,
        ) {
            Ok(surface) => {
                self.extent = Some(extent);
                self.configured = None;
                self.pending_surface = Some(surface);
            }
            Err(error) => self.error = Some(error.into()),
        }
    }

    fn send_surface(&mut self) {
        let (Some(renderer), Some(surface)) = (&self.renderer, self.pending_surface) else {
            return;
        };
        match renderer.try_send(RendererCommand::ConfigureSurface(surface)) {
            Ok(()) => self.pending_surface = None,
            Err(SendError::Full(_)) => {}
            Err(SendError::NotReady(_)) => unreachable!("surface configuration is not an upload"),
            Err(SendError::Stopped(_)) => {
                self.error = Some(EngineError::Renderer("renderer service stopped".into()));
            }
            Err(SendError::ShutdownManaged(_)) => {
                unreachable!("surface configuration cannot shut down the renderer")
            }
        }
    }

    fn handle_renderer_event(
        &mut self,
        window: &x173ge_platform::winit::window::Window,
        event: RendererEvent,
    ) {
        self.definition.on_renderer_event(&event);
        let mut initialize_renderer = false;
        match &event {
            RendererEvent::SurfaceConfigured(token) if Some(*token) == self.current_surface() => {
                self.configured = Some(*token);
                initialize_renderer = !self.renderer_initialized;
                self.renderer_initialized = true;
            }
            RendererEvent::SurfaceInvalidated { surface, .. }
                if Some(*surface) == self.current_surface() =>
            {
                if !self.suspended {
                    if let Some(extent) = self.extent {
                        self.configure(window, extent, true);
                    }
                }
            }
            RendererEvent::Failed(fault) => {
                self.error = Some(match fault {
                    RendererFault::Configuration(message) => {
                        EngineError::RendererConfiguration(message.clone())
                    }
                    fault => EngineError::Renderer(format!("{fault:?}")),
                });
            }
            RendererEvent::FrameDropped {
                fault: RendererFault::Backend(message),
                ..
            } => {
                self.error = Some(EngineError::Renderer(message.clone()));
            }
            RendererEvent::FrameDropped {
                fault: RendererFault::UnconfiguredSurface(_),
                ..
            } => {
                self.error = Some(EngineError::Renderer(
                    "frame reached an unconfigured surface".into(),
                ));
            }
            RendererEvent::FrameDropped {
                fault: RendererFault::MissingMesh(id),
                ..
            } => {
                self.error = Some(EngineError::Renderer(format!(
                    "frame references mesh {} before its upload was processed",
                    id.get()
                )));
            }
            RendererEvent::FrameDropped {
                fault: RendererFault::MissingMaterial(id),
                ..
            } => {
                self.error = Some(EngineError::Renderer(format!(
                    "frame references material {} before its upload was processed",
                    id.get()
                )));
            }
            RendererEvent::Stopped if self.error.is_none() => {
                self.error = Some(EngineError::Renderer(
                    "renderer stopped unexpectedly".into(),
                ));
            }
            _ => {}
        }
        if initialize_renderer {
            if let Some(renderer) = &self.renderer {
                let handle = renderer.handle();
                self.definition.on_renderer_ready(&handle);
            }
        }
    }

    fn current_surface(&self) -> Option<SurfaceToken> {
        self.extent.map(|_| SurfaceToken {
            id: self.surface_id,
            generation: self.generation,
        })
    }

    fn poll_renderer(&mut self, window: &x173ge_platform::winit::window::Window) {
        for _ in 0..MAX_RENDERER_EVENTS_PER_POLL {
            let result = match self.renderer.as_ref() {
                Some(renderer) => renderer.try_recv(),
                None => break,
            };
            let event = match result {
                Ok(event) => event,
                Err(x173ge_renderer::RecvError::Empty) => break,
                Err(x173ge_renderer::RecvError::Stopped) => {
                    if self.error.is_none() {
                        self.error = Some(EngineError::Renderer(
                            "renderer event channel disconnected".into(),
                        ));
                    }
                    break;
                }
            };
            #[cfg(feature = "profiling")]
            self.profile_counts.observe(&event);
            #[cfg(feature = "profiling")]
            let completed_frame = match &event {
                RendererEvent::FramePresented(id) => Some((*id, true)),
                RendererEvent::SurfaceInvalidated { frame, .. }
                | RendererEvent::FrameDropped { frame, .. } => Some((*frame, false)),
                _ => None,
            };
            self.handle_renderer_event(window, event);
            #[cfg(feature = "profiling")]
            if let Some((id, presented)) = completed_frame {
                if let Some(sample) = self.cpu_profiles_pending.remove(&id) {
                    if presented && self.definition.profile_cpu_frame(id) {
                        if self.cpu_profiles.len() < MAX_PROFILE_SAMPLES {
                            self.cpu_profiles.push(sample);
                        }
                    }
                }
            }
        }
    }

    fn render(&mut self, interpolation: f32) -> Option<(FrameId, [f64; 2], bool)> {
        if self.suspended || !self.definition.can_submit_frame() {
            return None;
        }
        let (Some(renderer), Some(surface), Some(extent)) =
            (&self.renderer, self.current_surface(), self.extent)
        else {
            return None;
        };
        if self.configured != Some(surface) || self.pending_surface.is_some() {
            return None;
        }
        let Some(id) = FrameId::new(self.next_frame) else {
            self.error = Some(EngineError::FrameIdExhausted);
            return None;
        };
        #[cfg(feature = "profiling")]
        let extract_started = Instant::now();
        let frame = match self.definition.extract(
            self.game.world(),
            ExtractFrame {
                id,
                surface,
                extent,
                alpha: interpolation,
            },
        ) {
            Ok(frame) => frame,
            Err(error) => {
                self.error = Some(EngineError::Extract(error.to_string()));
                return None;
            }
        };
        if frame.id != id || frame.surface != surface || frame.extent != extent {
            self.error = Some(EngineError::SnapshotMismatch);
            return None;
        }
        if let Err(reason) = frame.validate() {
            self.error = Some(EngineError::SnapshotInvalid(reason.into()));
            return None;
        }
        #[cfg(feature = "profiling")]
        let extract_ms = extract_started.elapsed().as_secs_f64() * 1000.0;
        self.next_frame = match self.next_frame.checked_add(1) {
            Some(next) => next,
            None => {
                self.error = Some(EngineError::FrameIdExhausted);
                return None;
            }
        };
        #[cfg(feature = "profiling")]
        {
            self.profile_counts.attempted = self.profile_counts.attempted.saturating_add(1);
        }
        #[cfg(feature = "profiling")]
        let enqueue_started = Instant::now();
        let _dropped = match renderer.try_send(RendererCommand::SubmitFrame(frame)) {
            Ok(()) => false,
            Err(SendError::Full(_)) => true,
            Err(SendError::NotReady(_)) => unreachable!("frame submission is not an upload"),
            Err(SendError::Stopped(_)) => {
                self.error = Some(EngineError::Renderer("renderer service stopped".into()));
                return None;
            }
            Err(SendError::ShutdownManaged(_)) => {
                unreachable!("frame submission cannot shut down the renderer")
            }
        };
        #[cfg(feature = "profiling")]
        if !_dropped {
            self.profile_counts.enqueued = self.profile_counts.enqueued.saturating_add(1);
        }
        #[cfg(feature = "profiling")]
        return Some((
            id,
            [extract_ms, enqueue_started.elapsed().as_secs_f64() * 1000.0],
            _dropped,
        ));
        #[cfg(not(feature = "profiling"))]
        None
    }
}

impl<G, F> WindowApplication for EngineRuntime<G, F>
where
    G: GameDefinition + 'static,
    G::Error: Display,
    F: FnOnce() -> Result<Box<dyn RendererBackend>, BackendError> + Send + 'static,
{
    fn on_window(&mut self, window: &std::sync::Arc<x173ge_platform::winit::window::Window>) {
        self.definition.on_window(window);
        let Some(factory) = self.factory.take() else {
            return;
        };
        match RendererService::spawn_with_capabilities(
            factory,
            2,
            self.definition.render_features(),
            self.definition.initial_render_features(),
        ) {
            Ok(renderer) => self.renderer = Some(renderer),
            Err(error) => {
                self.error = Some(EngineError::RendererStart(error));
                return;
            }
        }
        let size = window.inner_size();
        match drawable_extent(size.width, size.height) {
            Some(extent) => self.configure(window, extent, false),
            None => self.suspended = true,
        }
    }

    fn on_event(
        &mut self,
        window: &std::sync::Arc<x173ge_platform::winit::window::Window>,
        event: PlatformEvent,
        input: &mut InputState,
    ) {
        self.definition.on_event(self.game.world(), event, input);
        if let PlatformEvent::Resized { width, height } = event {
            match drawable_extent(width, height) {
                Some(extent) => {
                    let force = std::mem::take(&mut self.suspended);
                    self.configure(window, extent, force);
                }
                None => self.suspended = true,
            }
        }
    }

    fn about_to_wait(
        &mut self,
        window: &std::sync::Arc<x173ge_platform::winit::window::Window>,
        input: &mut InputState,
    ) -> Option<Instant> {
        self.poll_renderer(window);
        self.send_surface();
        if self.error.is_some() {
            return None;
        }

        let now = Instant::now();
        let delta = now.saturating_duration_since(self.last_update);
        self.last_update = now;
        #[cfg(feature = "profiling")]
        let update_started = Instant::now();
        let EngineRuntime {
            definition, game, ..
        } = self;
        let schedule = match game.advance_with_input(delta, input, |world, time, input| {
            definition.before_fixed_update(world, time, input);
        }) {
            Ok(schedule) => schedule,
            Err(error) => {
                self.error = Some(error.into());
                return None;
            }
        };
        let frame_pacing = self.definition.frame_pacing();
        let vsync_changed = self.frame_pacing.vsync != frame_pacing.vsync;
        self.frame_pacing = frame_pacing;
        if vsync_changed {
            if let Some(extent) = self.extent {
                self.configure(window, extent, true);
            }
            if self.error.is_some() {
                return None;
            }
        }
        #[cfg(feature = "profiling")]
        let update_ms = update_started.elapsed().as_secs_f64() * 1000.0;
        let _render_profile = self.render(schedule.interpolation);
        #[cfg(feature = "profiling")]
        if let Some((id, [extract_ms, enqueue_ms], dropped)) = _render_profile {
            if dropped {
                self.queue_full_dropped = self.queue_full_dropped.saturating_add(1);
            } else {
                self.cpu_profiles_pending
                    .insert(id, [update_ms, extract_ms, enqueue_ms]);
            }
        }
        Some(now + self.frame_pacing.interval())
    }

    fn should_exit(&self) -> bool {
        self.error.is_some() || self.definition.should_exit()
    }

    fn on_shutdown(&mut self) {
        if let Some(renderer) = self.renderer.take() {
            #[cfg(feature = "profiling")]
            let result = renderer.shutdown_with(|event| self.profile_counts.observe(event));
            #[cfg(not(feature = "profiling"))]
            let result = renderer.shutdown();
            if let Err(error) = result {
                if self.error.is_none() {
                    self.error = Some(EngineError::Renderer(error.to_string()));
                }
            }
        }
        #[cfg(feature = "profiling")]
        report_cpu_profile(
            &self.cpu_profiles,
            self.queue_full_dropped,
            &self.profile_counts,
        );
    }
}

#[cfg(feature = "profiling")]
fn report_cpu_profile(rows: &[[f64; 3]], queue_full_dropped: u64, counts: &FrameProfileCounts) {
    if rows.is_empty() {
        eprintln!(
            "X173GE_CPU samples=0 queue_full_dropped_run={queue_full_dropped} frames_attempted_run={} frames_enqueued_run={} frames_presented_run={} frames_dropped_run={} surface_invalidated_run={}",
            counts.attempted, counts.enqueued, counts.presented, counts.dropped, counts.invalidated,
        );
        return;
    }
    let mut percentiles = [[0.0; 2]; 3];
    for column in 0..3 {
        let mut samples = rows.iter().map(|row| row[column]).collect::<Vec<_>>();
        samples.sort_by(f64::total_cmp);
        let percentile = |rank| samples[samples.len().saturating_mul(rank).div_ceil(100) - 1];
        percentiles[column] = [percentile(50), percentile(95)];
    }
    eprintln!(
        "X173GE_CPU samples={} queue_full_dropped_run={queue_full_dropped} frames_attempted_run={} frames_enqueued_run={} frames_presented_run={} frames_dropped_run={} surface_invalidated_run={} update_p50_ms={:.4} update_p95_ms={:.4} extract_p50_ms={:.4} extract_p95_ms={:.4} enqueue_p50_ms={:.4} enqueue_p95_ms={:.4}",
        rows.len(),
        counts.attempted,
        counts.enqueued,
        counts.presented,
        counts.dropped,
        counts.invalidated,
        percentiles[0][0],
        percentiles[0][1],
        percentiles[1][0],
        percentiles[1][1],
        percentiles[2][0],
        percentiles[2][1]
    );
}
