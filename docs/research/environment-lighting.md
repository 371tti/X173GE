# Environment lighting and SSR-only reflections

## Scope

The selected Aloe Farm Shade House Radiance HDR supplies distant diffuse
illumination only. The importer projects it into nine real spherical-harmonic
coefficients and the PBR shader evaluates the cosine-convolved irradiance
against the world-space normal. The implementation and derivation are recorded
in [`diffuse-environment.md`](diffuse-environment.md), based on Ramamoorthi and
Hanrahan's SIGGRAPH 2001 paper.

Environment-map specular reflection is deliberately absent. There is no
environment cube, roughness prefilter, split-sum lookup, or sky/ground
reflection fallback. With SSR disabled, the scene receives no reflected-scene
contribution from the environment. With SSR enabled, only screen-space ray
hits add reflected scene color; misses keep the existing PBR color. SSR cannot
reflect off-screen or hidden geometry.

The interactive demo starts the HDRI diffuse intensity at `0.1`; F11 disables
or enables those SH diffuse coefficients. The independently scaled directional
light starts at `10x`. These are scene-tuning defaults for the standard GLB,
not universal lighting values.

The direct-light GGX specular lobe remains part of the metallic-roughness BRDF:
it represents highlights from explicit directional and punctual lights, not a
sampled environment reflection. Diffuse global environment lighting and direct
light are evaluated independently. The user's reported fixture log has one
directional light and two punctual lights; these are passed through the frame
snapshot and consumed by the PBR path.

## Source and implementation choice

The diffuse variant uses the paper's nine-coefficient real SH basis and
Lambertian cosine convolution. HDR pixels are integrated with their
lat-long texel solid angles in f64; the renderer receives only the resulting
linear RGB coefficients. The HDRI source is no longer uploaded to Vulkan.
This avoids inventing an approximate specular image for every material and
keeps the environment path small and explicit.

SSR follows McGuire and Mara's one-layer perspective-correct screen-space ray
tracing method, documented in [`ssr.md`](ssr.md). The current composition is
an additive Fresnel-weighted SSR hit over the already shaded PBR scene; an SSR
miss performs no reflection fallback.

## Visual review

Use the unchanged rebuild1 GLB. First compare HDRI on/off with GTAO and SSR
disabled: diffuse-lit faces and explicit-light highlights should remain, while
the HDRI must not paint a mirror image onto the floor. Then enable SSR and
check the floor from multiple camera angles, including ray hits, misses, and
screen edges. A miss should show the original black/rough PBR floor without an
environment image. Review hit brightness and edge transitions separately.

Vulkan validation and shader validation do not establish visual quality. The
rendered standard model still requires human review.

## References

- Ramamoorthi and Hanrahan, “An Efficient Representation for Irradiance
  Environment Maps,” SIGGRAPH 2001,
  [paper](https://www-graphics.stanford.edu/papers/envmap/envmap.pdf).
- McGuire and Mara, “Efficient GPU Screen-Space Ray Tracing,” JCGT 2014,
  [paper and supplemental implementation](https://jcgt.org/published/0003/04/04/).
- Khronos, [glTF 2.0 material model](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#materials).
