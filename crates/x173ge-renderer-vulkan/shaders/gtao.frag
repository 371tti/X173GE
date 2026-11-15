#version 450

layout(set = 0, binding = 0, std140) uniform Frame {
    mat4 view_projection;
    mat4 inverse_view_projection;
    vec4 camera_position;
    vec4 ambient_diffuse;
    vec4 ambient_diffuse_y;
    vec4 camera_forward;
    vec4 gtao_parameters;
    uvec4 light_count;
    vec4 cascade_splits;
    mat4 shadow_matrices[4];
    vec4 shadow_depth[4];
    vec4 shadow_parameters;
    vec4 lights[32];
} frame;

layout(set = 0, binding = 6) uniform sampler2D scene_depth;
layout(set = 0, binding = 7) uniform sampler2D scene_normal;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_ao;

const int SLICE_COUNT = 4;
const int SAMPLE_COUNT = 4;
const float PI = 3.14159265358979323846;
const float EFFECT_RADIUS = 1.0;
const float THIN_OCCLUDER_BIAS = 0.06;

vec3 world_position(vec2 texcoord, float depth) {
    vec4 clip = vec4(texcoord.x * 2.0 - 1.0, 1.0 - texcoord.y * 2.0, depth, 1.0);
    vec4 world = frame.inverse_view_projection * clip;
    return world.xyz / world.w;
}

void main() {
    ivec2 extent = textureSize(scene_depth, 0);
    ivec2 half_pixel = ivec2(gl_FragCoord.xy);
    ivec2 pixel = min(half_pixel * 2 + ivec2(1), extent - 1);
    float center_depth = texelFetch(scene_depth, pixel, 0).r;
    if (center_depth <= 0.000001) {
        out_ao = vec4(1.0, 0.0, 0.0, 1.0);
        return;
    }

    vec2 center_uv = (vec2(pixel) + 0.5) / vec2(extent);
    vec3 center = world_position(center_uv, center_depth);
    vec3 view = frame.gtao_parameters.w > 0.5
        ? normalize(frame.camera_position.xyz - center)
        : -normalize(frame.camera_forward.xyz);
    vec3 normal = normalize(texelFetch(scene_normal, pixel, 0).rgb * 2.0 - 1.0);
    float visibility = 0.0;

    for (int slice = 0; slice < SLICE_COUNT; ++slice) {
        float phi = PI * float(slice) / float(SLICE_COUNT);
        vec2 omega = vec2(cos(phi), -sin(phi));
        vec3 direction = world_position(center_uv + omega / vec2(extent), center_depth) - center;
        vec3 ortho_direction = direction - dot(direction, view) * view;
        vec3 axis = cross(direction, view);
        float axis_length = length(axis);
        float pixel_world_size = length(ortho_direction);
        if (axis_length < 1e-5 || pixel_world_size < 1e-6) {
            visibility += 1.0;
            continue;
        }
        axis /= axis_length;
        vec3 projected_normal = normal - axis * dot(normal, axis);
        float projected_length = length(projected_normal);
        if (projected_length < 1e-5) {
            visibility += 1.0;
            continue;
        }
        float sign_normal = sign(dot(ortho_direction, projected_normal));
        float cos_normal = clamp(dot(projected_normal, view) / projected_length, 0.0, 1.0);
        float angle_normal = sign_normal * acos(cos_normal);
        float radius_pixels = min(EFFECT_RADIUS / pixel_world_size, 64.0);
        vec2 pixel_offset = omega / vec2(extent);

        for (int side = 0; side < 2; ++side) {
            float horizon_cos = -1.0;
            float horizon_distance = 0.0;
            float side_sign = float(side) * 2.0 - 1.0;
            for (int sample_index = 0; sample_index < SAMPLE_COUNT; ++sample_index) {
                float fraction = (float(sample_index) + 0.5) / float(SAMPLE_COUNT);
                float offset_pixels = mix(1.3, radius_pixels, fraction);
                vec2 sample_uv = center_uv + pixel_offset * (side_sign * offset_pixels);
                if (any(lessThan(sample_uv, vec2(0.0))) || any(greaterThanEqual(sample_uv, vec2(1.0)))) {
                    continue;
                }
                ivec2 sample_pixel = clamp(ivec2(sample_uv * vec2(extent)), ivec2(0), extent - 1);
                float sample_depth = texelFetch(scene_depth, sample_pixel, 0).r;
                if (sample_depth <= 0.000001) continue;
                vec2 sample_center_uv = (vec2(sample_pixel) + 0.5) / vec2(extent);
                vec3 sample_position = world_position(sample_center_uv, sample_depth);
                vec3 delta = sample_position - center;
                float distance = length(delta);
                if (distance <= 1e-5 || distance > EFFECT_RADIUS) continue;
                float sample_horizon = dot(delta / distance, view);
                if (sample_horizon >= horizon_cos) {
                    horizon_cos = sample_horizon;
                    horizon_distance = distance;
                } else if (distance > horizon_distance * 1.5) {
                    horizon_cos = max(horizon_cos - THIN_OCCLUDER_BIAS, -1.0);
                }
            }

            float horizon_angle = acos(clamp(horizon_cos, -1.0, 1.0));
            float h = angle_normal + clamp(side_sign * horizon_angle - angle_normal, -PI * 0.5, PI * 0.5);
            visibility += projected_length
                * (cos_normal + 2.0 * h * sin(angle_normal) - cos(2.0 * h - angle_normal)) * 0.25;
        }
    }

    out_ao = vec4(clamp(visibility / float(SLICE_COUNT), 0.0, 1.0), 0.0, 0.0, 1.0);
}
