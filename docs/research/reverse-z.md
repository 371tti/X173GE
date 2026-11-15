# Reverse-Z camera depth

## Root cause

The rebuild1 default GLB has post-deformation bounds of roughly 72 × 599 × 148
source units and a half-diagonal radius of 310.426. The review harness keeps
those glTF coordinates and initially placed its camera about 900 units away,
with near=0.01 and far≈1257. Forward-Z D32 float loses useful precision in
that range: the model's front and back surfaces advance by roughly 4–6 world
units per representable depth step. Inverting that forward-Z matrix also
recovered the far plane about 5.2 units short, which made cascade fitting
reject the camera metadata. Increasing near would improve precision by
discarding the close geometry the review is intended to inspect.

## Selected convention

Use finite reverse-Z for camera depth with Vulkan's 0..1 clip range: near maps
to 1 and far maps to 0. For right-handed view-space z<0, the perspective
matrix uses `clip.z = z * near/(far-near) + near*far/(far-near)` and
`clip.w = -z`; the orthographic matrix maps depth with
`clip.z = (view.z + far)/(far-near)`. `perspective_rh_reverse_z` and
`orthographic_rh_reverse_z` centralize these matrices in `x173ge-core`.

Camera depth attachments clear to 0 and use GREATER. Sampled depth reconstructs
world positions with the inverse view-projection matrix. Where linear view
depth is needed, perspective uses `near*far/(near + depth*(far-near))` and
orthographic uses `mix(far, near, depth)`. Camera frustum extraction uses
depth 1 for near and 0 for far.

Directional shadow maps are a separate projection and remain forward-Z, with
clear=1 and LESS. This avoids changing PCSS comparisons, bias, and shadow
sampling as part of the camera-depth fix.

## Evidence and limits

The finite projection maps both endpoints correctly in unit tests, and the
shadow fit accepts a camera at z=900 with near=0.01 and far=1257. Shader
variants compile and pass `spirv-val` for Vulkan 1.0. Full-scene visual review
of close geometry and depth edges remains pending. Reverse-Z's strong
floating-point precision benefit relies on D32_SFLOAT; the D16_UNORM fallback
does not have floating-point depth spacing.

Reference: NVIDIA, [Visualizing Depth Precision](https://developer.nvidia.com/blog/visualizing-depth-precision/).
