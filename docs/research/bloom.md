# Bloom

## Source and scope

Jorge Jimenez, “Next Generation Post Processing in Call of Duty: Advanced Warfare,” SIGGRAPH 2014 Advances in Real-Time Rendering in Games course. This is a production course/talk, not a research paper. The author describes bloom as a pyramidal filter hierarchy intended to improve temporal stability and robustness. [Slides and author discussion](https://www.iryoku.com/next-generation-post-processing-in-call-of-duty-advanced-warfare/)

## Chosen first variant

Run after linear HDR scene lighting and before tone mapping. Build a downsampled mip pyramid with a proper weighted 2×2 downsample. The author’s firefly suppression uses weights w = 1/(1 + luma) at the first downsample and renormalizes the weighted RGB sum by Σw; this is a weighted average, not a color-range clamp. Reconstruct from the coarsest level toward finer levels, applying a 3×3 tent upsample and adding each coarser result. Keep the upsample radius fixed in UV space by default so the visual radius is similar across mip levels. Composite with a low configurable intensity; the author notes that physically based HDR often does not require a hard threshold because low bloom gain keeps ordinary values subtle.

## Assumptions and deliberate omissions

This production method assumes linear HDR with meaningful values above display white. A hard bright-pass threshold is a separate artistic variant and must be specified if used; it is not the primary technique in this reference. Excessive radius can reveal undersampled holes; fireflies can dominate unless the weighted downsample is normalized. Bloom changes perceived energy, so tune it before exposure/tone mapping is locked and keep all timing/color comparisons at identical exposure.

Do not describe this production technique as a paper-derived result. The author's intensity example around 0.04 is discussion guidance, not a formula or acceptance target. X173GE's current demo uses intensity `0.04` and radius `0.003` as scene-tuned values.

## X173GE implementation

The Vulkan backend uses separate ceil-halved HDR attachments for each downsample and reconstructed level, stopping at 1×1. The first pass uses normalized Karis weights; later passes use four-tap box downsampling. Reconstruction samples separate source and destination images, applies the normalized 3×3 tent, and adds the same-resolution downsample. The final scene-plus-bloom sum is tone-mapped before SMAA. The reconstruction is additive across pyramid levels: for a constant radiance `C`, an `N`-level pyramid reconstructs approximately `N*C`. At 1280×720, the pyramid has 11 levels, so the old composite gain `.04` could add about `.44*C` across the image, explaining the fog-like lift even without a bright-pass threshold. Composite gain is now divided by the number of levels, making `BloomSettings.intensity` an overall pyramid gain rather than a resolution-dependent per-level gain. This keeps the `.04` preset while making the same gain comparable across extents. `BloomSettings` exposes finite, non-negative intensity and a finite `radius_uv` in 0..=0.5; the demo's `.04` / `.003` setting is a user-selected, scene-specific default. Bloom images, framebuffers, and descriptors belong to a surface generation and are recreated with its swapchain.

The lifecycle smoke now includes an emissive copy of one glTF mesh (strength 8) and verifies resource creation, rendering, resize, and shutdown. A scene-specific visual calibration pass with isolated, moving HDR emitters and image capture remains outstanding.

## Visual validation

Render isolated emissive points spanning HDR intensity, a small highlight on a dark surface, broad bright surfaces, and a bright source moving near the screen edge. Check for firefly trails, pyramid seams, clipped/wrapped edges, radius variation, energy/pixelation changes with resolution, and unintended glow on ordinary diffuse surfaces. Compare the pyramid against a reference Gaussian blur at the same apparent radius.
