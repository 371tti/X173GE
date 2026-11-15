#version 450

// Linear HDR first level of the bloom pyramid, with normalized Karis weights.
layout(set = 0, binding = 0) uniform sampler2D source_tex;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

const vec3 LUMA = vec3(0.2126, 0.7152, 0.0722);

void main() {
    vec2 half_texel = 0.5 / vec2(textureSize(source_tex, 0));
    vec3 weighted_color = vec3(0.0);
    float weight_sum = 0.0;

    for (int y = -1; y <= 1; y += 2) {
        for (int x = -1; x <= 1; x += 2) {
            vec3 color = textureLod(source_tex, uv + half_texel * vec2(x, y), 0.0).rgb;
            float luma = dot(max(color, vec3(0.0)), LUMA);
            float weight = 1.0 / (1.0 + luma);
            weighted_color += weight * color;
            weight_sum += weight;
        }
    }

    out_color = vec4(weighted_color / weight_sum, 1.0);
}
