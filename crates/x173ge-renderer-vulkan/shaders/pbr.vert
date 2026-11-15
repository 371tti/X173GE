#version 450

struct DirectionalLight {
    vec4 direction_to_light;
    vec4 radiance;
};

layout(set = 0, binding = 0, std140) uniform Frame {
    mat4 view_projection;
    mat4 inverse_view_projection;
    vec4 camera_position;
    vec4 ambient_diffuse;
    vec4 ambient_diffuse_y;
    vec4 camera_forward;
    vec4 gtao_parameters;
    uvec4 light_count;
    vec4 cascade_splits;
    mat4 shadow_matrices[4];
    vec4 shadow_depth[4];
    vec4 shadow_parameters;
    DirectionalLight lights[16];
} frame;

layout(push_constant, std430) uniform Draw { mat4 model; } draw;

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec3 in_normal;
layout(location = 2) in vec2 in_uv_0;
layout(location = 3) in vec4 in_color_0;
layout(location = 4) in vec4 in_tangent;

layout(location = 0) out vec3 world_position;
layout(location = 1) out vec3 world_normal;
layout(location = 2) out vec2 uv_0;
layout(location = 3) out vec4 vertex_color;
layout(location = 4) out vec4 world_tangent;

void main() {
    vec4 position = draw.model * vec4(in_position, 1.0);
    world_position = position.xyz;
    world_normal = normalize(transpose(inverse(mat3(draw.model))) * in_normal);
    if (in_tangent.w == 0.0) {
        world_tangent = vec4(0.0);
    } else {
        vec3 tangent = mat3(draw.model) * in_tangent.xyz;
        tangent = normalize(tangent - world_normal * dot(world_normal, tangent));
        world_tangent = vec4(tangent, in_tangent.w * sign(determinant(mat3(draw.model))));
    }
    uv_0 = in_uv_0;
    vertex_color = in_color_0;
    vec4 clip_position = frame.view_projection * position;
    gl_Position = vec4(clip_position.x, -clip_position.y, clip_position.z, clip_position.w);
}
