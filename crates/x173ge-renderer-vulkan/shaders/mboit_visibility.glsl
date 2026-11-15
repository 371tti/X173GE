layout(set = 0, binding = 10) uniform sampler2D mboit_moments_0123_0;
layout(set = 0, binding = 11) uniform sampler2D mboit_moments_456_0;
layout(set = 0, binding = 12) uniform sampler2D mboit_moments_0123_1;
layout(set = 0, binding = 13) uniform sampler2D mboit_moments_456_1;
layout(set = 0, binding = 14) uniform sampler2D mboit_moments_0123_2;
layout(set = 0, binding = 15) uniform sampler2D mboit_moments_456_2;
layout(set = 0, binding = 16) uniform sampler2D mboit_moments_0123_3;
layout(set = 0, binding = 17) uniform sampler2D mboit_moments_456_3;

vec4 mboit_first_moments(int cascade, vec2 uv) {
    switch (cascade) {
        case 0: return texture(mboit_moments_0123_0, uv);
        case 1: return texture(mboit_moments_0123_1, uv);
        case 2: return texture(mboit_moments_0123_2, uv);
        default: return texture(mboit_moments_0123_3, uv);
    }
}

vec4 mboit_last_moments(int cascade, vec2 uv) {
    switch (cascade) {
        case 0: return texture(mboit_moments_456_0, uv);
        case 1: return texture(mboit_moments_456_1, uv);
        case 2: return texture(mboit_moments_456_2, uv);
        default: return texture(mboit_moments_456_3, uv);
    }
}

vec3 mboit_solve_cubic(vec4 coefficient) {
    coefficient.xyz /= coefficient.w;
    coefficient.yz /= 3.0;
    vec3 delta = vec3(
        fma(-coefficient.z, coefficient.z, coefficient.y),
        fma(-coefficient.y, coefficient.z, coefficient.x),
        dot(vec2(coefficient.z, -coefficient.y), coefficient.xy)
    );
    float discriminant = dot(vec2(4.0 * delta.x, -delta.y), delta.zy);
    vec2 depressed = vec2(fma(-2.0 * coefficient.z, delta.x, delta.y), delta.x);
    float theta = atan(sqrt(max(discriminant, 0.0)), -depressed.x) / 3.0;
    vec2 cos_sin = vec2(cos(theta), sin(theta));
    vec3 roots = vec3(
        cos_sin.x,
        dot(vec2(-0.5, -0.5 * sqrt(3.0)), cos_sin),
        dot(vec2(-0.5, 0.5 * sqrt(3.0)), cos_sin)
    );
    return fma(vec3(2.0 * sqrt(max(-depressed.y, 0.0))), roots, vec3(-coefficient.z));
}

float mboit_transmittance(float b0, vec3 even, vec3 odd, float depth) {
    if (b0 <= 1e-6) return 1.0;
    even = mix(even, vec3(0.48, 0.451, 0.45), 5e-6);
    odd = mix(odd, vec3(0.0), 5e-6);
    vec4 b = vec4(odd.x, even.x, odd.y, even.y);
    float inv_d11 = 1.0 / fma(-b.x, b.x, b.y);
    float l21_d11 = fma(-b.x, b.y, b.z);
    float l21 = l21_d11 * inv_d11;
    float d22 = fma(-l21_d11, l21, fma(-b.y, b.y, b.w));
    float l31_d11 = fma(-b.x, b.z, b.w);
    float l31 = l31_d11 * inv_d11;
    float inv_d22 = 1.0 / d22;
    float l32_d22 = fma(-l21_d11, l31, fma(-b.y, b.z, odd.z));
    float l32 = l32_d22 * inv_d22;
    float d33 = fma(-b.z, b.z, even.z) - dot(vec2(l31_d11, l32_d22), vec2(l31, l32));
    float inv_d33 = 1.0 / d33;

    vec4 c = vec4(1.0, depth, depth * depth, depth * depth * depth);
    c.y -= b.x;
    c.z -= fma(l21, c.y, b.y);
    c.w -= odd.y + dot(vec2(l31, l32), c.yz);
    c.yzw *= vec3(inv_d11, inv_d22, inv_d33);
    c.z -= l32 * c.w;
    c.y -= dot(vec2(l21, l31), c.zw);
    c.x -= dot(vec3(b.x, b.y, odd.y), c.yzw);
    vec4 z = vec4(depth, mboit_solve_cubic(c));

    vec4 f = vec4(0.0, float(z.y <= z.x), float(z.z <= z.x), float(z.w <= z.x));
    float f01 = (f.y - f.x) / (z.y - z.x);
    float f12 = (f.z - f.y) / (z.z - z.y);
    float f23 = (f.w - f.z) / (z.w - z.z);
    float f012 = (f12 - f01) / (z.z - z.x);
    float f123 = (f23 - f12) / (z.w - z.y);
    float f0123 = (f123 - f012) / (z.w - z.x);
    float p0 = fma(-f0123, z.z, f012);
    float p1 = f0123;
    float p2 = p1;
    p1 = fma(p1, -z.y, p0);
    p0 = fma(p0, -z.y, f01);
    float p3 = p2;
    p2 = fma(p2, -z.x, p1);
    p1 = fma(p1, -z.x, p0);
    p0 = fma(p0, -z.x, 0.0);
    float absorbance = dot(vec4(p0, p1, p2, p3), vec4(1.0, b.x, b.y, odd.y));
    return clamp(exp(-b0 * absorbance), 0.0, 1.0);
}

float mboit_visibility(int cascade, vec2 uv, float depth) {
    if (any(lessThan(uv, vec2(0.0))) || any(greaterThan(uv, vec2(1.0)))) return 1.0;
    vec4 first = mboit_first_moments(cascade, uv);
    vec4 last = mboit_last_moments(cascade, uv);
    float b0 = first.x;
    if (b0 <= 1e-6) return 1.0;
    vec3 even = vec3(first.z, last.x, last.z) / b0;
    vec3 odd = vec3(first.y, first.w, last.y) / b0;
    return mboit_transmittance(b0, even, odd, depth);
}
