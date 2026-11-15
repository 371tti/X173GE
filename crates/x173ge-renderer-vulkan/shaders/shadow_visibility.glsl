#include "shadow_map_resolution.glsl"
#ifdef X173GE_MBOIT
#include "mboit_visibility.glsl"
#endif

#extension GL_EXT_control_flow_attributes : require

layout(set = 0, binding = 18) uniform sampler2DShadow shadow_compare_map_0;
layout(set = 0, binding = 19) uniform sampler2DShadow shadow_compare_map_1;
layout(set = 0, binding = 20) uniform sampler2DShadow shadow_compare_map_2;
layout(set = 0, binding = 21) uniform sampler2DShadow shadow_compare_map_3;

const vec2 PCSS_FILTER_ROTATION = vec2(0.4798611942, 0.8773444217);
const int PCSS_SAMPLES = 16;
const vec2 PCSS_DISK[PCSS_SAMPLES] = vec2[PCSS_SAMPLES](
    vec2(0.1767766953, 0.0000000000),
    vec2(-0.2257721879, 0.2068258184),
    vec2(0.0345580520, -0.3937711785),
    vec2(0.2845712198, 0.3711727642),
    vec2(-0.5222231872, -0.0923739287),
    vec2(0.4946953915, -0.3146847146),
    vec2(-0.1654659271, 0.6155250011),
    vec2(-0.3155614679, -0.6075944041),
    vec2(0.6846421616, 0.2500302194),
    vec2(-0.7122560862, 0.2940089584),
    vec2(0.3433544987, -0.7337286203),
    vec2(0.2537302409, 0.8089319902),
    vec2(-0.7647458920, -0.4431858760),
    vec2(0.8971339835, -0.1972323896),
    vec2(-0.5475069048, 0.7787722319),
    vec2(-0.1264867729, -0.9760896968)
);

float shadow_depth_sample(int cascade, vec2 uv) {
    switch (cascade) {
        case 0: return texture(shadow_map_0, uv).r;
        case 1: return texture(shadow_map_1, uv).r;
        case 2: return texture(shadow_map_2, uv).r;
        default: return texture(shadow_map_3, uv).r;
    }
}

vec2 shadow_texel_center(int cascade, vec2 uv) {
    float resolution = shadow_resolution(cascade);
    ivec2 maximum = ivec2(int(resolution) - 1);
    ivec2 texel = clamp(ivec2(floor(uv * resolution)), ivec2(0), maximum);
    return (vec2(texel) + 0.5) / resolution;
}

float shadow_comparison(int cascade, vec2 uv, float depth) {
    switch (cascade) {
        case 0: return texture(shadow_compare_map_0, vec3(uv, depth));
        case 1: return texture(shadow_compare_map_1, vec3(uv, depth));
        case 2: return texture(shadow_compare_map_2, vec3(uv, depth));
        default: return texture(shadow_compare_map_3, vec3(uv, depth));
    }
}

float shadow_compare(
    int cascade, vec2 uv, float receiver_depth, vec2 receiver_uv,
    vec2 depth_gradient, float bias, float footprint_bias
) {
    if (any(lessThan(uv, vec2(0.0))) || any(greaterThan(uv, vec2(1.0)))) return 1.0;
    float depth = receiver_depth + dot(depth_gradient, uv - receiver_uv) - bias - footprint_bias;
    if (depth < 0.0 || depth > 1.0) return 1.0;
    return shadow_comparison(cascade, uv, depth);
}

float pcss_world_rotation(vec2 light_position, float half_extent, vec2 uv_dx, vec2 uv_dy) {
    float footprint = sqrt(max(dot(uv_dx, uv_dx), dot(uv_dy, uv_dy)));
    float cell_size = max(2.0 * half_extent * footprint, 1e-5);
    ivec2 cell = ivec2(floor(light_position / cell_size));
    uint hash = uint(cell.x) * 0x9e3779b9u + uint(cell.y) * 0x85ebca6bu;
    hash ^= hash >> 16u;
    hash *= 0x7feb352du;
    hash ^= hash >> 15u;
    hash *= 0x846ca68bu;
    hash ^= hash >> 16u;
    return float(hash & 0x00ffffffu) * (6.28318530718 / 16777216.0);
}

vec2 pcss_rotate_sample(vec2 disk_point, vec2 rotation) {
    return vec2(
        disk_point.x * rotation.x - disk_point.y * rotation.y,
        disk_point.x * rotation.y + disk_point.y * rotation.x
    );
}

vec3 shadow_visibility_cascade(
    int cascade, vec3 world_position, vec3 normal, vec3 light_direction,
    vec3 world_dx, vec3 world_dy
) {
    mat4 shadow_matrix = frame.shadow_matrices[cascade];
    vec4 light_clip = shadow_matrix * vec4(world_position, 1.0);
    vec3 projected = light_clip.xyz;
    if (projected.z < 0.0 || projected.z > 1.0) return vec3(1.0);
    vec2 uv = vec2(projected.x * 0.5 + 0.5, -projected.y * 0.5 + 0.5);
    // CSM uses orthographic matrices: clip.w is 1 and its screen derivatives are zero.
    vec3 projected_dx = mat3(shadow_matrix) * world_dx;
    vec3 projected_dy = mat3(shadow_matrix) * world_dy;
    vec2 uv_dx = vec2(projected_dx.x, -projected_dx.y) * 0.5;
    vec2 uv_dy = vec2(projected_dy.x, -projected_dy.y) * 0.5;
    float determinant = uv_dx.x * uv_dy.y - uv_dy.x * uv_dx.y;
    vec2 depth_gradient = abs(determinant) > 1e-12
        ? vec2(
            (projected_dx.z * uv_dy.y - projected_dy.z * uv_dx.y) / determinant,
            (uv_dx.x * projected_dy.z - uv_dy.x * projected_dx.z) / determinant
        )
        : vec2(0.0);
    float bias = max(0.00002 * (1.0 - dot(normal, light_direction)), 0.000005);
    float footprint_bias = (abs(depth_gradient.x) + abs(depth_gradient.y)) *
                           (0.5 / shadow_resolution(cascade));
    float angle = frame.shadow_parameters.x;
    float visibility = 1.0;
    if (angle <= 0.0) {
        visibility = shadow_compare(cascade, uv, projected.z, uv, depth_gradient, bias, footprint_bias);
    } else {
        float light_near = frame.shadow_depth[cascade].x;
        float depth_range = frame.shadow_depth[cascade].y - light_near;
        float half_extent = frame.shadow_depth[cascade].z;
        float receiver_distance = light_near + projected.z * depth_range;
        float tangent = tan(angle);
        float search_radius = min(frame.shadow_parameters.y, max(receiver_distance - light_near, 0.0) * tangent);
        vec2 search_uv = vec2(search_radius / (2.0 * half_extent));
        float rotation = pcss_world_rotation(
            (light_clip.xy - shadow_matrix[3].xy) * half_extent, half_extent, uv_dx, uv_dy
        );
        vec2 blocker_rotation = vec2(cos(rotation), sin(rotation));
        float blocker_depth = 0.0;
        float blocker_weight = 0.0;
        float blocker_softness = max(bias, 2.0 / 65535.0);
        [[unroll]] for (int i = 0; i < PCSS_SAMPLES; ++i) {
            vec2 sample_uv = uv + pcss_rotate_sample(PCSS_DISK[i], blocker_rotation) * search_uv;
            if (all(greaterThanEqual(sample_uv, vec2(0.0))) && all(lessThanEqual(sample_uv, vec2(1.0)))) {
                vec2 sample_center = shadow_texel_center(cascade, sample_uv);
                float depth = shadow_depth_sample(cascade, sample_uv);
                float receiver_depth = projected.z + dot(depth_gradient, sample_center - uv) - bias;
                float weight = smoothstep(0.0, blocker_softness, receiver_depth - depth);
                blocker_depth += depth * weight;
                blocker_weight += weight;
            }
        }
        if (blocker_weight > 0.0001) {
            float average_blocker = light_near + blocker_depth / blocker_weight * depth_range;
            float penumbra = min(max(receiver_distance - average_blocker, 0.0) * tangent,
                                 frame.shadow_parameters.y);
            vec2 filter_uv = vec2(penumbra / (2.0 * half_extent));
            vec2 filter_rotation = pcss_rotate_sample(blocker_rotation, PCSS_FILTER_ROTATION);
            visibility = 0.0;
            [[unroll]] for (int i = 0; i < PCSS_SAMPLES; ++i) {
                vec2 tap = uv + pcss_rotate_sample(PCSS_DISK[i], filter_rotation) * filter_uv;
                visibility += shadow_compare(cascade, tap, projected.z, uv, depth_gradient, bias, footprint_bias);
            }
            visibility /= float(PCSS_SAMPLES);
        }
    }
#ifdef X173GE_MBOIT
    if (frame.shadow_parameters.w > 0.5) {
        visibility *= mboit_visibility(cascade, uv, projected.z * 2.0 - 1.0);
    }
#endif
    return vec3(visibility);
}

vec3 shadow_visibility(
    vec3 world_position, vec3 normal, vec3 light_direction,
    vec3 world_dx, vec3 world_dy
) {
    float view_depth = dot(world_position - frame.camera_position.xyz, frame.camera_forward.xyz);
    int cascade = 0;
    while (cascade < 3 && view_depth > frame.cascade_splits[cascade]) ++cascade;
    if (view_depth > frame.cascade_splits[3]) return vec3(1.0);
    vec3 visibility = shadow_visibility_cascade(
        cascade, world_position, normal, light_direction, world_dx, world_dy
    );
    if (cascade == 3 || view_depth < frame.shadow_depth[cascade].w) return visibility;
    float blend = smoothstep(frame.shadow_depth[cascade].w, frame.cascade_splits[cascade], view_depth);
    return mix(visibility, shadow_visibility_cascade(
        cascade + 1, world_position, normal, light_direction, world_dx, world_dy
    ), blend);
}
