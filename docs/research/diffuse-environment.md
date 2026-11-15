# Diffuse environment lighting

## Source and selected model

Ravi Ramamoorthi and Pat Hanrahan, “An Efficient Representation for Irradiance
Environment Maps,” SIGGRAPH 2001. The paper derives diffuse irradiance as
`E(n) = ∫ L(ω) max(n·ω, 0) dω` and shows that cosine convolution attenuates
high spherical-harmonic bands. The selected preset keeps only its constant and
world-Y first-order terms, which exactly represent this smooth analytic
environment:

`L(ω) = (sky + ground)/2 + (sky - ground) * ω.y/2`

After diffuse convolution:

`E(n) = π * (sky + ground)/2 + π * (sky - ground) * n.y/3`

`FrameSnapshot` carries those two incoming-irradiance coefficients in linear
RGB. The fragment shader evaluates them against the world-space normal and
applies the Lambert `1/π` factor once. GTAO multiplies this diffuse environment
term; direct lights, shadows, and emissive output remain separate. These
coefficients are diffuse-only and do not reconstruct a sky reflection.

## Scope and verification

The analytic sky/ground term remains the fallback when no HDR environment is
selected. The viewer stages the selected HDRI, whose nine irradiance
coefficients replace that approximation after the renderer confirms upload.
This is distant diffuse environment lighting; it does not provide local
probes, environment reflections, or near-field multi-bounce GI. Scene
reflections are provided only by SSR when enabled. Directional and punctual
lights are stored separately in `FrameSnapshot` and evaluated by the PBR path;
the reported standard-model run contains one directional and two punctual
lights.

The protocol and importer tests verify constant radiance and the vertical
gradient's convolved irradiance. The HDRI path is integrated and its shaders
pass SPIR-V validation; actual Vulkan rendering and visual equivalence remain
unverified until a fresh build is reviewed. Human review uses the standard
rebuild1 GLB with GTAO off and on, comparing upward/downward-facing plant leaves,
shelf undersides, floor, and shadowed areas. Keep GTAO strength at 25% until that
comparison is reviewed.

**Reference:** [Stanford paper page](https://vis.stanford.edu/papers/envmap/),
[original paper PDF](https://www-graphics.stanford.edu/papers/envmap/envmap.pdf).
