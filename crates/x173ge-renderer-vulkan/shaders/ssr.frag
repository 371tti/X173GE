#version 450
#extension GL_GOOGLE_include_directive : require

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
    vec4 lights[32];
    vec4 god_rays_parameters;
    uvec4 god_rays_counts;
    vec4 punctual_lights[64];
    uvec4 punctual_count;
    vec4 environment_irradiance_sh[9];
    vec4 environment_parameters;
} frame;

layout(set = 0, binding = 6) uniform sampler2D scene_depth;
layout(set = 0, binding = 7) uniform sampler2D scene_normal_roughness;
layout(set = 0, binding = 9) uniform sampler2D scene_reflectance;
layout(set = 1, binding = 0) uniform sampler2D scene_color;
layout(push_constant) uniform SsrSettings {
    vec2 ray; // max world distance, camera-forward depth thickness
    uint max_steps;
    float max_roughness;
    vec2 resolve; // intensity, edge-fade width in pixels
    vec2 padding;
} settings;

layout(location = 0) out vec4 out_color;

vec3 reconstruct_world(vec2 pixel, float depth, ivec2 extent) {
    vec2 uv = (pixel + 0.5) / vec2(extent);
    vec4 clip = vec4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth, 1.0);
    vec4 world = frame.inverse_view_projection * clip;
    return world.xyz / world.w;
}

float camera_depth(vec3 world) {
    return dot(world - frame.camera_position.xyz, frame.camera_forward.xyz);
}

bool clip_viewport(inout vec2 p1, inout vec3 q1, inout float k1, vec2 p0, vec3 q0, float k0, vec2 extent) {
    vec2 edge_min = vec2(0.5);
    vec2 edge_max = extent - 0.5;
    float alpha = 0.0;
    if (p1.x < edge_min.x) alpha = max(alpha, (p1.x - edge_min.x) / (p1.x - p0.x));
    if (p1.x > edge_max.x) alpha = max(alpha, (p1.x - edge_max.x) / (p1.x - p0.x));
    if (p1.y < edge_min.y) alpha = max(alpha, (p1.y - edge_min.y) / (p1.y - p0.y));
    if (p1.y > edge_max.y) alpha = max(alpha, (p1.y - edge_max.y) / (p1.y - p0.y));
    alpha = clamp(alpha, 0.0, 1.0);
    p1 = mix(p1, p0, alpha);
    q1 = mix(q1, q0, alpha);
    k1 = mix(k1, k0, alpha);
    return alpha < 1.0;
}

bool trace_reflection(
    vec3 origin, vec3 direction, ivec2 origin_pixel, ivec2 extent,
    out vec2 hit_position
) {
    float near_depth = frame.gtao_parameters.y;
    float start_depth = camera_depth(origin);
    float depth_direction = dot(direction, frame.camera_forward.xyz);
    if (start_depth <= near_depth || settings.ray.x <= 0.0 || settings.max_steps == 0u) return false;

    float ray_length = settings.ray.x;
    if (depth_direction < 0.0) {
        ray_length = min(ray_length, (near_depth - start_depth) / depth_direction);
    }
    if (ray_length <= 0.0) return false;
    vec3 end = origin + direction * ray_length;

    vec4 h0 = frame.view_projection * vec4(origin, 1.0);
    vec4 h1 = frame.view_projection * vec4(end, 1.0);
    if (h0.w <= 1e-6 || h1.w <= 1e-6) return false;
    float k0 = 1.0 / h0.w;
    float k1 = 1.0 / h1.w;
    vec3 q0 = origin * k0;
    vec3 q1 = end * k1;
    vec2 p0 = vec2(h0.x * k0 * 0.5 + 0.5, 0.5 - h0.y * k0 * 0.5) * vec2(extent);
    vec2 p1 = vec2(h1.x * k1 * 0.5 + 0.5, 0.5 - h1.y * k1 * 0.5) * vec2(extent);
    if (!clip_viewport(p1, q1, k1, p0, q0, k0, vec2(extent))) return false;

    vec2 delta = p1 - p0;
    if (dot(delta, delta) < 1e-4) p1.x += 0.01;
    delta = p1 - p0;
    bool permute = abs(delta.x) < abs(delta.y);
    if (permute) {
        delta = delta.yx;
        p0 = p0.yx;
        p1 = p1.yx;
    }
    if (abs(delta.x) < 1e-6) return false;

    float step_dir = sign(delta.x);
    float inv_dx = step_dir / delta.x;
    vec2 dp = vec2(step_dir, delta.y * inv_dx);
    vec3 dq = (q1 - q0) * inv_dx;
    float dk = (k1 - k0) * inv_dx;
    float line_end = p1.x * step_dir;
    vec2 p = p0;
    vec3 q = q0;
    float k = k0;
    float previous_depth = start_depth;

    for (uint i = 0u; i < settings.max_steps && p.x * step_dir <= line_end; ++i) {
        vec2 sample_p = permute ? p.yx : p;
        ivec2 pixel = ivec2(floor(sample_p));
        if (any(lessThan(pixel, ivec2(0))) || any(greaterThanEqual(pixel, extent))) break;

        float half_k = k + 0.5 * dk;
        if (abs(half_k) > 1e-8) {
            float next_depth = dot(q + 0.5 * dq - frame.camera_position.xyz * half_k,
                                   frame.camera_forward.xyz) / half_k;
            float ray_min = min(previous_depth, next_depth);
            float ray_max = max(previous_depth, next_depth);
            previous_depth = next_depth;

            if (any(notEqual(pixel, origin_pixel))) {
                float depth = texelFetch(scene_depth, pixel, 0).r;
                if (depth > 0.000001) {
                    vec3 surface = reconstruct_world(vec2(pixel), depth, extent);
                    float surface_depth = camera_depth(surface);
                    if (ray_max >= surface_depth && ray_min <= surface_depth + settings.ray.y) {
                        hit_position = sample_p;
                        return true;
                    }
                }
            }
        }
        p += dp;
        q += dq;
        k += dk;
    }
    return false;
}

void main() {
    ivec2 extent = textureSize(scene_depth, 0);
    ivec2 pixel = ivec2(gl_FragCoord.xy);
    vec4 scene = texelFetch(scene_color, pixel, 0);
    vec3 base = scene.rgb;
    float depth = texelFetch(scene_depth, pixel, 0).r;
    vec4 normal_roughness = texelFetch(scene_normal_roughness, pixel, 0);
    vec4 reflectance = texelFetch(scene_reflectance, pixel, 0);
    vec3 f0 = reflectance.rgb;
    float f90 = reflectance.a;

    if (depth <= 0.000001 || max(max(f0.r, f0.g), max(f0.b, f90)) <= 1e-5
        || normal_roughness.a > settings.max_roughness || settings.resolve.x <= 0.0) {
        out_color = vec4(base, 1.0);
        return;
    }

    vec3 world = reconstruct_world(vec2(pixel), depth, extent);
    vec3 normal = normalize(normal_roughness.rgb * 2.0 - 1.0);
    vec3 view = normalize(frame.camera_position.xyz - world);
    vec3 direction = normalize(reflect(-view, normal));
    vec2 hit_position;
    if (!trace_reflection(world, direction, pixel, extent, hit_position)) {
        out_color = vec4(base, 1.0);
        return;
    }

    vec3 reflected = textureLod(scene_color, hit_position / vec2(extent), 0.0).rgb;
    float fresnel_weight = pow(1.0 - max(dot(normal, view), 0.0), 5.0);
    vec3 fresnel = f0 + (f90 - f0) * fresnel_weight;
    float edge_distance = min(min(hit_position.x, float(extent.x) - hit_position.x),
                              min(hit_position.y, float(extent.y) - hit_position.y));
    float edge_fade = settings.resolve.y > 0.0
        ? smoothstep(0.0, settings.resolve.y, edge_distance)
        : 1.0;
    out_color = vec4(base + reflected * fresnel * (settings.resolve.x * edge_fade), 1.0);
}
