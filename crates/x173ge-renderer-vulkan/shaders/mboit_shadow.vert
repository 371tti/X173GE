#version 450

layout(push_constant, std430) uniform ShadowDraw {
    mat4 model;
    mat4 light_view_projection;
} draw;

layout(location = 0) in vec3 in_position;
layout(location = 2) in vec2 in_uv_0;
layout(location = 3) in vec4 in_color_0;
layout(location = 2) out vec2 uv_0;
layout(location = 3) out vec4 vertex_color;

void main() {
    uv_0 = in_uv_0;
    vertex_color = in_color_0;
    vec4 clip = draw.light_view_projection * draw.model * vec4(in_position, 1.0);
    gl_Position = vec4(clip.x, -clip.y, clip.z, clip.w);
}
