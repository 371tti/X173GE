#version 450

// Input is linear display RGB. SMAA's luma detector compares gamma-encoded
// values; keep this source texture non-sRGB to avoid a second decode.
layout(set = 0, binding = 0) uniform sampler2D post_color;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec2 out_edges;

const vec3 LUMA = vec3(0.2126, 0.7152, 0.0722);
const float EDGE_THRESHOLD = 0.1;
const float LOCAL_CONTRAST = 2.0;

ivec2 clamp_pixel(ivec2 pixel, ivec2 extent) {
    return clamp(pixel, ivec2(0), extent - ivec2(1));
}

float srgb_luma(ivec2 pixel, ivec2 extent) {
    vec3 linear_rgb = max(texelFetch(post_color, clamp_pixel(pixel, extent), 0).rgb, vec3(0.0));
    vec3 encoded = mix(12.92 * linear_rgb,
                       1.055 * pow(linear_rgb, vec3(1.0 / 2.4)) - 0.055,
                       greaterThan(linear_rgb, vec3(0.0031308)));
    return dot(encoded, LUMA);
}

void main() {
    ivec2 extent = textureSize(post_color, 0);
    ivec2 pixel = ivec2(gl_FragCoord.xy);
    float center = srgb_luma(pixel, extent);
    float left = srgb_luma(pixel + ivec2(-1, 0), extent);
    float top = srgb_luma(pixel + ivec2(0, -1), extent);
    vec2 delta = abs(center - vec2(left, top));
    vec2 edges = step(vec2(EDGE_THRESHOLD), delta);

    if (edges.x + edges.y == 0.0) {
        out_edges = vec2(0.0);
        return;
    }

    vec2 opposite = abs(center - vec2(
        srgb_luma(pixel + ivec2(1, 0), extent),
        srgb_luma(pixel + ivec2(0, 1), extent)));
    vec2 direct_max = max(delta, opposite);
    vec2 extended = abs(vec2(left, top) - vec2(
        srgb_luma(pixel + ivec2(-2, 0), extent),
        srgb_luma(pixel + ivec2(0, -2), extent)));
    float local_max = max(max(direct_max.x, direct_max.y), max(extended.x, extended.y));
    edges *= step(vec2(local_max), LOCAL_CONTRAST * delta);
    out_edges = edges;
}
