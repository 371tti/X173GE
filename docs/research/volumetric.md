# Volumetric light shafts / participating-media lighting

The renderer embeds checked-in SPIR-V and Cargo does not compile GLSL. After changing `FrameUniform` or a shader `Frame` block, run `./crates/x173ge-renderer-vulkan/shaders/compile_frame_ubo.ps1` in PowerShell before building. The script compiles the standard and MBOIT God Rays variants together with the other affected frame shaders, then validates each module for Vulkan 1.0.

## Source and scope

Balázs Tóth and Tamás Umenhoffer, “Real-time Volumetric Lighting in Participating Media,” Eurographics 2009 short paper. [Original paper](https://diglib.eg.org/items/07d33179-3056-49d8-b404-c13be46c8baf)

## Chosen first variant

Implement single scattering in a homogeneous medium by marching along the camera ray between the visible surface and camera. At each sample, evaluate incident light using the light’s shadow map, attenuation, and phase function, then attenuate the contribution along the path to the eye. The paper’s transport solution is:

L(s) = exp(−τs)L(0) + ∫[0,s] Li(l) exp(−τ(s−l)) dl,

approximated by a finite sum with Δl = s/N:

L(s) ≈ exp(−τs)L(0) + Σ Li(l_n) exp(−τ(s−l_n)) Δl.

For a point light, the paper uses Li(x,ω) = τa Φ/(4πd²) × visibility(x) × exp(−τd) × P(ω_l,ω). Keep extinction, scattering albedo, phase function, and light visibility as distinct inputs so the physical assumptions remain inspectable.

### Initial directional-light variant

X173GE has directional lights and CSM, but no finite light-to-volume path or point-light distance. Start with its single shadow-casting directional light and adapt the incident term to `Li(x) = τa × directional_radiance × CSM_visibility(x) × T_light × phase(−ω_l, ω_v)`. This is an extension of the paper's point-light equation, not its exact point-light variant. For the initial uniform-light approximation, define `directional_radiance` at the medium boundary and set `T_light = 1`; this omits attenuation between that boundary and the sample by assumption. A directional light having no finite origin does not itself justify omitting light-path attenuation. `ω_l` points from the sample toward the source, and `ω_v` from the sample toward the eye, so the Henyey–Greenstein cosine uses the incoming photon direction `−ω_l`.

Use the paper's M×M spatial interleaving as the first sampling variant: distribute the N integration terms across each M×M pixel block so each pixel evaluates N/M² terms, then reconstruct with separable horizontal and vertical addition filters. Require `N % M² == 0`; this is an implementation constraint so each pixel gets the same number of terms. Interleaving assumes nearby rays terminate at nearby visible surfaces. Detect depth discontinuities and evaluate all N terms for affected pixels instead of blending samples across an edge; all-background blocks also use full-N because their far-plane endpoints vary by ray. These fallbacks are X173GE adaptations, not guarantees in the paper. The paper does not specify a half-resolution render target; half-resolution rendering is a separate optimization and must not be described as paper interleaving. Choose M and N from GPU profiling and compare interleaved output with a full-N reference in the deterministic scene. No temporal accumulation is included in this first variant.

The real-model review showed that the original `τ=0.015` preset was too dense for the rebuild1 model: its radius is 310.426 world units, while `τ=0.015` gives an e-folding distance of only 66.7 units. The current demo preset is `τ=0.0006` per world unit, albedo `0.6`, HG `g=0.5`, `N=128`, and `M=2`. At 300 units, this keeps about 84% transmittance (`exp(−0.0006 × 300)`), leaving visible light shafts without replacing most scene radiance with fog. This is a scene-scale calibration, not a universal atmospheric constant; larger or denser worlds must select extinction for their world units and intended visibility. CSM visibility currently uses one depth comparison with a constant receiver bias, rather than the surface-normal PCSS path; review shadow acne and leakage before tuning it.

## Assumptions and deliberate omissions

The paper assumes a homogeneous, non-emissive medium and single scattering; it ignores multiple scattering. Its detailed light formula is for a point source and uses shadow maps for visibility. Directional and spot lights need explicit incident-radiance extensions. Do not substitute a radial image-space blur: that method needs a camera-visible source and is not volumetric scattering. The method is expensive when every pixel performs every march step; do not raise sample counts before GPU pass timing is available. The initial single-depth post-process also does not correctly fog transparent surfaces; validate the opaque subset first and record this limit when reviewing the standard model.

Temporal accumulation of interleaved samples is an additional extension, not part of the paper’s described spatial block reconstruction. If added, validate history against depth and camera motion.

## Visual validation

Use a homogeneous fog volume with one shadow-casting directional light, an opaque high-contrast receiver, and a geometric blocker that puts shadowed and lit samples in the same frame. Compare a full-N reference against M=1 and multiple interleaving patterns. Move the camera across a depth edge and move the light; inspect block patterns, halos, depth leakage, attenuation, and density changes. Record ray-march and horizontal/vertical reconstruction pass timings separately.
