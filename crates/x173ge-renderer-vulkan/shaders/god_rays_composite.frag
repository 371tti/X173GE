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
    vec4 god_rays_parameters;
    uvec4 god_rays_counts;
} frame;
layout(set = 0, binding = 6) uniform sampler2D scene_depth;
layout(set = 1, binding = 0) uniform sampler2D scene_color;
layout(set = 1, binding = 1) uniform sampler2D scattering;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

vec3 world_position(float depth) {
    vec4 world = frame.inverse_view_projection * vec4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth, 1.0);
    return world.xyz / world.w;
}

void main() {
    float depth = texelFetch(scene_depth, ivec2(gl_FragCoord.xy), 0).r;
    bool perspective = frame.gtao_parameters.w > 0.5;
    float distance_to_surface = perspective
        ? length(world_position(depth) - frame.camera_position.xyz)
        : max(dot(world_position(depth) - world_position(1.0), frame.camera_forward.xyz), 0.0);
    float transmittance = exp(-frame.god_rays_parameters.x * distance_to_surface);
    vec3 surface = texture(scene_color, uv).rgb * transmittance;
    vec3 in_scattered = texelFetch(scattering, ivec2(gl_FragCoord.xy), 0).rgb;
    out_color = vec4(surface + in_scattered, 1.0);
}
