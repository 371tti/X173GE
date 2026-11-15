# Punctual lights

## Sources

- Khronos [`KHR_lights_punctual`](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_lights_punctual/README.md) is the ratified glTF interoperability specification. It defines directional, point, and spot lights, node attachment, linear RGB color, intensity units, range, and spot cones.
- Pharr, Jakob, and Humphreys, [*Physically Based Rendering*, 4th ed., Point Lights](https://www.pbr-book.org/4ed/Light_Sources/Point_Lights), is a production rendering reference, not a research paper. It gives the inverse-square point-source model and smooth spotlight falloff.

## Selected variant

Keep X173GE's directional light and shadow path. Add bounded point and spot
lights to the owned frame protocol, and import `KHR_lights_punctual` instances
from the selected glTF scene. The Vulkan shaders evaluate them with the current
metallic-roughness BRDF. Point and spot lights do not cast shadows in this
variant; God Rays continue to use directional lights only.

For a point or spot light at `p` and shaded position `x`, use
`d = length(p - x)` and `l = (p - x) / d`. Without a range, attenuation is
`1 / d²`. With range `r`, use the Khronos recommendation
`max(min(1 - (d/r)^4, 1), 0) / d²`, and return zero when `d >= r`. Spot
attenuation uses `c = dot(spotDirection, -l)`, where `spotDirection` points
out of the light along the node's local `-Z` axis. Interpolate between the
inner and outer cone cosines, clamp to `[0, 1]`, then square the result.

The Khronos defaults are white linear RGB, intensity 1, unbounded range, inner
angle 0, and outer angle `π/4`; point and spot intensity is candela. Range,
intensity, and cone angles do not scale with the node. Position and direction
inherit the scene-node transform. A non-unit game-provided spot direction is
normalized once while packing the CPU frame uniform. A zero or near-zero `d²` is skipped to avoid
the point-source singularity and invalid shader normalization.

The first implementation has a fixed limit of 16 point/spot lights per frame
and evaluates each for each shaded fragment. It does not add tiled culling or
point/spot shadow maps; those would add runtime and resource complexity without
a measured need. Directional shadows use PCSS for opaque casters; optional
MBOIT transmittance multiplies that visibility for BLEND casters
in one frame.

## Validation scene

Use the unchanged `../rebuild1/assets/model.glb` as the primary regression
scene. The importer unit fixture covers meshless nodes, repeated references,
node transforms, defaults, required-extension declarations, and malformed
values. The review example can add one synthetic point and one synthetic spot
light to the standard model for visual review. Exact `d` versus `2d` falloff,
range cutoff, cone endpoints, and scale semantics still need a deterministic
fixture before they can be claimed as verified. Also check zero, one, and the
16-light protocol limit. Compare the standard model before and after with its
original directional light to catch unintended shading changes.

## Known limits

The point-source singularity is regularized by skipping positions within the
documented distance threshold. This is a deliberate finite-precision choice,
not part of the KHR extension. Point/spot shadow maps, punctual-light
volumetrics, and light culling are outside this first path.
