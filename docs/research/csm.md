# Cascaded shadow maps (CSM / PSSM)

## Sources and scope

Zhang et al.'s ACM VRCAI 2006 paper introduces PSSM: split the camera frustum by view depth, fit a light frustum to each slice and render one shadow map per slice. Its split formula is the mean of logarithmic and uniform splits, with an optional non-negative split bias. The original experiments compared two to four splits and reported three as their best tradeoff on the tested hardware; this is historical evidence, not a current default. GPU Gems 3 is the authors' implementation chapter, not a separate paper. [Original paper](https://dl.acm.org/doi/10.1145/1128923.1128975), [GPU Gems 3 implementation chapter](https://developer.nvidia.com/gpugems/gpugems3/part-ii-light-and-shadows/chapter-10-parallel-split-shadow-maps-programmable-gpus).

The enclosing-sphere fit and texel snapping below are a stabilization variant from production presenter notes, not part of Zhang et al.'s split equation. [Killzone 2 extended presenter notes](https://www.guerrilla-games.com/media/News/Files/GDC09_Valient_Rendering_Technology_Of_Killzone_2_Extended_Presenter_Notes.pdf)

## Chosen first variant

For a perspective camera with `0 < n < f`, divide its positive view-depth interval into `m` cascades:

```text
C_0 = n
C_1 = λ n (f/n)^(1/m) + (1−λ) [n + (f−n)/m]
C_i = C_1 (f/C_1)^((i−1)/(m−1)),  i = 2…m
```

Zhang et al.'s practical split uses `λ = 0.5`; GPU Gems 3 explicitly treats the weight as application-adjustable because logarithmic splits improve near sampling while uniform splits protect far sampling. X173GE uses `λ = 0.9` for the first perspective split to preserve contact-edge resolution near the camera, then log-spaces the remaining three intervals between that split and `far`. This post-first redistribution is an X173GE adaptation, not the paper's global-weight formula. With the fixture's `near=0.001`, `far=356.9903`, and four maps, the first split remains `8.9468` world units; at the 1280×720, 50° default demo camera, its measured half extent is `9.6317`, or `0.00941` world units per 2048² map texel, versus `0.04691` with `λ = 0.5` (about 5× finer). The later splits are `30.5725` and `104.4706`, instead of `18.3873` and `39.9185`. With the 10% blend bands, the far cascade starts contributing at about `97.1` units and takes over fully at `104.5`, instead of starting at about `37.8`; this delays the coarse far-map footprint. The wider middle receiver ranges can include more casters, so their raster work may increase. Keep `C_0 = n` and `C_m = f`, and keep all splits strictly increasing. Orthographic cameras do not have the same perspective-aliasing distribution; use uniform splits or leave CSM disabled for them.

GPU Gems 3 §10.4 explicitly allows different map sizes per split even though its sample uses equal sizes. X173GE keeps the three near maps at 2048² and uses 4096² for the far map. On the fixture, the far map half extent is about `363.88` units, so its world texel width falls from `0.3553` to `0.1777`; at the far cascade's start, the estimated screen footprint falls from `2.63` to `1.31` pixels per texel. Near-contact density is unchanged. The final map has four times the texel area; its depth attachment adds 48 MiB with D32 or 24 MiB with D16, while actual raster cost depends on the casters. The half-resolution MBOIT map for that cascade uses two RGBA32F attachments at 2048², or 128 MiB nominally; all four MBOIT cascades total 224 MiB. This is a resolution-for-bandwidth/memory trade, not a free split adjustment. [GPU Gems 3 §10.4, “Varying Texture Resolution”](https://developer.nvidia.com/gpugems/gpugems3/part-ii-light-and-shadows/chapter-10-parallel-split-shadow-maps-programmable-gpus#other-considerations)

Fit an orthographic light projection to each slice's receiver bounds and include casters outside the camera frustum whose light-space projection can overlap those receivers. A receiver-frustum-only fit can omit such casters. X173GE uses four cascades, an enclosing-sphere fit rounded upward to 1/16 world unit, then snaps the light-space XY center to shadow-texel increments. The radius includes the maximum half-diagonal snap displacement, so snapping cannot clip receiver corners. All opaque render items are casters; their uploaded local AABBs are transformed and retained only when their light-space XY bounds overlap the receiver map. Orthographic cameras use uniform splits. Only one directional light can cast shadows.

Zhang et al.'s PSSM paper defines split placement, not cascade blending. For the visible transition seam, X173GE now blends both adjacent visibility results over the final 10% of each split interval:

```text
t = smoothstep(C_i − 0.1 × (C_i − C_(i−1)), C_i, view_depth)
visibility = mix(visibility_i, visibility_(i+1), t)
```

The official Microsoft CSM technical article (an implementation reference, not a paper) recommends evaluating both maps in a blend band, interpolating their shadow results, and branching so pixels outside the band sample only one map. X173GE uses the same method for PCF/PCSS, MBOIT, and God Rays. The next cascade's receiver fit extends to the start of the previous cascade's band; blending without this overlap would sample the next map outside its fitted receiver bounds and could incorrectly brighten shadows. The overlap start is stored in the previously unused `shadow_depth.w`, so the frame uniform layout and descriptor set remain unchanged. The last cascade has no outgoing blend band. [Microsoft CSM blend guidance](https://learn.microsoft.com/windows/win32/dxtecharts/cascaded-shadow-maps#blend-between-cascades)

The Vulkan backend keeps one initialized 1×1 fallback at device creation and allocates maps at `[2048, 2048, 2048, 4096]` on the first shadowed frame. This avoids their depth storage—about 112 MiB with D32 or 56 MiB with D16, plus framebuffer overhead—in scenes that do not request shadows. Descriptor replacement waits for the prior frame and transitions new maps to read-only layout before the update.

## Vulkan and depth contract

The camera uses reverse-Z, but the separately fitted light shadow maps retain forward depth (`0..1`, clear `1`, compare `LESS`). Use the 1.0 `VK_IMAGE_LAYOUT_DEPTH_STENCIL_READ_ONLY_OPTIMAL` for sampling and a legacy `vkCmdPipelineBarrier` or render-pass dependency from depth writes to fragment-shader reads. The shadow image needs both depth-attachment and sampled usage, and its format must support both. Prefer a depth-only format; with a combined depth/stencil format, include both aspects in 1.0 barriers unless separate depth/stencil layouts are enabled. [Vulkan image layouts](https://registry.khronos.org/vulkan/specs/latest/man/html/VkImageLayout.html), [image usage](https://registry.khronos.org/vulkan/specs/latest/man/html/VkImageUsageFlagBits.html), [image barriers](https://registry.khronos.org/vulkan/specs/latest/man/html/VkImageMemoryBarrier.html).

Use the same Vulkan `0..1` clip-depth and framebuffer-Y convention in the shadow pass and its lookup as in the PBR pass: flip clip Y once for the positive-height viewport, map NDC XY to texture coordinates consistently (`uv = (0.5*x/w+0.5, -0.5*y/w+0.5)` when the vertex shader flips Y), and do not remap clip Z from `[-1,1]`. Cascade selection uses linear camera view depth, not nonlinear clip depth.

## Assumptions and validation

Initial support is one shadowed directional light. The camera must supply valid near/far and projection-kind data; its VP matrix must be invertible when the renderer reconstructs the frustum. Point lights need another shadow projection. All submitted opaque render items are casters until the scene API exposes a smaller, explicit caster set. Bias must scale with each cascade's light-depth range; CSM does not remove projection aliasing, acne or the cost of repeated caster rendering.

CPU checks cover practical split ordering, uniform orthographic splits, sub-texel snapping, receiver containment after snapping, singular matrices and camera near/far consistency. A fixed visual scene still needs a caster whose shadow crosses every split, including one outside the view frustum, and near/far receivers. Move the camera by sub-texel increments and rotate it slowly; check missing casters, shimmer, split selection, acne and seams. Repeat with changed FOV, aspect, near/far and resize. Record per-cascade texel footprint and GPU pass time. Use zero angular radius for deterministic hard-shadow CSM inspection before enabling PCSS. For this change, review the rebuild1 greenhouse model at pot/leaf contact, through the `8.95`, `30.57`, and `104.47` unit transition bands, and at distant receivers; repeat with PCSS and MBOIT enabled. Check for double-dark/bright shadows, discontinuity, and extra softness where both maps are evaluated. Confirm that the far map is less blocky without a sudden filter-width change at the third-to-fourth transition, and record its GPU pass time and memory cost.
