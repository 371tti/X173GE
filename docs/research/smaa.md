# SMAA

## Source and scope

Jorge Jimenez et al., “SMAA: Enhanced Subpixel Morphological Antialiasing,” Computer Graphics Forum / Eurographics 2012. [Paper](https://graphics.unizar.es/papers/SMAA-Enhanced-Subpixel-Morphological-Antialiasing.pdf) and [author reference code and lookup textures](https://github.com/iryoku/smaa). The reference code and lookup textures are MIT-licensed; preserve its copyright/license notice if any of it is reused.

## Chosen first variant

Use spatial SMAA 1x High, with luminance edge detection. Follow the full three-pass structure:
1. Detect left/top edges into a two-channel edge mask.
2. Search edge patterns and use the Area and Search lookup textures to calculate blending weights, including the reference pattern/corner handling.
3. Apply neighborhood blending using those weights.

The shader choices are threshold `0.1`, local contrast factor `2`, at most 16 horizontal/vertical search steps, 8 diagonal steps, 25% corner rounding, and subsample index `0`. Edge directions use R for the edge to the left and G for the edge above. The fullscreen UV origin is top-left with Y increasing down, matching the Vulkan viewport used here.

The scene remains HDR through the tone-map pass. SMAA reads linear display RGB in a non-sRGB sampled image. The edge pass applies the sRGB transfer function before luma detection, while the neighborhood pass blends linear RGB and writes to the sRGB swapchain. Per-surface targets hold linear display color, RGBA8 edge values, and RGBA8 weights; each intermediate is cleared every frame. The official 160x560 RG8 AreaTex and 64x16 R8 SearchTex payloads are stored under `crates/x173ge-renderer-vulkan/assets/smaa/` with their MIT notice and attribution.

Keep lookup texture formats, normalized coordinates, and search limits matched to the reference. The paper’s subpixel pattern handling depends on the lookup data; replacing it with a local edge-direction guess is a different, lower-quality method.

## Assumptions and deliberate omissions

Luminance detection is a lower-cost choice recommended by the reference for initial integration, but it can miss chroma-only boundaries. Add color detection only if the visual tests establish that need. Do not start with SMAA T2x or temporal predication. SMAA is spatial and cannot remove temporal shimmer from undersampled textures.

## Visual validation

Use static and moving one-pixel diagonals, subpixel wires, a high-contrast silhouette, texture-only edges, and a red/green boundary with similar luminance. Inspect line preservation, corner over-blending, halos, and temporal shimmer while panning. Compare against the reference SMAA output and an unfiltered capture at the same resolution.

## Integration status

The three shaders compile for Vulkan 1.0 and pass `spirv-val`. The Vulkan validation smoke completed with SMAA enabled and disabled, both with and without shadow maps, including resize and shutdown without validation messages. Visual output has not yet been captured and compared against the reference; no image-quality result is claimed.
