//! CPU-side fit for stable directional shadow cascades.

use glam::{Mat4, Vec3, Vec4};
use x173ge_core::MeshId;
use x173ge_protocol::{CameraData, CameraProjection, RenderItem, Vertex};

pub const CASCADE_COUNT: usize = 4;
pub const SHADOW_MAP_SIZES: [u32; CASCADE_COUNT] = [2048, 2048, 2048, 4096];
const PERSPECTIVE_SPLIT_LOG_WEIGHT: f32 = 0.9;
const CASCADE_BLEND_FRACTION: f32 = 0.1;

#[derive(Clone, Copy, Debug)]
pub struct Aabb {
    min: Vec3,
    max: Vec3,
}

impl Aabb {
    pub fn from_vertices(vertices: &[Vertex]) -> Option<Self> {
        let first = Vec3::from_array(vertices.first()?.position);
        let (min, max) = vertices
            .iter()
            .skip(1)
            .fold((first, first), |(min, max), vertex| {
                let p = Vec3::from_array(vertex.position);
                (min.min(p), max.max(p))
            });
        Some(Self { min, max })
    }

    pub fn center(self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    fn corners(self, transform: Mat4) -> [Vec3; 8] {
        std::array::from_fn(|i| {
            transform.transform_point3(Vec3::new(
                if i & 1 == 0 { self.min.x } else { self.max.x },
                if i & 2 == 0 { self.min.y } else { self.max.y },
                if i & 4 == 0 { self.min.z } else { self.max.z },
            ))
        })
    }
}

#[derive(Clone, Debug)]
pub struct Cascade {
    pub view_projection: Mat4,
    pub split_far: f32,
    /// Start depth for blending to the next cascade; the last cascade uses `split_far`.
    pub blend_start: f32,
    pub light_near: f32,
    pub light_far: f32,
    pub half_extent: f32,
    /// Indices in the fitted frame whose transformed mesh bounds overlap this cascade.
    pub caster_indices: Vec<usize>,
}

pub fn fit(
    camera: CameraData,
    direction_to_light: Vec3,
    items: &[RenderItem],
    mut mesh_bounds: impl FnMut(MeshId) -> Option<Aabb>,
) -> Result<Vec<Cascade>, &'static str> {
    let (near, far, perspective) = match camera.projection {
        CameraProjection::Perspective { near, far } => (near, far, true),
        CameraProjection::Orthographic { near, far } => (near, far, false),
    };
    let view_projection = Mat4::from_cols_array_2d(&camera.view_projection);
    let determinant = view_projection.determinant();
    if !determinant.is_finite() || determinant == 0.0 {
        return Err("shadow camera view-projection matrix is singular");
    }
    let inverse = view_projection.inverse();
    if !inverse.to_cols_array().iter().all(|v| v.is_finite()) {
        return Err("shadow camera inverse is non-finite");
    }
    let near_point = unproject(inverse, Vec3::Z)?;
    let far_point = unproject(inverse, Vec3::ZERO)?;
    let forward = (far_point - near_point)
        .try_normalize()
        .ok_or("shadow camera forward direction is invalid")?;
    let position = Vec3::from_array(camera.position);
    let actual_near = (near_point - position).dot(forward);
    let actual_far = (far_point - position).dot(forward);
    let depth_tolerance = (far * 1e-3).max(1e-3);
    if (actual_near - near).abs() > depth_tolerance || (actual_far - far).abs() > depth_tolerance {
        return Err("shadow camera near/far do not match its view-projection matrix");
    }
    let to_light = direction_to_light
        .try_normalize()
        .ok_or("shadow light direction is invalid")?;
    let travel = -to_light;
    let reference_up = if travel.dot(Vec3::Y).abs() > 0.95 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    let right = travel.cross(reference_up).normalize();
    let light_up = right.cross(travel).normalize();
    let splits = split_depths(near, far, perspective);
    let frustum = frustum_rays(inverse)?;
    let mut cascades = Vec::with_capacity(CASCADE_COUNT);

    for cascade in 0..CASCADE_COUNT {
        let map_size = SHADOW_MAP_SIZES[cascade] as f32;
        let split_near = splits[cascade];
        let split_far = splits[cascade + 1];
        let receiver_near = if cascade == 0 {
            split_near
        } else {
            split_near - (split_near - splits[cascade - 1]) * CASCADE_BLEND_FRACTION
        };
        let blend_start = if cascade + 1 == CASCADE_COUNT {
            split_far
        } else {
            split_far - (split_far - split_near) * CASCADE_BLEND_FRACTION
        };
        let near_t = (receiver_near - near) / (far - near);
        let far_t = (split_far - near) / (far - near);
        let receivers: [Vec3; 8] = std::array::from_fn(|i| {
            let ray = frustum[i % 4].1 - frustum[i % 4].0;
            frustum[i % 4].0 + ray * if i < 4 { near_t } else { far_t }
        });
        let center = receivers.iter().copied().sum::<Vec3>() / 8.0;
        let radius = receivers
            .iter()
            .map(|point| point.distance(center))
            .fold(0.0, f32::max);
        let quantized_extent = (radius * 16.0).ceil() / 16.0;
        let half_extent = quantized_extent / (1.0 - std::f32::consts::SQRT_2 / map_size);
        if !half_extent.is_finite() || half_extent <= 0.0 {
            return Err("shadow cascade has invalid receiver bounds");
        }
        let texel = 2.0 * half_extent / map_size;
        let x = center.dot(right);
        let y = center.dot(light_up);
        let snapped = center
            + right * ((x / texel).round() * texel - x)
            + light_up * ((y / texel).round() * texel - y);

        let mut min_to_light = f32::INFINITY;
        let mut max_to_light = f32::NEG_INFINITY;
        for point in receivers {
            let distance = (point - snapped).dot(to_light);
            min_to_light = min_to_light.min(distance);
            max_to_light = max_to_light.max(distance);
        }
        let mut caster_indices = Vec::with_capacity(items.len());
        for (item_index, item) in items.iter().enumerate() {
            let bounds = mesh_bounds(item.mesh).ok_or("shadow caster mesh has no bounds")?;
            let corners = bounds.corners(Mat4::from_cols_array_2d(&item.model_matrix));
            let (min_x, max_x, min_y, max_y) = corners.iter().fold(
                (
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                ),
                |(min_x, max_x, min_y, max_y), point| {
                    let relative = *point - snapped;
                    let x = relative.dot(right);
                    let y = relative.dot(light_up);
                    (min_x.min(x), max_x.max(x), min_y.min(y), max_y.max(y))
                },
            );
            if min_x <= half_extent
                && max_x >= -half_extent
                && min_y <= half_extent
                && max_y >= -half_extent
            {
                caster_indices.push(item_index);
                for point in corners {
                    let distance = (point - snapped).dot(to_light);
                    min_to_light = min_to_light.min(distance);
                    max_to_light = max_to_light.max(distance);
                }
            }
        }

        let margin = (half_extent * 0.02).max(0.01);
        let light_near = margin;
        let light_far = (max_to_light - min_to_light + margin * 2.0).max(light_near + margin);
        let eye = snapped + to_light * (max_to_light + margin);
        let light_view = Mat4::look_at_rh(eye, snapped, reference_up);
        let light_projection = orthographic_rh_zo(half_extent, light_near, light_far);
        let matrix = light_projection * light_view;
        if !matrix.to_cols_array().iter().all(|v| v.is_finite()) {
            return Err("shadow cascade projection is non-finite");
        }
        cascades.push(Cascade {
            view_projection: matrix,
            split_far,
            blend_start,
            light_near,
            light_far,
            half_extent,
            caster_indices,
        });
    }
    Ok(cascades)
}

pub fn view_direction(camera: CameraData) -> Result<Vec3, &'static str> {
    let view_projection = Mat4::from_cols_array_2d(&camera.view_projection);
    let determinant = view_projection.determinant();
    if !determinant.is_finite() || determinant == 0.0 {
        return Err("camera view-projection matrix is singular");
    }
    let inverse = view_projection.inverse();
    let near = unproject(inverse, Vec3::Z)?;
    let far = unproject(inverse, Vec3::ZERO)?;
    (far - near)
        .try_normalize()
        .ok_or("camera forward direction is invalid")
}

fn split_depths(near: f32, far: f32, perspective: bool) -> [f32; CASCADE_COUNT + 1] {
    let first_split = if perspective {
        let t = 1.0 / CASCADE_COUNT as f32;
        PERSPECTIVE_SPLIT_LOG_WEIGHT * near * (far / near).powf(t)
            + (1.0 - PERSPECTIVE_SPLIT_LOG_WEIGHT) * (near + (far - near) * t)
    } else {
        near
    };
    std::array::from_fn(|i| {
        let t = i as f32 / CASCADE_COUNT as f32;
        if i == 0 {
            near
        } else if i == CASCADE_COUNT {
            far
        } else if perspective && i == 1 {
            first_split
        } else if perspective {
            first_split * (far / first_split).powf((i - 1) as f32 / (CASCADE_COUNT - 1) as f32)
        } else {
            near + (far - near) * t
        }
    })
}

fn frustum_rays(inverse: Mat4) -> Result<[(Vec3, Vec3); 4], &'static str> {
    let corners = [
        Vec3::new(-1.0, -1.0, 1.0),
        Vec3::new(1.0, -1.0, 1.0),
        Vec3::new(-1.0, 1.0, 1.0),
        Vec3::new(1.0, 1.0, 1.0),
    ];
    corners
        .map(|near| {
            Ok((
                unproject(inverse, near)?,
                unproject(inverse, near.with_z(0.0))?,
            ))
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .try_into()
        .map_err(|_| "invalid shadow frustum")
}

fn unproject(inverse: Mat4, point: Vec3) -> Result<Vec3, &'static str> {
    let world = inverse * Vec4::new(point.x, point.y, point.z, 1.0);
    if !world.is_finite() || world.w.abs() <= f32::EPSILON {
        return Err("shadow frustum cannot be unprojected");
    }
    Ok(world.truncate() / world.w)
}

fn orthographic_rh_zo(half_extent: f32, near: f32, far: f32) -> Mat4 {
    Mat4::from_cols(
        Vec4::new(1.0 / half_extent, 0.0, 0.0, 0.0),
        Vec4::new(0.0, 1.0 / half_extent, 0.0, 0.0),
        Vec4::new(0.0, 0.0, -1.0 / (far - near), 0.0),
        Vec4::new(0.0, 0.0, -near / (far - near), 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use x173ge_core::perspective_rh_reverse_z;
    use x173ge_protocol::CameraProjection;

    fn camera(position: Vec3) -> CameraData {
        let view = Mat4::look_at_rh(position, position + Vec3::NEG_Z, Vec3::Y);
        let projection = perspective_rh_reverse_z(60_f32.to_radians(), 16.0 / 9.0, 0.1, 100.0);
        CameraData {
            view_projection: (projection * view).to_cols_array_2d(),
            position: position.to_array(),
            projection: CameraProjection::Perspective {
                near: 0.1,
                far: 100.0,
            },
        }
    }

    #[test]
    fn perspective_splits_are_practical_and_strictly_increasing() {
        let splits = split_depths(0.1, 100.0, true);
        assert_eq!(splits[0], 0.1);
        assert_eq!(splits[CASCADE_COUNT], 100.0);
        assert!(splits.windows(2).all(|split| split[0] < split[1]));
        assert!(splits[1] < 25.0);
    }

    #[test]
    fn orthographic_splits_are_uniform() {
        assert_eq!(split_depths(1.0, 9.0, false), [1.0, 3.0, 5.0, 7.0, 9.0]);
    }

    #[test]
    fn cascade_fit_is_finite_and_snaps_subtexel_camera_motion() {
        let first = camera(Vec3::new(0.0, 0.0, 5.0));
        let a = fit(first, Vec3::X, &[], |_| None).unwrap();
        let moved = camera(Vec3::new(0.0, 0.0, 5.00001));
        let b = fit(moved, Vec3::X, &[], |_| None).unwrap();
        assert_eq!(a.len(), CASCADE_COUNT);
        assert!(a.iter().all(|cascade| cascade.view_projection.is_finite()));
        assert_eq!(a[0].view_projection, b[0].view_projection);
        assert!(view_direction(first).unwrap().dot(Vec3::NEG_Z) > 0.999);
    }

    #[test]
    fn cascade_fit_keeps_far_plane_stable_for_large_world_units() {
        let position = Vec3::new(0.0, 0.0, 900.0);
        let view = Mat4::look_at_rh(position, Vec3::ZERO, Vec3::Y);
        let projection = perspective_rh_reverse_z(50_f32.to_radians(), 16.0 / 9.0, 0.01, 1257.0);
        let camera = CameraData {
            view_projection: (projection * view).to_cols_array_2d(),
            position: position.to_array(),
            projection: CameraProjection::Perspective {
                near: 0.01,
                far: 1257.0,
            },
        };
        assert_eq!(
            fit(camera, Vec3::Y, &[], |_| None).unwrap().len(),
            CASCADE_COUNT
        );
    }

    #[test]
    fn view_direction_is_shared_across_an_orthographic_view() {
        let position = Vec3::new(4.0, 3.0, 7.0);
        let target = Vec3::ZERO;
        let view = Mat4::look_at_rh(position, target, Vec3::Y);
        let projection = x173ge_core::orthographic_rh_reverse_z(-4.0, 4.0, -3.0, 3.0, 0.1, 100.0);
        let camera = CameraData {
            view_projection: (projection * view).to_cols_array_2d(),
            position: position.to_array(),
            projection: CameraProjection::Orthographic {
                near: 0.1,
                far: 100.0,
            },
        };

        assert!(
            view_direction(camera)
                .unwrap()
                .dot((target - position).normalize())
                > 0.999
        );
    }

    #[test]
    fn snapped_cascades_keep_frustum_receivers_inside_their_bounds() {
        let position = Vec3::new(73.0, 41.0, 128.0);
        let target = Vec3::new(-2.0, 1.0, -4.0);
        let view = Mat4::look_at_rh(position, target, Vec3::Y);
        let projection = perspective_rh_reverse_z(73_f32.to_radians(), 2.3, 0.1, 300.0);
        let camera = CameraData {
            view_projection: (projection * view).to_cols_array_2d(),
            position: position.to_array(),
            projection: CameraProjection::Perspective {
                near: 0.1,
                far: 300.0,
            },
        };
        let cascades = fit(camera, Vec3::new(0.3, 1.0, 0.2), &[], |_| None).unwrap();
        let inverse = Mat4::from_cols_array_2d(&camera.view_projection).inverse();
        let rays = frustum_rays(inverse).unwrap();
        let splits = split_depths(0.1, 300.0, true);

        for (index, cascade) in cascades.iter().enumerate() {
            let near_t = (splits[index] - 0.1) / 299.9;
            let far_t = (splits[index + 1] - 0.1) / 299.9;
            for (origin, end) in rays {
                for t in [near_t, far_t] {
                    let point = origin + (end - origin) * t;
                    let clip = cascade.view_projection * point.extend(1.0);
                    let ndc = clip.truncate() / clip.w;
                    assert!(ndc.x.abs() <= 1.0, "cascade {index}: x={}", ndc.x);
                    assert!(ndc.y.abs() <= 1.0, "cascade {index}: y={}", ndc.y);
                    assert!((-1e-5..=1.0 + 1e-5).contains(&ndc.z));
                }
            }
        }
    }

    #[test]
    fn cascade_fit_rejects_depth_metadata_that_disagrees_with_the_matrix() {
        let mut camera = camera(Vec3::new(0.0, 0.0, 5.0));
        camera.projection = CameraProjection::Perspective {
            near: 0.1,
            far: 50.0,
        };
        assert!(
            fit(camera, Vec3::X, &[], |_| None)
                .unwrap_err()
                .contains("near/far")
        );
    }
}
