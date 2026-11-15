float shadow_resolution(int cascade) {
    switch (cascade) {
        case 0: return float(textureSize(shadow_map_0, 0).x);
        case 1: return float(textureSize(shadow_map_1, 0).x);
        case 2: return float(textureSize(shadow_map_2, 0).x);
        default: return float(textureSize(shadow_map_3, 0).x);
    }
}
