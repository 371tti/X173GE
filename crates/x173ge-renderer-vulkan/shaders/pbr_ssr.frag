#version 450
#extension GL_GOOGLE_include_directive : require

const float PI = 3.14159265358979323846;
const float GTAO_STRENGTH = 0.25;

struct DirectionalLight {
    vec4 direction_to_light;
    vec4 radiance;
};
struct PunctualLight {
    vec4 position_range;
    vec4 direction_type;
    vec4 color_intensity;
    vec4 cone_cosines;
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
    vec4 god_rays_parameters;
    uvec4 god_rays_counts;
    PunctualLight punctual_lights[16];
    uvec4 punctual_count;
    vec4 environment_irradiance_sh[9];
    vec4 environment_parameters; // diffuse intensity, positive yaw, reserved, enabled
} frame;

layout(set = 0, binding = 1) uniform sampler2D shadow_map_0;
layout(set = 0, binding = 2) uniform sampler2D shadow_map_1;
layout(set = 0, binding = 3) uniform sampler2D shadow_map_2;
layout(set = 0, binding = 4) uniform sampler2D shadow_map_3;
layout(set = 0, binding = 5) uniform sampler2D gtao_texture;
layout(set = 1, binding = 0) uniform sampler2D base_color_texture;
layout(set = 1, binding = 1) uniform sampler2D metallic_roughness_texture;
layout(set = 1, binding = 2) uniform sampler2D normal_texture;
layout(set = 1, binding = 3) uniform sampler2D occlusion_texture;
layout(set = 1, binding = 4) uniform sampler2D emissive_texture;
layout(set = 1, binding = 5, std140) uniform Material {
    vec4 base_color_factor;
    vec4 pbr; // metallic, roughness, emissive strength, specular strength
    vec4 emissive_factor;
    vec4 specular_color_factor;
    vec4 surface; // normal scale, occlusion strength, alpha cutoff, transmission factor
    uvec4 flags; // alpha mode, double sided, unlit, has normal texture
} material;

#include "environment.glsl"

layout(location = 0) in vec3 world_position;
layout(location = 1) in vec3 world_normal;
layout(location = 2) in vec2 uv_0;
layout(location = 3) in vec4 vertex_color;
layout(location = 4) in vec4 world_tangent;
layout(location = 0) out vec4 out_color;
layout(location = 1) out vec4 out_normal_roughness;
layout(location = 2) out vec4 out_reflectance;

vec3 fresnel_schlick(float voh, vec3 f0) {
    return f0 + (1.0 - f0) * pow(1.0 - clamp(voh, 0.0, 1.0), 5.0);
}

#include "shadow_visibility.glsl"
float distribution_ggx(float noh, float alpha2) {
    float denominator = noh * noh * (alpha2 - 1.0) + 1.0;
    return alpha2 / (PI * denominator * denominator);
}

float smith_ggx_g1(float nox, float alpha2) {
    return 2.0 * nox /
        (nox + sqrt(alpha2 + (1.0 - alpha2) * nox * nox));
}

float ambient_visibility() {
    if (frame.gtao_parameters.x < 0.5) return 1.0;
    return texelFetch(gtao_texture, ivec2(gl_FragCoord.xy), 0).r;
}

#include "pbr_direct.glsl"

void main() {
    vec3 world_dx = dFdx(world_position);
    vec3 world_dy = dFdy(world_position);
    vec3 n = normalize(world_normal);
    if (material.flags.y != 0u && !gl_FrontFacing) n = -n;
    if (material.flags.w != 0u) {
        vec3 t = normalize(world_tangent.xyz - n * dot(n, world_tangent.xyz));
        vec3 b = cross(n, t) * world_tangent.w;
        vec3 tangent_normal = texture(normal_texture, uv_0).xyz * 2.0 - 1.0;
        tangent_normal.xy *= material.surface.x;
        n = normalize(mat3(t, b, n) * tangent_normal);
    }
    vec3 v = normalize(frame.camera_position.xyz - world_position);
    vec4 base_sample = texture(base_color_texture, uv_0);
    vec4 metallic_roughness_sample = texture(metallic_roughness_texture, uv_0);
    float material_alpha = material.base_color_factor.a * vertex_color.a * base_sample.a;
    if (material.flags.x == 1u && material_alpha < material.surface.z) discard;
    if (material.flags.z != 0u) {
        vec3 base = material.base_color_factor.rgb * vertex_color.rgb * base_sample.rgb;
        vec3 emissive = material.emissive_factor.rgb * texture(emissive_texture, uv_0).rgb
            * material.pbr.z;
        out_color = vec4(base + emissive, material.flags.x == 2u ? material_alpha : 1.0);
        out_normal_roughness = material.flags.x == 2u ? vec4(0.0) : vec4(n * 0.5 + 0.5, 1.0);
        out_reflectance = vec4(0.0);
        return;
    }
    vec3 base_color = material.base_color_factor.rgb * vertex_color.rgb * base_sample.rgb;
    float metallic = clamp(material.pbr.x * metallic_roughness_sample.b, 0.0, 1.0);
    float roughness = clamp(material.pbr.y * metallic_roughness_sample.g, 0.04, 1.0);
    float alpha = roughness * roughness;
    float alpha2 = alpha * alpha;
    vec3 dielectric_f0 = clamp(0.04 * material.specular_color_factor.rgb, 0.0, 1.0);
    vec3 emissive = material.emissive_factor.rgb * texture(emissive_texture, uv_0).rgb
        * material.pbr.z;
    vec3 radiance = emissive;
    float material_ao = mix(1.0, texture(occlusion_texture, uv_0).r, material.surface.y);
    float diffuse_energy = 1.0 - material.pbr.w * max(max(dielectric_f0.r, dielectric_f0.g), dielectric_f0.b);
    vec3 diffuse_albedo = (1.0 - metallic) * diffuse_energy * base_color;
    float ao = ambient_visibility();
    vec3 bounce_visibility = vec3(ao)
        / max(vec3(1.0) - diffuse_albedo * (1.0 - ao), vec3(1e-4));
    vec3 ambient_visibility = mix(vec3(1.0), bounce_visibility, GTAO_STRENGTH);
    vec3 diffuse_irradiance = environment_irradiance(n);
    radiance += diffuse_irradiance * diffuse_albedo * ambient_visibility * material_ao / PI;
    for (uint i = 0; i < frame.light_count.x; ++i) {
        vec3 l = normalize(frame.lights[i].direction_to_light.xyz);
        float nol = max(dot(n, l), 0.0);
        float nov = max(dot(n, v), 0.0);
        if (nol <= 0.0 || nov <= 0.0) continue;
        vec3 visibility = frame.light_count.w != 0u && i == frame.light_count.y
            ? shadow_visibility(world_position, n, l, world_dx, world_dy)
            : vec3(1.0);
        radiance += frame.lights[i].radiance.rgb * visibility * direct_brdf(
            n, v, base_color, metallic, alpha2, dielectric_f0, diffuse_energy,
            material.pbr.w, l
        );
    }
    radiance += punctual_lighting(
        n, v, world_position, base_color, metallic, alpha2, dielectric_f0, diffuse_energy,
        material.pbr.w
    );

    out_color = vec4(radiance, material.flags.x == 2u ? material_alpha : 1.0);
    out_normal_roughness = material.flags.x == 2u
        ? vec4(0.0)
        : vec4(n * 0.5 + 0.5, roughness);
    out_reflectance = material.flags.x == 2u
        ? vec4(0.0)
        : vec4(mix(dielectric_f0 * material.pbr.w, base_color, metallic),
               mix(material.pbr.w, 1.0, metallic));
}
