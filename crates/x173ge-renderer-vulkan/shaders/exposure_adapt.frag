#version 450

layout(set = 0, binding = 0) uniform sampler2D average_log_luminance;
layout(set = 0, binding = 1) uniform sampler2D previous_exposure;
layout(push_constant) uniform ExposureSettings {
    float reset;
    float seconds;
    float key_luminance;
    float min_exposure;
    float max_exposure;
    float adaptation_seconds;
} settings;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_value;

void main() {
    if (settings.reset > 0.5) {
        out_value = vec4(1.0);
        return;
    }
    float target = clamp(settings.key_luminance / exp2(texelFetch(average_log_luminance, ivec2(0), 0).r), settings.min_exposure, settings.max_exposure);
    float previous = texelFetch(previous_exposure, ivec2(0), 0).r;
    float weight = 1.0 - exp(-max(settings.seconds, 0.0) / settings.adaptation_seconds);
    out_value = vec4(mix(previous, target, weight));
}
