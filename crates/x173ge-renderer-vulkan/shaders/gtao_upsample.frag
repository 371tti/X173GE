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
layout(set = 0, binding = 8) uniform sampler2D raw_ao;
layout(location = 0) out vec4 out_ao;

float view_depth(float depth) {
    float near_plane = frame.gtao_parameters.y;
    float far_plane = frame.gtao_parameters.z;
    return frame.gtao_parameters.w > 0.5
        ? near_plane * far_plane / max(near_plane + depth * (far_plane - near_plane), 1e-6)
        : mix(far_plane, near_plane, depth);
}

void main() {
    ivec2 full_size = textureSize(scene_depth, 0);
    ivec2 full_pixel = clamp(ivec2(gl_FragCoord.xy), ivec2(0), full_size - 1);
    float depth = texelFetch(scene_depth, full_pixel, 0).r;
    if (depth <= 0.000001) {
        out_ao = vec4(1.0, 0.0, 0.0, 1.0);
        return;
    }

    ivec2 ao_size = textureSize(raw_ao, 0);
    vec2 ao_pixel = vec2(full_pixel) * 0.5 - 0.5;
    ivec2 base = ivec2(floor(ao_pixel));
    vec2 fraction = fract(ao_pixel);
    float center_depth = view_depth(depth);
    vec3 normal = normalize(texelFetch(scene_normal, full_pixel, 0).rgb * 2.0 - 1.0);
    float result = 0.0;
    float weight_sum = 0.0;

    for (int y = 0; y <= 1; ++y) {
        for (int x = 0; x <= 1; ++x) {
            ivec2 ao_coord = clamp(base + ivec2(x, y), ivec2(0), ao_size - 1);
            ivec2 sample_pixel = min(ao_coord * 2 + ivec2(1), full_size - 1);
            float sample_depth = view_depth(texelFetch(scene_depth, sample_pixel, 0).r);
            vec3 sample_normal = normalize(texelFetch(scene_normal, sample_pixel, 0).rgb * 2.0 - 1.0);
            vec2 axis_weight = mix(vec2(1.0) - fraction, fraction, vec2(x, y));
            float weight = axis_weight.x * axis_weight.y
                * exp(-abs(sample_depth - center_depth) / max(center_depth * 0.02, 0.001))
                * pow(max(dot(normal, sample_normal), 0.0), 8.0);
            result += texelFetch(raw_ao, ao_coord, 0).r * weight;
            weight_sum += weight;
        }
    }

    ivec2 nearest = clamp(ivec2(round(ao_pixel)), ivec2(0), ao_size - 1);
    float ao = weight_sum > 1e-5 ? result / weight_sum : texelFetch(raw_ao, nearest, 0).r;
    out_ao = vec4(ao, 0.0, 0.0, 1.0);
}
