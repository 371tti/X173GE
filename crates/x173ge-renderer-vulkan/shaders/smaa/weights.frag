#version 450

// SMAA 1x, High preset. Edge/lookup samplers must be linear-filtered and clamped.
layout(set = 0, binding = 0) uniform sampler2D edges_tex;
layout(set = 0, binding = 1) uniform sampler2D area_tex;
layout(set = 0, binding = 2) uniform sampler2D search_tex;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_weights;

const int MAX_SEARCH_STEPS = 16;
const int MAX_DIAGONAL_STEPS = 8;
const float AREA_MAX_DISTANCE = 16.0;
const float DIAGONAL_MAX_DISTANCE = 20.0;
const float CORNER_ROUNDING = 0.25;
const vec2 SEARCH_SIZE = vec2(66.0, 33.0);
const vec2 SEARCH_PACKED_SIZE = vec2(64.0, 16.0);
const vec2 AREA_SIZE = vec2(160.0, 560.0);
const float AREA_SUBTEXTURE_SIZE = 1.0 / 7.0;

vec2 rt_metrics() {
    return 1.0 / vec2(textureSize(edges_tex, 0));
}

vec2 edge_at(vec2 coord) {
    return textureLod(edges_tex, coord, 0.0).rg;
}

vec2 edge_at_offset(vec2 coord, ivec2 offset) {
    return edge_at(coord + vec2(offset) * rt_metrics());
}

float search_length(vec2 edge_pair, float side) {
    vec2 scale = (SEARCH_SIZE * vec2(0.5, -1.0) + vec2(-1.0, 1.0)) / SEARCH_PACKED_SIZE;
    vec2 bias = (SEARCH_SIZE * vec2(side, 1.0) + vec2(0.5, -0.5)) / SEARCH_PACKED_SIZE;
    return textureLod(search_tex, scale * edge_pair + bias, 0.0).r;
}

float search_x_left(vec2 coord, float limit) {
    vec2 texel = rt_metrics();
    vec2 edge = vec2(0.0, 1.0);
    for (int i = 0; i < MAX_SEARCH_STEPS && coord.x > limit && edge.y > 0.8281 && edge.x == 0.0; ++i) {
        edge = edge_at(coord);
        coord -= vec2(2.0 * texel.x, 0.0);
    }
    return coord.x + texel.x * (3.25 - (255.0 / 127.0) * search_length(edge, 0.0));
}

float search_x_right(vec2 coord, float limit) {
    vec2 texel = rt_metrics();
    vec2 edge = vec2(0.0, 1.0);
    for (int i = 0; i < MAX_SEARCH_STEPS && coord.x < limit && edge.y > 0.8281 && edge.x == 0.0; ++i) {
        edge = edge_at(coord);
        coord += vec2(2.0 * texel.x, 0.0);
    }
    return coord.x - texel.x * (3.25 - (255.0 / 127.0) * search_length(edge, 0.5));
}

float search_y_up(vec2 coord, float limit) {
    vec2 texel = rt_metrics();
    vec2 edge = vec2(1.0, 0.0);
    for (int i = 0; i < MAX_SEARCH_STEPS && coord.y > limit && edge.x > 0.8281 && edge.y == 0.0; ++i) {
        edge = edge_at(coord);
        coord -= vec2(0.0, 2.0 * texel.y);
    }
    return coord.y + texel.y * (3.25 - (255.0 / 127.0) * search_length(edge.gr, 0.0));
}

float search_y_down(vec2 coord, float limit) {
    vec2 texel = rt_metrics();
    vec2 edge = vec2(1.0, 0.0);
    for (int i = 0; i < MAX_SEARCH_STEPS && coord.y < limit && edge.x > 0.8281 && edge.y == 0.0; ++i) {
        edge = edge_at(coord);
        coord += vec2(0.0, 2.0 * texel.y);
    }
    return coord.y - texel.y * (3.25 - (255.0 / 127.0) * search_length(edge.gr, 0.5));
}

vec2 area_weight(vec2 distance, float crossing_a, float crossing_b, float subsample) {
    vec2 coord = AREA_MAX_DISTANCE * round(4.0 * vec2(crossing_a, crossing_b)) + distance;
    coord = (coord + 0.5) / AREA_SIZE;
    coord.y += AREA_SUBTEXTURE_SIZE * subsample;
    return textureLod(area_tex, coord, 0.0).rg;
}

vec2 area_diagonal(vec2 distance, vec2 crossing, float subsample) {
    vec2 coord = DIAGONAL_MAX_DISTANCE * crossing + distance;
    coord = (coord + 0.5) / AREA_SIZE;
    coord.x += 0.5;
    coord.y += AREA_SUBTEXTURE_SIZE * subsample;
    return textureLod(area_tex, coord, 0.0).rg;
}

vec2 decode_diagonal(vec2 edge) {
    edge.x = round(edge.x * abs(5.0 * edge.x - 3.75));
    return round(edge);
}

vec2 search_diagonal(vec2 coord, vec2 direction, out vec2 edge) {
    vec2 texel = rt_metrics();
    float distance = -1.0;
    float edge_run = 1.0;
    edge = vec2(0.0);
    for (int i = 0; i < MAX_DIAGONAL_STEPS && distance < float(MAX_DIAGONAL_STEPS - 1) && edge_run > 0.9; ++i) {
        coord += direction * texel;
        edge = edge_at(coord);
        distance += 1.0;
        edge_run = dot(edge, vec2(0.5));
    }
    return vec2(distance, edge_run);
}

vec2 search_diagonal_crossing(vec2 coord, vec2 direction, out vec2 edge) {
    vec2 texel = rt_metrics();
    coord.x += 0.25 * texel.x;
    float distance = -1.0;
    float edge_run = 1.0;
    edge = vec2(0.0);
    for (int i = 0; i < MAX_DIAGONAL_STEPS && distance < float(MAX_DIAGONAL_STEPS - 1) && edge_run > 0.9; ++i) {
        coord += direction * texel;
        edge = decode_diagonal(edge_at(coord));
        distance += 1.0;
        edge_run = dot(edge, vec2(0.5));
    }
    return vec2(distance, edge_run);
}

vec2 diagonal_weights(vec2 coord, vec2 edge) {
    vec2 result = vec2(0.0);
    vec4 distance;
    vec2 terminal;

    if (edge.r > 0.0) {
        distance.xz = search_diagonal(coord, vec2(-1.0, 1.0), terminal);
        distance.x += float(terminal.y > 0.9);
    } else {
        distance.xz = vec2(0.0);
    }
    distance.yw = search_diagonal(coord, vec2(1.0, -1.0), terminal);
    if (distance.x + distance.y > 2.0) {
        vec2 texel = rt_metrics();
        vec4 p = coord.xyxy + texel.xyxy * vec4(-distance.x + 0.25, distance.x,
                                                 distance.y, -distance.y - 0.25);
        vec4 c;
        c.xy = edge_at(p.xy + texel * vec2(-1.0, 0.0)).rg;
        c.zw = edge_at(p.zw + texel * vec2(1.0, 0.0)).rg;
        c = vec4(round(c.y), round(c.x * abs(5.0 * c.x - 3.75)),
                 round(c.w), round(c.z * abs(5.0 * c.z - 3.75)));
        vec2 crossing = 2.0 * c.xz + c.yw;
        if (distance.z >= 0.9) crossing.x = 0.0;
        if (distance.w >= 0.9) crossing.y = 0.0;
        result += area_diagonal(distance.xy, crossing, 0.0);
    }

    distance.xz = search_diagonal_crossing(coord, vec2(-1.0, -1.0), terminal);
    if (edge_at_offset(coord, ivec2(1, 0)).r > 0.0) {
        distance.yw = search_diagonal_crossing(coord, vec2(1.0, 1.0), terminal);
        distance.y += float(terminal.y > 0.9);
    } else {
        distance.yw = vec2(0.0);
    }
    if (distance.x + distance.y > 2.0) {
        vec2 texel = rt_metrics();
        vec4 p = coord.xyxy + texel.xyxy * vec4(-distance.x, -distance.x,
                                                 distance.y, distance.y);
        vec4 c;
        c.x = edge_at(p.xy + texel * vec2(-1.0, 0.0)).g;
        c.y = edge_at(p.xy + texel * vec2(0.0, -1.0)).r;
        c.zw = edge_at(p.zw + texel * vec2(1.0, 0.0)).gr;
        vec2 crossing = 2.0 * c.xz + c.yw;
        if (distance.z >= 0.9) crossing.x = 0.0;
        if (distance.w >= 0.9) crossing.y = 0.0;
        result += area_diagonal(distance.xy, crossing, 0.0).gr;
    }
    return result;
}

void reduce_horizontal_corners(inout vec2 weight, vec4 coord, vec2 distance) {
    vec2 side = step(distance.xy, distance.yx);
    vec2 rounding = (1.0 - CORNER_ROUNDING) * side;
    rounding /= max(side.x + side.y, 1.0);
    vec2 factor = vec2(1.0);
    factor.x -= rounding.x * edge_at_offset(coord.xy, ivec2(0, 1)).r;
    factor.x -= rounding.y * edge_at_offset(coord.zw, ivec2(1, 1)).r;
    factor.y -= rounding.x * edge_at_offset(coord.xy, ivec2(0, -2)).r;
    factor.y -= rounding.y * edge_at_offset(coord.zw, ivec2(1, -2)).r;
    weight *= clamp(factor, 0.0, 1.0);
}

void reduce_vertical_corners(inout vec2 weight, vec4 coord, vec2 distance) {
    vec2 side = step(distance.xy, distance.yx);
    vec2 rounding = (1.0 - CORNER_ROUNDING) * side;
    rounding /= max(side.x + side.y, 1.0);
    vec2 factor = vec2(1.0);
    factor.x -= rounding.x * edge_at_offset(coord.xy, ivec2(1, 0)).g;
    factor.x -= rounding.y * edge_at_offset(coord.zw, ivec2(1, 1)).g;
    factor.y -= rounding.x * edge_at_offset(coord.xy, ivec2(-2, 0)).g;
    factor.y -= rounding.y * edge_at_offset(coord.zw, ivec2(-2, 1)).g;
    weight *= clamp(factor, 0.0, 1.0);
}

void main() {
    vec2 texel = rt_metrics();
    vec2 pixel_coord = uv / texel;
    vec2 edge = edge_at(uv);
    vec4 weight = vec4(0.0);

    if (edge.g > 0.0) {
        vec2 diagonal = diagonal_weights(uv, edge);
        if (diagonal.x + diagonal.y == 0.0) {
            vec2 start_left = uv + texel * vec2(-0.25, -0.125);
            vec2 start_right = uv + texel * vec2(1.25, -0.125);
            float x_left = search_x_left(start_left, start_left.x - 2.0 * float(MAX_SEARCH_STEPS) * texel.x);
            float x_right = search_x_right(start_right, start_right.x + 2.0 * float(MAX_SEARCH_STEPS) * texel.x);
            float cross_y = uv.y - 0.25 * texel.y;
            float cross_left = edge_at(vec2(x_left, cross_y)).r;
            float cross_right = edge_at(vec2(x_right, cross_y) + texel * vec2(1.0, 0.0)).r;
            vec2 distance = abs(round(vec2(x_left, x_right) / texel.x - pixel_coord.x));
            weight.rg = area_weight(sqrt(distance), cross_left, cross_right, 0.0);
            reduce_horizontal_corners(weight.rg, vec4(x_left, uv.y, x_right, uv.y), distance);
        } else {
            edge.r = 0.0;
        }
    }

    if (edge.r > 0.0) {
        vec2 start_up = uv + texel * vec2(-0.125, -0.25);
        vec2 start_down = uv + texel * vec2(-0.125, 1.25);
        float y_up = search_y_up(start_up, start_up.y - 2.0 * float(MAX_SEARCH_STEPS) * texel.y);
        float y_down = search_y_down(start_down, start_down.y + 2.0 * float(MAX_SEARCH_STEPS) * texel.y);
        float cross_x = uv.x - 0.25 * texel.x;
        float cross_top = edge_at(vec2(cross_x, y_up)).g;
        float cross_bottom = edge_at(vec2(cross_x, y_down) + texel * vec2(0.0, 1.0)).g;
        vec2 distance = abs(round(vec2(y_up, y_down) / texel.y - pixel_coord.y));
        weight.ba = area_weight(sqrt(distance), cross_top, cross_bottom, 0.0);
        reduce_vertical_corners(weight.ba, vec4(uv.x, y_up, uv.x, y_down), distance);
    }

    out_weights = weight;
}
