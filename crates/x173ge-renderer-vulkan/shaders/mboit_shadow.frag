#version 450

layout(set = 1, binding = 0) uniform sampler2D base_color_texture;
layout(set = 1, binding = 5, std140) uniform Material {
    vec4 base_color_factor;
    vec4 pbr;
    vec4 emissive_factor;
    vec4 specular_color_factor;
    vec4 surface;
    uvec4 flags;
} material;

layout(location = 2) in vec2 uv_0;
layout(location = 3) in vec4 vertex_color;
layout(location = 0) out vec4 moments_0123;
layout(location = 1) out vec4 moments_456;

void main() {
    float alpha = clamp(material.base_color_factor.a * vertex_color.a
        * texture(base_color_texture, uv_0).a, 0.0, 0.999);
    if (material.flags.x != 2u || alpha < 1e-6) discard;

    float absorbance = -log(1.0 - alpha);
    float z = gl_FragCoord.z * 2.0 - 1.0;
    float z2 = z * z;
    float z3 = z2 * z;
    float z4 = z2 * z2;
    moments_0123 = absorbance * vec4(1.0, z, z2, z3);
    moments_456 = absorbance * vec4(z4, z4 * z, z4 * z2, 0.0);
}
