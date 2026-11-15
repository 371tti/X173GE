# Translucent shadow maps

## Source and selected variant

The implementation follows the six-power-moment shadow variant from Münstermann et al., “Moment-Based Order-Independent Transparency” (ACM I3D 2018, §6). The paper is the algorithm source; the supplementary material supplies the numerical bias constants. The authors' HLSL is used as an equation-level cross-check, not copied as a source file.

For each `BLEND` fragment with opacity `alpha`, accumulate absorbance and six light-depth moments with additive blending:

```text
a  = -ln(1 - alpha)
b0 += a
bj += a * z^j, j = 1..6
```

`z` is the linear shadow clip depth remapped from Vulkan `[0,1]` to `[-1,1]`. The seven values are stored in two `RGBA32F` color attachments. `MASK` and opaque geometry stay in the existing cutoff-tested CSM depth pass. The transparent pass has no opaque-depth test: the paper notes that rejecting transparent casters against opaque depth causes light leaks around opaque silhouettes.

At the receiver, bilinearly sample the **raw, unnormalized** moments, then normalize `b1..b6` by `b0`. Reconstruct the normalized cumulative absorbance with the six-moment Hankel/Cholesky and cubic-root method, then multiply the existing opaque visibility by:

```text
T(z_receiver) = exp(-b0 * A_normalized(z_receiver))
```

Use `b0 = 0` as an unshadowed early-out, the paper's recommended six-moment bias vector `(0, 0.48, 0, 0.451, 0, 0.45)` with bias `5e-6`, and `beta = 0` for shadow overestimation. The bias applies to normalized moments and is unrelated to the CSM depth comparison bias. Scalar absorbance models alpha coverage; colored `KHR_materials_transmission`, refraction, and caustics are not implemented by this path.

MBOIT multiplies the existing hard/PCSS result, so PCSS remains active when both features are enabled. Cascade blending evaluates the full opaque-times-transparent visibility in both cascades before interpolation. God Rays use the same transparent transmittance with their existing hard opaque-shadow comparison.

## Cost and deliberate deviations

The paper's filtered shadow path uses a 9x9 Gaussian on the raw moments before normalization. X173GE instead uses hardware bilinear sampling of raw moments to keep receiver cost low. The maps use half the linear resolution of the corresponding opaque cascade: `[1024, 1024, 1024, 2048]` for the current `[2048, 2048, 2048, 4096]` CSM maps. These are explicit quality/performance deviations and need visual review on the fixture.

Two `RGBA32F` attachments use 32 bytes per transparent-shadow texel. At the selected extents, the nominal map payload is 224 MiB across all cascades, excluding driver allocation alignment. The renderer logs the actual Vulkan allocation total. It allocates these maps lazily on first use and retains them while the feature is toggled off so descriptors remain valid.

The moment images require blendable, linearly filterable `RGBA32F` color attachments. The renderer checks format/extent support and the fragment sampler limits before creating MBOIT pipelines. No additional Cargo dependency is used.

## Build and validation

With the Vulkan SDK's `glslc` and `spirv-val` on `PATH`, compile and validate the checked-in modules:

```powershell
./crates/x173ge-renderer-vulkan/shaders/compile_pbr.ps1
./crates/x173ge-renderer-vulkan/shaders/compile_frame_ubo.ps1
./crates/x173ge-renderer-vulkan/shaders/compile_mboit.ps1
```

Use the unchanged `../rebuild1/assets/model.glb` fixture. Compare MBOIT off/on with identical camera, environment, PCSS settings, and feature toggles. Inspect the stacked leaves and pots, greenhouse floor, opaque/translucent intersections, receiver depth, and each cascade transition. Move and rotate the camera while shadows remain static; inspect noise, light leaks, edge sharpness, cascade seams, and stability. Record GPU shadow-pass time and the `X173GE_MBOIT_MAP` allocation log. Automated validation does not replace the required human visual review.

## References

- Münstermann et al., [Moment-Based Order-Independent Transparency, ACM I3D 2018](https://momentsingraphics.de/Media/I3D2018/Muenstermann2018-MBOIT.pdf) — primary research paper.
- Münstermann et al., [MBOIT supplementary material](https://momentsingraphics.de/Media/I3D2018/Muenstermann2018-MBOITSupplementary.pdf) — numerical bias and quantization details.
- [Authors' implementation page](https://www.momentsingraphics.de/I3D2018.html) — reference implementation link used to cross-check the reconstruction equations.
