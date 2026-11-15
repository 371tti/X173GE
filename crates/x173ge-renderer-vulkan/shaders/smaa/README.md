# SMAA 1x shaders

Three fullscreen fragment passes implement the luminance SMAA 1x High preset:
linear PostColor to local-contrast edges, edge/AreaTex/SearchTex to weights,
then linear neighborhood blending. Bind each source and intermediate with a
linear, clamp-to-edge sampler. `edges_tex` and `blend_tex` are normalized RG
and RGBA render targets respectively; clear both each frame.

The edge pass reads linear RGB, converts it to sRGB transfer values for luma,
and writes RG edge flags (`R` is the edge to the left, `G` is the edge above).
UV origin is top-left and Y increases downward, matching the Vulkan fullscreen
triangle. The resolve pass blends the original linear RGB. The weight pass uses
the standard 160x560 RG8 AreaTex and 64x16 R8 SearchTex packed lookup layouts.
See `assets/smaa/README.md` for lookup-table provenance and license.

Compile with `glslc --target-env=vulkan1.0 -O` and validate with `spirv-val
--target-env vulkan1.0`.
