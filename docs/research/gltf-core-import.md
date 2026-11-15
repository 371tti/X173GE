# glTF 2.0 core import

## Source and review target

The [Khronos glTF 2.0 specification](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html)
is the format authority. This note records X173GE's static scene import subset;
it does not claim full support for every optional core feature or extension.

The required visual checkpoint is the exact `../rebuild1/assets/model.glb`
that rebuild1 opens by default. Rebuild1 supplies the test data only; its
loader and rendering behavior are not implementation references. In
particular, rebuild1 silently skips non-triangle primitive modes; X173GE rejects
an unsupported topology explicitly so a partially imported scene is not
reported as complete. The file is 75,654,468 bytes (SHA-256
`E9852865A6D6B7779C8A6CB390A0F9414B0AB247AE45F62E3FEAC190C65A6694`) and
contains a 75,073,492-byte BIN buffer. X173GE's local
safety bounds (not glTF format limits) are 96 MiB per GLB and buffer, 64 MiB
per JSON document/chunk, encoded image, decoded image, and mesh upload, and 96
MiB per primitive's morph data. It also caps a document at 65,536 nodes and a
hierarchy at 256 levels. These are per-item bounds, with no aggregate
asset-size limit. Image headers and dimensions are inspected during
import; compressed image bytes remain owned by `GltfImage` and are decoded to
RGBA8 only as each upload enters the bounded renderer queue. The model stays in
the sibling rebuild1 checkout and is loaded from disk; it is not copied into
the executable. The 64 MiB JSON cap is a local memory bound, not a Khronos
format limit, so a GLB with a larger JSON chunk is rejected even if its total
size remains below 96 MiB.

## GLB container checks

For the published Khronos glTF 2.0.1 registry, the importer checks the 12-byte header, declared file length,
container version, JSON `asset.version`/`minVersion`, four-byte chunk alignment,
and the required JSON-first ordering. A
referenced implicit buffer must be buffer 0 and must have a second BIN chunk;
its payload may contain only the up-to-three zero padding bytes allowed by
the spec. Duplicate or misplaced JSON/BIN chunks are rejected, while unknown
chunks are skipped without allocating their payload. An unreferenced BIN is
accepted and ignored; omitting it is a spec recommendation, not a MUST. After
resolving buffers, all buffer views and accessors are checked before any glTF
reader sees their data. See [Khronos GLB structure and chunk rules](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#glb-file-format-specification).

One chunk-order edge is not explicit in the 2.0.1 wording: BIN is optional,
while unknown chunks must be ignored "following the first two chunks." X173GE
accepts an unknown second chunk when JSON declares no implicit buffer, and
rejects any BIN that follows it. This preserves the forward-compatible
unknown-chunk behavior without allowing a misplaced BIN; the case is covered
by `glb_ignores_unknown_chunks_without_loading_them`.

The JSON chunk stores only its padded `chunkLength`, not the original JSON byte
length, so a reader cannot identify which trailing JSON whitespace belongs to
the document and which bytes were added for alignment. X173GE checks alignment
and parses the complete chunk as JSON rather than guessing that boundary. The
selected `gltf-json` parser also rejects schema integers written as integral decimals or exponents
(such as `byteLength: 4.0`) even though the spec permits them. X173GE retries
only after a parse error, rewriting nonnegative integer-valued tokens through
`u64::MAX` with exact decimal digit arithmetic; signed zero is rewritten as
zero. No binary64 safe-integer cutoff is applied. Strings, fractions,
nonzero negative numbers, and values above `u64::MAX` remain unchanged.

The pinned `gltf-json` validator also incorrectly requires `bufferView` when
an accessor has neither a view nor sparse data; glTF 2.0 defines those
accessors as zero-initialized. X173GE keeps the library's reference checks and
filters only that specific false-positive error, then materializes zero values
within the existing mesh and morph upload budgets. Matrix column starts,
including sparse values, are checked for the spec's four-byte alignment. Every
declared accessor bound is compared component-wise against the stored values
before integer normalization and after sparse substitution. Under §5.1.7–8,
integer `min` and `max` tokens are checked for exact integrality and
representability in their `componentType` before parsing can round them through
`f64` or `f32`; this also applies when `bufferView` and `sparse` are absent.
Floating-point bounds are rounded to `f32` as §3.6.2.5 recommends. When both
`bufferView` and `sparse` are absent, §3.6.2.5 allows any bounds relative to the
synthesized zero data, so they are type-checked but not compared against those
zero values. Animation sampler input accessors must also define both bounds.
The animation subset validates
scalar-float time accessors and nonnegative increasing timestamps for all
samplers, plus target-specific output types and keyframe counts for retained
node-targeted channels. A node with a matrix transform is rejected when any
animation channel targets it, including a morph-weight channel, as required by
§3.5.3. Node-less
channels removed by the compatibility
fallback do not get target-specific output checks because this importer ignores
them and does not play animations. Every float accessor is scanned for NaN and
infinity even when bounds are absent. The
pinned `gltf-json` parser requires `animation.channel.target.node` and only
accepts enumerated core paths; on that parse-error fallback, X173GE checks the
required channel envelope and sampler reference, then removes each channel
whose node is undefined, as glTF recommends. Because the initial importer does
not play animations, it does not interpret the discarded target path or any
extension payload attached to it. Following the 2.0.1 schemas, known glTF core
fields reject `null`, while unknown additional properties are ignored.
`extras` values and extension payload contents remain uninterpreted and may
contain `null`; extension sets and each extension payload must still be JSON
objects as required by §5.15. Morph-target entries remain plain
semantic-to-accessor maps with no `glTFProperty` extension fields. Required
unsupported extensions still fail import.

## Static scene semantics

- Traverse the default scene (or the first scene when no default is selected),
  preserving node transforms and mesh instances.
- Validate the complete node forest before scene expansion: every node has at
  most one parent, the hierarchy has no cycles, and every `scene.nodes` entry
  is a unique root, and an explicitly supplied `scene.nodes` list is nonempty.
  Each skin's joints must share a hierarchy root, `skin.skeleton` must be the
  closest common root or one of its ancestors, and every scene containing a
  skinned node must also contain that skin's joints (§3.5.1, §3.5.2,
  §3.7.3.2, §5.24.6, §5.28.2).
- Apply node morph weights when present, otherwise mesh weights, otherwise
  zero. All primitives in a mesh must have the same target count and order
  (§3.7.2.2); the importer rejects a mismatch, including a mixture of zero and
  nonzero target counts. Each target must contain at least one supported
  attribute, as required by glTF 2.0.1 schema A.27. Apply POSITION, NORMAL,
  and TANGENT deltas before skinning. Core `TEXCOORD_n` and `COLOR_n` morph
  deltas are optional client features; reject them explicitly because the
  pinned parser discards those fields. Reject all other unsupported target
  semantics rather than silently dropping them. The review harness does not
  play animation; glTF leaves runtime playback behavior to the client.
- Preserve canonical, consecutive indexed base semantics beginning at zero.
  For normalized integer `WEIGHTS_0`, check each raw U8/U16 sum equals 255 or
  65535 before converting weights to floats (§3.7.3.3).
- If NORMAL is omitted, generate flat triangle normals as required by §3.7.2.1.
  In this case ignore provided TANGENT data and tangent morph deltas. When a
  normal texture is used without usable tangents, generate MikkTSpace tangents
  from the final static pose, positions, normals, and UV0 after morphing and
  skinning. This computes tangents once for the selected static pose; glTF
  recommends calculating per morph target when the base primitive has no
  tangents (§3.7.2.2). Provided tangent deltas are applied before skinning
  when the base primitive has normals and tangents.
- Skin with linear blend skinning: each joint matrix is its scene-global
  transform multiplied by its inverse bind matrix. A skinned mesh node's own
  transform is ignored, as required by §3.7.3.2. The selected initial subset
  supports one JOINTS/WEIGHTS set.
- Consume UV0 for material textures. Extra UV sets may be ignored only when no
  supported material slot references them. A texture selecting a missing or
  unsupported set is an explicit import error.
- Preserve core metallic-roughness factors, texture transfer functions,
  alpha mode/cutoff, double-sided state, normal scale, occlusion strength, and
  emissive factors/textures. Clamp COLOR_0 components to [0, 1]. The importer
  validates but ignores application-specific underscore attributes, as they do
  not affect the renderer's supported vertex inputs. It supports
  `KHR_materials_unlit` and `KHR_materials_emissive_strength`, and uses core
  fallbacks for unsupported optional extensions. Optional
  `KHR_materials_transmission` values and its linear texture are preserved, but
  visible transmission shading is not yet implemented. Such materials use the
  core fallback, and files that require the extension are rejected. Enforce the
  Khronos exclusions per material: emissive strength cannot combine with
  unlit; specular cannot combine with unlit or
  `KHR_materials_pbrSpecularGlossiness`; transmission cannot combine with
  unlit or `KHR_materials_pbrSpecularGlossiness`. The Khronos
  [`KHR_materials_emissive_strength`](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_materials_emissive_strength/README.md),
  [`KHR_materials_specular`](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_materials_specular/README.md),
  and
  [`KHR_materials_transmission`](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_materials_transmission/README.md)
  specifications define these exclusions. Although optional
  `KHR_materials_specular` uses the core fallback, its exclusions are still
  checked. Alpha mode
  remains an independent coverage setting. `transmissionFactor` is range
  checked from its borrowed decimal token before narrowing to `f32`. A streaming
  pass verifies that every extension payload is an object declared once in
  `extensionsUsed`, declaration arrays are non-empty when present and contain no
  duplicates, and `extensionsRequired` is a subset, without building a second
  JSON tree. Scene root lists are checked for non-empty unique indices, exact
  integer values (including decimal/exponent spellings that equal integers),
  and in-range nodes before hierarchy traversal. Integer spellings are
  converted with decimal digit arithmetic rather than binary floating point,
  through the `u64` representation limit; non-integral tokens remain unchanged
  for the typed glTF parser to reject where required.
- The pinned `gltf-json` 1.4.1 validator checks `Vec` elements but not JSON
  Schema `minItems`, so an omitted optional array must remain distinct from an
  explicit empty array. The streaming metadata pass rejects empty root arrays
  and `node.children`; importer checks cover `scene.nodes`, animation channel
  and sampler arrays, mesh primitives and weights, node weights, skin joints,
  morph targets, accessor bounds, and `KHR_lights_punctual.lights`. This follows
  the [Khronos core schema](https://github.com/KhronosGroup/glTF/blob/main/specification/2.0/schema/glTF.schema.json)
  and the extension's [lights schema](https://raw.githubusercontent.com/KhronosGroup/glTF/main/extensions/2.0/Khronos/KHR_lights_punctual/schema/glTF.KHR_lights_punctual.schema.json).
  Regression coverage exercises every root `minItems` array and the previously
  missed empty light list.
- Before any glTF accessor reader runs, check buffer-view ranges, accessor
  alignment and element bounds, sparse index/value ranges, and the required
  strictly increasing in-range sparse indices. Buffer and buffer-view lengths
  must be nonzero; `accessor.byteOffset` requires a `bufferView`; `UNSIGNED_INT`
  accessors are only valid when referenced by primitive indices. Enforce the
  single-data-kind rule for views used by images, vertex indices, vertex
  attributes and inverse bind matrices; sparse payloads also obey their own
  target, stride and alignment rules. Distinct vertex attribute accessors
  sharing a view require `byteStride`; other accessors may share a view without
  stride. Unused views remain ignorable.
  Matrix column padding is included while allowing the spec-permitted omitted
  padding after the final element. Validate node matrices as finite affine
  transforms whose basis columns are orthogonal, as required for TRS
  decomposition.
- Each image must define exactly one of `uri` or `bufferView`; a `bufferView`
  image must also define `mimeType`, and supported MIME values are PNG and
  JPEG. Check these invariants before calling `gltf::image::Image::source()`,
  whose API assumes a valid source and can panic on malformed JSON. Image
  buffer views cannot specify `byteStride`; `target` remains an optional hint
  and is accepted. Data URI schemes are matched case-insensitively. Image data
  URIs support base64 or RFC 2397 percent-encoded bytes, including MIME
  parameters; the declared `mimeType`, when present, must agree with the URI
  media type. Buffer data URIs require base64 and the core octet-stream or
  glTF-buffer media type. External resource paths validate every percent
  escape, split path segments before decoding, and reject encoded separators
  that the host filesystem would reinterpret (`/` on every platform and `\\`
  on Windows). Raw reserved characters must be percent-encoded as required for
  glTF relative paths. Relative paths with schemes, authorities, queries, or
  fragments are rejected; local `file:` URIs are supported, while remote
  authorities and other schemes are explicit errors.
- The renderer subset accepts triangle-list primitives and UV0 material
  textures. It returns an explicit error for other primitive topologies or a
  material texture that selects a non-zero UV set. Unsupported optional
  texture transforms use the base texture-info values; unsupported required
  extensions are rejected by the glTF parser.

These choices cover the default model's static scene: 180 triangle primitives,
default morph weights, 38 skinned nodes, base-color/metallic-roughness/normal
textures, and opaque/masked/blended double-sided materials. Its one animation
is deliberately not played during the first repeatable image-quality review.
The model's 26 PNG images comprise four 4096², thirteen 2048², eight 1024²,
and one 512² sources: 497 MiB after RGBA8 decode. A full RGBA8 mip chain for
every image is about 662.67 MiB before Vulkan allocation alignment; this is an
asset-derived estimate, not measured physical GPU residency. Image pixels are
now decoded one at a time as uploads enter the bounded renderer queue.
Khronos glTF Validator 2.0.0-dev.3.10 reports zero errors, 152 warnings and
188 informational messages for this file. Warnings are 114 generated-tangent
space notices and 38 skinned-mesh parent-transform notices; these are retained
as observations about the required test fixture, not reasons to alter it. The
full-scene fit includes a `Cube` node scaled about 599 units along Y, so it
makes the rest of the scene look very small in the current capture; the GLB
data itself has not been changed.

Raw JSON confirms node 0 is a root of the selected default scene and references
mesh 0. Its unit cube POSITION bounds are `[-1, -1, -1]` to `[1, 1, 1]`, while
its node scale is `[2.3083718, 299.26697, 5.0946512]`, giving a valid world Y
extent of about 598.53 units. X173GE preserves that mesh instance and transform;
whole-scene framing is therefore small by design, not a loader defect. A
human-focused view must be an explicit review-camera choice and must keep the
imported scene intact. This follows glTF's node hierarchy and TRS semantics
([nodes and hierarchy](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#nodes-and-hierarchy),
[transformations](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#transformations)).

## Punctual lights

The ratified [`KHR_lights_punctual` extension](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_lights_punctual/README.md) is imported from nodes in the selected scene, including nodes without meshes and repeated references to one light definition. Directional lights use the node's local `-Z` emission axis and remain on the existing shadow path; point and spot lights are forwarded to the renderer with linear RGB color, candela intensity, optional range, and spot cone angles. Numeric constraints are checked explicitly because the pinned typed validator does not enforce them. The renderer accepts up to 16 point/spot lights per frame. The chosen attenuation variant and limits are recorded in [`punctual-lights.md`](punctual-lights.md).

The light schema allows a `spot` object on any light definition, while its
parameters affect only lights whose `type` is `spot`; X173GE validates the
object and ignores it for point/directional lights. A positive `range` on a
directional light is likewise validated but ignored, because range applies to
point/spot attenuation only.

## Deliberate limits

The initial visual checkpoint imports static default poses only; it does not
provide an animation playback API. The renderer also handles triangle-list
primitives first. Other supported glTF core behavior must either be added with
spec-based validation or rejected with a specific import error. Valid unknown
optional extensions may be ignored only when the core representation retains
the asset's required behavior; unknown required extensions must never be
silently accepted. Singular unskinned node transforms are explicitly rejected
because the current renderer requires invertible model matrices for normals;
they are never silently dropped. Extension-enabled non-vertex `byteStride`
layouts are outside the current subset and are reported as unsupported.

## Review checks

Use the exact default model in the visual checkpoint. Check the result before
and after enabling GTAO, and record the camera, GPU, resolution, settings, and
visible defects. Automated importer and Vulkan validation do not establish
visual correctness.
