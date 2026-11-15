//! Public game-facing facade for X173GE.

mod game;
mod runtime;

pub use game::{FixedClock, FixedTime, FrameSchedule, Game, GameError, InputFrame};
#[cfg(feature = "vulkan")]
pub use runtime::run;
pub use runtime::{EngineError, FramePacing, GameDefinition, run_with_backend};
pub use x173ge_assets as assets;
pub use x173ge_core as core;
pub use x173ge_ecs as ecs;
pub use x173ge_platform as platform;
pub use x173ge_protocol as protocol;
pub use x173ge_renderer as renderer;
pub use x173ge_scene as scene;

#[cfg(feature = "vulkan")]
pub use x173ge_renderer_vulkan as vulkan;
