# GTAO

## Source and scope

Jorge Jimenez, Xian-Chun Wu, Angelo Pesce, and Adrian Jarabo, “Practical
Real-Time Strategies for Accurate Indirect Occlusion,” Technical Memo
ATVI-TR-19-01. This is a technical memo, not a peer-reviewed paper.
[Original PDF](https://www.activision.com/cdn/research/Practical_Real_Time_Strategies_for_Accurate_Indirect_Occlusion_NEW%20VERSION_COLOR.pdf)

This implementation follows Algorithm 1 and the horizon integral in Sections
4.1 and 4.3. It also uses the thin-occluder horizon correction from Eq. 13
and the near-field indirect-light approximation from Eq. 12. Directional GTAO,
specular occlusion, and temporal reprojection remain deferred.

## Chosen variant

The renderer draws a world-normal/depth prepass, evaluates the horizon integral
at half resolution, bilaterally upsamples to full resolution, then draws the
usual PBR pass. PBR reads only the finished AO texture. This pass order avoids
reading a normal or depth image while the same image is attached for writing.

For each of four uniform azimuth slices, the shader projects the world normal
into the plane containing the view vector and slice direction. It traces four
depth samples on each side and integrates Equation 7 analytically for the two
horizons:

`a(h₁, h₂, γ) = ¼ Σᵢ[-cos(2hᵢ−γ) + cos(γ) + 2hᵢ sin(γ)]`, for `i ∈ {1,2}`.

The projected-normal length scales this integral, and the result is averaged
across slices. For perspective projection, each pixel uses its camera ray as
the view vector; for orthographic projection, every pixel uses the shared
camera-forward direction. The 1.0-world-unit radius skips samples outside the
near field and caps the projected gather at 64 pixels. This is 32 depth taps
per half-resolution pixel before bilateral reconstruction. When a later sample
falls behind the current horizon and is more than 1.5 times as far from the
surface as the horizon sample, its cosine is reduced by `β = 0.06`. This is the
paper's Eq. 13 with an X173GE distance gate and a β calibrated for four samples
per side; the sample remains bounded by the same 1-unit radius. The gate
attenuates repeated thin features while retaining the closer maximum horizon.

Horizon cosine starts at −1 and takes the maximum of the raw signed dot product
between the normalized sample direction and view direction, as in Algorithm 1.
Keep the value in [−1, 1]: clamping it to zero closes open horizons and lowers
visibility on surfaces where the sample direction points away from the camera.

PBR applies the paper's Eq. 12 near-field interreflection approximation to
diffuse ambient light, using the material's diffuse albedo as the assumed
constant neighborhood albedo. Before the signed-horizon fix, a greenhouse A/B
review found the GTAO contribution too dark; the current PBR blend remains 25%
with unoccluded ambient irradiance to isolate the algorithm correction. That
blend is provisional until the corrected result is reviewed. Material AO
remains a separate multiplier. Direct light, shadows, and emissive radiance
remain unaffected by GTAO.

The Vulkan backend flips clip-space Y for the framebuffer. Depth unprojection
therefore uses `(2u-1, 1-2v, depth)` with Vulkan's 0..1 depth range. Surface
normals are encoded world-space geometric normals from the prepass.

`FrameSnapshot.ambient_diffuse` means incoming diffuse irradiance in linear RGB,
already integrated over the hemisphere. When the environment has constant
radiance `L`, its unoccluded irradiance is `pi*L`. GTAO still cannot see
off-screen occluders or recover geometry hidden by the depth buffer. The Eq. 12
approximation assumes similar nearby albedo, which is imperfect for the
standard model's mixed foliage and greenhouse surfaces.

## Visual validation

Run the full-scene review harness described in
[`render-review.md`](render-review.md). Inspect ground/building contact, room
corners, thin leaves/branches, depth discontinuities, and AO changes after
resize. The rebuild1 model's dense, double-sided foliage is the primary thin-
occluder case; visual calibration remains a human review gate.
The automated Vulkan smoke verifies pass order, descriptors, image layouts,
resize, and shutdown; it does not verify image quality. Record the human visual
review separately before treating GTAO quality as accepted.

## Root-cause review (2026-11-11)

The user compared the greenhouse with GTAO enabled and disabled and confirmed
that the added surface darkening follows GTAO. The initial source audit missed
a defect in Algorithm 1: `gtao.frag` clamped each signed horizon dot product to
zero before taking its maximum. Negative values therefore became zero, making
their horizon angle π/2 instead of the correct angle greater than π/2 and
closing open directions. The shader now retains the signed dot product. The
current 25% PBR blend is held constant for the A/B review; its previous
calibration was made while the horizon bug was present and must be reconsidered
after visual review.

The reverse-Z reconstruction, world-space center/sample/view/normal data flow,
pass order, and bilateral upsample did not show a related broad-darkness error.
GTAO only modifies the diffuse ambient term; directional light, shadows, and
emissive radiance bypass it. The PBR and SSR PBR variants each consume the
finished AO once.

The viewer also supplies an asset-free vertical sky/ground diffuse environment
documented in [`diffuse-environment.md`](diffuse-environment.md); the optional
HDRI replaces it with nine diffuse irradiance coefficients. Environment-map
specular reflection is disabled and SSR has no miss fallback. One paper deviation remains:
the memo fades horizon cosine toward −1 outside the
near-field radius, while the shader skips samples beyond its 1-unit radius.
This may cause a cutoff or halo near that radius, but it does not explain broad
darkening. The standard model has no `occlusionTexture`, so material AO is 1.0
for this fixture. Recheck GTAO off/on at the same camera pose with shadows and
other screen-space effects disabled; inspect raw AO grayscale on an unoccluded
slanted surface, then on thin foliage, contact regions, and a corner.
