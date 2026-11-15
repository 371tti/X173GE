#version 450

// Later linear HDR pyramid levels use a normalized 2x2 box downsample.
layout(set = 0, binding = 0) uniform sampler2D source_tex;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

void main() {
    vec2 half_texel = 0.5 / vec2(textureSize(source_tex, 0));
    vec3 sum = vec3(0.0);

    for (int y = -1; y <= 1; y += 2) {
        for (int x = -1; x <= 1; x += 2) {
            sum += textureLod(source_tex, uv + half_texel * vec2(x, y), 0.0).rgb;
        }
    }

    out_color = vec4(sum * 0.25, 1.0);
}
