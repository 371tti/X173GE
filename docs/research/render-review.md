# Standard-model render review

Use the unchanged rebuild1 scene at `../rebuild1/assets/model.glb` (SHA-256
`E9852865A6D6B7779C8A6CB390A0F9414B0AB247AE45F62E3FEAC190C65A6694`). Keep
the model, camera, 1280×720 resolution, GPU, and exposure fixed between runs.

The current Release viewer is built at `target/release/examples/clear.exe`.
Run these commands separately from the X173GE workspace root:

```powershell
$viewer = '.\target\release\examples\clear.exe'
& $viewer --asset ..\rebuild1\assets\model.glb --no-smaa --no-gtao --no-ssr --no-bloom --no-god-rays --no-mboit --no-point-light --no-spot-light --interactive
& $viewer --asset ..\rebuild1\assets\model.glb --no-smaa --no-environment --no-gtao --no-ssr --no-bloom --no-god-rays --no-mboit --no-point-light --no-spot-light --interactive
& $viewer --asset ..\rebuild1\assets\model.glb --no-smaa --gtao --no-ssr --no-bloom --no-god-rays --no-mboit --no-point-light --no-spot-light --interactive
& $viewer --asset ..\rebuild1\assets\model.glb --no-smaa --no-gtao --ssr --no-bloom --no-god-rays --no-mboit --no-point-light --no-spot-light --interactive
```

The demo enables every compiled effect by default, so these runs explicitly
disable unrelated effects, MBOIT, and synthetic demo lights. The first checks
direct light plus diffuse HDRI with GTAO and SSR off. The second removes the
HDRI to compare the analytic diffuse fallback, the third adds GTAO to HDRI
lighting, and the fourth adds SSR while keeping GTAO off. These are separate
launches; the viewer does not switch these options at runtime. Shadows remain
enabled; `--no-shadow` disables them. Volumetric single scattering is isolated
from AO and environment lighting by the `--no-god-rays` switch. Inspect the
same camera views in every run.
The default build loads the selected HDRI for diffuse illumination only.
Environment-map specular reflection is absent; when SSR is enabled, only a
screen-space hit adds a reflected scene contribution, and a miss adds none.
`--no-environment` isolates the analytic sky/ground diffuse fallback.
Environment lighting does not include nearby geometry or local parallax. The
viewer's background is still a fixed clear color, not the HDRI sky, so inspect
lit surfaces rather than inferring light from the background. See
[`environment-lighting.md`](environment-lighting.md).

The initial camera uses the saved review pose. In interactive mode use
WASD or the arrows to move, mouse look after left-click capture, Space/E to go
up, Q/Shift to go down, Ctrl to move faster, and the wheel to adjust speed.
Scroll down to slow movement for close inspection; its minimum step is now
below the 0.001 near-clip distance on the standard model. Esc releases the
cursor.

The standard model's radius is 310.426 world units and its interactive near
plane is 0.001. The previous speed floor moved the camera about 0.116 units
per 60 Hz fixed step, over 100 times the near distance, so keyboard movement
could jump across nearby surfaces. The new floor moves about 0.00023 units per
step. The near plane itself remains unchanged; verify close-up geometry with
the lower speed before changing projection precision.

Check these areas and record each defect with the view and settings:

- Move from the center toward plant leaves, shelves, floor, and greenhouse
  frame; confirm near geometry stays visible and the far plane does not cut the
  model unexpectedly.
- Compare GTAO off/on at the same position. Check upward and downward leaves,
  contact areas, and shadowed regions; distinguish diffuse occlusion from
  direct light and diffuse environment illumination.
- Rotate around glossy and metallic surfaces with SSR disabled. Confirm direct
  light highlights and diffuse lighting remain without an environment image
  being painted over the material.
- With SSR enabled, inspect hit transitions, screen edges, thin foliage, and
  camera motion. A miss should keep the base PBR color without a reflection
  fallback; a hit should add only the visible screen-space scene contribution.
- Check the model from both sides of thin foliage and double-sided surfaces.
  Static import/render review found 27 of the model's 28 materials marked
  `doubleSided`; the Bush and Plants groups account for 108 such primitives.
  The `face` and `line` materials, plus primitives without a material, retain
  back-face culling. The Vulkan variants disable culling for `doubleSided`
  materials and flip their back-face normals. Check those single-sided
  exceptions separately from the foliage.

Record build features, command-line flags, GPU and driver, resolution, camera
position and direction, model hash, and screenshots. A passing import, shader
validation, or Vulkan smoke does not count as visual approval. Do not tune AO,
environment strength, fog, or SSR from a different camera or build between
comparisons.
