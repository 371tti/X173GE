# Full rewrite acceptance status

Updated 2026-11-15. This tracks the approved rewrite plan; binary measurements
and smoke-log paths are in [`baseline.md`](baseline.md). A successful compile
or smoke run does not close the human image-review gate.

| Requirement | Status | Evidence and remaining work |
|---|---|---|
| Crate boundaries, Shipyard, fixed update, bounded renderer protocol | Implemented; CPU verified | Workspace check/tests pass. `FixedClock`'s interpolation alpha reaches `ExtractFrame`, and `TransformHistory::interpolate` is available to the game-owned extractor; the engine cannot apply it to game-specific snapshots automatically. The regression test covers resize to a new surface generation, rejection of the old snapshot before backend rendering, and presentation of a current snapshot. |
| Runtime render controls | Implemented; all-effects Release smoke passed, interactive review pending | `SetRenderFeatures` travels through the bounded renderer queue. If the queue is full, frame extraction pauses until the update is enqueued, keeping MBOIT and frame shadow data in FIFO order. F2–F12 toggle render effects, demo lights, and VSync; Tab/Backspace select compiled numeric controls (27 when all render effects are built), Up/Down adjust, and Home resets the selected value. The max-FPS cap defaults to 60 and is available from `GameDefinition::frame_pacing`; VSync changes recreate the surface generation and select a supported Vulkan present mode. Every compiled effect starts enabled; startup `--no-*` switches isolate them. Defaults: SSR `(15, 0.1, 192, 0.85, 1, 16)`, Bloom `(0.04, 0.003)`, God Rays `(0.0006, 0.6, 0.5, 128, 2)`, tone mapping `(contrast 1, key 0.1, exposure 0.21..16, delay 1 s)`, and PCSS `(0.5°, 0.2 world units)`. The demo also starts with HDRI diffuse `0.1`, directional scale `10x`, point/spot intensities `30/60`, and MBOIT enabled when compiled with shadows. Initial numeric selection is SSR steps (`192`) when compiled. F11 toggles diffuse HDRI illumination only; environment specular reflections are not implemented. Interactive shutdown prints every active toggle and retained setting as `X173GE_SETTINGS_FINAL`. The standard-GLB smoke presented twelve measured frames with no present drops. |
| Tone mapping, contrast, and Bloom | Implemented; visual review pending | Exposure key, bounds, adaptation time, and post-Reinhard contrast are adjustable. Bloom composite gain is normalized by pyramid level count after tracing its broad lift to additive reconstruction across all levels. Check a gray/saturated patch scene and the unchanged standard model for contrast clipping and residual screen-wide Bloom. |
| Translucent shadows | MBOIT integration compiled and smoke-rendered; human visual review pending | Six-power absorbance moments replace stochastic translucent shadows. Transparent BLEND casters use additive half-resolution moment maps; opaque and MASK casters keep CSM, and MBOIT transmittance multiplies existing PCSS visibility. The standard-GLB all-effects Release smoke completed 12 measured frames with no presented-frame drops and `live_bytes=0`; the four maps allocated 234,881,024 bytes. Compare the open viewer with MBOIT off/on, then inspect the transparent casters and their receivers for grain, leaks, contact, and cascade seams. |
| Windows GLB/glTF import | Implemented; standard fixture CPU-smoked | `research/gltf-core-import.md` and `research/gltf-textures.md` record format rules and unsupported extensions. The standard GLB imported and its 180 scene primitives, 28 materials, 26 images, and all decoded pixels were verified without starting Vulkan. Per-file GLB/buffer cap is 96 MiB; there is no aggregate asset cap, as selected. |
| PBR, lights, shadows, post effects | Implemented; human image review pending | Before TAA removal, standard and all-effects Release profiles completed 480 selected frames on the standard GLB. The TAA-free all-effects Release smoke now completes twelve measured frames with no frame drops and `live_bytes=0`. The earlier `ERROR_DEVICE_LOST` coincided with a Windows GPU watchdog event; its exact trigger remains unproven. |
| Camera scale, close inspection, global lighting | Visual review pending | The interactive camera starts at the saved pose `(1.2659273, -6.2382364, 11.584539)`, yaw `-467.52°`, pitch `-15.08°`, speed `0.044`, with a 0.001 near plane. HDRI contributes diffuse irradiance only; environment specular reflection is not implemented. Demo defaults are HDRI diffuse intensity 0.1 and directional-light scale 10x. Review close clipping, global-light brightness, SSR hits/misses, and backside geometry. |
| GTAO darkening and fog diagnosis | Signed-horizon root cause fixed; visual A/B pending | GTAO now preserves the signed horizon cosine required by Activision Algorithm 1. The 25% ambient blend remains provisional. God Rays are a separate scattering path. Both are off in the current review viewer. |
| Standard and high-quality executable/package sizes | TAA-free executable bytes measured; package size pending | `baseline.md` records standard/high-quality Release bytes and hashes for the TAA-free build. No distribution archive was rebuilt; the earlier review ZIPs describe a prior source snapshot. The model's redistribution rights remain unverified. |
| Same-scene rebuild1 vs X173GE performance baseline | Exploratory capture only; controlled comparison pending | The unchanged model, 1280×720 surface, hardware, and camera framing were used. The rebuild1 GPU passes ramped through the nominal warm-up and much of the sample; effect settings and framebuffer readback also differ. `baseline.md` records the runs and why the values are not a speed target. |
| Asset upload and standard/high-quality memory | Current Release profiles captured; unexplained upload interval remains | `baseline.md` records per-stage upload rows, 3.68 s standard / 3.60 s high-quality / 3.61 s no-HDRI end-to-end intervals, and owned GPU peaks of 837,456,768 / 1,049,121,152 / 833,229,696 bytes. All returned to `live_bytes=0`. Stage timings do not account for the full upload interval. |
| Vulkan resize, stale-surface, resource lifetime, shutdown | TAA-free all-effects Release smoke passed; resize validation pending | The current all-effects Release smoke presented twelve measured frames with no frame drops and `live_bytes=0`. Khronos resize/shutdown validation and stale-surface checks were completed before TAA removal; rerun them against this source snapshot when doing the next validation pass. |
| TAA removal | Implemented; current all-effects Release smoke passed | Removed its Cargo/API surface, renderer history and attachments, shader permutations, timing, and now-unused render-item identity. Historical measurements remain in `baseline.md`. |
| Project Codex configuration | Written; reload pending | Trusted project config registers the three scoped agents and caps concurrent threads at three. TOML parses; restart Codex after acceptance work to load the updated settings. |

The current interactive viewer is open from the Release `high-quality` build
with the standard GLB and saved camera pose. The previous per-run targets under
`.codex-target` were cleaned on 2026-11-15; logs and packaged fixtures were
retained. A one-frame all-features smoke's `X173GE_SETTINGS_FINAL` output is in
`target/settings-defaults-interactive-smoke.log`. Human review of the new
default quality settings and saved camera pose is pending.

## Previous D-drive review session (closed 2026-11-12)

The previous session was PID 16328, launched from
`.codex-target/ibl-render-review-20261111-high-quality/release/examples/clear.exe`
with the standard model, `--no-shadow --no-smaa --interactive`, and default
HDRI enabled. GTAO, SSR, bloom, and God Rays are off. The startup log
`.codex-target/render-review-20261111-current.err.log` confirms the 1,762,844-byte
HDRI loaded at 1024×512, the unchanged 75,654,468-byte GLB imported, and the
environment upload reached `ready=true`. The CPU import took 16.359 ms for the
HDRI and 101.736 ms for the GLB; image dimensions imply 521,142,272 bytes for
RGBA8 output. The earlier log called this `decoded_rgba8_bytes`, but it was a
size estimate before per-image decoding; profiling now measures actual decode
bytes and time separately. Total asset upload took 4,205.453 ms. This
non-profiling run is not a clean timing measurement. The user later confirmed
the window closed, and process inspection verified PID 16328 had exited. Its
final stderr line reports 754,005,888 engine-owned GPU bytes at peak and zero
live bytes at shutdown. This is not a full-quality Release peak. Current-source
Debug validation results and pending Release profile captures are recorded in
[`baseline.md`](baseline.md).

## Current D-drive review session (closed 2026-11-12)

PID 8660 runs
`.codex-target/release-current-diagnostic-high-quality/release/examples/clear.exe`
with the unchanged standard model, `--no-shadow --no-smaa --interactive`, and
default HDRI. The user confirmed the window is closed. The current
non-profiling executable hash is
`EE4DDDA0420006F2DAB5A17CBF3B1DB3DDE375C0A0930F9801717216EBA54B22`. Runtime
GTAO, SSR, bloom, and God Rays are off. The startup log
`.codex-target/render-review-20261112-current.err.log` confirms
`environment ready=true`, `upload_ms=3125.446`, and camera position `(0,0,0)`
with near plane `0.001`. Its shutdown report records 754,005,888 engine-owned
GPU bytes at peak and `live_bytes=0`.

Click the window to capture the mouse. Use WASD to move, mouse movement to look,
Space/E to rise, Q/left Shift to descend, left Ctrl to sprint, and the scroll
wheel to adjust speed. Escape releases the mouse.

Visual feedback remains pending. Please report whether close geometry still
clips, indirect lighting keeps the plant leaves and interior surfaces visible,
HDRI reflections respond plausibly, and the camera can inspect both sides of
thin surfaces. The old fog and SSR artifacts were disabled in this isolated
lighting view. Issues can be sent together or one at a time; screenshots and
the nearest camera position help pinpoint a rendering defect.
