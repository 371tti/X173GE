//! Winit event loop with normalized events and explicit wake scheduling.

use std::{sync::Arc, time::Instant};

use thiserror::Error;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{DeviceEvent, DeviceId, ElementState, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::PhysicalKey,
    window::{Window, WindowAttributes, WindowId},
};
use x173ge_core::Extent2d;

use crate::input::{InputState, PlatformEvent};

/// Native event-loop or window creation error.
#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("event loop: {0}")]
    EventLoop(#[from] winit::error::EventLoopError),
    #[error("window creation: {0}")]
    Window(String),
}

/// Initial native window properties.
pub struct WindowConfig {
    pub title: String,
    pub extent: Extent2d,
}

impl WindowConfig {
    /// Creates a window configuration with an explicit nonzero physical extent.
    pub fn new(title: impl Into<String>, extent: Extent2d) -> Self {
        Self {
            title: title.into(),
            extent,
        }
    }
}

/// Receives platform events without taking ownership of winit's event loop.
pub trait WindowApplication {
    /// Called after the native window is created.
    fn on_window(&mut self, _window: &Arc<Window>) {}

    /// Receives input and window events after `InputState` has been updated.
    fn on_event(&mut self, _window: &Arc<Window>, _event: PlatformEvent, _input: &mut InputState) {}

    /// Pumps game and renderer work; clear input transients only after a fixed update consumes them.
    fn about_to_wait(&mut self, _window: &Arc<Window>, _input: &mut InputState) -> Option<Instant> {
        None
    }

    /// Tells the host to exit after startup or application failure.
    fn should_exit(&self) -> bool {
        false
    }

    /// Shuts down owned services when the event loop exits.
    fn on_shutdown(&mut self) {}
}

/// Runs a winit event loop and leaves event policy with the application.
pub fn run_windowed<A: WindowApplication + 'static>(
    config: WindowConfig,
    app: A,
) -> Result<(), PlatformError> {
    run_windowed_with(config, app, |_| ())
}

/// Runs a window app and extracts its result before the app and native window are dropped.
pub fn run_windowed_with<A: WindowApplication + 'static, R>(
    config: WindowConfig,
    app: A,
    extract: impl FnOnce(&mut A) -> R,
) -> Result<R, PlatformError> {
    let event_loop = EventLoop::new()?;
    let mut host = WindowHost {
        config,
        app,
        window: None,
        input: InputState::default(),
        error: None,
        shutdown: false,
    };
    let run_result = event_loop.run_app(&mut host);
    host.shutdown();
    run_result?;
    if let Some(error) = host.error {
        return Err(error);
    }
    Ok(extract(&mut host.app))
}

struct WindowHost<A> {
    config: WindowConfig,
    app: A,
    window: Option<Arc<Window>>,
    input: InputState,
    error: Option<PlatformError>,
    shutdown: bool,
}

impl<A: WindowApplication> WindowHost<A> {
    fn shutdown(&mut self) {
        if !self.shutdown {
            self.app.on_shutdown();
            self.shutdown = true;
        }
    }
}

impl<A: WindowApplication> ApplicationHandler for WindowHost<A> {
    /// Creates one native window after winit resumes the application.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes: WindowAttributes = Window::default_attributes()
            .with_title(&self.config.title)
            .with_inner_size(PhysicalSize::new(
                self.config.extent.width(),
                self.config.extent.height(),
            ));
        match event_loop.create_window(attributes) {
            Ok(window) => {
                let window = Arc::new(window);
                self.app.on_window(&window);
                self.window = Some(window);
            }
            Err(error) => {
                self.error = Some(PlatformError::Window(error.to_string()));
                event_loop.exit();
            }
        }
    }

    /// Normalizes relevant input and window events for the game-facing API.
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(window) = &self.window else { return };
        if window.id() != id {
            return;
        }
        let event = match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                Some(PlatformEvent::CloseRequested)
            }
            WindowEvent::Resized(size) => Some(PlatformEvent::Resized {
                width: size.width,
                height: size.height,
            }),
            WindowEvent::Focused(focused) => {
                if !focused {
                    self.input.clear_held();
                }
                Some(PlatformEvent::Focused(focused))
            }
            WindowEvent::KeyboardInput { event, .. } => match event.physical_key {
                PhysicalKey::Code(code) => {
                    let pressed = event.state == ElementState::Pressed;
                    self.input.set_key(code, pressed);
                    Some(PlatformEvent::Key {
                        code,
                        pressed,
                        repeat: event.repeat,
                    })
                }
                _ => None,
            },
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                self.input.set_button(button, pressed);
                Some(PlatformEvent::MouseButton { button, pressed })
            }
            WindowEvent::CursorMoved { position, .. } => {
                let position = [position.x, position.y];
                self.input.move_cursor(position);
                Some(PlatformEvent::CursorMoved { position })
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let delta = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(position) => (position.y / 32.0).clamp(-4.0, 4.0),
                };
                self.input.scroll(delta);
                Some(PlatformEvent::MouseWheel { delta })
            }
            WindowEvent::RedrawRequested => Some(PlatformEvent::RedrawRequested),
            WindowEvent::ScaleFactorChanged { .. } => {
                let size = window.inner_size();
                Some(PlatformEvent::Resized {
                    width: size.width,
                    height: size.height,
                })
            }
            _ => None,
        };
        if let Some(event) = event {
            self.app.on_event(window, event, &mut self.input);
            if event == PlatformEvent::CloseRequested {
                event_loop.exit();
            }
        }
    }

    /// Forwards raw mouse movement for first-person controls while cursor-grabbed.
    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        let (Some(window), DeviceEvent::MouseMotion { delta }) = (&self.window, event) else {
            return;
        };
        self.app.on_event(
            window,
            PlatformEvent::MouseMotion {
                delta: [delta.0, delta.1],
            },
            &mut self.input,
        );
    }

    /// Lets the application choose its next fixed-update wake deadline.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            if self.app.should_exit() {
                event_loop.exit();
                return;
            }
            let deadline = self.app.about_to_wait(window, &mut self.input);
            if self.app.should_exit() {
                event_loop.exit();
                return;
            }
            event_loop.set_control_flow(deadline.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
        }
    }

    /// Ensures renderer and worker services are shut down after loop exit.
    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.shutdown();
    }
}
