vec3 environment_rotate_y(vec3 direction, float angle) {
    float c = cos(angle);
    float s = sin(angle);
    return vec3(c * direction.x + s * direction.z, direction.y,
                -s * direction.x + c * direction.z);
}

vec3 environment_irradiance(vec3 world_normal) {
    if (frame.environment_parameters.w < 0.5) {
        return max(frame.ambient_diffuse.rgb
            + frame.ambient_diffuse_y.rgb * world_normal.y, vec3(0.0));
    }
    vec3 n = environment_rotate_y(world_normal, -frame.environment_parameters.y);
    float x = n.x;
    float y = n.y;
    float z = n.z;
    float basis[9] = float[](
        0.282094792, 0.488602512 * y, 0.488602512 * z, 0.488602512 * x,
        1.092548431 * x * y, 1.092548431 * y * z,
        0.315391565 * (3.0 * y * y - 1.0), 1.092548431 * x * z,
        0.546274215 * (x * x - z * z)
    );
    vec3 irradiance = vec3(0.0);
    for (int i = 0; i < 9; ++i) {
        irradiance += frame.environment_irradiance_sh[i].rgb * basis[i];
    }
    return max(irradiance, vec3(0.0)) * frame.environment_parameters.x;
}

