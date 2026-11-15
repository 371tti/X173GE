#version 450

layout(location = 0) out vec2 uv;

void main() {
    vec2 position = gl_VertexIndex == 0 ? vec2(-1.0, -1.0)
        : gl_VertexIndex == 1 ? vec2(3.0, -1.0)
        : vec2(-1.0, 3.0);
    uv = position * 0.5 + 0.5;
    gl_Position = vec4(position, 0.0, 1.0);
}
