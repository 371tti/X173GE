//! Backend-neutral, bounded renderer service and Null backend.

use std::{
    collections::HashMap,
    collections::HashSet,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};

use x173ge_core::{
    EnvironmentId, Extent2d, ImageId, MaterialId, MeshId, SurfaceGeneration, SurfaceId,
};
use x173ge_protocol::{
    EnvironmentUpload, FrameSnapshot, ImageUpload, MaterialUpload, MeshUpload, RenderFeatures,
    RendererCommand, RendererEvent, RendererFault, SurfaceDescriptor,
};

/// Result of drawing a frame on the selected backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentResult {
    Presented,
    SurfaceInvalidated,
    NotReady,
}

/// Error returned by a renderer backend operation.
#[derive(Debug, thiserror::Error)]
#[error("renderer backend: {0}")]
pub struct BackendError(pub String);

/// The renderer thread owns one backend and all its resources.
pub trait RendererBackend: Send + 'static {
    /// Validates effect requests against the backend before it announces readiness.
    fn configure_features(&mut self, _: RenderFeatures) -> Result<(), BackendError> {
        Ok(())
    }

    /// Whether uploads can run before a native surface creates backend resources.
    fn uploads_ready(&self) -> bool {
        true
    }

    /// Configures or recreates a native surface.
    fn configure_surface(&mut self, surface: SurfaceDescriptor) -> Result<(), BackendError>;

    /// Draws one owned frame using the latest renderer-thread feature command.
    fn render(
        &mut self,
        frame: &FrameSnapshot,
        features: RenderFeatures,
    ) -> Result<PresentResult, BackendError>;

    /// Uploads immutable CPU mesh data into renderer-owned resources.
    fn upload_mesh(&mut self, _: MeshUpload) -> Result<(), BackendError> {
        Err(BackendError(
            "mesh upload is unsupported by this backend".into(),
        ))
    }

    /// Uploads one shared RGBA8 image into renderer-owned resources.
    fn upload_image(&mut self, _: ImageUpload) -> Result<(), BackendError> {
        Err(BackendError(
            "image upload is unsupported by this backend".into(),
        ))
    }

    /// Uploads one RGB32F environment and its diffuse irradiance coefficients.
    fn upload_environment(&mut self, _: EnvironmentUpload) -> Result<(), BackendError> {
        Err(BackendError(
            "environment upload is unsupported by this backend".into(),
        ))
    }

    /// Uploads one metallic-roughness material into renderer-owned resources.
    fn upload_material(&mut self, _: MaterialUpload) -> Result<(), BackendError> {
        Err(BackendError(
            "material upload is unsupported by this backend".into(),
        ))
    }
}

/// A clear-only backend for protocol and headless integration work.
#[derive(Default)]
pub struct NullRenderer {
    surfaces: HashMap<SurfaceId, (SurfaceGeneration, x173ge_core::Extent2d)>,
}

impl RendererBackend for NullRenderer {
    /// Records the newest extent for a configured surface.
    fn configure_surface(&mut self, surface: SurfaceDescriptor) -> Result<(), BackendError> {
        self.surfaces
            .insert(surface.token.id, (surface.token.generation, surface.extent));
        Ok(())
    }

    /// Accepts a frame only when its surface generation and extent match.
    fn render(
        &mut self,
        frame: &FrameSnapshot,
        _: RenderFeatures,
    ) -> Result<PresentResult, BackendError> {
        Ok(
            if self.surfaces.get(&frame.surface.id)
                == Some(&(frame.surface.generation, frame.extent))
            {
                PresentResult::Presented
            } else {
                PresentResult::SurfaceInvalidated
            },
        )
    }

    fn upload_mesh(&mut self, _: MeshUpload) -> Result<(), BackendError> {
        Ok(())
    }

    fn upload_material(&mut self, _: MaterialUpload) -> Result<(), BackendError> {
        Ok(())
    }

    fn upload_image(&mut self, _: ImageUpload) -> Result<(), BackendError> {
        Ok(())
    }

    fn upload_environment(&mut self, _: EnvironmentUpload) -> Result<(), BackendError> {
        Ok(())
    }
}

/// Running renderer service; sending commands is bounded and shutdown joins its thread.
pub struct RendererService {
    commands: SyncSender<RendererCommand>,
    events: Receiver<RendererEvent>,
    upload_ready: Arc<AtomicBool>,
    uploads_open: Arc<UploadGate>,
    thread: Option<JoinHandle<()>>,
}

impl RendererService {
    /// Starts a backend inside its owner thread with bounded command and event queues.
    pub fn spawn(
        factory: impl FnOnce() -> Result<Box<dyn RendererBackend>, BackendError> + Send + 'static,
        capacity: usize,
    ) -> Result<Self, std::io::Error> {
        Self::spawn_with_features(factory, capacity, RenderFeatures::none())
    }

    /// Starts a backend with effect requests validated before it becomes ready.
    pub fn spawn_with_features(
        factory: impl FnOnce() -> Result<Box<dyn RendererBackend>, BackendError> + Send + 'static,
        capacity: usize,
        features: RenderFeatures,
    ) -> Result<Self, std::io::Error> {
        Self::spawn_with_capabilities(factory, capacity, features, features)
    }

    /// Starts a backend with separate startup resources and active render effects.
    pub fn spawn_with_capabilities(
        factory: impl FnOnce() -> Result<Box<dyn RendererBackend>, BackendError> + Send + 'static,
        capacity: usize,
        capabilities: RenderFeatures,
        initial_features: RenderFeatures,
    ) -> Result<Self, std::io::Error> {
        let capacity = capacity.max(1);
        let (commands_tx, commands_rx) = mpsc::sync_channel(capacity);
        let (events_tx, events_rx) = mpsc::sync_channel(capacity);
        let upload_ready = Arc::new(AtomicBool::new(false));
        let uploads_open = Arc::new(UploadGate(AtomicUsize::new(0)));
        let thread_upload_ready = Arc::clone(&upload_ready);
        let thread = thread::Builder::new()
            .name("x173ge-renderer".into())
            .spawn(move || {
                run(
                    factory,
                    commands_rx,
                    events_tx,
                    thread_upload_ready,
                    capabilities,
                    initial_features,
                )
            })?;
        Ok(Self {
            commands: commands_tx,
            events: events_rx,
            upload_ready,
            uploads_open,
            thread: Some(thread),
        })
    }

    /// Tries to enqueue a command without blocking a window callback.
    pub fn try_send(&self, command: RendererCommand) -> Result<(), SendError> {
        if matches!(&command, RendererCommand::Shutdown) {
            return Err(SendError::ShutdownManaged(command));
        }
        if is_upload(&command) && !self.upload_ready.load(Ordering::Acquire) {
            return Err(SendError::NotReady(command));
        }
        send(&self.commands, command)
    }

    /// Queues active effect changes in order with frames and asset operations.
    pub fn try_set_render_features(&self, features: RenderFeatures) -> Result<(), SendError> {
        send(&self.commands, RendererCommand::SetRenderFeatures(features))
    }

    /// Gives the game a bounded handle for asset uploads without exposing shutdown control.
    pub fn handle(&self) -> RendererHandle {
        RendererHandle {
            commands: self.commands.clone(),
            upload_ready: Arc::clone(&self.upload_ready),
            uploads_open: Arc::clone(&self.uploads_open),
        }
    }

    /// Reads one renderer event without blocking the game thread.
    pub fn try_recv(&self) -> Result<RendererEvent, RecvError> {
        self.events.try_recv().map_err(|error| match error {
            TryRecvError::Empty => RecvError::Empty,
            TryRecvError::Disconnected => RecvError::Stopped,
        })
    }

    /// Sends shutdown, drains pending events, and joins after the backend is destroyed.
    pub fn shutdown(mut self) -> Result<(), ShutdownError> {
        self.stop(|_| {})
    }

    /// Observes pending events while the final queued commands complete during shutdown.
    pub fn shutdown_with(
        mut self,
        observe: impl FnMut(&RendererEvent),
    ) -> Result<(), ShutdownError> {
        self.stop(observe)
    }

    fn stop(&mut self, mut observe: impl FnMut(&RendererEvent)) -> Result<(), ShutdownError> {
        let mut failure = None;
        self.uploads_open.close();
        while self.thread.is_some() {
            match self.commands.try_send(RendererCommand::Shutdown) {
                Ok(()) => break,
                Err(TrySendError::Full(_)) => match self.events.recv() {
                    Ok(event) => {
                        observe(&event);
                        if let RendererEvent::Failed(fault) = event {
                            failure.get_or_insert(ShutdownError::Failed(fault));
                        }
                    }
                    Err(_) => break,
                },
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        while self.thread.is_some() {
            match self.events.recv() {
                Ok(event) => {
                    observe(&event);
                    match event {
                        RendererEvent::Stopped => break,
                        RendererEvent::Failed(fault) => {
                            failure.get_or_insert(ShutdownError::Failed(fault));
                        }
                        _ => {}
                    }
                }
                Err(_) => break,
            }
        }
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                failure.get_or_insert(ShutdownError::Panicked);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

/// Cloneable producer handle for asynchronous renderer asset uploads.
///
/// Wait for the matching uploaded event before referencing an ID from another producer.
#[derive(Clone)]
pub struct RendererHandle {
    commands: SyncSender<RendererCommand>,
    upload_ready: Arc<AtomicBool>,
    uploads_open: Arc<UploadGate>,
}

impl RendererHandle {
    /// Enqueues one mesh without blocking; a full error returns the owned command for retry.
    pub fn try_upload_mesh(&self, mesh: MeshUpload) -> Result<(), SendError> {
        self.try_upload(RendererCommand::UploadMesh(mesh))
    }

    /// Enqueues one decoded image without blocking; a full error returns it for retry.
    pub fn try_upload_image(&self, image: ImageUpload) -> Result<(), SendError> {
        self.try_upload(RendererCommand::UploadImage(image))
    }

    /// Enqueues one bounded environment map without blocking.
    pub fn try_upload_environment(&self, environment: EnvironmentUpload) -> Result<(), SendError> {
        self.try_upload(RendererCommand::UploadEnvironment(environment))
    }

    /// Enqueues one material without blocking; a full error returns the command for retry.
    pub fn try_upload_material(&self, material: MaterialUpload) -> Result<(), SendError> {
        self.try_upload(RendererCommand::UploadMaterial(material))
    }

    /// Queues active effect changes in order with frames and asset operations.
    pub fn try_set_render_features(&self, features: RenderFeatures) -> Result<(), SendError> {
        send(&self.commands, RendererCommand::SetRenderFeatures(features))
    }

    fn try_upload(&self, command: RendererCommand) -> Result<(), SendError> {
        let Some(_permit) = self.uploads_open.enter() else {
            return Err(SendError::Stopped(command));
        };
        if !self.upload_ready.load(Ordering::Acquire) {
            return Err(SendError::NotReady(command));
        }
        send(&self.commands, command)
    }
}

impl Drop for RendererService {
    /// Joins the backend thread so native surfaces outlive Vulkan resources.
    fn drop(&mut self) {
        let _ = self.stop(|_| {});
    }
}

/// Failure reported while joining the renderer service.
#[derive(Debug, thiserror::Error)]
pub enum ShutdownError {
    #[error("renderer stopped after backend failure: {0:?}")]
    Failed(RendererFault),
    #[error("renderer thread panicked")]
    Panicked,
}

/// Failure to enqueue a command into the bounded protocol.
#[derive(Debug)]
pub enum SendError {
    /// The backend needs a configured surface before it can accept this upload.
    NotReady(RendererCommand),
    Full(RendererCommand),
    Stopped(RendererCommand),
    /// Shutdown is owned by `RendererService` so accepted uploads stay ordered before it.
    ShutdownManaged(RendererCommand),
}

fn send(commands: &SyncSender<RendererCommand>, command: RendererCommand) -> Result<(), SendError> {
    commands.try_send(command).map_err(|error| match error {
        TrySendError::Full(command) => SendError::Full(command),
        TrySendError::Disconnected(command) => SendError::Stopped(command),
    })
}

fn is_upload(command: &RendererCommand) -> bool {
    matches!(
        command,
        RendererCommand::UploadMesh(_)
            | RendererCommand::UploadImage(_)
            | RendererCommand::UploadEnvironment(_)
            | RendererCommand::UploadMaterial(_)
    )
}

const UPLOAD_GATE_CLOSED: usize = 1usize << (usize::BITS - 1);
const UPLOAD_GATE_ACTIVE: usize = UPLOAD_GATE_CLOSED - 1;

/// The high bit closes admission; the remaining bits count active sends.
struct UploadGate(AtomicUsize);

struct UploadPermit<'a> {
    gate: &'a UploadGate,
}

impl UploadGate {
    fn enter(&self) -> Option<UploadPermit<'_>> {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            if state & UPLOAD_GATE_CLOSED != 0 || state & UPLOAD_GATE_ACTIVE == UPLOAD_GATE_ACTIVE {
                return None;
            }
            match self.0.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(UploadPermit { gate: self }),
                Err(current) => state = current,
            }
        }
    }

    fn close(&self) {
        self.0.fetch_or(UPLOAD_GATE_CLOSED, Ordering::AcqRel);
        while self.0.load(Ordering::Acquire) & UPLOAD_GATE_ACTIVE != 0 {
            thread::yield_now();
        }
    }
}

impl Drop for UploadPermit<'_> {
    fn drop(&mut self) {
        self.gate.0.fetch_sub(1, Ordering::Release);
    }
}

/// Failure to read an event without blocking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecvError {
    Empty,
    Stopped,
}

/// Runs a backend command loop and rejects stale surface generations centrally.
fn run(
    factory: impl FnOnce() -> Result<Box<dyn RendererBackend>, BackendError>,
    commands: Receiver<RendererCommand>,
    events: SyncSender<RendererEvent>,
    upload_ready: Arc<AtomicBool>,
    capabilities: RenderFeatures,
    mut active_features: RenderFeatures,
) {
    let mut backend = match factory() {
        Ok(backend) => backend,
        Err(error) => {
            let _ = events.send(RendererEvent::Failed(RendererFault::Backend(
                error.to_string(),
            )));
            let _ = events.send(RendererEvent::Stopped);
            return;
        }
    };
    if let Err(reason) = capabilities.validate() {
        let _ = events.send(RendererEvent::Failed(RendererFault::Configuration(
            reason.into(),
        )));
        let _ = events.send(RendererEvent::Stopped);
        return;
    }
    if let Err(reason) = active_features.validate() {
        let _ = events.send(RendererEvent::Failed(RendererFault::Configuration(
            reason.into(),
        )));
        let _ = events.send(RendererEvent::Stopped);
        return;
    }
    if !capabilities.supports(active_features) {
        let _ = events.send(RendererEvent::Failed(RendererFault::Configuration(
            "initial render features exceed startup capabilities".into(),
        )));
        let _ = events.send(RendererEvent::Stopped);
        return;
    }
    if let Err(error) = backend.configure_features(capabilities) {
        let _ = events.send(RendererEvent::Failed(RendererFault::Configuration(
            error.to_string(),
        )));
        let _ = events.send(RendererEvent::Stopped);
        return;
    }
    upload_ready.store(backend.uploads_ready(), Ordering::Release);
    let _ = events.send(RendererEvent::Ready);
    let mut surfaces = HashMap::<SurfaceId, SurfaceState>::new();
    let mut meshes = HashSet::<MeshId>::new();
    let mut images = HashSet::<ImageId>::new();
    let mut environments = HashSet::<EnvironmentId>::new();
    let mut materials = HashSet::<MaterialId>::new();

    while let Ok(command) = commands.recv() {
        match command {
            RendererCommand::ConfigureSurface(surface) => {
                if let Some(current) = surfaces.get(&surface.token.id) {
                    if surface.token.generation <= current.generation {
                        let _ = events.send(RendererEvent::Failed(RendererFault::StaleSurface {
                            received: surface.token.generation,
                            current: current.generation,
                        }));
                        break;
                    }
                }
                match backend.configure_surface(surface) {
                    Ok(()) => {
                        upload_ready.store(backend.uploads_ready(), Ordering::Release);
                        surfaces.insert(
                            surface.token.id,
                            SurfaceState {
                                generation: surface.token.generation,
                                extent: surface.extent,
                                valid: true,
                            },
                        );
                        let _ = events.send(RendererEvent::SurfaceConfigured(surface.token));
                    }
                    Err(error) => {
                        let _ = events.send(RendererEvent::Failed(RendererFault::Backend(
                            error.to_string(),
                        )));
                        break;
                    }
                }
            }
            RendererCommand::UploadMesh(mesh) => {
                let id = mesh.id;
                match backend.upload_mesh(mesh) {
                    Ok(()) => {
                        meshes.insert(id);
                        let _ = events.send(RendererEvent::MeshUploaded(id));
                    }
                    Err(error) => {
                        let _ = events.send(RendererEvent::Failed(RendererFault::Backend(
                            error.to_string(),
                        )));
                        break;
                    }
                }
            }
            RendererCommand::UploadImage(image) => {
                let id = image.id;
                if let Err(reason) = image.validate() {
                    let _ =
                        events.send(RendererEvent::Failed(RendererFault::Backend(reason.into())));
                    break;
                }
                match backend.upload_image(image) {
                    Ok(()) => {
                        images.insert(id);
                        let _ = events.send(RendererEvent::ImageUploaded(id));
                    }
                    Err(error) => {
                        let _ = events.send(RendererEvent::Failed(RendererFault::Backend(
                            error.to_string(),
                        )));
                        break;
                    }
                }
            }
            RendererCommand::UploadEnvironment(environment) => {
                let id = environment.id;
                if let Err(reason) = environment.validate() {
                    let _ =
                        events.send(RendererEvent::Failed(RendererFault::Backend(reason.into())));
                    break;
                }
                match backend.upload_environment(environment) {
                    Ok(()) => {
                        environments.insert(id);
                        let _ = events.send(RendererEvent::EnvironmentUploaded(id));
                    }
                    Err(error) => {
                        let _ = events.send(RendererEvent::Failed(RendererFault::Backend(
                            error.to_string(),
                        )));
                        break;
                    }
                }
            }
            RendererCommand::UploadMaterial(material) => {
                let id = material.id;
                if let Err(reason) = material.data.validate() {
                    let _ =
                        events.send(RendererEvent::Failed(RendererFault::Backend(reason.into())));
                    break;
                }
                if let Some(missing) = material
                    .data
                    .base_color_texture
                    .iter()
                    .chain(material.data.metallic_roughness_texture.iter())
                    .find(|texture| !images.contains(&texture.image))
                {
                    let _ = events.send(RendererEvent::Failed(RendererFault::MissingImage(
                        missing.image,
                    )));
                    break;
                }
                match backend.upload_material(material) {
                    Ok(()) => {
                        materials.insert(id);
                        let _ = events.send(RendererEvent::MaterialUploaded(id));
                    }
                    Err(error) => {
                        let _ = events.send(RendererEvent::Failed(RendererFault::Backend(
                            error.to_string(),
                        )));
                        break;
                    }
                }
            }
            RendererCommand::SetRenderFeatures(features) => {
                let rejection = features.validate().err().map(str::to_owned).or_else(|| {
                    (!capabilities.supports(features))
                        .then(|| "render features exceed startup capabilities".into())
                });
                if let Some(reason) = rejection {
                    let _ = events.send(RendererEvent::RenderFeaturesRejected { reason });
                } else {
                    active_features = features;
                    let _ = events.send(RendererEvent::RenderFeaturesChanged(features));
                }
            }
            RendererCommand::SubmitFrame(frame) => {
                let fault = frame
                    .validate()
                    .err()
                    .map(|reason| RendererFault::Backend(reason.into()))
                    .or_else(|| match surfaces.get(&frame.surface.id) {
                        Some(current)
                            if current.generation == frame.surface.generation && !current.valid =>
                        {
                            Some(RendererFault::SurfaceRequiresReconfigure(frame.surface.id))
                        }
                        Some(current) if current.generation != frame.surface.generation => {
                            Some(RendererFault::StaleSurface {
                                received: frame.surface.generation,
                                current: current.generation,
                            })
                        }
                        Some(current) if current.extent != frame.extent => {
                            Some(RendererFault::Backend(
                                "frame extent does not match configured surface".into(),
                            ))
                        }
                        Some(_) => None,
                        None => Some(RendererFault::UnconfiguredSurface(frame.surface.id)),
                    })
                    .or_else(|| {
                        frame
                            .render_items
                            .iter()
                            .find(|item| !meshes.contains(&item.mesh))
                            .map(|item| RendererFault::MissingMesh(item.mesh))
                    })
                    .or_else(|| {
                        frame
                            .render_items
                            .iter()
                            .find(|item| !materials.contains(&item.material))
                            .map(|item| RendererFault::MissingMaterial(item.material))
                    })
                    .or_else(|| {
                        frame.environment.and_then(|environment| {
                            (!environments.contains(&environment.id))
                                .then_some(RendererFault::MissingEnvironment(environment.id))
                        })
                    });
                if let Some(fault) = fault {
                    let _ = events.send(RendererEvent::FrameDropped {
                        frame: frame.id,
                        fault,
                    });
                    continue;
                }
                match backend.render(&frame, active_features) {
                    Ok(PresentResult::Presented) => {
                        let _ = events.send(RendererEvent::FramePresented(frame.id));
                    }
                    Ok(PresentResult::SurfaceInvalidated) => {
                        if let Some(surface) = surfaces.get_mut(&frame.surface.id) {
                            surface.valid = false;
                        }
                        let _ = events.send(RendererEvent::SurfaceInvalidated {
                            frame: frame.id,
                            surface: frame.surface,
                        });
                    }
                    Ok(PresentResult::NotReady) => {
                        let _ = events.send(RendererEvent::FrameDropped {
                            frame: frame.id,
                            fault: RendererFault::SurfaceNotReady,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(RendererEvent::Failed(RendererFault::Backend(
                            error.to_string(),
                        )));
                        break;
                    }
                }
            }
            RendererCommand::Shutdown => break,
        }
    }

    let _ = events.send(RendererEvent::Stopped);
}

#[derive(Clone, Copy)]
struct SurfaceState {
    generation: SurfaceGeneration,
    extent: Extent2d,
    valid: bool,
}

/// Returns the newer generation for a resize or invalidated surface.
pub fn next_generation(current: SurfaceGeneration) -> Option<SurfaceGeneration> {
    current
        .get()
        .checked_add(1)
        .and_then(SurfaceGeneration::new)
}

#[cfg(test)]
mod tests {
    use std::{
        num::NonZeroIsize,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use x173ge_core::{FrameId, ImageId, MaterialId, MeshId};
    use x173ge_protocol::{
        ImageUpload, MaterialData, MaterialUpload, MeshData, MeshUpload, NativeSurfaceHandle,
        RenderItem, SurfaceToken, TextureBinding, TextureSampler, Vertex,
    };

    struct InvalidatingBackend(Arc<AtomicUsize>);

    impl RendererBackend for InvalidatingBackend {
        fn configure_surface(&mut self, _: SurfaceDescriptor) -> Result<(), BackendError> {
            Ok(())
        }

        fn render(
            &mut self,
            _: &FrameSnapshot,
            _: RenderFeatures,
        ) -> Result<PresentResult, BackendError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(PresentResult::SurfaceInvalidated)
        }
    }

    struct CountingBackend(Arc<AtomicUsize>);

    impl RendererBackend for CountingBackend {
        fn configure_surface(&mut self, _: SurfaceDescriptor) -> Result<(), BackendError> {
            Ok(())
        }

        fn render(
            &mut self,
            _: &FrameSnapshot,
            _: RenderFeatures,
        ) -> Result<PresentResult, BackendError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(PresentResult::Presented)
        }
    }

    struct DropBackend(Arc<std::sync::atomic::AtomicBool>);

    impl RendererBackend for DropBackend {
        fn configure_surface(&mut self, _: SurfaceDescriptor) -> Result<(), BackendError> {
            Ok(())
        }

        fn render(
            &mut self,
            _: &FrameSnapshot,
            _: RenderFeatures,
        ) -> Result<PresentResult, BackendError> {
            Ok(PresentResult::Presented)
        }
    }

    struct UploadCountingBackend(Arc<AtomicUsize>);

    impl RendererBackend for UploadCountingBackend {
        fn configure_surface(&mut self, _: SurfaceDescriptor) -> Result<(), BackendError> {
            Ok(())
        }

        fn render(
            &mut self,
            _: &FrameSnapshot,
            _: RenderFeatures,
        ) -> Result<PresentResult, BackendError> {
            Ok(PresentResult::Presented)
        }

        fn upload_material(&mut self, _: MaterialUpload) -> Result<(), BackendError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    struct SurfaceGatedBackend(bool);

    impl RendererBackend for SurfaceGatedBackend {
        fn uploads_ready(&self) -> bool {
            self.0
        }

        fn configure_surface(&mut self, _: SurfaceDescriptor) -> Result<(), BackendError> {
            self.0 = true;
            Ok(())
        }

        fn render(
            &mut self,
            _: &FrameSnapshot,
            _: RenderFeatures,
        ) -> Result<PresentResult, BackendError> {
            Ok(PresentResult::Presented)
        }

        fn upload_image(&mut self, _: ImageUpload) -> Result<(), BackendError> {
            Ok(())
        }
    }

    struct FeatureRejectBackend;

    impl RendererBackend for FeatureRejectBackend {
        fn configure_features(&mut self, features: RenderFeatures) -> Result<(), BackendError> {
            Err(BackendError(format!("unsupported effects: {features:?}")))
        }

        fn configure_surface(&mut self, _: SurfaceDescriptor) -> Result<(), BackendError> {
            Ok(())
        }

        fn render(
            &mut self,
            _: &FrameSnapshot,
            _: RenderFeatures,
        ) -> Result<PresentResult, BackendError> {
            Ok(PresentResult::Presented)
        }
    }

    impl Drop for DropBackend {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    fn receive(service: &RendererService) -> RendererEvent {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match service.try_recv() {
                Ok(event) => return event,
                Err(RecvError::Stopped) => panic!("renderer stopped"),
                Err(RecvError::Empty) if Instant::now() < deadline => thread::yield_now(),
                Err(RecvError::Empty) => panic!("renderer event timed out"),
            }
        }
    }

    fn surface() -> SurfaceDescriptor {
        let id = SurfaceId::new(1).unwrap();
        SurfaceDescriptor {
            token: SurfaceToken {
                id,
                generation: SurfaceGeneration::new(1).unwrap(),
            },
            extent: Extent2d::new(64, 64).unwrap(),
            native: NativeSurfaceHandle::Win32 {
                hwnd: NonZeroIsize::new(1).unwrap(),
                hinstance: NonZeroIsize::new(1).unwrap(),
            },
            vsync: true,
        }
    }

    fn frame(id: u64) -> FrameSnapshot {
        let surface = surface();
        FrameSnapshot {
            id: FrameId::new(id).unwrap(),
            surface: surface.token,
            extent: surface.extent,
            clear_color: [0.0; 4],
            ambient_diffuse: [0.0; 3],
            ambient_diffuse_y: [0.0; 3],
            environment: None,
            camera: Default::default(),
            directional_lights: Vec::new(),
            punctual_lights: Vec::new(),
            render_items: Vec::new(),
        }
    }

    fn frame_with_item(id: u64) -> FrameSnapshot {
        let mut frame = frame(id);
        frame.render_items.push(RenderItem {
            mesh: MeshId::new(1).unwrap(),
            material: MaterialId::new(1).unwrap(),
            model_matrix: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
        });
        frame
    }

    #[test]
    fn frame_requires_an_uploaded_environment() {
        let service = RendererService::spawn(
            || Ok(Box::new(NullRenderer::default()) as Box<dyn RendererBackend>),
            2,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);
        service
            .try_send(RendererCommand::ConfigureSurface(surface()))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::SurfaceConfigured(_)
        ));

        let environment = EnvironmentId::new(1).unwrap();
        let mut frame = frame(1);
        frame.environment = Some(x173ge_protocol::EnvironmentSettings {
            id: environment,
            intensity: 1.0,
            rotation_y_radians: 0.0,
        });
        service
            .try_send(RendererCommand::SubmitFrame(frame.clone()))
            .unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::FrameDropped {
                frame: FrameId::new(1).unwrap(),
                fault: RendererFault::MissingEnvironment(environment),
            }
        );

        service
            .handle()
            .try_upload_environment(EnvironmentUpload {
                id: environment,
                diffuse_irradiance_sh: [[0.0; 3]; 9],
            })
            .unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::EnvironmentUploaded(environment)
        );
        frame.id = FrameId::new(2).unwrap();
        service
            .try_send(RendererCommand::SubmitFrame(frame))
            .unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::FramePresented(FrameId::new(2).unwrap())
        );
        service.shutdown().unwrap();
    }

    #[test]
    fn shutdown_observer_receives_events_from_queued_frames() {
        let service = RendererService::spawn(
            || Ok(Box::new(NullRenderer::default()) as Box<dyn RendererBackend>),
            2,
        )
        .unwrap();
        service
            .try_send(RendererCommand::ConfigureSurface(surface()))
            .unwrap();
        service
            .try_send(RendererCommand::SubmitFrame(frame(1)))
            .unwrap();

        let mut events = Vec::new();
        service
            .shutdown_with(|event| events.push(event.clone()))
            .unwrap();

        assert!(events.contains(&RendererEvent::FramePresented(FrameId::new(1).unwrap())));
    }

    #[test]
    fn invalidated_surface_rejects_frames_until_new_generation() {
        let rendered = Arc::new(AtomicUsize::new(0));
        let service = RendererService::spawn(
            {
                let rendered = Arc::clone(&rendered);
                move || Ok(Box::new(InvalidatingBackend(rendered)) as Box<dyn RendererBackend>)
            },
            2,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);
        service
            .try_send(RendererCommand::ConfigureSurface(surface()))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::SurfaceConfigured(_)
        ));
        service
            .try_send(RendererCommand::SubmitFrame(frame(1)))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::SurfaceInvalidated { frame, .. } if frame == FrameId::new(1).unwrap()
        ));
        service
            .try_send(RendererCommand::SubmitFrame(frame(2)))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::FrameDropped {
                fault: RendererFault::SurfaceRequiresReconfigure(_),
                ..
            }
        ));
        assert_eq!(rendered.load(Ordering::Relaxed), 1);
        service.shutdown().unwrap();
    }

    #[test]
    fn resize_rejects_old_generation_snapshot_before_backend_render() {
        let rendered = Arc::new(AtomicUsize::new(0));
        let service = RendererService::spawn(
            {
                let rendered = Arc::clone(&rendered);
                move || Ok(Box::new(CountingBackend(rendered)) as Box<dyn RendererBackend>)
            },
            2,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);

        let old_surface = surface();
        service
            .try_send(RendererCommand::ConfigureSurface(old_surface))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::SurfaceConfigured(_)
        ));
        service
            .try_send(RendererCommand::SubmitFrame(frame(1)))
            .unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::FramePresented(FrameId::new(1).unwrap())
        );

        let mut new_surface = old_surface;
        new_surface.token.generation = SurfaceGeneration::new(2).unwrap();
        new_surface.extent = Extent2d::new(128, 64).unwrap();
        service
            .try_send(RendererCommand::ConfigureSurface(new_surface))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::SurfaceConfigured(token) if token == new_surface.token
        ));

        service
            .try_send(RendererCommand::SubmitFrame(frame(2)))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::FrameDropped {
                frame,
                fault: RendererFault::StaleSurface { received, current },
            } if frame == FrameId::new(2).unwrap()
                && received == old_surface.token.generation
                && current == new_surface.token.generation
        ));
        assert_eq!(rendered.load(Ordering::Relaxed), 1);

        let mut new_frame = frame(3);
        new_frame.surface = new_surface.token;
        new_frame.extent = new_surface.extent;
        service
            .try_send(RendererCommand::SubmitFrame(new_frame))
            .unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::FramePresented(FrameId::new(3).unwrap())
        );
        assert_eq!(rendered.load(Ordering::Relaxed), 2);
        service.shutdown().unwrap();
    }

    #[test]
    fn shutdown_drains_capacity_one_factory_failure() {
        let service = RendererService::spawn(
            || Err(BackendError("expected initialization failure".into())),
            1,
        )
        .unwrap();
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Failed(RendererFault::Backend(message)))
                if message.ends_with("expected initialization failure")
        ));
    }

    #[test]
    fn requested_unavailable_effects_fail_before_ready() {
        let service = RendererService::spawn_with_features(
            || Ok(Box::new(FeatureRejectBackend) as Box<dyn RendererBackend>),
            1,
            RenderFeatures::standard(),
        )
        .unwrap();
        assert!(matches!(
            service.shutdown(),
            Err(ShutdownError::Failed(RendererFault::Configuration(message)))
                if message.contains("unsupported effects")
        ));
    }

    #[test]
    fn dropping_service_joins_backend_before_returning() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let service = RendererService::spawn(
            {
                let dropped = Arc::clone(&dropped);
                move || Ok(Box::new(DropBackend(dropped)) as Box<dyn RendererBackend>)
            },
            1,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);
        drop(service);
        assert!(dropped.load(Ordering::Relaxed));
    }

    #[test]
    fn shutdown_is_owned_and_closes_cloned_upload_handles() {
        let service = RendererService::spawn(
            || Ok(Box::new(NullRenderer::default()) as Box<dyn RendererBackend>),
            1,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);
        let handle = service.handle();
        assert!(matches!(
            service.try_send(RendererCommand::Shutdown),
            Err(SendError::ShutdownManaged(RendererCommand::Shutdown))
        ));
        let material = MaterialId::new(1).unwrap();
        handle
            .try_upload_material(MaterialUpload {
                id: material,
                data: MaterialData::default(),
            })
            .unwrap();
        assert_eq!(receive(&service), RendererEvent::MaterialUploaded(material));
        service.shutdown().unwrap();
        assert!(matches!(
            handle.try_upload_material(MaterialUpload {
                id: MaterialId::new(2).unwrap(),
                data: MaterialData::default(),
            }),
            Err(SendError::Stopped(RendererCommand::UploadMaterial(_)))
        ));
    }

    #[test]
    fn shutdown_processes_every_concurrently_accepted_upload() {
        const PRODUCERS: usize = 4;
        let processed = Arc::new(AtomicUsize::new(0));
        let service = RendererService::spawn(
            {
                let processed = Arc::clone(&processed);
                move || Ok(Box::new(UploadCountingBackend(processed)) as Box<dyn RendererBackend>)
            },
            1,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);
        let accepted = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(PRODUCERS + 1));
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|_| {
                let handle = service.handle();
                let accepted = Arc::clone(&accepted);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    loop {
                        match handle.try_upload_material(MaterialUpload {
                            id: MaterialId::new(1).unwrap(),
                            data: MaterialData::default(),
                        }) {
                            Ok(()) => {
                                accepted.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(SendError::Full(_)) => thread::yield_now(),
                            Err(SendError::Stopped(_)) => break,
                            Err(error) => panic!("unexpected renderer upload error: {error:?}"),
                        }
                    }
                })
            })
            .collect();
        start.wait();
        while accepted.load(Ordering::Relaxed) == 0 {
            thread::yield_now();
        }
        service.shutdown().unwrap();
        for producer in producers {
            producer.join().unwrap();
        }
        assert_eq!(
            processed.load(Ordering::Relaxed),
            accepted.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn frames_require_mesh_and_material_uploads_to_be_processed_first() {
        let service = RendererService::spawn(
            || Ok(Box::new(NullRenderer::default()) as Box<dyn RendererBackend>),
            2,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);
        service
            .try_send(RendererCommand::ConfigureSurface(surface()))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::SurfaceConfigured(_)
        ));
        service
            .try_send(RendererCommand::SubmitFrame(frame_with_item(1)))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::FrameDropped {
                fault: RendererFault::MissingMesh(id),
                ..
            } if id == MeshId::new(1).unwrap()
        ));
        let handle = service.handle();
        handle
            .try_upload_mesh(MeshUpload {
                id: MeshId::new(1).unwrap(),
                data: MeshData::new(
                    vec![
                        Vertex {
                            position: [0.0, 0.0, 0.0],
                            normal: [0.0, 0.0, 1.0],
                            uv_0: [0.0; 2],
                            color_0: [1.0; 4],
                            tangent: [0; 4],
                        },
                        Vertex {
                            position: [1.0, 0.0, 0.0],
                            normal: [0.0, 0.0, 1.0],
                            uv_0: [1.0, 0.0],
                            color_0: [1.0; 4],
                            tangent: [0; 4],
                        },
                        Vertex {
                            position: [0.0, 1.0, 0.0],
                            normal: [0.0, 0.0, 1.0],
                            uv_0: [0.0, 1.0],
                            color_0: [1.0; 4],
                            tangent: [0; 4],
                        },
                    ],
                    vec![0, 1, 2],
                )
                .unwrap(),
            })
            .unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::MeshUploaded(MeshId::new(1).unwrap())
        );
        service
            .try_send(RendererCommand::SubmitFrame(frame_with_item(2)))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::FrameDropped {
                fault: RendererFault::MissingMaterial(id),
                ..
            } if id == MaterialId::new(1).unwrap()
        ));
        handle
            .try_upload_material(MaterialUpload {
                id: MaterialId::new(1).unwrap(),
                data: MaterialData::default(),
            })
            .unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::MaterialUploaded(MaterialId::new(1).unwrap())
        );
        service
            .try_send(RendererCommand::SubmitFrame(frame_with_item(3)))
            .unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::FramePresented(FrameId::new(3).unwrap())
        );
        service.shutdown().unwrap();
    }

    #[test]
    fn material_upload_requires_its_texture_image_to_be_ready() {
        let service = RendererService::spawn(
            || Ok(Box::new(NullRenderer::default()) as Box<dyn RendererBackend>),
            2,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);
        let handle = service.handle();
        let image = ImageId::new(9).unwrap();
        handle
            .try_upload_image(ImageUpload {
                id: image,
                extent: Extent2d::new(1, 1).unwrap(),
                color_space: x173ge_protocol::ImageColorSpace::Linear,
                rgba8: Arc::from([255, 255, 255, 255]),
            })
            .unwrap();
        assert_eq!(receive(&service), RendererEvent::ImageUploaded(image));
        let material = MaterialId::new(3).unwrap();
        handle
            .try_upload_material(MaterialUpload {
                id: material,
                data: MaterialData {
                    base_color_texture: Some(TextureBinding {
                        image,
                        sampler: TextureSampler::default(),
                    }),
                    ..MaterialData::default()
                },
            })
            .unwrap();
        assert_eq!(receive(&service), RendererEvent::MaterialUploaded(material));
        service.shutdown().unwrap();

        let service = RendererService::spawn(
            || Ok(Box::new(NullRenderer::default()) as Box<dyn RendererBackend>),
            1,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);
        service
            .handle()
            .try_upload_material(MaterialUpload {
                id: material,
                data: MaterialData {
                    base_color_texture: Some(TextureBinding {
                        image,
                        sampler: TextureSampler::default(),
                    }),
                    ..MaterialData::default()
                },
            })
            .unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::Failed(RendererFault::MissingImage(image))
        );
        assert_eq!(receive(&service), RendererEvent::Stopped);
    }

    #[test]
    fn surface_gated_upload_is_returned_until_configuration() {
        let service = RendererService::spawn(
            || Ok(Box::new(SurfaceGatedBackend(false)) as Box<dyn RendererBackend>),
            2,
        )
        .unwrap();
        assert_eq!(receive(&service), RendererEvent::Ready);
        let handle = service.handle();
        let image = ImageUpload {
            id: ImageId::new(1).unwrap(),
            extent: Extent2d::new(1, 1).unwrap(),
            color_space: x173ge_protocol::ImageColorSpace::Linear,
            rgba8: Arc::from([255, 255, 255, 255]),
        };
        let image = match handle.try_upload_image(image) {
            Err(SendError::NotReady(RendererCommand::UploadImage(image))) => image,
            _ => panic!("surface-gated backend accepted an early image upload"),
        };
        assert!(matches!(
            service.try_send(RendererCommand::UploadImage(image.clone())),
            Err(SendError::NotReady(RendererCommand::UploadImage(_)))
        ));
        service
            .try_send(RendererCommand::ConfigureSurface(surface()))
            .unwrap();
        assert!(matches!(
            receive(&service),
            RendererEvent::SurfaceConfigured(_)
        ));
        handle.try_upload_image(image).unwrap();
        assert_eq!(
            receive(&service),
            RendererEvent::ImageUploaded(ImageId::new(1).unwrap())
        );
        service.shutdown().unwrap();
    }
}
