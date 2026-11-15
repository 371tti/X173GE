//! Scene components and the sole ECS-to-renderer extraction boundary.

use x173ge_core::{Extent2d, FrameId, MaterialId, MeshId, Transform};
use x173ge_ecs::{Component, World};
use x173ge_protocol::{FrameSnapshot, SurfaceToken};

/// Previous and current simulation transforms for interpolated rendering.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct TransformHistory {
    pub previous: Transform,
    pub current: Transform,
}

impl TransformHistory {
    /// Interpolates simulation transforms without advancing game state.
    pub fn interpolate(self, alpha: f32) -> Transform {
        Transform {
            translation: self
                .previous
                .translation
                .lerp(self.current.translation, alpha),
            rotation: self.previous.rotation.slerp(self.current.rotation, alpha),
            scale: self.previous.scale.lerp(self.current.scale, alpha),
        }
    }
}

/// A reference to an uploaded renderable asset pair.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct MeshInstance {
    pub mesh: MeshId,
    pub material: MaterialId,
}

/// Frame values available to an extraction implementation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExtractFrame {
    pub id: FrameId,
    pub surface: SurfaceToken,
    pub extent: Extent2d,
    pub alpha: f32,
}

/// Converts game-owned state into a renderer-owned snapshot.
pub trait RenderExtract<W = World> {
    type Error;

    /// Returns owned frame data; no ECS borrow crosses into the renderer.
    fn extract(&self, world: &W, frame: ExtractFrame) -> Result<FrameSnapshot, Self::Error>;
}
