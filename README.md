vulkan-test(valkanってtypoしてたやつ) の rebuild 1 をAIにrewriteさせたやつ

# X173GE

Rust 2024 workspace for a Windows-first Vulkan game engine rewrite. Rebuild1
provides design context and review assets; X173GE owns a new implementation.

The game thread owns Shipyard state, fixed updates, and render extraction. A
bounded protocol carries owned snapshots to a dedicated renderer thread, which
owns Vulkan and GPU resources. Audio and editor are outside the initial scope.

## Crates

| Crate | Responsibility |
|---|---|
| `x173ge` | Game API and fixed update loop |
| `x173ge-core` | IDs, math, constrained values |
| `x173ge-protocol` | Commands, events, surface generations, frame data |
| `x173ge-ecs` | Shipyard integration |
| `x173ge-scene` | Scene data and render extraction |
| `x173ge-assets` | CPU-side glTF/GLB import and image decode |
| `x173ge-platform` | Window, input, native surface adapter |
| `x173ge-renderer` | Backend-neutral renderer service and Null backend |
| `x173ge-renderer-vulkan` | Fixed render graph, Vulkan resources, GPU assets, shaders |


## Current implementation

The Vulkan path draws PBR meshes with glTF static morph weights and skinning,
normal maps, alpha modes, double-sided materials, `KHR_lights_punctual`
directional/point/spot lights, a procedural sky/ground diffuse environment,
directional CSM with PCSS and optional MBOIT translucent shadows, SMAA, GTAO,
bloom, feature-gated SSR, and paper-based volumetric God Rays. The environment provides
diffuse fill plus a low-frequency analytic specular reflection approximation.
It is not a GGX-prefiltered HDRI/cubemap and provides no scene-derived GI; see
[`specular-environment.md`](docs/research/specular-environment.md).
Point and spot lights use inverse-square falloff and currently have no shadows.
The glTF importer follows Khronos glTF 2.0 for its documented triangle-list
subset; it rejects unsupported material and texture-coordinate features
explicitly. It does not play glTF animations. Cargo features select the
available render effects; requesting an effect omitted from the build returns
an explicit configuration error.

God Rays use homogeneous single scattering, CSM visibility, and M×M spatial
interleaving with depth-edge fallback. The coefficients still need actual-model
visual calibration. Human image review, clean model profiling, runtime GPU
memory-counter verification, and distribution size measurement remain open
acceptance gates.

The image-quality checkpoint uses the exact default model from
`../rebuild1/assets/model.glb`. The file remains in rebuild1 and is loaded from
disk, not embedded in X173GE.

## Build and review

```powershell
cargo check --workspace --all-targets --all-features --locked
cargo test --workspace --all-features --locked
cargo build --locked --release --example clear
cargo run --release --locked --features high-quality --example clear -- --asset ../rebuild1/assets/model.glb --interactive
```

Use the unchanged rebuild1 default model for visual review. The clear demo
starts each compiled effect on; `--no-gtao`, `--no-ssr`, `--no-bloom`,
`--no-god-rays`, and `--no-smaa` disable them at startup. MBOIT starts on when
`translucent-shadows` is compiled and shadows are enabled; `--no-mboit`
disables it. MBOIT multiplies its transmittance with opaque PCSS visibility.
Demo point and spot lights also start on and can be disabled with
`--no-point-light` and `--no-spot-light`. Close the interactive window after
review. Smoke tests do not replace the required human image-quality check.
The model-specific comparison procedure and review log fields are in
[`docs/research/render-review.md`](docs/research/render-review.md).

In `--interactive` mode, F2–F6 toggle GTAO, SSR, Bloom, God Rays, and SMAA;
F7 toggles shadows, F8 MBOIT, F9/F10 the demo point/spot lights, and F11 the
loaded HDRI's diffuse environment illumination (not reflections). F12 toggles
VSync. Tab/Backspace select any compiled numeric renderer, lighting, shadow,
environment, camera, and FPS-cap control (27 with all effects); Up/Down adjust
it and Home resets that value. The FPS cap defaults to 60 and can also be set
at startup with `--max-fps N`; `--no-vsync` requests immediate presentation.
The title shows the current presented FPS, active cap, and VSync state.
PgUp/PgDn adjust the directional light and the mouse wheel adjusts camera speed. Effect changes are
sent as `RendererCommand::SetRenderFeatures` through the bounded renderer
queue and apply in order before later frames. Applications declare the effects
they need allocated with `GameDefinition::render_features`, choose the initial
active set with `initial_render_features`, and can enqueue later changes with
`RendererHandle::try_set_render_features`; `Full` returns the command for retry.
The demo starts with diffuse HDRI intensity `0.1`, directional-light scale
`10x`, point-light intensity `30`, and spot-light intensity `60` (override the
directional value with `--global-light-intensity`). On interactive shutdown it
prints every toggle and retained numeric setting in `X173GE_SETTINGS_FINAL`
lines. It starts at the saved camera pose with speed `0.044`; when SSR is
compiled, the initial numeric selection is SSR steps (`192`).

The release profile prioritizes runtime speed with fat LTO, one codegen unit,
and symbol stripping. Compare standard and high-quality build sizes separately;
baseline details and remaining measurement limits are in
[`docs/baseline.md`](docs/baseline.md). Algorithm sources, selected variants,
assumptions, deviations, and visual checks are recorded in
[`docs/research/`](docs/research/). Current acceptance gates are tracked in
[`docs/acceptance.md`](docs/acceptance.md).
