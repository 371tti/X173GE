#version 450

// Reconstruct the coarse pyramid level and add its same-resolution fine level.
layout(set = 0, binding = 0) uniform sampler2D coarse_bloom;
layout(set = 0, binding = 1) uniform sampler2D fine_level;
layout(push_constant) uniform BloomSettings { float intensity; float radius_uv; } settings;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

void main() {
    vec3 tent_sum = vec3(0.0);

    for (int y = -1; y <= 1; ++y) {
        float wy = y == 0 ? 2.0 : 1.0;
        for (int x = -1; x <= 1; ++x) {
            float wx = x == 0 ? 2.0 : 1.0;
            vec2 offset = settings.radius_uv * vec2(x, y);
            tent_sum += (wx * wy) * textureLod(coarse_bloom, uv + offset, 0.0).rgb;
        }
    }

    vec3 reconstructed = tent_sum * (1.0 / 16.0);
    vec3 fine = textureLod(fine_level, uv, 0.0).rgb;
    out_color = vec4(fine + reconstructed, 1.0);
}
