# Metallic-roughness PBR

## Sources and scope

- **Format specification:** [glTF 2.0](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html) defines `baseColorFactor`, `metallicFactor`, `roughnessFactor`, and the texture channels. It permits implementation choices for the BRDF.
- **Papers:** [Trowbridge–Reitz (1975)](https://opg.optica.org/josa/abstract.cfm?uri=josa-65-5-531) supplies the distribution now commonly called GGX; [Walter et al. (2007)](https://doi.org/10.2312/EGWR/EGSR07/195-206) applies microfacet distributions to rough surfaces; [Heitz (2014)](https://jcgt.org/published/0003/02/03/) analyzes masking and shadowing; [Schlick (1994)](https://doi.org/10.1111/1467-8659.1330233) gives the Fresnel approximation.
- **Background:** Cook–Torrance (1982) is an early microfacet reflectance model ([paper](https://doi.org/10.1145/357290.357293)). Burley (2012) is production course material, not a peer-reviewed paper ([notes](https://disneyanimation.com/publications/physically-based-shading-at-disney/)).

The implementation targets opaque, isotropic metallic-roughness materials under direct lights and diffuse environment lighting described in [`diffuse-environment.md`](diffuse-environment.md). The BRDF equations below consume linear base color and metallic/roughness values; the glTF texture subset in [`gltf-textures.md`](gltf-textures.md) supplies those values by multiplying the samples by their material factors. Environment-map specular reflection is disabled; SSR is the only reflected-scene contribution. Transmission and clearcoat are outside this scope.

## Chosen variant

Use the glTF metallic-roughness inputs with a Trowbridge–Reitz/GGX normal distribution, separable Smith GGX masking-shadowing, and Schlick Fresnel. Clamp perceptual roughness to `0.04` before squaring to avoid the singular zero-roughness limit; this is an explicit renderer choice consistent with rebuild1's floor. Diffuse spherical-harmonic irradiance and explicit-light GGX highlights are evaluated separately. Direct-light specular is retained; SSR adds reflected screen color only on a hit and has no environment-map fallback.

For normalized normal `N`, light `L`, view `V`, and `H = normalize(L + V)`, let `NoX = max(dot(N, X), 0)`, `alpha = roughness²`, and `a2 = alpha²`:

```text
D(NoH) = a2 / (π * (NoH² * (a2 - 1) + 1)²)

G1(NoX) = 2 * NoX /
          (NoX + sqrt(a2 + (1 - a2) * NoX²))
G = G1(NoL) * G1(NoV)

F0 = mix(0.04, baseColor, metallic)
F  = F0 + (1 - F0) * (1 - VoH)^5
diffuse  = (1 - metallic) * baseColor * (1 - F) / π
specular = F * D * G / (4 * NoL * NoV)
Lo += lightRadiance * NoL * (diffuse + specular)
```

`baseColor` is linear after sRGB decoding and is multiplied by `baseColorFactor`. glTF's metallic-roughness texture is linear: green is roughness and blue is metallic. The selected Smith `G` differs from rebuild1's Schlick-GGX approximation; the GGX distribution, roughness-squared mapping, Schlick Fresnel, metallic diffuse reduction, and `0.04` floor retain its core model.

## Vulkan frame convention

The camera matrix is column-major, uses Vulkan clip depth `0..=1` with reverse-Z (near maps to 1, far to 0), and follows conventional `+Y`-up coordinates. Build it with X173GE's perspective_rh_reverse_z or orthographic_rh_reverse_z helper. Camera depth clears to 0 and uses GREATER; light shadow maps keep forward depth, clear to 1, and use LESS. With the Vulkan 1.0 positive-height viewport, framebuffer Y increases in the clip-space `+Y` direction. The vertex shader flips clip Y once, and the pipeline uses clockwise front faces after that reflection. This follows the [Vulkan vertex post-processing rules](https://docs.vulkan.org/spec/latest/chapters/vertexpostproc.html) and [front-face definition](https://docs.vulkan.org/spec/latest/chapters/primsrast.html). PBR radiance and clear color are written linearly to the HDR scene attachment; the post pass tone maps to linear display RGB, then the sRGB nonlinear swapchain attachment performs display encoding. The selected curve and its limits are recorded in [`tone-mapping.md`](tone-mapping.md).

## Visual validation

Render a fixed-camera sphere grid with fixed exposure, a white directional light, and `baseColorFactor = [0.8, 0.15, 0.05]`. Use roughness `[0.04, 0.1, 0.25, 0.5, 0.9]` across metallic `[0, 0.5, 1]`. Check finite output, widening highlights with roughness, reduced diffuse contribution with metallic, neutral dielectric highlights, and tinted metal reflections. Keep texture, normal-map, and IBL checks out of this scene so this BRDF variant stays independently reviewable.
