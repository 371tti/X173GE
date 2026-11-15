vec3 direct_brdf(
    vec3 n,
    vec3 v,
    vec3 base_color,
    float metallic,
    float alpha2,
    vec3 dielectric_f0,
    float diffuse_energy,
    float specular_scale,
    vec3 l
) {
    float nol = max(dot(n, l), 0.0);
    float nov = max(dot(n, v), 0.0);
    if (nol <= 0.0 || nov <= 0.0) return vec3(0.0);
    vec3 h = normalize(l + v);
    float noh = max(dot(n, h), 0.0);
    float voh = max(dot(v, h), 0.0);
    vec3 f = mix(
        fresnel_schlick(voh, dielectric_f0) * specular_scale,
        fresnel_schlick(voh, base_color),
        metallic
    );
    float d = distribution_ggx(noh, alpha2);
    float g = smith_ggx_g1(nol, alpha2) * smith_ggx_g1(nov, alpha2);
    vec3 diffuse = (1.0 - metallic) * base_color * diffuse_energy / PI;
    vec3 specular = f * (d * g / max(4.0 * nol * nov, 1e-5));
    return nol * (diffuse + specular);
}

vec3 punctual_lighting(
    vec3 n,
    vec3 v,
    vec3 world_position,
    vec3 base_color,
    float metallic,
    float alpha2,
    vec3 dielectric_f0,
    float diffuse_energy,
    float specular_scale
) {
    vec3 result = vec3(0.0);
    for (uint i = 0; i < frame.punctual_count.x; ++i) {
        PunctualLight light = frame.punctual_lights[i];
        vec3 delta = light.position_range.xyz - world_position;
        float distance_squared = dot(delta, delta);
        if (isnan(distance_squared) || isinf(distance_squared) || distance_squared <= 1e-8) {
            continue;
        }
        float distance = sqrt(distance_squared);
        float range = light.position_range.w;
        if (range > 0.0 && distance >= range) continue;
        float attenuation = 1.0 / distance_squared;
        if (range > 0.0) {
            float ratio = distance / range;
            float ratio_squared = ratio * ratio;
            attenuation *= clamp(1.0 - ratio_squared * ratio_squared, 0.0, 1.0);
        }
        vec3 l = delta * inversesqrt(distance_squared);
        if (light.direction_type.w > 0.5) {
            float cone_width = max(light.cone_cosines.x - light.cone_cosines.y, 1e-20);
            float cone = clamp(
                (dot(light.direction_type.xyz, -l) - light.cone_cosines.y) / cone_width,
                0.0,
                1.0
            );
            attenuation *= cone * cone;
        }
        result += light.color_intensity.rgb * light.color_intensity.a * attenuation
            * direct_brdf(
                n, v, base_color, metallic, alpha2, dielectric_f0, diffuse_energy,
                specular_scale, l
            );
    }
    return result;
}
