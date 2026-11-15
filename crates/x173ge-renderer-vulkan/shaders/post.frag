#version 450

layout(set = 0, binding = 0) uniform sampler2D scene_color;
layout(set = 0, binding = 2) uniform sampler2D adapted_exposure;
layout(push_constant) uniform PostSettings { float intensity; float radius_uv; float contrast; } settings;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

void main() {
    float exposure = texelFetch(adapted_exposure, ivec2(0), 0).r;
    vec3 hdr = max(texture(scene_color, uv).rgb * exposure, vec3(0.0));
    vec3 mapped = vec3(1.0) - 1.0 / (vec3(1.0) + hdr);
    const float pivot = 0.18 / 1.18;
    mapped = clamp(vec3(pivot) + settings.contrast * (mapped - vec3(pivot)), 0.0, 1.0);
    out_color = vec4(mapped, 1.0);
}
