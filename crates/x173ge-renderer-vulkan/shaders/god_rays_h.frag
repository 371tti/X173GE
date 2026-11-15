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
layout(set = 1, binding = 0) uniform sampler2D input_scattering;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_scattering;

void main() {
    ivec2 extent = textureSize(input_scattering, 0);
    ivec2 pixel = ivec2(gl_FragCoord.xy);
    int block = int(frame.god_rays_counts.y);
    ivec2 start = pixel / block * block;
    ivec2 end = min(start + ivec2(block, 1), extent);
    vec4 current = texelFetch(input_scattering, pixel, 0);
    if (current.a > 0.5) {
        out_scattering = current;
        return;
    }
    vec3 sum = vec3(0.0);
    for (int x = start.x; x < end.x; ++x) sum += texelFetch(input_scattering, ivec2(x, pixel.y), 0).rgb;
    out_scattering = vec4(sum, 0.0);
}
