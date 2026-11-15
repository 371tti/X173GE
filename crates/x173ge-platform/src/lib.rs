//! Window/input integration and conversion of native windows to protocol values.

mod event_loop;
mod input;
mod surface;

pub use event_loop::{
    PlatformError, WindowApplication, WindowConfig, run_windowed, run_windowed_with,
};
pub use input::{InputState, PlatformEvent};
pub use surface::{SurfaceError, describe_surface, drawable_extent};
pub use winit;
