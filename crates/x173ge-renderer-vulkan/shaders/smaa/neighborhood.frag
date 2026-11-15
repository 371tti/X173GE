#version 450

// Keep PostColor and output in linear RGB. The swapchain performs the final
// sRGB encoding; blending here must not happen in gamma space.
layout(set = 0, binding = 0) uniform sampler2D post_color;
layout(set = 0, binding = 1) uniform sampler2D blend_tex;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

void main() {
    vec2 texel = 1.0 / vec2(textureSize(post_color, 0));
    vec4 weights;
    weights.x = textureLod(blend_tex, uv + vec2(texel.x, 0.0), 0.0).a;
    weights.y = textureLod(blend_tex, uv + vec2(0.0, texel.y), 0.0).g;
    vec2 current = textureLod(blend_tex, uv, 0.0).xz;
    weights.wz = current;

    if (dot(weights, vec4(1.0)) < 1e-5) {
        out_color = textureLod(post_color, uv, 0.0);
        return;
    }

    bool horizontal = max(weights.x, weights.z) > max(weights.y, weights.w);
    vec4 offset = vec4(0.0, weights.y, 0.0, weights.w);
    vec2 blend = weights.yw;
    if (horizontal) {
        offset = vec4(weights.x, 0.0, weights.z, 0.0);
        blend = weights.xz;
    }
    blend /= dot(blend, vec2(1.0));
    vec4 coord = uv.xyxy + offset * vec4(texel, -texel);
    out_color = blend.x * textureLod(post_color, coord.xy, 0.0)
              + blend.y * textureLod(post_color, coord.zw, 0.0);
}
