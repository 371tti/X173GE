# glTF material textures

## Source and scope

[glTF 2.0 specification](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html) defines the material texture encodings, UV selection, and sampler behavior. This note records the format contract and X173GE's implemented first subset; it is a specification note, not a research paper.

## Adopted renderer path

- Support the core base-color, metallic-roughness, normal, occlusion, and emissive slots using `TEXCOORD_0`. `textureInfo.texCoord` defaults to 0; a material selecting another set or using an unsupported texture-info extension is rejected. A referenced slot requires UV0 on its primitive.
- Decode base-color RGB as sRGB and use linear values for shading; base-color alpha is linear coverage. Read metallic-roughness from G (roughness) and B (metalness) as linear values; ignore R and A. Multiply sampled values by their corresponding material factors. When a slot is absent, use 1 for its sampled components. `COLOR_0` remains an additional linear multiplier for base color.
- Normal, occlusion, and metallic-roughness image views use linear UNORM; base-color and emissive use sRGB views. If the same image serves both roles, the importer keeps separate role-tagged image records that share the encoded source bytes; uploads retain the appropriate color-space behavior for mip generation and Vulkan views. Normal maps use `normalTexture.scale`; occlusion maps affect indirect diffuse only; emissive maps multiply emissive factor and strength.
- Image headers and dimensions are validated during import, while encoded image bytes stay in `GltfImage`. `GltfImage::upload` decodes one source to bounded RGBA8 only as its upload reaches the bounded renderer queue; this avoids retaining the full decoded pixel set for a large scene. The choice changes when decoding occurs, not the accepted PNG/JPEG subset or the per-image 64 MiB source/decode limits.
- Keep texture bindings and sampler state independent. All six glTF minification choices are mapped to Vulkan's texel filter and mip filter, and supported devices get a complete linear-filtered mip chain. Repeat, clamp-to-edge, and mirrored-repeat wrapping are retained. A missing sampler uses glTF's repeat default and X173GE's explicit linear/linear filtering choice. Devices without filtered RGBA8 blits can still render non-mipmapped textures; a material requesting unavailable mip levels fails at upload with an explicit error.
- MASK materials discard fragments below `alphaCutoff`; BLEND materials are drawn back-to-front with color alpha blending and no depth writes. They do not contribute to opaque depth, shadow, or GTAO passes. Double-sided materials disable face culling and reverse back-face normals.
- Reject `KHR_texture_transform` rather than silently ignoring it. It changes texture coordinates and is outside this UV0-only subset.
- Limit GLB files and individual buffers to 96 MiB so the rebuild1 default model remains a usable review fixture. Limit each glTF JSON document/chunk, encoded or decoded image, external image source, and mesh upload individually to 64 MiB. External buffer files also have a 96 MiB per-file limit. GLB BIN allows up to three zero padding bytes beyond its buffer length; JSON and BIN are read separately, and unknown chunks are skipped without allocation. Data-URI size, external file size, image dimensions, and mesh attribute counts are checked before decode/read or mesh construction. There is no aggregate CPU or GPU asset budget.
- The GLB reader follows Khronos chunk order: JSON first, optional BIN second, extension chunks afterward; the BIN-backed buffer must be the first buffer. [glTF 2.0 specification](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html)
- A data-URI glTF is also subject to the 64 MiB JSON-document limit, so base64 expansion can make its effective embedded resource limit lower than the independent buffer/image limits. External URIs and GLB chunks avoid that document-text constraint.

## Not implemented in this subset

The importer and Vulkan backend currently do not support UV sets beyond 0, `KHR_texture_transform`, 16-bit image formats, or `KHR_materials_specular` texture slots. They reject these cases explicitly. A direct `MeshData` normal-mapped draw must provide a complete tangent frame; the importer generates MikkTSpace tangents for the GLB path. There is no aggregate CPU/GPU asset cap or resource unload API yet, so individually valid uploads can accumulate in memory.

## Validation target

Use the exact rebuild1 default `../rebuild1/assets/model.glb` for the human checkpoint. Also use a small focused scene with known sRGB swatches, distinct G/B metal-roughness regions, a normal map, an occlusion map, and an emissive map. Verify UV orientation, repeat/clamp behavior, mip filtering, and two texture bindings sharing one source image with different samplers.

## Primary source

- Khronos, [glTF 2.0 §3.8 Texture Data and §3.9 Materials](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#texture-data), especially the [metallic-roughness model](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#metallic-roughness-material), [sampler behavior](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#samplers), and [Texture Info / `texCoord`](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#texture-info).
