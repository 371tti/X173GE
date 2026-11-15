//! Small shared types used across engine boundaries.

use std::num::{NonZeroU32, NonZeroU64};

pub use glam::{Mat4, Quat, Vec3, Vec4};

/// Builds a right-handed Vulkan perspective matrix with reverse-Z depth.
///
/// The near and far planes map to depth 1 and 0 respectively. This keeps
/// floating-point depth precision useful across large view distances.
pub fn perspective_rh_reverse_z(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (fov_y * 0.5).tan();
    Mat4::from_cols(
        Vec4::new(f / aspect, 0.0, 0.0, 0.0),
        Vec4::new(0.0, f, 0.0, 0.0),
        Vec4::new(0.0, 0.0, near / (far - near), -1.0),
        Vec4::new(0.0, 0.0, near * far / (far - near), 0.0),
    )
}

/// Builds a right-handed Vulkan orthographic matrix with reverse-Z depth.
pub fn orthographic_rh_reverse_z(
    left: f32,
    right: f32,
    bottom: f32,
    top: f32,
    near: f32,
    far: f32,
) -> Mat4 {
    Mat4::from_cols(
        Vec4::new(2.0 / (right - left), 0.0, 0.0, 0.0),
        Vec4::new(0.0, 2.0 / (top - bottom), 0.0, 0.0),
        Vec4::new(0.0, 0.0, 1.0 / (far - near), 0.0),
        Vec4::new(
            -(right + left) / (right - left),
            -(top + bottom) / (top - bottom),
            far / (far - near),
            1.0,
        ),
    )
}

macro_rules! id {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Creates an ID when its raw value is nonzero.
            pub const fn new(raw: u64) -> Option<Self> {
                match NonZeroU64::new(raw) {
                    Some(raw) => Some(Self(raw)),
                    None => None,
                }
            }

            /// Returns the stable numeric value sent across the engine protocol.
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    )+};
}

id!(
    SurfaceId,
    SurfaceGeneration,
    FrameId,
    MeshId,
    MaterialId,
    ImageId,
    EnvironmentId,
);

/// A nonzero render extent that cannot describe a minimized surface.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Extent2d {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl Extent2d {
    /// Creates an extent only when both dimensions are nonzero.
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        match (NonZeroU32::new(width), NonZeroU32::new(height)) {
            (Some(width), Some(height)) => Some(Self { width, height }),
            _ => None,
        }
    }

    /// Returns the pixel width.
    pub const fn width(self) -> u32 {
        self.width.get()
    }

    /// Returns the pixel height.
    pub const fn height(self) -> u32 {
        self.height.get()
    }
}

/// Interpolatable position, orientation and scale used by scene extraction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Transform {
    /// The identity transform.
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };

    /// Converts the transform to a column-major matrix for GPU extraction.
    pub fn matrix(self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
}

impl Default for Transform {
    /// Returns the identity transform.
    fn default() -> Self {
        Self::IDENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_z_projection_maps_near_to_one_and_far_to_zero() {
        let perspective = perspective_rh_reverse_z(60_f32.to_radians(), 16.0 / 9.0, 0.01, 1257.0);
        let near = perspective.project_point3(Vec3::new(0.0, 0.0, -0.01)).z;
        let far = perspective.project_point3(Vec3::new(0.0, 0.0, -1257.0)).z;
        assert!((near - 1.0).abs() < 1e-5);
        assert!(far.abs() < 1e-5);

        let orthographic = orthographic_rh_reverse_z(-2.0, 2.0, -1.0, 1.0, 0.1, 100.0);
        assert!((orthographic.project_point3(Vec3::new(0.0, 0.0, -0.1)).z - 1.0).abs() < 1e-5);
        assert!(
            orthographic
                .project_point3(Vec3::new(0.0, 0.0, -100.0))
                .z
                .abs()
                < 1e-5
        );
    }

    #[test]
    fn ids_reject_zero_and_keep_distinct_types() {
        assert!(SurfaceId::new(0).is_none());
        assert_eq!(SurfaceId::new(7).unwrap().get(), 7);
        assert_ne!(
            SurfaceId::new(7).unwrap().get(),
            FrameId::new(8).unwrap().get()
        );
    }

    #[test]
    fn extent_rejects_minimized_surface() {
        assert!(Extent2d::new(0, 720).is_none());
        assert_eq!(Extent2d::new(1280, 720).unwrap().width(), 1280);
    }
}
