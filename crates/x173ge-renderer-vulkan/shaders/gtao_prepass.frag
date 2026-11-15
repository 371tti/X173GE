#version 450

layout(location = 1) in vec3 world_normal;
layout(location = 2) in vec2 uv_0;
layout(location = 3) in vec4 vertex_color;
layout(location = 0) out vec4 out_normal;

layout(set = 1, binding = 0) uniform sampler2D base_color_texture;
layout(set = 1, binding = 5, std140) uniform Material {
    vec4 base_color_factor;
    vec4 pbr;
    vec4 emissive_factor;
    vec4 specular_color_factor;
    vec4 surface;
    uvec4 flags;
} material;

void main() {
    float alpha = material.base_color_factor.a * vertex_color.a * texture(base_color_texture, uv_0).a;
    if (material.flags.x == 1u && alpha < material.surface.z) discard;
    vec3 normal = normalize(world_normal);
    if (material.flags.y != 0u && !gl_FrontFacing) normal = -normal;
    out_normal = vec4(normal * 0.5 + 0.5, 1.0);
}
