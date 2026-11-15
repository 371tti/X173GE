# rebuild1 baseline

This is a historical measurement archive. TAA was removed from the current
X173GE feature set on 2026-11-13; older commands and measurements that mention
it describe the earlier source snapshot only.

This captures the source state before X173GE changes. Existing user edits in rebuild1 and memo were preserved and included; the hash below makes that exact patch identifiable without changing it.

## Source and machine

- Git commit: `26d37702d50acf2740b044e3c4e933b2bd00ac71`
- Pre-existing working-tree patch SHA-256: `166925670804caa3ea8880d79222e7fe00e6ae7b36ca3b8c3f9367062cdfda0f`
- Rust: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, target `x86_64-pc-windows-msvc`
- CPU: Intel Core Ultra 9 285K, 24 logical processors
- GPU: NVIDIA GeForce RTX 4070 SUPER
- Window: 1280 × 720
- Scene: `rebuild1/assets/stage9_translucent_shadow.r1scene`

## Measurements

Both release runs used the same scene and submitted 180 loaded frames. GPU timing uses Vulkan timestamps and excludes WSI present wait. The smoke path also reads back a frame every 12 frames; those pass times are included below, so repeat that path for direct comparison and report readback separately.

| Runtime quality | GPU frame median | p95 | Mean | Dominant pass median (p95) |
|---|---:|---:|---:|---|
| performance | 0.152 ms | 0.3748 ms | 0.2047 ms | readback 0.1387 ms (0.1423 ms) |
| high | 5.1861 ms | 7.0111 ms | 5.3538 ms | god-ray mask 1.9743 ms (2.9051 ms) |

Other high-quality pass medians: post 0.4751 ms; four translucent-shadow passes 0.4198–0.4260 ms each. Release executable size was 5,548,032 bytes. The fixture is 256 bytes; no distribution archive was built.

One additional high-quality run recorded process CPU time of 562.5 ms over 3,462.3 ms wall time and peak working set of 111,517,696 bytes. This includes startup, resource creation, import and 180 frames; it is not a per-frame CPU measurement. GPU memory was not exposed by the existing logger.

The Debug Vulkan smoke submitted and presented 12 loaded frames with the Khronos validation layer and debug utils enabled; it exited successfully with no validation message.

## Limits and next measurement work

- The scene is a small transparent-shadow fixture, so it is a stable starting point but does not represent a large PBR scene. Add a fixed textured/cutout scene run beside it.
- `gpu_frame_ms` is command-buffer time, not CPU frame time and not present latency. X173GE needs separate CPU update/extraction/submit timers and a GPU allocation/memory report before setting numerical improvement targets.
- The release executable includes the pre-existing dirty changes listed above. Keep the same user-tree state, Rust toolchain, GPU, dimensions, scene, runtime settings and smoke/readback policy when comparing X173GE.
- Baseline logs are retained in the ignored repository `.codex-target` directory as `rebuild1-performance-baseline.log` and `rebuild1-high-baseline.log`.

## X173GE clear-only predecessor (historical; not comparable to rebuild1)

The first X173GE integration example opens a 1280 × 720 Win32 window, configures Vulkan, presents two clear-only frames, resizes to 800 × 600, then presents twelve frames after the new surface generation is configured before shutting down. It has no mesh draw, scene import, or distribution assets; the bytes below are not an engine-versus-engine performance comparison.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear` | 913,408 | `15C886DCED3CD54E54312BEFB7DB9D4FAA9AEC2F5FC3C4E86712C89FADD1200D` |
| High quality | `cargo build --locked --release --example clear --features high-quality` | 913,408 | `13D86B7E445AE34E537D152549DD5944986AA6D72AC953FB2F05324E6EFDC930` |

Both builds used `rustc 1.99.1`, `x86_64-pc-windows-msvc`, and the release profile above. They came from Git HEAD `26d37702d50acf2740b044e3e4c933b2bd00ac71` plus the current uncommitted X173GE files; the sorted source-file SHA-256 fingerprint (Cargo manifests/lockfile, `src/`, `crates/`, and `examples/`) was `B9BE8F9ADB2646237BB11BB2B49A4B024C6CE218569A3E33C9D3F177809434DB`. `Cargo.lock` SHA-256: `074EE893734F764D5F75F6921CE7B88E1F7AF0DD5B61C594B77D063DA4D059E5`.

The identical byte count confirms that the declared `smaa` and `high-quality` feature flags were placeholders in this clear-only build. The hashes differ because the linker emits build-specific data. No distribution archive or external asset was produced. The debug clear/present example exited after the resize and presentations. At that point validation callback output was not captured, so the smoke only confirmed process completion, presentation and resize/reconfiguration.

## X173GE factor-only PBR mesh milestone (historical; not comparable to rebuild1)

At this milestone, the smoke imported the embedded `examples/assets/triangle.gltf` bytes, uploaded its indexed triangle and factor-only material, drew one regular and one mirrored instance with direct directional lighting, resized from 1280 × 720 to 800 × 600, presented twelve frames after reconfiguration, and shut down. The glTF asset and embedded shader modules were in the executable; Vulkan loader/runtime remained system dependencies.

The Debug smoke exited successfully with `Vulkan validation layer enabled: true` and no validation warning or error. CPU and GPU frame-time instrumentation was not implemented, so this milestone did not establish a performance comparison or numerical target. The selected GGX/Smith/Schlick variant and its limits are recorded in [`research/pbr-metallic-roughness.md`](research/pbr-metallic-roughness.md).

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear` | 1,665,024 | `BC8938CF87D9AF5F493353682F748406D99910F1FE98EF969F4D07CDFA448E63` |
| High quality | `cargo build --locked --release --example clear --features high-quality` | 1,665,024 | `9BA0BE33334CE63F8301FC0AAB76EA7FB51BD08CCACB00A92473B1170BD19DEC` |

The exact source fingerprint and `Cargo.lock` hash for these historical release runs are recorded below. Equal byte counts showed that `smaa` and `high-quality` compiled no additional effect implementation at that time. No distribution archive was built. The asset path then called `import_buffers` and did not decode images.

These runs used Git HEAD `26d37702d50acf2740b044e3e4c933b2bd00ac71` plus the current uncommitted X173GE source. The source fingerprint hashes each regular file under `src/`, `crates/`, and `examples/`, plus root `Cargo.toml` and `Cargo.lock`; paths are normalized to `/`, sorted by ordinal UTF-8 path, and the SHA-256 stream receives `path UTF-8, NUL, file bytes, NUL` for each file. Fingerprint: `D8DAA4279F376241741F689FAF66C648F0F6BB12984308F895B7BC5DE42F3AA2`. `Cargo.lock` SHA-256: `33A8FBEEA69B45800185931CA18B2A34AEB92393398246F08862CBA3ADB6CA22`.


## X173GE glTF base-color and metallic-roughness texture milestone (not comparable to rebuild1)

The smoke fixture now embeds one 2 × 2 RGBA8 data-URI image, referenced by both material texture slots, with a nearest sampler and UV0. The importer decodes the shared image once; the renderer uploads it once and binds an sRGB view for base color and a linear UNORM view for metallic/roughness. The factor-only material path uses a white fallback. Mipmap minification currently falls back to its nearest/linear non-mipmap filter. Textured output passed Vulkan validation on the existing resize and present smoke, but no screenshot or independent visual review was captured, so this establishes resource and synchronization behavior rather than color correctness.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear` | 2,072,576 | `11D827229FABD62BB59632A021EEBDB51E537229EDC7D76B9F62E5A2DA366E5F` |
| High quality | `cargo build --locked --release --example clear --features high-quality` | 2,072,576 | `FF35EB91AC1DBE9EB36AADCF5EC74164675909992B303312EE4402DACD2EAC93` |

Both commands used `rustc 1.99.1`, `x86_64-pc-windows-msvc`, and the workspace release profile. They were built from Git HEAD `26d37702d50acf2740b044e3c4e933b2bd00ac71` plus current uncommitted source. The source fingerprint covers each regular file under `src/`, `crates/`, and `examples/`, plus root `Cargo.toml` and `Cargo.lock`; normalized `/` paths are sorted by ordinal UTF-8 and hashed as `path, NUL, file bytes, NUL`. Fingerprint: `41163EC086CAC80CC9C3386CA97756236DBF129E907C640520335529CF0BC557` (31 files). `Cargo.lock` SHA-256: `D5D8BC4891BE7B388483F6D79635607188F279192650C0EE4824EDD67034DAF4`.

The factor-only milestone was 1,665,024 bytes; this texture-enabled executable is 407,552 bytes larger under the same release profile. The equal standard and high-quality byte counts confirm the declared effect features still contain no additional implementation. The fixture image and shaders are embedded; no distribution archive was built. Per the selected policy, there is no aggregate CPU or GPU asset budget. At this milestone each JSON document/chunk, glTF buffer, encoded/decoded image and mesh had a 64 MiB individual cap; the GLB and external-buffer caps were later raised to 96 MiB for the selected review model. GLB unknown chunks are skipped without allocation. Renderer resources have no unload API, so GPU residency can grow with uploads. CPU/GPU frame-time instrumentation, the deterministic 5 × 3 sphere-grid visual review, and a rebuild1-equivalent scene remained outstanding at this point.

## X173GE opaque CSM/PCSS milestone (not comparable to rebuild1)

The Win32/Vulkan smoke imports the textured glTF triangle, places instances as a floor and two casters, renders four stabilized directional cascades, performs angular PCSS, resizes from 1280 × 720 to 800 × 600, and exits after twelve post-resize presents. The Khronos validation layer was enabled and reported no warning or error after correcting the shared push-constant stage mask. Shader binaries passed `spirv-val --target-env vulkan1.0`. CPU tests cover split ordering, metadata consistency, sub-texel snapping, and receiver containment. The smoke scene is for pipeline validation only; no visual capture or independent review was recorded.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear` | 2,134,528 | `6388EC73ADA5A787AFD95917C5CC36CE3AAA89FC4BEE83B01B5533142B60A5D9` |
| High quality | `cargo build --locked --release --example clear --features high-quality` | 2,134,528 | `10FC4000B19998557379702B7850FB3E62062956C4D13A9A8783869AC4E75855` |

These builds used `rustc 1.99.1`, target `x86_64-pc-windows-msvc`, and the workspace release profile, from Git HEAD `26d37702d50acf2740b044e3c4e933b2bd00ac71` plus uncommitted X173GE files. Source fingerprint: `8D850C0F050E9D0A02EDE915944258DB96B9595E1B86015F8D17186100933A60` (36 files). `Cargo.lock` SHA-256: `5215B658F4F7A476BAFB77A50C6C9BFCE73A126B53073340C1157911F2B70407`. Equal executable sizes confirm that the effect feature names still have no implementation; PCSS and the current PBR shader are compiled into both builds. No distribution archive was built. Four 2048² shadow maps reserve about 32–64 MiB GPU memory depending on the selected depth format, including frames that do not request shadows.

Per-pass GPU and CPU frame timing is still not instrumented, so this milestone provides no frame-time comparison. The remaining GTAO, SSR, bloom, volumetric GodRay, SMAA, translucent shadows and TAA implementations are not present.

## X173GE offscreen HDR scene target milestone

PBR now renders into per-surface HDR color (`R16G16B16A16_SFLOAT` when supported), normal/roughness, and sampleable depth targets. A surface-owned final pass tone maps HDR color into the sRGB swapchain. Targets are recreated with the surface extent. At 1280 × 720, the expected scene target storage is about 14.06 MiB (7.03 MiB HDR, 3.52 MiB normal/roughness, 3.52 MiB D32 depth); device format fallbacks can change this. This is groundwork for post effects, not an implementation of them.

The debug Win32/Vulkan example drew the textured/shadowed fixture, resized from 1280 × 720 to 800 × 600, and shut down with the Khronos validation layer enabled and no validation messages. The fullscreen pipeline warning found on the first run was traced to a missing empty vertex-input state and fixed. A read-only reviewer also caught and prompted fixes for depth-stage masks, linear-filter format support, and old-swapchain retirement on failed resize construction. No visual capture, CPU/GPU timings, or per-pass measurements were produced.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear` | 2,145,792 | `DD6D858A2C2221C4662395D1DAC812CEC9F8853FBAB632679EF5EEBEC3A990E8` |
| High quality | `cargo build --locked --release --example clear --features high-quality` | 2,145,792 | `5F3C870A3D0C9402DD8088611B53F3B3B913C55D46D613AF4A90C97BBD3826EC` |

Both used `rustc 1.99.1`, target `x86_64-pc-windows-msvc`, and the workspace release profile, from Git HEAD `26d37702d50acf2740b044e3c4e933b2bd00ac71` plus uncommitted X173GE files. The source fingerprint covers files under `src/`, `crates/`, and `examples/`, plus root `Cargo.toml` and `Cargo.lock`, using the sorted path/content SHA-256 procedure described above: `E233F0394D2C1577E734A8175B7AEAC16481B53F6004ABB976F670071FE024D4` (40 files). `Cargo.lock` SHA-256: `5215B658F4F7A476BAFB77A50C6C9BFCE73A126B53073340C1157911F2B70407`. The equal size confirms that the named effect features still compile no additional effect implementation. No distribution archive was built.

## X173GE lazy shadow-map allocation

Device startup now creates one cleared 1 × 1 depth fallback, with all four cascade descriptors pointing to it. The first shadowed frame allocates and clears the four 2048² maps after the frame fence, updates descriptors, then drops the fallback. Scenes without shadows therefore avoid approximately 32–64 MiB of shadow image payload. The `clear --no-shadow` and default shadow smoke paths both resized and shut down with validation enabled and no warnings.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear` | 2,153,984 | `4FC8214B6297ACAC04DCF6535024EA7539B67F50CA932FD789465FF2C26D695A` |
| High quality | `cargo build --locked --release --example clear --features high-quality` | 2,153,984 | `C80D94B297CCBA15E102AAFDB1AE6C642984539CB72BA2943FACFB125600B68A` |

Both builds used `rustc 1.99.1`, target `x86_64-pc-windows-msvc`, and the workspace release profile from Git HEAD `26d37702d50acf2740b044e3c4e933b2bd00ac71` plus the current uncommitted files. Fingerprint: `11563AD5145BCAAF4E8E4110383E58003106BB6238F00CC5EE2A956233D77A2D` (40 files); `Cargo.lock` SHA-256: `5215B658F4F7A476BAFB77A50C6C9BFCE73A126B53073340C1157911F2B70407`. No distribution archive was built. Frame-time instrumentation and visual capture remain outstanding.

## X173GE spatial SMAA 1x milestone

The standard build now runs tone mapping, luma edge detection, AreaTex/SearchTex weight calculation, and linear neighborhood blending before present. The LUTs and compiled Vulkan 1.0 shader modules are embedded. Validation smokes passed with SMAA enabled and disabled, with and without shadows; each run resized and shut down without Khronos validation messages. All-feature workspace checks and tests passed. A read-only integration review found no concrete resource, descriptor, synchronization or feature-off defect. Visual captures have not been compared, so this only establishes the Vulkan pass path.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear` | 2,370,048 | `8F8844E9504A49D29BA92BC2A79B49D649DA2BF6E77BBFD4C597FF2BF1D0535E` |
| High quality | `cargo build --locked --release --example clear --features high-quality` | 2,370,048 | `0831B0EE25A687592C9F9B437CE387A8DD68110FE9D0AFDA4E5E9BC8A5E9598C` |
| Vulkan without SMAA | `cargo build --locked --release --example clear --no-default-features --features vulkan` | 2,154,496 | `67F8BB61F4A93C1E17D4FC723E2EF1056E6043C79E4E43FC5E77835013C0A6C0` |

All used `rustc 1.99.1`, target `x86_64-pc-windows-msvc`, the workspace release profile, and Git HEAD `26d37702d50acf2740b044e3c4e933b2bd00ac71` with the current uncommitted source. Fingerprint: `FEC11AD2606317960C6B6E5F6A3A700222C1B0E751E58FB4A72120B360E7CC5E` (51 files); `Cargo.lock` SHA-256: `5215B658F4F7A476BAFB77A50C6C9BFCE73A126B53073340C1157911F2B70407`. SMAA increases this example executable by 215,552 bytes; the two embedded lookup payloads account for 180,224 bytes of that difference. Equal standard and high-quality sizes reflect that the other high-quality effect flags still have no implementation. No distribution archive was built, so packaged size is not yet measured. CPU/GPU frame and per-pass timings are also still outstanding.

## Historical X173GE Windows release binary audit

These historical feature-selection measurements for the `clear` example were built on Windows with Rust 1.99.1, target `x86_64-pc-windows-msvc`, and the workspace release profile. The source fingerprint includes the root Cargo files and regular files under `src/`, `crates/`, and `examples/`, normalized and hashed using the procedure described above. Source fingerprint: `8967C5D8D4261EC93FE9EE24045B6EABE5AB37D6157073DA8534CF6F92754D37` (66 files); `Cargo.lock` SHA-256: `00F493E72EF82A8A2BFD76DC999392BC1BCEB0C4E087148FC6934BA8A8A0B1AC`.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear` | 2,586,112 | `991489941F69A9FF4CADE2654E8AD3F47A8DC429E26D08B1854CAA38E85556DF` |
| High quality | `cargo build --locked --release --example clear --features high-quality` | 2,623,488 | `4FCD8D28F0F227E20D058691AE180330E5866FB09E3E795DC4D2C5BDAE1587CC` |
| Vulkan without SMAA | `cargo build --locked --release --example clear --no-default-features --features vulkan` | 2,371,584 | `5822E5F466D7293F039981225728AAEE70B792E014665B499A075C0593CAB6F8` |

All three used isolated output under `.codex-target/binary-audit-20261110`. Standard was a cold 41.034-second build; the two subsequent builds were incremental and their elapsed times are not comparable. SMAA adds 214,528 bytes (about 8.3%) to this example; the explicitly selected Vulkan-only build retains PBR and shadows. At that source snapshot, high quality added 37,376 bytes over standard. The SSR renderer path was implemented after this audit, so these byte counts do not represent the current high-quality build. God Rays, translucent shadows, and TAA remain unavailable and cause a configuration error when requested.

The selected rebuild1 fixture `../rebuild1/assets/model.glb` is 75,654,468 bytes. No distribution archive has been created, so packaged size is not measured. Raw executable-plus-fixture sums, without compression, are 78,240,580 bytes for standard, 78,277,956 for high quality, and 78,026,052 without SMAA; these are arithmetic totals only. The release build generates a 1,519,616-byte PDB containing debug information; include it only if the distribution will ship it. Detailed command output is retained under `.codex-target/binary-audit-20261110`.

## Windows release binary audit before TAA integration (historical)

After adding the God Rays path, these `clear` example binaries were rebuilt into separate target directories so one feature selection could not overwrite the other. Both used `rustc 1.99.1`, `x86_64-pc-windows-msvc`, and the workspace release profile. The sorted path/content fingerprint covers files under `src/`, `crates/`, and `examples/`, plus root `Cargo.toml` and `Cargo.lock`: `F77D36973F58733E09EE48FC7EF19977933E7A335DED5C2BB63DE72BDDB77D86` (78 files). `Cargo.lock` SHA-256: `BAC5DE3AD0A6564725AAECA3C673E8C7DAB35BC9D6A718B148EA3B332AAE6D9A`.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear --target-dir .codex-target/size-standard` | 2,768,384 | `B7517EB2D3DA13A09BAAC743A354B80E6E0223855A7A3980878BBB661680F621` |
| High quality | `cargo build --locked --release --example clear --features high-quality --target-dir .codex-target/size-high-quality` | 2,870,272 | `F9CED535A0EDE8A0EE7067B81A6F28F115ADC4A2E9259C50108B13D43B88F25F` |

The high-quality build is 101,888 bytes (3.7%) larger than standard. Both executables load `../rebuild1/assets/model.glb` externally; no asset or distribution archive is included in these sizes. Translucent shadows and TAA still have no renderer implementation, so a runtime request containing either continues to fail explicitly. These are binary-size results only; they do not validate GPU behavior or packaged size.

## GLB specification-audit release sizes

After tightening raw glTF validation, the `clear` example was rebuilt into isolated standard and high-quality target directories. Both used `rustc 1.99.1`, target `x86_64-pc-windows-msvc`, and the workspace release profile, from Git HEAD `26d37702d50acf2740b044e3e4c933b2bd00ac71` plus the current uncommitted source. `Cargo.lock` SHA-256: `BB9C8A8410F08A15B6C5AF625BCC37638F7D64BC97AFB3171A96AE268AB9307E`.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear --target-dir .codex-target/spec-size-standard` | 2,806,272 | `FD16DC161288DBE893660CCCDC61136D766DEBDC8A6802A424E3190DF639609B` |
| High quality | `cargo build --locked --release --example clear --features high-quality --target-dir .codex-target/spec-size-high-quality` | 2,907,648 | `7A21E7E8FD88F9A7DC75D9E1DD787906BE5C4E7254F9FFBC1930A55602DC992F` |

The high-quality binary is 101,376 bytes (3.6%) larger. The 75,654,468-byte model remains external; no distribution archive was built, so packaged size is still unmeasured. This audit measures binary size only and makes no GPU behavior claim.

The same standard release build imported the model and extracted its default scene three times without starting Vulkan. `GltfAsset::import` took 64.2, 61.4, and 61.6 ms (median 61.6 ms); `scene_primitives` took 174.7, 172.9, and 176.7 ms (median 174.7 ms). Each extraction produced 180 scene primitives. This is an X173GE CPU reference for the exact model, not a rebuild1 comparison; the previous rebuild1 timing used a different scene.

## Optional frame profiling added after the binary audit

Build with `--features profiling` to measure fixed update, render extraction,
successful frame enqueue duration into the bounded queue, Vulkan command
recording/queue submission, and GPU passes. CPU rows are included only when the
same frame is later presented and selected; full-queue attempts are counted
separately. The GPU query path is enabled only when the selected graphics queue
reports nonzero `timestampValidBits`; otherwise the build prints an explicit
unavailable marker while retaining CPU measurements. GPU times use twelve
timestamp queries (eleven intervals) for shadow, GTAO, scene, post-scene SSR,
TAA, God Rays ray-march, horizontal/vertical reconstruction and composite
work, Bloom, and final post.
Inactive pass intervals remain near zero.
The total excludes acquire/present wait. CPU timings are kept by `FrameId` and
enter p50/p95 only after that frame returns `FramePresented` and the game selects
it. The summary stores up to 8,192 such rows; `queue_full_dropped_run` counts
bounded-queue failures across the whole run, and those attempts are not included
in the CPU percentiles. Renderer recording time is reported per frame after its
fence completes. The `scene_ms` interval includes
BLEND items when SSR is disabled; `ssr_separate_blend_ms` is only post-scene SSR
and its separate BLEND pass when SSR is enabled. Profiling is optional and adds
no query or timer work to the standard build. Model runs also log the image count
and estimated decoded RGBA8 payload from validated image dimensions; this is
asset payload size, not measured process or GPU residency.

`clear --frames N --warmup M` selects CPU rows for exactly the N presented
frames after every asset upload is ready and M loaded frames have been
presented. This excludes asset loading, warmup, and unpresented frames from its
CPU p50/p95. Other game definitions include every presented frame by default
and can override the profiling hook to select a narrower interval.

For GPU parity with rebuild1's `FreeCamera::frame_bounds`, pass
`--r1-benchmark-camera`. It reproduces the model-center-relative eye offset,
60-degree vertical FOV, and near/far rules for the imported model bounds. The
normal interactive viewer starts at the saved review pose; `--r1-benchmark-camera`
remains a separate, model-relative framing. Synthetic demo point and spot lights
are disabled in benchmark-camera mode unless explicitly requested.

These timers are ready to collect a baseline but have not yet produced an
X173GE-versus-rebuild1 comparison. The selected comparison scene remains the
75,654,468-byte `../rebuild1/assets/model.glb` with SHA-256
`E9852865A6D6B7779C8A6CB390A0F9414B0AB247AE45F62E3FEAC190C65A6694`; prior
rebuild1 numbers above use a different scene and must not be treated as
comparable. Vulkan allocation tracking is now implemented, but its runtime
high-water measurement is still pending. Process CPU-memory comparison and a
packaged-size comparison remain outstanding.

The review example can hold a fixed 1280×720 surface, wait for all scene uploads,
run warmup frames, then count an exact number of presented frames:

```powershell
cargo run --release --locked --example clear --all-features --features profiling -- --asset ../rebuild1/assets/model.glb --warmup 60 --frames 180 --gtao --ssr --bloom --god-rays
```

The all-effects command above is a feature smoke, not yet a measured parity
run. The commands below are a descriptive workload comparison, not controlled
engine-speed or image parity. They align the model-framing camera, 1280×720
window, nominal 60 Hz cap, release optimization, and disabled post effects.
Keep shadows enabled in X173GE: rebuild1's `performance` preset disables post
effects but still uses 2048² CSM/PCSS shadows. `--no-environment` selects
X173GE's analytic sky/ground fallback; run once more without it to measure the
separate HDRI feature cost.

The camera match is derived from source, not estimated from screenshots.
Rebuild1's `FreeCamera::frame_bounds` uses the imported bounds center plus an
eye offset of `(0, 0.24, 2.45) × radius`, and `camera_snapshot` uses a 60°
vertical FOV. X173GE recenters the same transformed bounds at the origin and
uses that same eye offset, FOV, and scene-dependent near/far formulas in
`FreeCamera::r1_benchmark_view` and `camera_clip_range`. Both request a
1280×720 surface. For this model, the corresponding near plane is about
0.1086 world units. This aligns projection and framing while leaving renderer
and scene-lighting differences documented below.

Material differences remain. Rebuild1 starts its day/night light at sunrise,
changes ambient strength with that cycle, applies camera auto-exposure and white
balance, and reads the final image back every 12 frames. X173GE uses a fixed
white directional light and fixed analytic ambient values, has no matching
auto-exposure path, and normally enables the HDRI. Rebuild1 also allows only
one frame in flight, while X173GE keeps a two-message renderer queue and may
drop a submission when it is full. The recorded runs used a nominal 60 Hz
schedule (the current `FramePacing` cap is configurable); actual CPU pacing and
throughput are not equivalent. Compare GPU pass times as
descriptive measurements; record CPU timings, per-run drops, presented sample
count, first/last measured frame IDs, wall duration, and the light/exposure and
draw-count differences alongside them.
The rebuild1 smoke sample starts the ten-minute lighting cycle at sunrise; its
600-frame sample at 60 Hz spans about ten seconds and therefore remains a
short, slowly changing-light interval rather than a fixed-light benchmark.

Discard the first 120 loaded frames from rebuild1's GPU trace and use X173GE's
`--warmup 120 --frames 480`. Save both logs and record GPU/driver, executable
hash, model hash, camera pose, extent, quality settings, warm-up/measured frame
counts, asset upload time, and memory high-water marks. The X173GE CPU summary
prints selected presented-frame samples and run-wide attempted, enqueued,
presented, renderer-dropped, invalidated, and queue-full counts. Its run counters
include asset loading and warm-up; the exact measurement interval is identified
by the first/last measured frame IDs and selected sample count. The viewer also
prints time between the first and last measured presentation events and their
inter-frame rate. Do not interpret CPU samples as time spent by dropped frames.

Reproduce after every interactive review window is closed so it cannot occupy
the GPU:

```powershell
# rebuild1; the selected asset is the same unchanged GLB as X173GE
Set-Location D:\RP\valkan-test\rebuild1
$env:REBUILD1_WINDOW_ASSET = 'D:\RP\valkan-test\rebuild1\assets\model.glb'
$env:REBUILD1_WINDOW_QUALITY = 'performance'
$env:REBUILD1_WINDOW_SMOKE_FRAMES = '600'
$env:REBUILD1_MAX_FPS = '60'
$env:RUST_LOG = 'gr_render::renderer::vulkan=info,gr_render::renderer::vulkan::gpu_timing=trace'
cargo run --release -- --window-smoke 2>&1 | Tee-Object .codex-target\bench-model-r1.log

# X173GE; same framing, resolution, shadow presence and disabled post-effects
Set-Location D:\RP\valkan-test\X173GE
cargo run --release --locked --features profiling --example clear -- --asset D:\RP\valkan-test\rebuild1\assets\model.glb --r1-benchmark-camera --no-smaa --no-environment --warmup 120 --frames 480 2>&1 | Tee-Object .codex-target\bench-model-x173ge.log

# Repeat the X173GE run with its default HDRI enabled to isolate IBL cost.
cargo run --release --locked --features profiling --example clear -- --asset D:\RP\valkan-test\rebuild1\assets\model.glb --r1-benchmark-camera --no-smaa --warmup 120 --frames 480 2>&1 | Tee-Object .codex-target\bench-model-x173ge-hdri.log
```

Do not use the first release profile attempt as a baseline. The exact scene and
180-frame interval were captured while the separate interactive Debug review
window was still rendering on the same GPU. The logged range reached 29.09 ms
and is marked contaminated; its raw output is
`.codex-target/model-profile-release-20261110.log`. Rerun only after the review
window is closed, then record the first/last measured frame IDs, feature flags,
and machine state with the results.

## Static scene extraction fast path (2026-11-10)

The selected 75,654,468-byte model was imported and expanded to its default
scene five times in a CPU-only release helper. Moving the no-op path ahead of
the per-instance vertex copy changed `scene_primitives` median time from
233.9 ms to 230.6 ms (about 1.4%); each run produced 180 primitives. The
before range was 233.1–235.7 ms and the after range was 228.8–232.8 ms.
Import medians were 64.3 ms before and 65.1 ms after, within the run-to-run
variation. This is a small local improvement measurement, not a rebuild1
comparison or a GPU result.

## Previous Windows release binary audit (2026-11-10, before punctual-light integration)

The `clear` review example now loads the exact sibling
`../rebuild1/assets/model.glb` by default; `--asset` remains an override. A
CPU-only import after the GLB specification fixes extracted 180 primitives
from its default scene. These release executables were built into separate
target directories and contain no model data:

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --locked --release --example clear --target-dir .codex-target/size-current-standard` | 2,856,448 | `0F018AF8B54EEDC03EE56C305E8927573CABBA453650C6C8E7B2DBBAA8804502` |
| High quality | `cargo build --locked --release --example clear --features high-quality --target-dir .codex-target/size-current-high-quality` | 3,148,800 | `495170C501FA6F0545B19BAB87A52F5DEA50088DDDC85AA1ED84BEE4C5C87FFE` |

The high-quality build is 292,352 bytes (10.2%) larger; this feature bundle
includes TAA. The external model is 75,654,468 bytes, giving arithmetic
executable-plus-model totals of 78,510,916 and 78,803,268 bytes respectively.
No distribution archive was built, so package size is not measured. The runs
used `rustc 1.99.1`, `x86_64-pc-windows-msvc`, the workspace release profile,
and Git HEAD `26d37702d50acf2740b044e3c4e933b2bd00ac71` plus the working tree at
binary-build time. The sorted path/content source fingerprint covers `src/`, `crates/`, and
`examples/`, plus root `Cargo.toml` and `Cargo.lock`:
`67790575A732426A2D77E3B3287773F024A0771577E5C2C4F375F3303A5FFBAB` (99
files). `Cargo.lock` SHA-256:
`BB9C8A8410F08A15B6C5AF625BCC37638F7D64BC97AFB3171A96AE268AB9307E`.
After that binary build, one `#[cfg(test)]` GLB regression test was added to
assert that an implicit buffer without a BIN chunk is rejected; this test is
excluded from the release executable.

The GPU path has passed Debug Vulkan validation on the NVIDIA GeForce RTX
4070 SUPER: a God Rays-only run and a full high-quality run with GTAO, SSR,
Bloom, God Rays, and TAA presented two frames without validation messages.
The Debug full high-quality run also completed resize and post-resize
presentation checks and shut down cleanly. A Release smoke run with the exact
GLB and all five effects enabled also completed resize and twelve
post-resize frames; its output is `.codex-target/vulkan-high-quality-resize-release-final.log`.
Human visual evaluation of the model remains pending. GPU memory and clean
frame-time measurements are still outstanding; no distribution archive was
built, so package size remains unmeasured. These executable sizes predate the
current punctual-light implementation and are historical until standard and
high-quality release builds are repeated.

## Windows release artifacts before allocation tracking (2026-11-11)

These standard and high-quality artifacts were built before the Vulkan memory
tracker was added. Their sizes, hashes, and source fingerprint are historical;
repeat the release builds after the review windows close to measure the current
source without interrupting visual evaluation.

The current tree was built on `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, target
`x86_64-pc-windows-msvc`, with the RTX 4070 SUPER / driver 610.88. The standard
build uses Cargo's default `vulkan` + `smaa` features; the high-quality build
adds GTAO, SSR, Bloom, God Rays, translucent shadows, and TAA. Both use the
workspace release profile (`opt-level=3`, fat LTO, one codegen unit, stripped
symbols):

```powershell
cargo build --locked --release --example clear --target-dir .codex-target/final-size-standard
cargo build --locked --release --example clear --features high-quality --target-dir .codex-target/final-size-high-quality
```

| Selection | Executable bytes | SHA-256 |
|---|---:|---|
| Standard | 2,960,384 | `3B90F5A4C8BEDDCADCC89BCC87BE74615E82FE406C298A6A12C4EB7D8CDACF1C` |
| High quality | 3,318,272 | `D6B673A7192CB079D76070F2B36660CC275009728FFDAEE19D2EEAAA7055969A` |

The high-quality feature bundle adds 357,888 bytes (12.09%) to the example
executable. For a concrete fixture package, each ZIP contains only its
`clear.exe` and the exact external 75,654,468-byte `../rebuild1/assets/model.glb`
(SHA-256 `E9852865A6D6B7779C8A6CB390A0F9414B0AB247AE45F62E3FEAC190C65A6694`),
compressed with PowerShell `Compress-Archive -CompressionLevel Optimal`:

| Selection | ZIP bytes | SHA-256 |
|---|---:|---|
| Standard | 65,434,649 | `EDE06E53FDE0F1C5B37659CA64BCB41CC9603F93CE8FF3EA6A068B5C4B5539E2` |
| High quality | 65,538,659 | `692C1FF15A051FBA63AE6D5046324349C0B4F9DE262A100A668F26CBE28080D7` |

These are viewer-fixture package sizes, not a general game distribution: they
include the large review model and rely on the machine's Vulkan loader. The
archives and executables are retained under `.codex-target/package-*-ready-20261111`
and `.codex-target/final-size-*`. The source build is Git HEAD
`26d37702d50acf2740b044e3c4e933b2bd00ac71` plus the preserved pre-existing
parent-tree patch (SHA-256
`166925670804caa3ea8880d79222e7fe00e6ae7b36ca3b8c3f9367062cdfda0f`) and the
untracked X173GE workspace. The source fingerprint over `Cargo.toml`,
`Cargo.lock`, `src/`, `crates/`, and `examples/` is
`E4DD321490553120616174389B77F6B0B736B74372DDF372BC95D4300B2358B4` (102
files); `Cargo.lock` SHA-256 is
`BB9C8A8410F08A15B6C5AF625BCC37638F7D64BC97AFB3171A96AE268AB9307E`.

The standard and high-quality Release smoke logs both show successful GLB
import, `upload_ms`, resize generation 4, and twelve post-resize frames after
all uploads completed. The Debug high-quality smoke used the Khronos validation
layer (`Vulkan validation layer enabled: true`) and the same completed-upload
and resize gate; it exited successfully without validation diagnostics. Logs:
`.codex-target/vulkan-standard-resize-20261111.log`,
`.codex-target/vulkan-high-quality-resize-20261111.log`, and
`.codex-target/vulkan-high-quality-debug-validation-ready-20261111.log`.
At this historical source snapshot, `cargo test --workspace --all-features
--locked` and `cargo fmt --all -- --check` passed. Current Vulkan tracker
verification is recorded below. Neither these checks nor the old artifact
sizes provide a clean frame-time comparison; that measurement must wait until
the interactive review windows are closed and rebuild1 is run with matching
scene and settings.

## Vulkan allocation accounting (2026-11-11)

The Vulkan backend now tracks the exact `VkMemoryRequirements.size` bytes it
requests for each engine-owned `VkDeviceMemory` allocation. The per-device live
counter is decremented on matching frees; a lifetime high-water mark includes
staging buffers and temporary resources during uploads and resize. The
`X173GE_GPU_MEMORY` shutdown record labels this as engine-owned allocation
bytes, not total VRAM. Swapchain-managed images and driver-internal allocations
are excluded. All direct Vulkan allocate/free calls are centralized in the
tracked wrapper, including bind/view failure cleanup, surface teardown, and
resource replacement.

The Windows `x173ge-renderer-vulkan` all-feature check passes, its 17 unit tests
pass, and workspace formatting is clean. A later GTAO-on review run ended with
`ERROR_DEVICE_LOST` after 84 frames and emitted `live_bytes=0`,
`peak_bytes=821799104`. This verifies allocation cleanup on that error path;
normal shutdown and separate standard/high-quality peaks remain unverified.
At this historical check, the matched GTAO-off review window and three other
review windows were still open. A later check found one old viewer process;
its current state is recorded below.

## Superseded Windows release artifacts with environment lighting and error context (2026-11-11)

Historical sizes from this snapshot; the later GTAO calibration section below
contains the current release artifacts.

These Release builds include the diffuse sky/ground environment coefficients
and input validation, plus operation names on frame-path Vulkan errors. They
use `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, target
`x86_64-pc-windows-msvc`, and the workspace optimized Release profile. The
standard build uses default `vulkan` + `smaa`; high-quality adds GTAO, SSR,
Bloom, God Rays, translucent shadows, and TAA. `profiling` is excluded from
both size measurements.

```powershell
cargo build --locked --release --example clear --target-dir .codex-target/current-size-standard
cargo build --locked --release --example clear --features high-quality --target-dir .codex-target/current-size-high-quality
```

| Selection | Executable bytes | SHA-256 |
|---|---:|---|
| Standard | 2,982,912 | `5AC18E79D877DFCDC368857976A3BBFAAF1647ED07EE871EE22FC5AEEE527140` |
| High quality | 3,350,016 | `093E45C8FD52D0991BBE7452676EC8F506AA189057B2063AF9E419D5B154247C` |

The high-quality selection adds 367,104 bytes (12.31%) to the example
executable. Each fixture ZIP contains only that `clear.exe` and the unchanged
75,654,468-byte `../rebuild1/assets/model.glb` (SHA-256
`E9852865A6D6B7779C8A6CB390A0F9414B0AB247AE45F62E3FEAC190C65A6694`):

| Selection | ZIP bytes | SHA-256 |
|---|---:|---|
| Standard | 65,443,774 | `BC93050CC680CD5637F93D3FB79B2CCD8231A5AEEDCD51356310626890E71282` |
| High quality | 65,552,445 | `5F5FC0B30658C46BFD1332F96089B9392F8D0796151BAFF1F013349D5EB9D92B` |

The source snapshot fingerprint is `6B5FBDA209862EF1BA0D1C47A5C3AABB622844C9D5B0FCC72FF54FDF86F0D245`
over 102 files under `Cargo.toml`, `Cargo.lock`, `src/`, `crates/`, and
`examples/`. It is the SHA-256 of the UTF-8 text containing each sorted
relative path and that file's lowercase SHA-256, one entry per line. The lock
file hash remains `BB9C8A8410F08A15B6C5AF625BCC37638F7D64BC97AFB3171A96AE268AB9307E`.
The X173GE directory remains untracked inside the parent repository; the
parent HEAD and existing unrelated worktree changes are preserved. These
artifacts measure executable and fixture-package size only, not a complete
redistributable Vulkan runtime. Clean CPU/GPU performance comparison and
human image-quality acceptance remain open.

## Superseded Windows release artifacts after queue and specular-environment changes (2026-11-11)

Historical sizes from this snapshot; the later GTAO calibration section below
contains the current release artifacts.

These measurements were rebuilt from the D: workspace using the same Rust
toolchain and Release profile. Each quality selection used its own target
directory so Cargo feature artifacts could not overwrite the other build.
Standard is default `vulkan` + `smaa`; high quality adds GTAO, SSR, Bloom, God
Rays, translucent shadows, and TAA. Both exclude `profiling`.

```powershell
cargo build --locked --release --example clear --target-dir .codex-target/ibl-size-standard
cargo build --locked --release --example clear --features high-quality --target-dir .codex-target/ibl-size-high-quality
```

The builds use `rustc 1.99.1 (58cf8f9c3 2026-10-13)`,
`x86_64-pc-windows-msvc`, and `Cargo.lock` SHA-256
`BB9C8A8410F08A15B6C5AF625BCC37638F7D64BC97AFB3171A96AE268AB9307E`.
The source fingerprint covers 103 files under `Cargo.toml`, `Cargo.lock`,
`src/`, `crates/`, and `examples/`; each sorted line is the slash-separated
relative path, a space, and its lowercase SHA-256. SHA-256 of the UTF-8 lines
joined with `\n` and no trailing newline:
`3B0E398F5D257365A7098587DD6F6C03F3264242D05424333F69F9C1BE1EDE4C`.

| Selection | Executable bytes | SHA-256 |
|---|---:|---|
| Standard | 2,984,960 | `9A0AA23593F1E4E071A863EEF57E29EAADBBCC6B92C88A9D8C0FB72199FBB212` |
| High quality | 3,353,600 | `1DFC769FC234BF592E2D83F0AABFC8098DB191DC5998B88935D8A7729AB2BF09` |

The high-quality build adds 368,640 bytes over standard. Compared with the
previous recorded build, these combined queue and shader changes add 2,048
bytes to standard and 3,584 bytes to high quality; this is not an IBL-only
delta.

Each fixture ZIP contains only `clear.exe` and the unchanged 75,654,468-byte
`../rebuild1/assets/model.glb` (SHA-256
`E9852865A6D6B7779C8A6CB390A0F9414B0AB247AE45F62E3FEAC190C65A6694`). ZIPs
were made with PowerShell 7.6.5 `Compress-Archive -CompressionLevel Optimal`.

| Selection | ZIP bytes | SHA-256 |
|---|---:|---|
| Standard | 65,444,316 | `DF179B452E02A67AF3072F01D02B52387E9EA27E16B71CD0C2A5CDBC675A2670` |
| High quality | 65,557,524 | `5F7EEB0AC0CA0EA9F1E45CA5C6E3A51B0AC980D7B740C127FD9A1E9862A7CC4D` |

The ZIPs are size fixtures, not complete runtime distributions; they omit the
machine-provided Vulkan loader and do not include a redistributable runtime.
`fmt --check`, all-feature workspace check, and all-feature workspace tests
pass (137 tests). PBR shader variants and SSR compiled with `glslc` and passed
`spirv-val`. A four-frame Vulkan smoke using the standard GLB, SSR on, and GTAO
off passed on an NVIDIA GeForce RTX 4070 SUPER; it reported
`peak_bytes=761572032` engine-owned Vulkan allocations and `live_bytes=0` on
shutdown. This does not replace same-scene R1-versus-X173GE frame-time
measurement or the required human visual review; both remain open.

## Superseded Release artifacts after SSR subpixel sampling and camera precision controls (2026-11-11)

Historical sizes from this snapshot; the later GTAO calibration section below
contains the current release artifacts.

The SSR hit still uses the containing depth texel but samples scene color at
the hit's subpixel DDA location, as described under visual-quality limits in
[`research/ssr.md`](research/ssr.md). The example also has an opt-in
`--r1-benchmark-camera` that matches rebuild1's model-relative framing, FOV,
and clip-plane rules; the interactive default remains at model center. The
interactive speed range now permits a sub-near-plane movement step for
close-up inspection. Both
Release variants were rebuilt independently with the same Rust toolchain and
workspace profile; profiling is excluded from the binary measurements.

| Selection | Executable bytes | SHA-256 |
|---|---:|---|
| Standard (`vulkan` + `smaa`) | 2,990,592 | `7BEEB4B735DD2861E0FF7BAE9228AD607A41FBBD6D59162BCD789DC6F49A0D67` |
| High quality | 3,347,968 | `351D3591C719F9679B79A76AE59185BEAC579828AE8CA9E0148FB2EBA3B66635` |

The high-quality bundle adds 357,376 bytes (11.95%) over standard. Each
fixture ZIP contains only `clear.exe` and the unchanged 75,654,468-byte model
with SHA-256
`E9852865A6D6B7779C8A6CB390A0F9414B0AB247AE45F62E3FEAC190C65A6694`:

| Selection | ZIP bytes | SHA-256 |
|---|---:|---|
| Standard | 65,445,701 | `46BEE7E0BF5DB89370559F13D9E426DFE9E2C4EEE79FB38E6C076E23E846700E` |
| High quality | 65,554,645 | `F3F483D16162EB48FBC5E8C49C4A47A6E117C232100D8F1A7C2FA6712A090333` |

These viewer fixtures are not complete game distributions and depend on the
machine Vulkan loader. Source fingerprint over `Cargo.toml`, `Cargo.lock`,
`src/`, `crates/`, and `examples/`: `651E86D6343ABC8BB96C7651B32F7D782D1252E577C596D5F8ACFFE980F74352`
(103 files); `Cargo.lock` SHA-256 remains
`BB9C8A8410F08A15B6C5AF625BCC37638F7D64BC97AFB3171A96AE268AB9307E`.
`ssr.frag` and the TAA shader set compiled with `glslc` and passed
`spirv-val`; `cargo test --workspace --all-targets --all-features --locked`
passed 138 tests. A GPU timing comparison and visual approval of the updated
SSR shader are still outstanding; the existing review window predates this
shader change.

## GPU watchdog and asset upload investigation (2026-11-11)

The rebuild1 fixture is 75,654,468 bytes and contains 180 mesh primitives, 28
materials, 32 textures, and 26 images. The viewer decoded the images to
521,142,272 RGBA8 bytes and reported a peak of 821,799,104 engine-owned Vulkan
bytes. One interactive run reported `upload_ms=13445.896`. It also recorded
`queue_full_dropped_run=435`; this counts frames rejected by the bounded
capacity-2 renderer command queue while uploads were being processed. It does
not indicate a failed upload or GPU queue corruption.

The current Vulkan upload path processes each renderer command in order.
`upload_mesh` separately submits and waits for its vertex and index buffers,
and `upload_rgba8_image` submits and waits for each image and its mip chain.
This identifies repeated synchronous upload work as a structural cost, but
the 13.4-second run is not a clean single-process baseline: logs show a GTAO-off
viewer still running while a GTAO-on viewer was started. Measure again with
one viewer and no concurrent GPU work before quantifying an improvement.

At 01:27 local time, Windows recorded `nvlddmkm` Event ID 153 and a
LiveKernelEvent 141 while the GTAO-on viewer ended with Vulkan
`ERROR_DEVICE_LOST`. Microsoft defines 0x141 as a display engine that did not
respond in time ([VIDEO_ENGINE_TIMEOUT_DETECTED](https://learn.microsoft.com/windows-hardware/drivers/debugger/bug-check-0x141---video-engine-timeout-detected)).
The associated WATCHDOG dump is no longer present, and the viewer log does not
name the failing Vulkan call. The available evidence therefore does not
identify GTAO or any individual rendering/upload command as the cause. The
viewer concurrency is a confounder; the device-loss cause remains unresolved.

The `profiling` feature now logs image decode, buffer/image allocation and
transfer, and material registration times separately. It reports each
buffer's reuse and post-submit fence waits; these fine-grained timers and
their log strings compile out of the ordinary and high-quality builds. The
existing aggregate `upload_ms` and GPU memory counters remain in those builds.
A clean single-viewer capture is still pending so the measured total can be
attributed to the recorded stages.
The profiling standard viewer is 3,018,240 bytes
(`E0A9FCE27D09A2B90BA2B597FAF6FD6DFD35FC3328DEA4D4B645FEF11D56C614`),
27,648 bytes larger than standard. This also includes the existing GPU pass
timestamp profiler and is not a distributable build.

## GTAO strength calibration build (2026-11-11)

After the user confirmed that surface darkening followed GTAO, both PBR shader
variants reduced the ambient visibility blend from 50% to 25%. The equations
and sampled visibility are unchanged; this is a visual contribution
calibration. A source audit confirmed GTAO is applied once to diffuse ambient
lighting. The standard model contains no `occlusionTexture`, so baked material
AO does not stack with GTAO in this review fixture. `docs/research/gtao.md`
records the decision and remaining visual review condition.

All eight generated PBR fragment variants were recompiled with `glslc` and
validated with `spirv-val`. A surface-generation regression test resizes from
generation 1 to 2, verifies an old snapshot is dropped before backend
rendering, and verifies a generation-2 frame is presented.
`cargo fmt --all --check`, `cargo check --workspace --all-targets --all-features
--locked`, and `cargo test --workspace --all-targets --all-features --locked`
passed; the test suite contains 139 tests. The release builds use `rustc 1.99.1`,
`x86_64-pc-windows-msvc`, and the workspace release profile. The source
fingerprint covers 103 files under `Cargo.toml`, `Cargo.lock`, `src/`,
`crates/`, and `examples/`; paths are normalized to `/` and sorted by
case-sensitive Unicode ordinal order. Each line contains the path, a space,
and the file's lowercase SHA-256. The fingerprint is the SHA-256 of the UTF-8
lines joined with `\n` and no trailing newline:
`955576D4F86D59C85225343B56170A1191D24F19DA3EDFA253F57ED37D851E2E`.
`Cargo.lock` SHA-256 is
`7EC5B176C295CBF3DB2F525DA85847C4FE7E9BAE47E10AAA5140F6B31B912631`.

| Selection | Executable bytes | SHA-256 |
|---|---:|---|
| Standard (`vulkan` + `smaa`) | 2,978,816 | `78D868E2EDAEF9C29528B47FBCF00A36D8F33FC6EAE81F2E7AB0FE4D03DF0638` |
| High quality | 3,336,192 | `7472E8CBD65CF143F8EF7576744EA290D6DD199637124DBCD2718F353C3C3B45` |

The high-quality build adds 357,376 bytes (12.00%) over standard. Compared
with the preceding release snapshot, both executables are 11,776 bytes smaller.
Fixture
archives contain only `clear.exe` and the unchanged 75,654,468-byte model
(SHA-256 `E9852865A6D6B7779C8A6CB390A0F9414B0AB247AE45F62E3FEAC190C65A6694`):

| Selection | ZIP bytes | SHA-256 |
|---|---:|---|
| Standard | 65,441,035 | `086A8FCFBB2D169ED9D7A89C2081626AB2331A7B8040E752CAD1F19167446C1F` |
| High quality | 65,550,902 | `C8A81827D1380CA06A12F3B1C10B2DB3621616BC3A9E73F4066A670D9633DBC8` |

The GTAO-off/with-GTAO same-camera visual review remains pending. The open
review process uses the previous executable and was left running; the updated
high-quality viewer is built at
`.codex-target/final-size-high-quality/release/examples/clear.exe` and has not
been launched, so GPU visual output and timing are not yet verified.

## Current Debug validation and profiling builds (2026-11-11)

The latest source was also built into separate Debug and profiling targets in
preparation for the remaining GPU checks:

| Purpose | Build command | Executable bytes | SHA-256 |
|---|---|---:|---|
| High-quality Debug / validation | `cargo build --locked --example clear --features high-quality --target-dir .codex-target/final-validation-debug` | 8,028,160 | `DB148A108EA34DE0B449CBED74A7D92B05D8A9AD67C19ACA9BC11F86E52FC3C1` |
| Standard Release / profiling | `cargo build --locked --release --example clear --features profiling --target-dir .codex-target/final-upload-profile` | 3,006,464 | `7177306E513E9506552814C7081CD1379B3083DD0F0B093A43143FA2DA771880` |

Neither binary has been launched. The Debug code enables the Khronos
validation layer when both the layer and debug-utils extension are available;
the earlier Debug smoke logged that it was enabled. Use this build for the
latest resize/shutdown validation smoke, then use the profiling build for one
clean upload/frame capture after the existing review process exits.

## CPU-side integration audit after artifact builds (2026-11-11)

The renderer protocol now rejects vertical-environment coefficients that
would reconstruct negative sky or ground radiance. Material conversion builds
on the existing factor initializer and adds resolved texture bindings, removing
the duplicate field initialization. The asset crate now stores imported
buffers as byte vectors and no longer enables gltf's unused bulk-import
feature; the bounded image path remains unchanged.

A CPU-only smoke against `../rebuild1/assets/model.glb` imported and extracted
180 scene primitives and 28 materials, then decoded all 26 referenced images
to 521,142,272 RGBA8 bytes. This used a temporary runner outside the workspace
lock and is import/decode evidence only, not a release performance baseline or
a Vulkan upload/render check.

`cargo fmt --all --check`,
`cargo check --workspace --all-targets --all-features --locked`, and
`cargo test --workspace --all-targets --all-features --locked` pass (139
tests). `cargo check --workspace --no-default-features --all-targets --locked`
also passes, so the facade and Null renderer build without the Vulkan feature.
The final Release, Debug, profiling, and ZIP fixtures above were rebuilt from
this historical source fingerprint. Eight PBR variants compiled into
`.codex-target/shader-validation-current` and passed `spirv-val`. The live GPU
process recorded at this snapshot was PID 58728 using the prior
`.codex-target/ibl-size-high-quality` executable; the current review launch is
recorded below.

## HDR environment diffuse and specular integration (2026-11-11)

The selected HDRI now supplies nine-coefficient diffuse irradiance and a
renderer-owned, GGX-prefiltered cubemap. The Vulkan helper and shaders compile;
the CPU suite, all-feature check, and shader validation passed. The environment
upload path also emits a profiling-only stage timer covering the upload and
prefilter wait. A later Release review launch loaded this HDRI and the standard
model successfully; see “Current D-drive HDRI review launch” below. Human image
quality review, current-source Vulkan validation, and clean profiling timings
remain pending.

Release executables were built with `rustc 1.99.1`,
`x86_64-pc-windows-msvc`, and the workspace fat-LTO profile. The source
fingerprint covers files under `src/`, `crates/`, and `examples/`, plus root
`Cargo.toml` and `Cargo.lock`; paths are normalized to `/` and sorted
case-sensitively before hashing each path, NUL, file bytes, NUL. Fingerprint
(`109` files): `02562A11B1E0E7475A8108657D497B0ADC9613A8CE3D00EB34AAD7AA3A37A231`.
`Cargo.lock` SHA-256:
`7EC5B176C295CBF3DB2F525DA85847C4FE7E9BAE47E10AAA5140F6B31B912631`.

| Selection | Executable bytes | SHA-256 |
|---|---:|---|
| Standard (`vulkan` + `smaa`) | 3,183,104 | `C309DDA9840B640BD959DFEB2ADA04DEE39E13C1BDA84EF87F55DAD08E45C481` |
| High quality | 3,559,936 | `A9ED94652333B70E7918B31F215DA77F230723ACD15A37AF69DBB15EECF193AA` |
| Standard with `profiling` | 3,216,384 | `F79E18ADC979DB640DC115B65772473D6C59EE4D9AD888C05C94F33638D5B351` |

High quality adds 376,832 bytes (11.84%) over standard; profiling adds 33,280
bytes (1.05%). These are executable
bytes. Review-fixture ZIPs also bundle the selected 75.7 MB rebuild1 model,
the 1,762,844-byte HDRI, and a short run guide; these are local review fixtures,
not redistributable game packages because the model's distribution rights have
not been established.

| Selection | Fixture ZIP bytes | SHA-256 |
|---|---:|---|
| Standard | 67,127,614 | `23C7D6BA0FDCA032F3D20E8306B7888AFE8CB6F718A1961563CA9B16F4BFA37F` |
| High quality | 67,243,937 | `D5770DC9F0ACFDFAE8333EBFA51A02593D4583F488E06FB9045EFAE47CC0F714` |

The current fixture archives are `.codex-target/package-ibl-review-20261111-current-standard.zip`
and `.codex-target/package-ibl-review-20261111-current-high-quality.zip`.
Their executable entries match the current Release hashes, and the model,
HDRI, and attribution files were verified against their staged sources.

The high-quality fixture adds 116,323 compressed bytes (0.17%). The review binary is
`.codex-target/ibl-render-review-20261111-high-quality/release/examples/clear.exe`.

## GTAO signed-horizon correction and profiling totals (2026-11-11)

An independent root-cause audit found that `gtao.frag` clamped signed horizon
cosines to zero, unlike Algorithm 1 of the Activision technical memo. Negative
cosines therefore closed open horizons and lowered visibility. The shader now
uses the raw signed dot product; the new module was compiled for Vulkan 1.0 and
passed `spirv-val`. The earlier 25% blend is held constant for visual A/B and
remains provisional because it was calibrated with this bug present.

The optional runtime profiler now counts extracted frame attempts, accepted
queue submissions, presented events, renderer drops, surface invalidations,
and queue-full submissions. `RendererService::shutdown_with` observes accepted
frames completed while shutdown drains the bounded queue, so run totals include
the final pending events. The clear example records the wall interval between
the first and last measured presentation events and derives their inter-frame
rate. These are whole-run counts; only CPU percentile samples are restricted to
the selected measured frames. A renderer regression test verifies that the
shutdown observer sees a queued frame's final `FramePresented` event.

The then-current source snapshot was rebuilt with `rustc 1.99.1` for
`x86_64-pc-windows-msvc` using the workspace fat-LTO Release profile. Its
fingerprint is the 109-file `02562A11B1E0E7475A8108657D497B0ADC9613A8CE3D00EB34AAD7AA3A37A231`
above. The current high-quality Debug validation executable is
`.codex-target/gtao-validation-debug/debug/examples/clear.exe` (8,544,256
bytes, SHA-256 `AB8369A1455083F0766FCA007520B7B01E5EEA7577AD8C1D40AA17A3264BAD6D`).
At that snapshot the full workspace had 146 passing tests; all-feature and
no-default-feature checks passed. GTAO SPIR-V is 6,908 bytes with SHA-256
`F6F3B965DE46CC746B7B58905106CC49771B8A0DCD04752018ECFF9EC629D958` and passes
`spirv-val`. The current HDRI review binary includes this signed-horizon fix,
but GTAO is disabled in that run; human GTAO review and current-source Vulkan
validation remain pending.

## Current D-drive HDRI review launch (2026-11-11)

The default HDRI path initially pointed at a nonexistent crate-root
`assets/` directory. Startup logging showed environment import failing before
the standard GLB load. The path now resolves to
`examples/assets/aloe_farm_shade_house_1k.hdr`, with a regression test that
checks the built-in asset exists. The all-feature workspace suite now passes
147 tests, and all-feature plus no-default-feature workspace checks pass.

The review High-quality Release was rebuilt after the default-HDRI path fix at
`.codex-target/ibl-render-review-20261111-high-quality/release/examples/clear.exe`.
It is 3,559,936 bytes with SHA-256
`3844BE5F0A4CD8170FA4500B79DDD69837B1EA677736E470B74DCF046C21B954`; the
build source fingerprint is `2250D18825BB40C551A7A05DBACC4B9FFA45D98EEC9091250BDD6E6F8F85CE30`
over 109 files, and `Cargo.lock` remains
`7EC5B176C295CBF3DB2F525DA85847C4FE7E9BAE47E10AAA5140F6B31B912631`.
After that binary was launched, the current source corrected the misleading
pre-decode `decoded_rgba8_bytes` log label. The 109-file current source
fingerprint is `7913853068329D1A8CAF0966E958BC15F98766D9380E5EFD58413313762AE460`.
Formatting, both workspace checks, and the full 147-test suite pass on this
source. The standard/high-quality Release binaries and fixture ZIPs have since
been rebuilt from this fingerprint; see the current-source size record below.
The prior interactive review process used the preceding source binary. It
closed on 2026-11-12; current-source validation and Release profiling are
recorded below.

PID 16328 was the visual-review process. It used the unchanged standard
GLB and HDRI with GTAO, SSR, shadows, SMAA, bloom, God Rays, and TAA disabled.
Its stderr log, `.codex-target/render-review-20261111-current.err.log`, confirms
HDRI import in 16.359 ms, GLB import in 101.736 ms, 26 images
whose extents imply 521,142,272 bytes of RGBA8 output, environment readiness,
and 4,205.453 ms for asset upload. The old `decoded_rgba8_bytes` label was a
pre-decode size estimate, not a measured decoded payload; current source logs
it as `expected_rgba8_bytes`, while profiling records actual decode bytes and
time per image. This interactive run is not a clean performance or memory
measurement. The user confirmed it closed; process inspection found no
remaining `clear.exe`. Human image review remains pending.

At 2026-11-11 08:52:52 +09:00, Windows reported this sole X173GE process at
250,707,968 bytes current working set and 607,313,920 bytes peak working set
(579.2 MiB). The 3,559,936-byte executable is the High Quality build, but
shadows and every optional post effect are disabled in this interactive run.
This is an OS process-memory observation, not Vulkan allocation or a full
high-quality workload peak. NVIDIA's Windows process query returned `N/A` for
per-process GPU memory; the clean profiling build's engine-owned allocation
high-water mark remains the authoritative GPU metric.

## Asset-upload timing diagnosis (2026-11-11)

The 4,205.453 ms `X173GE_ASSET upload_ms` is one end-to-end interval beginning when
the renderer becomes ready and ending after all queued resources are uploaded.
It includes app-thread image decode, renderer queue pressure, GPU resource
creation/transfers, material setup, and HDRI prefiltering. It does not include
the separately timed GLB import. It must not be described as GPU transfer time.

The standard GLB has 180 primitives and 26 images. Each current mesh upload
creates vertex and index buffers separately; each buffer upload records one
copy, submits it, and waits for its fence. That means at least 360 serialized
mesh-buffer submissions and completion waits, plus the image/material and
environment work. This is a structural cost candidate, not yet a measured
dominant bottleneck. Current-source Release profiles report per-image decode,
image GPU work, mesh-buffer upload, material waits, and environment upload,
but these sums do not account for the full end-to-end upload interval. A
same-settings `--no-environment` capture remains needed to isolate IBL cost.
Do not infer a dominant GPU stage from the end-to-end total.

## Current-source release and package sizes (2026-11-12)

The standard and high-quality executables were rebuilt from the 109-file
fingerprint `6E04D83CFB7E4699BBC8BA09A57B0EC7556B9718D1451406C3DCAA420AF8B924`
using `rustc 1.99.1`, `x86_64-pc-windows-msvc`, and the workspace fat-LTO
Release profile. `Cargo.lock` SHA-256 is
`7EC5B176C295CBF3DB2F525DA85847C4FE7E9BAE47E10AAA5140F6B31B912631`.

| Selection | Executable bytes | SHA-256 |
|---|---:|---|
| Standard (`vulkan` + `smaa`) | 3,187,712 | `FEEC89F8C383E6E2CECC87D95DDD7BF21C298F36520FF8B1E3BE327F8E75106D` |
| High quality | 3,564,032 | `EE4DDDA0420006F2DAB5A17CBF3B1DB3DDE375C0A0930F9801717216EBA54B22` |
| Standard with `profiling` | 3,220,992 | `C3B5C287C9E4F31B8F8D8620E3E1DF61313B5EBB422341B2FA9FD38590E6829D` |
| High quality with `profiling` | 3,597,824 | `23E95EC20FE915584C758A2A27FD3C2D97046233DF45CA2BDC8ECB762BDEAB9B` |

High quality adds 376,320 bytes (11.81%) over standard. Local review ZIPs
contain these executables, the unchanged 75,654,468-byte standard model
(SHA-256 `E9852865A6D6B7779C8A6CB390A0F9414B0AB247AE45F62E3FEAC190C65A6694`),
the 1,762,844-byte CC0 HDRI, and their source/readme files:

| Selection | Fixture ZIP bytes | SHA-256 |
|---|---:|---|
| Standard | 67,128,603 | `8CD56B0C6A067756A9DEA3D94BDBD9D98123163C01F0D2E74F4372620A086E17` |
| High quality | 67,245,378 | `3E42D3AD6A0EDE2536660CA5FDDFD5C22633DE9EB6947B3BE24CBA777E4A1E5A` |

The archives are
`.codex-target/package-current-20261112-standard.zip` and
`.codex-target/package-current-20261112-high-quality.zip`. All five entries in
each archive were checked; executable, model, and HDRI hashes match their
current source files. These are local review fixtures only; the model's
redistribution rights are unverified.

## Current-source Vulkan validation smoke (2026-11-12)

The current source, including the upload operation-context diagnostics, passed
an all-effects Debug smoke with the Khronos validation layer enabled. It
resized through surface generation 4, presented twelve frames after resize,
reported no validation warnings or errors, and shut down with `live_bytes=0`.
The log is
`.codex-target/vulkan-high-quality-all-effects-current-diagnostic-20261112.log`.
It reported `upload_ms=12949.450` and an engine-owned GPU-memory peak of
1,077,256,480 bytes. Debug and validation overhead make those values unsuitable
as Release performance baselines. The earlier default Debug smoke predates the
diagnostic-only source change.

## Current-source Release profile captures (2026-11-12)

Both runs use the unchanged standard GLB, NVIDIA GeForce RTX 4070 SUPER
(driver 610.88), 1280×720, `--r1-benchmark-camera`, 120 warm-up frames, and
480 selected presented frames. The model SHA-256 is recorded above. These are
X173GE workload measurements, not a controlled rebuild1 speed comparison:

| Build/workload | Measured frame IDs | Presented FPS | GPU total p50 / p95 (ms) | Asset upload (ms) | Owned GPU peak (bytes) |
|---|---:|---:|---:|---:|---:|
| Standard; shadows, SMAA, HDRI | 299–778 | 58.68 | 2.5774 / 5.0780 | 3,684.520 | 837,456,768 |
| High quality; CSSM, GTAO, SSR, bloom, God Rays, TAA, SMAA, HDRI | 295–774 | 58.79 | 3.5978 / 4.6138 | 3,599.479 | 1,049,121,152 |
| Standard; shadows, SMAA; no HDRI | 296–775 | 58.72 | 2.7353 / 4.3508 | 3,606.319 | 833,229,696 |

Each run selected exactly 480 GPU rows. Standard p50/p95 shadow, scene, and
post times were 1.4920/3.0448 ms, 0.5315/1.6671 ms, and 0.4854/1.7910 ms.
High-quality p50/p95 times were shadow 1.1945/2.0222 ms, GTAO 0.4669/0.5192
ms, scene 0.4250/0.4731 ms, SSR 0.1577/0.1761 ms, TAA 0.1516/0.1679 ms,
God Rays march 0.0983/0.1085 ms, horizontal 0.0963/0.1065 ms, vertical
0.0952/0.1065 ms, composite 0.0963/0.1065 ms, bloom 0.3666/0.4035 ms, and
post 0.2970/0.3359 ms. Both runs reported `live_bytes=0` at shutdown and zero
presented-frame drops. Queue-full attempts were 25 standard and 47 high
quality; these are run counters, not selected GPU rows.

Current-source standard upload rows sum to 686.477 ms image decode (26 images),
113.861 ms image GPU work (29 images including built-ins), 272.306 ms mesh
buffers (360 uploads), 16.889 ms material work (29), and 10.733 ms environment
work. These stage samples do not account for the full end-to-end upload
interval. The no-HDRI run removes environment upload but differs by only
78.201 ms in total upload time. Its one-run GPU comparison also does not
isolate an IBL frame cost: the HDRI run measured 2.5774/5.0780 ms p50/p95,
while the no-HDRI run measured 2.7353/4.3508 ms. Run-to-run variation is too
large to attribute the residual to IBL or queueing.

Logs:

- `.codex-target/profile-current-standard-retry-20261112.log`
- `.codex-target/profile-current-high-quality-retry-20261112.log`

### Device-loss observation and instrumentation

The first 480-frame high-quality Release attempt,
`.codex-target/profile-current-high-quality-20261112.log`, exited with
`ERROR_DEVICE_LOST` while the renderer was handling a texture upload. It had
presented one frame, filled the bounded frame queue, and produced no selected
profile samples. Windows recorded `nvlddmkm` System event 153 at 00:19 and
LiveKernelEvent 141 at 00:19:12; an earlier 153/141 pair also exists on
2026-11-11. This timing is correlated, not proof that X173GE caused the driver
reset.

The previous upload error path omitted image ID and the failing Vulkan call.
The current code adds these failure contexts without changing upload order or
render behavior. A one-frame all-effects Release run then uploaded the complete
model and presented 161 frames during loading before its selected sample. The
480-frame high-quality retry and current-source standard profile also passed,
with no new driver event recorded during those retries. The trigger of the
first device loss remains undetermined. Current-source validation and human
review remain separate acceptance gates.

### Same-model rebuild1 profile attempt (2026-11-12; exploratory)

The rebuild1 Release smoke used the unchanged 75,654,468-byte standard GLB at
1280×720 on the same RTX 4070 SUPER. `--window-smoke` counts 600 presented
frames after model load. Its model-bounds camera and X173GE's
`--r1-benchmark-camera` use the same framing; both logs report scene radius
`310.426`, eye `(0, 74.502, 760.545)`, and a 60° field of view.

The rebuild1 nominal 120-frame warm-up plus 480-row sample (IDs 145–624) is
not steady: median command-buffer GPU time rises from 0.370 ms at IDs 145–244,
to 0.691 ms at 245–344, then 1.077 ms at 345–444. Multiple passes rise
together. The complete 480-row sample gives p50/p95 `1.0751/1.7327` ms, but
includes that ramp and framebuffer readback on 40 frames. Its final 280 rows,
IDs 345–624, give `1.0773/2.4229` ms after 320 loaded-asset frames. The
matching-length X173GE no-HDRI tail (IDs 496–775) gives `2.7402/5.4134` ms.
These tails are still exploratory, not a controlled speed comparison.

The source confirms the renderer adds every loaded model primitive to each
snapshot and resets the camera to the model bounds before input; no camera or
scene animation explains the ramp. Rebuild1 does animate a day/night light,
which can affect shadows, but does not explain the simultaneous rise in all
passes. The Vulkan timestamp code waits on the frame-slot fence, reports the
GPU command-buffer interval, and excludes WSI present wait. No missing timing
rows were found. GPU clocks and power state were not logged, so the ramp's
cause remains undetermined.

Quality settings also differ: rebuild1's `performance` log disables SSAO,
SSR, anti-aliasing, bloom, God Rays, and volumetric fog while still emitting
SMAA-labelled timing rows; X173GE's standard no-HDRI profile has SMAA enabled.
Rebuild1 also reads back the framebuffer every 12 frames. Do not use the
measured ratio as a performance target. A controlled baseline still needs a
longer capture with at least 320 loaded-frame warm-up plus 480 samples on both
engines, matched active passes, and GPU clock/power observations.

Logs: `.codex-target/bench-model-r1-20261112.log` and
`.codex-target/profile-current-standard-no-environment-20261112.log`.

## Windows release binary audit after TAA removal (2026-11-13)

Built the `clear` example from the TAA-free source into the same isolated target
directory. Both builds use the workspace release profile and exclude the
`profiling` feature. The GLB and HDRI remain external files and are not included
in executable sizes.

| Build selection | Command | Executable bytes | SHA-256 |
|---|---|---:|---|
| Standard | `cargo build --release --locked --example clear --target-dir .codex-target/taa-removed-size-audit` | 3,183,616 | `7D74711F76A46138E66BD428F10E012037B562FD0C309A57E7A4189A86BCC123` |
| High quality | `cargo build --release --locked --example clear --features high-quality --target-dir .codex-target/taa-removed-size-audit` | 3,387,392 | `83AC1CB1F32C3A8267A141A9FCE1AFF546533751803B61246C826184423C77A7` |

High quality is 203,776 bytes (6.4%) larger than standard in this build. This is
not a before/after TAA delta: the earlier binary audit used a different source
snapshot. No distribution archive was rebuilt.

## Isolated target cleanup (2026-11-15)

Forty-five Cargo target directories containing only build outputs were cleaned
from `.codex-target`, freeing about 27.5 GiB. Review logs, package archives,
fixture copies, source check crates, and validated SPIR-V files remain. Older
commands above still describe the measured builds; their isolated executables
can be regenerated with the recorded commands. Directories retained for review
are listed in [`cleanup-candidates.md`](../cleanup-candidates.md).
