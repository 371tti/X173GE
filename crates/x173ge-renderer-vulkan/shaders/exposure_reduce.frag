#version 450

layout(set = 0, binding = 0) uniform sampler2D source_color;
layout(push_constant) uniform ExposureSettings { float first_level; float unused; } settings;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_value;

void main() {
    ivec2 size = textureSize(source_color, 0);
    ivec2 base = ivec2(gl_FragCoord.xy) * 2;
    vec2 total = vec2(0.0);

    for (int y = 0; y < 2; ++y) {
        for (int x = 0; x < 2; ++x) {
            ivec2 p = base + ivec2(x, y);
            if (any(greaterThanEqual(p, size))) continue;
            vec2 value;
            if (settings.first_level > 0.5) {
                vec3 hdr = max(texelFetch(source_color, p, 0).rgb, vec3(0.0));
                value = vec2(log2(max(dot(hdr, vec3(0.2126, 0.7152, 0.0722)), 1e-6)), 1.0);
            } else {
                value = texelFetch(source_color, p, 0).rg;
            }
            total += vec2(value.x * value.y, value.y);
        }
    }
    out_value = vec4(total.x / max(total.y, 1e-6), total.y * 0.25, 0.0, 1.0);
}
