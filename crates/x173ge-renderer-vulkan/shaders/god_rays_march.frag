#version 450
#ifdef X173GE_MBOIT
#extension GL_GOOGLE_include_directive : require
#endif

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
    vec4 god_rays_parameters;
    uvec4 god_rays_counts;
} frame;
layout(set = 0, binding = 1) uniform sampler2D shadow_map_0;
layout(set = 0, binding = 2) uniform sampler2D shadow_map_1;
layout(set = 0, binding = 3) uniform sampler2D shadow_map_2;
layout(set = 0, binding = 4) uniform sampler2D shadow_map_3;
#ifdef X173GE_MBOIT
#include "shadow_map_resolution.glsl"
#include "mboit_visibility.glsl"
#endif
layout(set = 0, binding = 6) uniform sampler2D scene_depth;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_scattering;

const float PI = 3.14159265358979323846;

vec3 world_position(vec2 texcoord, float depth) {
    vec4 world = frame.inverse_view_projection * vec4(texcoord.x * 2.0 - 1.0, 1.0 - texcoord.y * 2.0, depth, 1.0);
    return world.xyz / world.w;
}

float linear_depth(float depth) {
    float near_plane = frame.gtao_parameters.y;
    float far_plane = frame.gtao_parameters.z;
    return frame.gtao_parameters.w > 0.5
        ? near_plane * far_plane / (near_plane + depth * (far_plane - near_plane))
        : mix(far_plane, near_plane, depth);
}

float shadow_depth_sample(int cascade, vec2 coordinates) {
    switch (cascade) {
        case 0: return texture(shadow_map_0, coordinates).r;
        case 1: return texture(shadow_map_1, coordinates).r;
        case 2: return texture(shadow_map_2, coordinates).r;
        default: return texture(shadow_map_3, coordinates).r;
    }
}

vec3 shadow_visibility_cascade(vec3 position, int cascade) {
    vec4 clip = frame.shadow_matrices[cascade] * vec4(position, 1.0);
    vec3 projected = clip.xyz / clip.w;
    vec2 coordinates = vec2(projected.x * 0.5 + 0.5, -projected.y * 0.5 + 0.5);
    if (projected.z < 0.0 || projected.z > 1.0 || any(lessThan(coordinates, vec2(0.0))) || any(greaterThan(coordinates, vec2(1.0)))) return vec3(1.0);
    float visibility = projected.z - 0.00005 > shadow_depth_sample(cascade, coordinates) ? 0.0 : 1.0;
#ifdef X173GE_MBOIT
    if (frame.shadow_parameters.w > 0.5) {
        visibility *= mboit_visibility(cascade, coordinates, projected.z * 2.0 - 1.0);
    }
#endif
    return vec3(visibility);
}

vec3 shadow_visibility(vec3 position) {
    if (frame.light_count.w == 0 || frame.light_count.z == 0) return vec3(1.0);
    float depth = dot(position - frame.camera_position.xyz, frame.camera_forward.xyz);
    int cascade = 0;
    while (cascade < 3 && depth > frame.cascade_splits[cascade]) ++cascade;
    if (depth > frame.cascade_splits[3]) return vec3(1.0);
    vec3 visibility = shadow_visibility_cascade(position, cascade);
    if (cascade == 3 || depth < frame.shadow_depth[cascade].w) return visibility;
    float blend = smoothstep(frame.shadow_depth[cascade].w, frame.cascade_splits[cascade], depth);
    return mix(visibility, shadow_visibility_cascade(position, cascade + 1), blend);
}

bool depth_edge(ivec2 start, int block, ivec2 extent) {
    ivec2 end = min(start + block, extent);
    if (any(lessThan(end - start, ivec2(block)))) return true;
    float minimum = 3.402823e38;
    float maximum = 0.0;
    bool geometry = false;
    bool background = false;
    for (int y = start.y; y < end.y; ++y) for (int x = start.x; x < end.x; ++x) {
        float depth = texelFetch(scene_depth, ivec2(x, y), 0).r;
        if (depth <= 0.000001) background = true;
        else {
            geometry = true;
            float z = linear_depth(depth);
            minimum = min(minimum, z);
            maximum = max(maximum, z);
        }
    }
    return !geometry || background || maximum - minimum > max(0.01, minimum * 0.01);
}

float phase_hg(float cosine) {
    float g = frame.god_rays_parameters.z;
    float denominator = 1.0 + g * g - 2.0 * g * cosine;
    return (1.0 - g * g) / (4.0 * PI * denominator * sqrt(denominator));
}

void main() {
    ivec2 extent = textureSize(scene_depth, 0);
    ivec2 pixel = ivec2(gl_FragCoord.xy);
    int block = int(frame.god_rays_counts.y);
    ivec2 block_start = pixel / block * block;
    bool edge = depth_edge(block_start, block, extent);
    float depth = texelFetch(scene_depth, pixel, 0).r;
    bool perspective = frame.gtao_parameters.w > 0.5;
    vec3 eye = perspective ? frame.camera_position.xyz : world_position(uv, 1.0);
    vec3 surface = depth <= 0.000001
        ? world_position(uv, 0.0)
        : world_position(uv, depth);
    vec3 to_surface = surface - eye;
    float distance_to_surface = length(to_surface);
    if (!perspective) distance_to_surface = max(dot(to_surface, frame.camera_forward.xyz), 0.0);
    if (distance_to_surface <= 0.0 || frame.light_count.x == 0) {
        out_scattering = vec4(0.0, 0.0, 0.0, edge ? 1.0 : 0.0);
        return;
    }

    int light_index = frame.light_count.w != 0 ? int(frame.light_count.y) : 0;
    vec3 direction_to_light = normalize(frame.lights[light_index * 2].xyz);
    vec3 radiance = frame.lights[light_index * 2 + 1].rgb;
    uint count = frame.god_rays_counts.x;
    uint block_area = uint(block * block);
    uint first = edge ? 0 : uint((pixel.y - block_start.y) * block + pixel.x - block_start.x);
    uint stride = edge ? 1 : block_area;
    float extinction = frame.god_rays_parameters.x;
    float scattering = extinction * frame.god_rays_parameters.y;
    float step_length = distance_to_surface / float(count);
    vec3 scatter = vec3(0.0);
    for (uint sample_index = first; sample_index < count; sample_index += stride) {
        float along = (float(sample_index) + 0.5) * step_length;
        vec3 position = surface - normalize(to_surface) * along;
        vec3 to_eye = normalize(eye - position);
        float phase = phase_hg(dot(-direction_to_light, to_eye));
        float transmittance = exp(-extinction * (distance_to_surface - along));
        scatter += radiance * (scattering * phase * shadow_visibility(position) * transmittance * step_length);
    }
    out_scattering = vec4(scatter, edge ? 1.0 : 0.0);
}
