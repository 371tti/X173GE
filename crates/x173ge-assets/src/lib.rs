//! CPU-side glTF/GLB import and image decoding.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
};

use gltf::{
    accessor::{DataType, Dimensions},
    material::AlphaMode as GltfAlphaMode,
    mesh::Semantic,
};
use image::{
    ColorType, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits,
    codecs::hdr::{HdrDecoder, HdrMetadata},
};
use mikktspace::Geometry;
use serde::de::{DeserializeSeed, Error as DeError, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use x173ge_core::{EnvironmentId, Extent2d, ImageId, Mat4, Vec3};
use x173ge_protocol::{
    AlphaMode, DirectionalLight, EnvironmentUpload, ImageColorSpace, ImageUpload,
    MAX_DIRECTIONAL_LIGHTS, MAX_IMAGE_UPLOAD_BYTES, MAX_MESH_UPLOAD_BYTES, MAX_PUNCTUAL_LIGHTS,
    MaterialData, MeshData, MipmapMode, PunctualLight, TextureBinding, TextureFilter,
    TextureSampler, TextureWrap, Vertex,
};

const MAX_SCENE_NODES: usize = 65_536;
const MAX_SCENE_DEPTH: usize = 256;
const MAX_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_GLB_BYTES: u64 = 96 * 1024 * 1024;
const MAX_BUFFER_BYTES: usize = 96 * 1024 * 1024;
const MAX_PRIMITIVE_MORPH_BYTES: usize = 96 * 1024 * 1024;
const MAX_IMAGE_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENVIRONMENT_SOURCE_BYTES: u64 = 96 * 1024 * 1024;

/// Parsed glTF document, loaded buffers, and encoded source images.
pub struct GltfAsset {
    pub document: gltf::Document,
    pub buffers: Vec<Vec<u8>>,
    pub images: Vec<GltfImage>,
}

/// Encoded source image; callers decode it when they are ready to upload.
#[derive(Clone, Debug, PartialEq)]
pub struct GltfImage {
    pub image_index: usize,
    pub color_space: ImageColorSpace,
    pub extent: Extent2d,
    format: ImageFormat,
    encoded: std::sync::Arc<[u8]>,
}

impl GltfImage {
    /// Decodes this image to bounded RGBA8 pixels for one renderer upload.
    pub fn upload(&self, id: ImageId) -> Result<ImageUpload, ImportError> {
        Ok(ImageUpload {
            id,
            extent: self.extent,
            color_space: self.color_space,
            rgba8: decode_rgba8(&self.encoded, self.format, self.extent)?.into(),
        })
    }
}

/// Equirectangular environment's diffuse irradiance and source extent.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvironmentMap {
    pub extent: Extent2d,
    diffuse_irradiance_sh: [[f32; 3]; 9],
}

impl EnvironmentMap {
    /// Creates a small owned diffuse-light payload with a caller-assigned ID.
    pub fn upload(&self, id: EnvironmentId) -> EnvironmentUpload {
        EnvironmentUpload {
            id,
            diffuse_irradiance_sh: self.diffuse_irradiance_sh,
        }
    }
}

/// A glTF texture object reference whose image ID is assigned by the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GltfTexture {
    pub image_index: usize,
    pub color_space: ImageColorSpace,
    pub sampler: TextureSampler,
}

/// Supported glTF material factors and texture references.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GltfMaterial {
    pub base_color_factor: [f32; 4],
    pub metallic_factor: f32,
    pub roughness_factor: f32,
    pub emissive_factor: [f32; 3],
    pub emissive_strength: f32,
    pub alpha_mode: AlphaMode,
    pub alpha_cutoff: f32,
    pub transmission_factor: f32,
    pub double_sided: bool,
    pub unlit: bool,
    pub normal_scale: f32,
    pub occlusion_strength: f32,
    pub specular_factor: f32,
    pub specular_color_factor: [f32; 3],
    pub base_color_texture: Option<GltfTexture>,
    pub metallic_roughness_texture: Option<GltfTexture>,
    pub normal_texture: Option<GltfTexture>,
    pub occlusion_texture: Option<GltfTexture>,
    pub emissive_texture: Option<GltfTexture>,
    pub transmission_texture: Option<GltfTexture>,
}

impl GltfMaterial {
    /// Binds this asset-local material to caller-assigned engine image IDs.
    pub fn renderer_data(
        self,
        image_ids: &HashMap<(usize, ImageColorSpace), ImageId>,
    ) -> Result<MaterialData, ImportError> {
        let bind = |texture: Option<GltfTexture>| {
            texture
                .map(|texture| {
                    Ok::<_, ImportError>(TextureBinding {
                        image: *image_ids
                            .get(&(texture.image_index, texture.color_space))
                            .ok_or(ImportError::Image("texture image index is out of range"))?,
                        sampler: texture.sampler,
                    })
                })
                .transpose()
        };
        let mut data = self.factors();
        data.base_color_texture = bind(self.base_color_texture)?;
        data.metallic_roughness_texture = bind(self.metallic_roughness_texture)?;
        data.normal_texture = bind(self.normal_texture)?;
        data.occlusion_texture = bind(self.occlusion_texture)?;
        data.emissive_texture = bind(self.emissive_texture)?;
        data.transmission_texture = bind(self.transmission_texture)?;
        Ok(data)
    }

    fn factors(self) -> MaterialData {
        MaterialData {
            base_color_factor: self.base_color_factor,
            metallic_factor: self.metallic_factor,
            roughness_factor: self.roughness_factor,
            emissive_factor: self.emissive_factor,
            emissive_strength: self.emissive_strength,
            alpha_mode: self.alpha_mode,
            alpha_cutoff: self.alpha_cutoff,
            transmission_factor: self.transmission_factor,
            double_sided: self.double_sided,
            unlit: self.unlit,
            normal_scale: self.normal_scale,
            occlusion_strength: self.occlusion_strength,
            specular_factor: self.specular_factor,
            specular_color_factor: self.specular_color_factor,
            base_color_texture: None,
            metallic_roughness_texture: None,
            normal_texture: None,
            occlusion_texture: None,
            emissive_texture: None,
            transmission_texture: None,
        }
    }
}

/// One imported triangle primitive and its renderer-independent material factors.
#[derive(Clone, Debug, PartialEq)]
pub struct GltfPrimitive {
    /// Mesh and primitive indices in the retained glTF document.
    pub mesh_index: usize,
    pub primitive_index: usize,
    pub material_index: Option<usize>,
    pub mesh: MeshData,
    pub material: GltfMaterial,
    flat_normals: bool,
    has_tangents: bool,
    morph_targets: Vec<GltfMorphTarget>,
    skin_weights: Option<GltfSkinWeights>,
}

#[derive(Clone, Debug, PartialEq)]
struct GltfMorphTarget {
    positions: Option<Vec<[f32; 3]>>,
    normals: Option<Vec<[f32; 3]>>,
    tangents: Option<Vec<[f32; 3]>>,
}

#[derive(Clone, Debug, PartialEq)]
struct GltfSkinWeights {
    joints: Vec<[u16; 4]>,
    weights: Vec<[f32; 4]>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BufferViewKind {
    Image,
    Index,
    VertexAttribute,
    InverseBindMatrices,
    Other,
}

fn register_kind<T: Copy + Eq>(kinds: &mut HashMap<usize, T>, index: usize, kind: T) -> bool {
    match kinds.get(&index) {
        Some(previous) => *previous == kind,
        None => {
            kinds.insert(index, kind);
            true
        }
    }
}

/// One glTF node instance referencing one source mesh and world transform.
#[derive(Clone, Debug, PartialEq)]
pub struct GltfInstance {
    pub node_index: usize,
    pub mesh_index: usize,
    pub world_transform: [[f32; 4]; 4],
    pub morph_weights: Vec<f32>,
    /// Shared scene-global joint matrices, empty when no skin is attached.
    pub skin_matrices: Arc<[Mat4]>,
}

/// A static, deformed primitive in one node's scene instance.
#[derive(Clone, Debug, PartialEq)]
pub struct GltfScenePrimitive {
    pub node_index: usize,
    pub mesh_index: usize,
    pub primitive_index: usize,
    pub material_index: Option<usize>,
    pub model_transform: [[f32; 4]; 4],
    pub mesh: MeshData,
    pub material: GltfMaterial,
}

/// One light instance from the selected glTF scene.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GltfSceneLight {
    Directional {
        node_index: usize,
        light: DirectionalLight,
    },
    Punctual {
        node_index: usize,
        light: PunctualLight,
    },
}

/// A parser error or a primitive that the current renderer cannot represent.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("glTF import failed: {0}")]
    Gltf(#[from] gltf::Error),
    #[error("mesh {mesh}, primitive {primitive}: {reason}")]
    Primitive {
        mesh: usize,
        primitive: usize,
        reason: &'static str,
    },
    #[error("glTF scene: {0}")]
    Scene(&'static str),
    #[error("glTF image: {0}")]
    Image(&'static str),
    #[error("glTF buffer: {0}")]
    Buffer(&'static str),
    #[error("glTF source: {0}")]
    Source(&'static str),
    #[error("image decode failed: {0}")]
    Decode(#[from] image::ImageError),
}

/// Imports a `.gltf` or `.glb` file, loading buffers and referenced images.
pub fn import(path: impl AsRef<Path>) -> Result<GltfAsset, ImportError> {
    let path = path.as_ref();
    let mut file = File::open(path).map_err(gltf::Error::Io)?;
    let length = file.metadata().map_err(gltf::Error::Io)?.len();
    let document = load_document(&mut file, length)?;
    from_gltf(document, path.parent())
}

/// Parses embedded glTF data with data-URI or buffer-view images.
pub fn import_slice(data: &[u8]) -> Result<GltfAsset, ImportError> {
    let document = if data.starts_with(b"glTF") {
        load_document(&mut std::io::Cursor::new(data), data.len() as u64)?
    } else {
        if data.len() > MAX_DOCUMENT_BYTES {
            return Err(ImportError::Source("glTF JSON exceeds the 64 MiB limit"));
        }
        parse_document(data)?
    };
    from_gltf(document, None)
}

/// Loads a bounded RGBE equirectangular map in Radiance's canonical `-Y +X` orientation.
/// Non-unit `EXPOSURE`, `COLORCORR`, and `PIXASPECT` metadata are rejected.
pub fn import_environment(path: impl AsRef<Path>) -> Result<EnvironmentMap, ImportError> {
    let path = path.as_ref();
    let mut file = File::open(path).map_err(gltf::Error::Io)?;
    if file.metadata().map_err(gltf::Error::Io)?.len() > MAX_ENVIRONMENT_SOURCE_BYTES {
        return Err(ImportError::Image(
            "environment source exceeds the 96 MiB limit",
        ));
    }
    let header = HdrDecoder::new(BufReader::new(file.try_clone().map_err(gltf::Error::Io)?))?;
    validate_environment_hdr_metadata(&header.metadata()).map_err(ImportError::Image)?;
    drop(header);
    file.seek(SeekFrom::Start(0)).map_err(gltf::Error::Io)?;
    let mut reader = ImageReader::with_format(BufReader::new(file), ImageFormat::Hdr);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_ENVIRONMENT_SOURCE_BYTES);
    reader.limits(limits);
    let image = reader.decode()?;
    let (width, height) = image.dimensions();
    let extent =
        Extent2d::new(width, height).ok_or(ImportError::Image("environment extent is empty"))?;
    let rgb32f = image.into_rgb32f().into_raw();
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(ImportError::Image("environment dimensions overflow"))?;
    if rgb32f.len() != expected
        || rgb32f
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(ImportError::Image(
            "environment radiance must be finite, non-negative RGB32F",
        ));
    }
    let diffuse_irradiance_sh =
        diffuse_irradiance_sh(&rgb32f, width, height).map_err(ImportError::Image)?;
    Ok(EnvironmentMap {
        extent,
        diffuse_irradiance_sh,
    })
}

fn validate_environment_hdr_metadata(metadata: &HdrMetadata) -> Result<(), &'static str> {
    if metadata.orientation != ((1, 0), (0, 1)) {
        return Err("environment HDR must use the canonical -Y +X orientation");
    }
    if !metadata
        .custom_attributes
        .iter()
        .any(|(key, value)| key == "FORMAT" && value.trim() == "32-bit_rle_rgbe")
    {
        return Err("environment HDR must declare FORMAT=32-bit_rle_rgbe");
    }
    if metadata.exposure.is_some_and(|value| value != 1.0)
        || metadata
            .color_correction
            .is_some_and(|value| value != (1.0, 1.0, 1.0))
        || metadata
            .pixel_aspect_ratio
            .is_some_and(|value| value != 1.0)
    {
        return Err("environment HDR requires unit EXPOSURE, COLORCORR, and PIXASPECT");
    }
    Ok(())
}

fn diffuse_irradiance_sh(
    rgb: &[f32],
    width: u32,
    height: u32,
) -> Result<[[f32; 3]; 9], &'static str> {
    let mut radiance_sh = [[0.0_f64; 3]; 9];
    let d_phi = std::f64::consts::TAU / f64::from(width);
    for y in 0..height {
        let theta0 = std::f64::consts::PI * f64::from(y) / f64::from(height);
        let theta1 = std::f64::consts::PI * f64::from(y + 1) / f64::from(height);
        let cos0 = theta0.cos();
        let cos1 = theta1.cos();
        let mean_y = 0.5 * (cos0 + cos1);
        let mean_y2 = (cos0 * cos0 + cos0 * cos1 + cos1 * cos1) / 3.0;
        let sin_theta = (1.0 - mean_y * mean_y).sqrt();
        let weight = d_phi * (cos0 - cos1);
        for x in 0..width {
            let phi = std::f64::consts::TAU * (f64::from(x) + 0.5) / f64::from(width)
                - std::f64::consts::PI;
            let direction = [sin_theta * phi.cos(), mean_y, sin_theta * phi.sin()];
            let mut basis = sh_basis(direction);
            basis[6] = 0.315_391_565_3 * (3.0 * mean_y2 - 1.0);
            let pixel = ((y * width + x) * 3) as usize;
            for coefficient in 0..9 {
                for channel in 0..3 {
                    radiance_sh[coefficient][channel] +=
                        f64::from(rgb[pixel + channel]) * basis[coefficient] * weight;
                }
            }
        }
    }

    let coefficients = std::array::from_fn(|index| {
        let cosine_kernel = match index {
            0 => std::f64::consts::PI,
            1..=3 => 2.0 * std::f64::consts::PI / 3.0,
            _ => std::f64::consts::PI / 4.0,
        };
        std::array::from_fn(|channel| (radiance_sh[index][channel] * cosine_kernel) as f32)
    });
    if coefficients
        .iter()
        .flatten()
        .any(|value| !value.is_finite())
    {
        return Err("environment radiance exceeds the finite diffuse SH range");
    }
    Ok(coefficients)
}

fn sh_basis([x, y, z]: [f64; 3]) -> [f64; 9] {
    [
        0.282_094_791_8,
        0.488_602_511_9 * y,
        0.488_602_511_9 * z,
        0.488_602_511_9 * x,
        1.092_548_430_6 * x * y,
        1.092_548_430_6 * y * z,
        0.315_391_565_3 * (3.0 * y * y - 1.0),
        1.092_548_430_6 * x * z,
        0.546_274_215_3 * (x * x - z * z),
    ]
}

fn load_document(reader: &mut (impl Read + Seek), length: u64) -> Result<gltf::Gltf, ImportError> {
    reader.seek(SeekFrom::Start(0)).map_err(gltf::Error::Io)?;
    let mut magic = [0; 4];
    reader.read_exact(&mut magic).map_err(gltf::Error::Io)?;
    if &magic != b"glTF" {
        if length > MAX_DOCUMENT_BYTES as u64 {
            return Err(ImportError::Source("glTF JSON exceeds the 64 MiB limit"));
        }
        reader.seek(SeekFrom::Start(0)).map_err(gltf::Error::Io)?;
        let mut json = Vec::new();
        reader
            .take(MAX_DOCUMENT_BYTES as u64 + 1)
            .read_to_end(&mut json)
            .map_err(gltf::Error::Io)?;
        if json.len() > MAX_DOCUMENT_BYTES {
            return Err(ImportError::Source("glTF JSON exceeds the 64 MiB limit"));
        }
        return parse_document(&json);
    }

    if length > MAX_GLB_BYTES {
        return Err(ImportError::Source("GLB exceeds the 96 MiB file limit"));
    }
    reader.seek(SeekFrom::Start(0)).map_err(gltf::Error::Io)?;
    let mut header = [0; 12];
    reader.read_exact(&mut header).map_err(gltf::Error::Io)?;
    if u32::from_le_bytes(header[4..8].try_into().expect("four-byte version")) != 2 {
        return Err(ImportError::Source("unsupported GLB version"));
    }
    let declared = u32::from_le_bytes(header[8..12].try_into().expect("four-byte length")) as u64;
    if declared != length || declared < 20 {
        return Err(ImportError::Source("invalid GLB length"));
    }
    let (json_length, json_type) = read_glb_chunk_header(reader)?;
    if &json_type != b"JSON" {
        return Err(ImportError::Source("GLB must start with a JSON chunk"));
    }
    if json_length as usize > MAX_DOCUMENT_BYTES {
        return Err(ImportError::Source("GLB JSON exceeds the 64 MiB limit"));
    }
    if json_length % 4 != 0 {
        return Err(ImportError::Source(
            "GLB chunk length is not four-byte aligned",
        ));
    }
    let json_end = 20_u64 + json_length as u64;
    if json_end > declared {
        return Err(ImportError::Source("invalid GLB JSON chunk length"));
    }
    let mut json = vec![0; json_length as usize];
    reader.read_exact(&mut json).map_err(gltf::Error::Io)?;
    let mut document = parse_document(&json)?;
    let mut binary_buffers = document
        .buffers()
        .enumerate()
        .filter(|(_, buffer)| matches!(buffer.source(), gltf::buffer::Source::Bin));
    let expected_bin = binary_buffers.next().map(|(index, buffer)| {
        if index != 0 {
            Err(ImportError::Buffer("GLB implicit buffer must be first"))
        } else {
            Ok(buffer.length())
        }
    });
    if binary_buffers.next().is_some() {
        return Err(ImportError::Buffer("GLB supports one implicit buffer"));
    }
    let expected_bin = expected_bin.transpose()?;
    if expected_bin.is_some_and(|length| length > MAX_BUFFER_BYTES) {
        return Err(ImportError::Buffer("buffer exceeds the 96 MiB limit"));
    }
    drop(json);
    let mut offset = json_end;
    let mut blob = None;
    let mut saw_bin = false;
    let mut second_chunk = true;
    while offset < declared {
        if declared - offset < 8 {
            return Err(ImportError::Source("truncated GLB chunk header"));
        }
        reader
            .seek(SeekFrom::Start(offset))
            .map_err(gltf::Error::Io)?;
        let (chunk_length, chunk_type) = read_glb_chunk_header(reader)?;
        if chunk_length % 4 != 0 {
            return Err(ImportError::Source(
                "GLB chunk length is not four-byte aligned",
            ));
        }
        let end = offset
            .checked_add(8 + chunk_length as u64)
            .filter(|&end| end <= declared)
            .ok_or(ImportError::Source("invalid GLB chunk length"))?;
        match &chunk_type {
            b"BIN\0" if second_chunk && !saw_bin => {
                saw_bin = true;
                if let Some(expected) = expected_bin {
                    let chunk_length = chunk_length as usize;
                    let padded_length = expected
                        .checked_add(3)
                        .ok_or(ImportError::Buffer("buffer exceeds the 96 MiB limit"))?
                        & !3;
                    if chunk_length != padded_length {
                        return Err(ImportError::Buffer(
                            "GLB binary chunk does not match its declared length",
                        ));
                    }
                    let mut bytes = vec![0; chunk_length];
                    reader.read_exact(&mut bytes).map_err(gltf::Error::Io)?;
                    if bytes[expected..].iter().any(|&byte| byte != 0) {
                        return Err(ImportError::Buffer("GLB binary padding must be zero"));
                    }
                    blob = Some(bytes);
                }
            }
            b"BIN\0" => return Err(ImportError::Source("GLB BIN chunk is not second")),
            b"JSON" => return Err(ImportError::Source("GLB has multiple JSON chunks")),
            _ => {
                reader.seek(SeekFrom::Start(end)).map_err(gltf::Error::Io)?;
            }
        }
        second_chunk = false;
        offset = end;
    }
    document.blob = blob;
    Ok(document)
}

fn read_glb_chunk_header(reader: &mut impl Read) -> Result<(u32, [u8; 4]), ImportError> {
    let mut chunk = [0; 8];
    reader.read_exact(&mut chunk).map_err(gltf::Error::Io)?;
    Ok((
        u32::from_le_bytes(chunk[..4].try_into().expect("four-byte chunk length")),
        chunk[4..].try_into().expect("four-byte chunk type"),
    ))
}

fn parse_document(json: &[u8]) -> Result<gltf::Gltf, ImportError> {
    let document = match gltf::Gltf::from_slice_without_validation(json) {
        Ok(document) => document,
        Err(_) => {
            let normalized = normalize_integer_json_numbers(json);
            let source = normalized.as_deref().unwrap_or(json);
            match gltf::Gltf::from_slice_without_validation(source) {
                Ok(document) => document,
                Err(error) => {
                    let Some(compatible) = ignore_node_less_animation_channels(source)? else {
                        return Err(error.into());
                    };
                    gltf::Gltf::from_slice_without_validation(&compatible)?
                }
            }
        }
    };
    validate_source_metadata(json)?;
    validate_gltf_document(&document)?;
    validate_extension_declarations(json, &document)?;
    Ok(document)
}

#[derive(serde::Deserialize)]
struct SourceMetadata<'a> {
    #[serde(default)]
    animations: Vec<SourceAnimation>,
    #[serde(default, borrow)]
    accessors: Vec<SourceAccessor<'a>>,
    #[serde(default)]
    meshes: Vec<SourceMesh>,
    #[serde(default, borrow)]
    materials: Vec<SourceMaterial<'a>>,
}

#[derive(serde::Deserialize)]
struct SourceAccessor<'a> {
    #[serde(borrow, rename = "componentType")]
    component_type: &'a RawValue,
    #[serde(default, borrow)]
    min: Option<&'a RawValue>,
    #[serde(default, borrow)]
    max: Option<&'a RawValue>,
}

#[derive(serde::Deserialize)]
struct SourceMaterial<'a> {
    #[serde(borrow)]
    extensions: Option<SourceMaterialExtensions<'a>>,
}

#[derive(serde::Deserialize)]
struct SourceMaterialExtensions<'a> {
    #[serde(borrow, rename = "KHR_materials_transmission")]
    transmission: Option<SourceTransmission<'a>>,
}

#[derive(serde::Deserialize)]
struct SourceTransmission<'a> {
    #[serde(default, borrow, rename = "transmissionFactor")]
    factor: SourceJson<'a>,
    #[serde(default, borrow, rename = "transmissionTexture")]
    texture: SourceJson<'a>,
}

#[derive(Default)]
enum SourceJson<'a> {
    #[default]
    Missing,
    Value(&'a RawValue),
}

impl<'de: 'a, 'a> serde::Deserialize<'de> for SourceJson<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <&'de RawValue as serde::Deserialize<'de>>::deserialize(deserializer)
            .map(|value| Self::Value(value))
    }
}

#[derive(serde::Deserialize)]
struct SourceAnimation {
    channels: Vec<IgnoredAny>,
    samplers: Vec<IgnoredAny>,
}

#[derive(serde::Deserialize)]
struct SourceMesh {
    primitives: Vec<SourcePrimitive>,
}

#[derive(serde::Deserialize)]
struct SourcePrimitive {
    attributes: HashMap<String, IgnoredAny>,
    #[serde(default)]
    targets: Option<Vec<HashMap<String, IgnoredAny>>>,
}

fn validate_source_metadata(json: &[u8]) -> Result<(), ImportError> {
    let source: SourceMetadata<'_> = serde_json::from_slice(json)
        .map_err(|_| ImportError::Source("glTF animation or mesh metadata is invalid"))?;
    for accessor in &source.accessors {
        let component_type = exact_json_integer(accessor.component_type.get())
            .ok_or(ImportError::Buffer("accessor componentType is invalid"))?;
        let Some((minimum, maximum)) = integer_component_range(component_type) else {
            continue;
        };
        for bound in [&accessor.min, &accessor.max].into_iter().flatten() {
            let values: Vec<&RawValue> = serde_json::from_str(bound.get())
                .map_err(|_| ImportError::Buffer("accessor bounds have an invalid shape"))?;
            if values.iter().any(|value| {
                exact_json_integer(value.get())
                    .is_none_or(|value| value < minimum || value > maximum)
            }) {
                return Err(ImportError::Buffer(
                    "integer accessor bounds exceed their component type",
                ));
            }
        }
    }
    for transmission in source
        .materials
        .iter()
        .filter_map(|material| material.extensions.as_ref()?.transmission.as_ref())
    {
        if let SourceJson::Value(factor) = &transmission.factor {
            if !json_number_in_unit_interval(factor.get()) {
                return Err(ImportError::Source(
                    "transmissionFactor must be a JSON number in 0..=1",
                ));
            }
        }
        if let SourceJson::Value(texture) = &transmission.texture {
            if !texture.get().trim_start().starts_with('{') {
                return Err(ImportError::Source(
                    "transmissionTexture must be a textureInfo object",
                ));
            }
        }
    }
    if source
        .animations
        .iter()
        .any(|animation| animation.channels.is_empty() || animation.samplers.is_empty())
    {
        return Err(ImportError::Source(
            "animations must contain at least one channel and sampler",
        ));
    }
    for (mesh_index, mesh) in source.meshes.iter().enumerate() {
        for (primitive_index, primitive) in mesh.primitives.iter().enumerate() {
            if !valid_indexed_semantic_sets(primitive.attributes.keys().map(String::as_str)) {
                return Err(ImportError::Primitive {
                    mesh: mesh_index,
                    primitive: primitive_index,
                    reason: "indexed attribute sets must start at zero and be consecutive",
                });
            }
            let Some(targets) = &primitive.targets else {
                continue;
            };
            if targets.is_empty() {
                return Err(ImportError::Primitive {
                    mesh: mesh_index,
                    primitive: primitive_index,
                    reason: "morph target arrays must not be empty",
                });
            }
            for target in targets {
                if target.is_empty() {
                    return Err(ImportError::Primitive {
                        mesh: mesh_index,
                        primitive: primitive_index,
                        reason: "morph targets must contain at least one attribute",
                    });
                }
                if !valid_indexed_semantic_names(target.keys().map(String::as_str)) {
                    return Err(ImportError::Primitive {
                        mesh: mesh_index,
                        primitive: primitive_index,
                        reason: "morph target semantic index is invalid",
                    });
                }
                if target
                    .keys()
                    .any(|name| name.starts_with("TEXCOORD_") || name.starts_with("COLOR_"))
                {
                    return Err(ImportError::Primitive {
                        mesh: mesh_index,
                        primitive: primitive_index,
                        reason: "texture-coordinate and color morph targets are not supported",
                    });
                }
                if target
                    .keys()
                    .any(|name| !["POSITION", "NORMAL", "TANGENT"].contains(&name.as_str()))
                {
                    return Err(ImportError::Primitive {
                        mesh: mesh_index,
                        primitive: primitive_index,
                        reason: "morph target semantic is not supported",
                    });
                }
            }
        }
    }
    Ok(())
}

fn exact_json_integer(number: &str) -> Option<i64> {
    if !number
        .bytes()
        .any(|byte| matches!(byte, b'.' | b'e' | b'E'))
    {
        return number.parse().ok();
    }
    let (negative, magnitude) = number
        .strip_prefix('-')
        .map_or((false, number), |magnitude| (true, magnitude));
    let integer = integer_json_number(magnitude.as_bytes())?
        .into_iter()
        .collect::<Vec<_>>();
    let magnitude = std::str::from_utf8(&integer).ok()?.parse::<i64>().ok()?;
    if negative {
        magnitude.checked_neg()
    } else {
        Some(magnitude)
    }
}

fn integer_component_range(component_type: i64) -> Option<(i64, i64)> {
    match component_type {
        5120 => Some((i8::MIN as i64, i8::MAX as i64)),
        5121 => Some((0, u8::MAX as i64)),
        5122 => Some((i16::MIN as i64, i16::MAX as i64)),
        5123 => Some((0, u16::MAX as i64)),
        5125 => Some((0, u32::MAX as i64)),
        _ => None,
    }
}

fn json_number_in_unit_interval(number: &str) -> bool {
    let (negative, number) = number
        .strip_prefix('-')
        .map_or((false, number), |number| (true, number));
    let (mantissa, exponent) = number
        .find(|character| matches!(character, 'e' | 'E'))
        .map_or((number, 0), |index| {
            let text = &number[index + 1..];
            (
                &number[..index],
                text.parse::<i64>().unwrap_or_else(|_| {
                    if text.starts_with('-') {
                        i64::MIN
                    } else {
                        i64::MAX
                    }
                }),
            )
        });
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits_length = whole.len() + fraction.len();
    let digits = || whole.bytes().chain(fraction.bytes());
    if digits_length == 0 || !digits().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let Some(first_nonzero) = digits().position(|byte| byte != b'0') else {
        return true;
    };
    if negative {
        return false;
    }
    let Some(scale) = exponent.checked_sub(fraction.len() as i64) else {
        return true;
    };
    let significant_length = digits_length - first_nonzero;
    match (significant_length as i64).checked_add(scale) {
        None if scale.is_positive() => false,
        None => true,
        Some(adjusted) if adjusted < 1 => true,
        Some(adjusted) if adjusted > 1 => false,
        Some(_) => {
            digits().nth(first_nonzero) == Some(b'1')
                && digits().skip(first_nonzero + 1).all(|byte| byte == b'0')
        }
    }
}

fn valid_indexed_semantic_sets<'a>(names: impl Iterator<Item = &'a str>) -> bool {
    let mut sets: [HashSet<u32>; 4] = std::array::from_fn(|_| HashSet::new());
    for name in names {
        let Some((set, suffix)) = indexed_semantic(name) else {
            continue;
        };
        let Ok(index) = suffix.parse::<u32>() else {
            return false;
        };
        if index.to_string() != suffix {
            return false;
        }
        sets[set].insert(index);
    }
    sets.iter().filter(|set| !set.is_empty()).all(|set| {
        set.contains(&0)
            && set
                .iter()
                .max()
                .and_then(|index| usize::try_from(*index).ok()?.checked_add(1))
                .is_some_and(|count| count == set.len())
    })
}

fn valid_indexed_semantic_names<'a>(names: impl Iterator<Item = &'a str>) -> bool {
    names.into_iter().all(|name| {
        indexed_semantic(name).is_none_or(|(_, suffix)| {
            suffix
                .parse::<u32>()
                .is_ok_and(|index| index.to_string() == suffix)
        })
    })
}

fn indexed_semantic(name: &str) -> Option<(usize, &str)> {
    ["TEXCOORD_", "COLOR_", "JOINTS_", "WEIGHTS_"]
        .iter()
        .enumerate()
        .find_map(|(set, prefix)| name.strip_prefix(prefix).map(|index| (set, index)))
}

// gltf-json requires target.node and only accepts core paths; validate the channel envelope
// before removing targets the static importer is required to ignore.
fn ignore_node_less_animation_channels(json: &[u8]) -> Result<Option<Vec<u8>>, ImportError> {
    let Ok(mut root) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Ok(None);
    };
    let Some(animations) = root
        .get_mut("animations")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(None);
    };
    let mut changed = false;
    for animation in animations {
        let sampler_count = animation
            .get("samplers")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len);
        if let Some(channels) = animation
            .get_mut("channels")
            .and_then(serde_json::Value::as_array_mut)
        {
            let mut kept = Vec::with_capacity(channels.len());
            for channel in channels.drain(..) {
                let Some(target) = channel.get("target").and_then(serde_json::Value::as_object)
                else {
                    kept.push(channel);
                    continue;
                };
                if target.contains_key("node") {
                    kept.push(channel);
                    continue;
                }
                let sampler_exists = channel
                    .get("sampler")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .zip(sampler_count)
                    .is_some_and(|(index, count)| index < count);
                if !sampler_exists {
                    return Err(ImportError::Scene(
                        "node-less animation channel has an invalid sampler",
                    ));
                }
                let valid_extensions = |value: Option<&serde_json::Value>| {
                    value.is_none_or(|value| {
                        value.as_object().is_some_and(|extensions| {
                            extensions.values().all(serde_json::Value::is_object)
                        })
                    })
                };
                if !target.get("path").is_some_and(serde_json::Value::is_string)
                    || !valid_extensions(target.get("extensions"))
                    || !valid_extensions(channel.get("extensions"))
                {
                    return Err(ImportError::Scene(
                        "node-less animation channel has invalid target metadata",
                    ));
                }
                changed = true;
            }
            *channels = kept;
        }
    }
    changed
        .then(|| {
            serde_json::to_vec(&root)
                .map_err(|_| ImportError::Source("glTF compatibility rewrite failed"))
        })
        .transpose()
}

fn validate_gltf_document(document: &gltf::Gltf) -> Result<(), ImportError> {
    use gltf::json::validation::{Error, Validate};

    let root = document.document.as_json();
    let zero_accessors: HashSet<_> = root
        .accessors
        .iter()
        .enumerate()
        .filter(|(_, accessor)| accessor.buffer_view.is_none() && accessor.sparse.is_none())
        .map(|(index, _)| index)
        .collect();
    let mut errors = Vec::new();
    root.validate(root, gltf::json::Path::new, &mut |path, error| {
        let path = path();
        let omitted_zero_view = error == Error::Missing
            && path
                .as_str()
                .strip_prefix("accessors[")
                .and_then(|path| path.split_once(']'))
                .is_some_and(|(index, field)| {
                    field == ".bufferView"
                        && index
                            .parse::<usize>()
                            .is_ok_and(|index| zero_accessors.contains(&index))
                });
        if !omitted_zero_view {
            errors.push((path, error));
        }
    });
    if errors.is_empty() {
        Ok(())
    } else {
        Err(gltf::Error::Validation(errors).into())
    }
}

fn normalize_integer_json_numbers(json: &[u8]) -> Option<Vec<u8>> {
    let mut normalized = Vec::with_capacity(json.len());
    let mut cursor = 0;
    let mut changed = false;
    while cursor < json.len() {
        let start = cursor;
        match json[cursor] {
            b'"' => {
                cursor += 1;
                while cursor < json.len() {
                    match json[cursor] {
                        b'\\' => cursor = (cursor + 2).min(json.len()),
                        b'"' => {
                            cursor += 1;
                            break;
                        }
                        _ => cursor += 1,
                    }
                }
                normalized.extend_from_slice(&json[start..cursor]);
            }
            b'-' | b'0'..=b'9' => {
                cursor += 1;
                while cursor < json.len()
                    && matches!(json[cursor], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    cursor += 1;
                }
                if let Some(integer) = integer_json_number(&json[start..cursor]) {
                    normalized.extend(integer);
                    changed = true;
                } else {
                    normalized.extend_from_slice(&json[start..cursor]);
                }
            }
            _ => {
                normalized.push(json[cursor]);
                cursor += 1;
            }
        }
    }
    changed.then_some(normalized)
}

// gltf-json rejects glTF integer fields written as valid integral decimals or exponents.
// Rewrite integral spellings exactly; normal asset imports stay on the fast path.
fn integer_json_number(number: &[u8]) -> Option<Vec<u8>> {
    if number == b"-0" {
        return Some(b"0".to_vec());
    }
    if !number
        .iter()
        .any(|&byte| matches!(byte, b'.' | b'e' | b'E'))
    {
        return None;
    }
    serde_json::from_slice::<serde_json::Number>(number).ok()?;
    let text = std::str::from_utf8(number).ok()?;
    let (negative, text) = text
        .strip_prefix('-')
        .map_or((false, text), |text| (true, text));
    let (mantissa, exponent) = if let Some(index) = text.find(|c| matches!(c, 'e' | 'E')) {
        (&text[..index], Some(&text[index + 1..]))
    } else {
        (text, None)
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits = String::with_capacity(whole.len() + fraction.len());
    digits.push_str(whole);
    digits.push_str(fraction);
    if digits.bytes().all(|byte| byte == b'0') {
        return Some(b"0".to_vec());
    }
    if negative {
        return None;
    }
    let exponent = exponent.map_or(Some(0), |exponent| exponent.parse::<i64>().ok())?;
    let scale = exponent.checked_sub(i64::try_from(fraction.len()).ok()?)?;
    if scale < 0 {
        let trim = usize::try_from(scale.checked_neg()?).ok()?;
        if trim >= digits.len()
            || digits.as_bytes()[digits.len() - trim..]
                .iter()
                .any(|&byte| byte != b'0')
        {
            return None;
        }
        digits.truncate(digits.len() - trim);
    } else {
        let first_nonzero = digits.find(|c| c != '0').unwrap_or(digits.len());
        digits.drain(..first_nonzero);
        let append = usize::try_from(scale).ok()?;
        if digits.len().checked_add(append)? > 20 {
            return None;
        }
        digits.extend(std::iter::repeat_n('0', append));
    }
    let first_nonzero = digits.find(|c| c != '0').unwrap_or(digits.len());
    digits.drain(..first_nonzero);
    let integer = digits.parse::<u64>().ok()?;
    Some(integer.to_string().into_bytes())
}

#[derive(Default)]
struct ExtensionDeclarations {
    used: HashSet<String>,
    required: HashSet<String>,
    found: HashSet<String>,
}

struct ExtensionScanner<'a> {
    declarations: &'a mut ExtensionDeclarations,
    object: CoreObject,
}

struct ExtensionSet<'a> {
    declarations: &'a mut ExtensionDeclarations,
}

impl<'de> DeserializeSeed<'de> for ExtensionSet<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ExtensionSet<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a glTF extensions object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<(), M::Error>
    where
        M: MapAccess<'de>,
    {
        while let Some(name) = map.next_key::<String>()? {
            self.declarations.found.insert(name);
            map.next_value_seed(ExtensionPayload)?;
        }
        Ok(())
    }
}

struct ExtensionPayload;

impl<'de> DeserializeSeed<'de> for ExtensionPayload {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ExtensionPayload {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("an extension payload object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<(), M::Error>
    where
        M: MapAccess<'de>,
    {
        while map.next_key::<String>()?.is_some() {
            map.next_value::<IgnoredAny>()?;
        }
        Ok(())
    }
}

struct SceneRoots;

impl<'de> DeserializeSeed<'de> for SceneRoots {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for SceneRoots {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a non-empty list of unique scene root indices")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut seen = [0_u64; MAX_SCENE_NODES / 64];
        let mut count = 0;
        while let Some(number) = seq.next_element::<&RawValue>()? {
            let text = number.get();
            let index = text
                .parse::<usize>()
                .ok()
                .or_else(|| {
                    integer_json_number(text.as_bytes())
                        .and_then(|integer| std::str::from_utf8(&integer).ok()?.parse().ok())
                })
                .ok_or_else(|| A::Error::custom("invalid scene root index"))?;
            if index >= MAX_SCENE_NODES {
                return Err(A::Error::custom("scene root index exceeds the node limit"));
            }
            let bit = 1_u64 << (index % 64);
            if seen[index / 64] & bit != 0 {
                return Err(A::Error::custom("duplicate scene root index"));
            }
            seen[index / 64] |= bit;
            count += 1;
        }
        if count == 0 {
            return Err(A::Error::custom("scene.nodes must not be empty"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CoreObject {
    Root,
    Asset,
    Accessor,
    SparseAccessor,
    SparseIndices,
    SparseValues,
    Animation,
    AnimationChannel,
    AnimationTarget,
    AnimationSampler,
    Buffer,
    BufferView,
    Camera,
    CameraOrthographic,
    CameraPerspective,
    Image,
    Material,
    NormalTextureInfo,
    OcclusionTextureInfo,
    TextureInfo,
    PbrMetallicRoughness,
    Mesh,
    Primitive,
    Node,
    Sampler,
    Scene,
    Skin,
    Texture,
}

#[derive(Clone, Copy)]
enum CoreField {
    Value,
    Object(CoreObject),
    Objects(CoreObject),
    NonEmptyArray,
}

fn core_field(object: CoreObject, key: &str) -> Option<CoreField> {
    use CoreField::{Object as O, Objects as A, Value as V};
    use CoreObject::*;
    Some(match (object, key) {
        (Root, "asset") => O(Asset),
        (Root, "accessors") => A(Accessor),
        (Root, "animations") => A(Animation),
        (Root, "buffers") => A(Buffer),
        (Root, "bufferViews") => A(BufferView),
        (Root, "cameras") => A(Camera),
        (Root, "images") => A(Image),
        (Root, "materials") => A(Material),
        (Root, "meshes") => A(Mesh),
        (Root, "nodes") => A(Node),
        (Root, "samplers") => A(Sampler),
        (Root, "scene") => V,
        (Root, "scenes") => A(Scene),
        (Root, "skins") => A(Skin),
        (Root, "textures") => A(Texture),
        (Asset, "copyright" | "generator" | "minVersion" | "version") => V,
        (
            Accessor,
            "bufferView" | "byteOffset" | "componentType" | "count" | "max" | "min" | "name"
            | "normalized" | "type",
        ) => V,
        (Accessor, "sparse") => O(SparseAccessor),
        (SparseAccessor, "count") => V,
        (SparseAccessor, "indices") => O(SparseIndices),
        (SparseAccessor, "values") => O(SparseValues),
        (SparseIndices, "bufferView" | "byteOffset" | "componentType") => V,
        (SparseValues, "bufferView" | "byteOffset") => V,
        (Animation, "name") => V,
        (Animation, "channels") => A(AnimationChannel),
        (Animation, "samplers") => A(AnimationSampler),
        (AnimationChannel, "sampler") => V,
        (AnimationChannel, "target") => O(AnimationTarget),
        (AnimationTarget, "node" | "path") => V,
        (AnimationSampler, "input" | "interpolation" | "output") => V,
        (Buffer, "byteLength" | "name" | "uri") => V,
        (BufferView, "buffer" | "byteLength" | "byteOffset" | "byteStride" | "name" | "target") => {
            V
        }
        (Camera, "type" | "name") => V,
        (Camera, "orthographic") => O(CameraOrthographic),
        (Camera, "perspective") => O(CameraPerspective),
        (CameraOrthographic, "xmag" | "ymag" | "zfar" | "znear") => V,
        (CameraPerspective, "aspectRatio" | "yfov" | "zfar" | "znear") => V,
        (Image, "bufferView" | "mimeType" | "name" | "uri") => V,
        (Material, "alphaCutoff" | "alphaMode" | "doubleSided" | "emissiveFactor" | "name") => V,
        (Material, "emissiveTexture") => O(TextureInfo),
        (Material, "normalTexture") => O(NormalTextureInfo),
        (Material, "occlusionTexture") => O(OcclusionTextureInfo),
        (Material, "pbrMetallicRoughness") => O(PbrMetallicRoughness),
        (NormalTextureInfo, "index" | "scale" | "texCoord") => V,
        (OcclusionTextureInfo, "index" | "strength" | "texCoord") => V,
        (TextureInfo, "index" | "texCoord") => V,
        (PbrMetallicRoughness, "baseColorFactor" | "metallicFactor" | "roughnessFactor") => V,
        (PbrMetallicRoughness, "baseColorTexture" | "metallicRoughnessTexture") => O(TextureInfo),
        (Mesh, "name" | "weights") => V,
        (Mesh, "primitives") => A(Primitive),
        (Primitive, "attributes" | "indices" | "material" | "mode" | "targets") => V,
        (
            Node,
            "camera" | "matrix" | "mesh" | "name" | "rotation" | "scale" | "skin" | "translation"
            | "weights",
        ) => V,
        (Node, "children") => CoreField::NonEmptyArray,
        (Sampler, "magFilter" | "minFilter" | "name" | "wrapS" | "wrapT") => V,
        (Scene, "name") => V,
        (Scene, "nodes") => V,
        (Skin, "inverseBindMatrices" | "joints" | "name" | "skeleton") => V,
        (Texture, "name" | "sampler" | "source") => V,
        _ => return None,
    })
}

struct ObjectArray<'a> {
    declarations: &'a mut ExtensionDeclarations,
    object: CoreObject,
}

impl<'de> DeserializeSeed<'de> for ObjectArray<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for ObjectArray<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("an array of glTF objects")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut count = 0;
        while seq
            .next_element_seed(ExtensionScanner {
                declarations: &mut *self.declarations,
                object: self.object,
            })?
            .is_some()
        {
            count += 1;
        }
        if count == 0 {
            return Err(A::Error::custom("glTF object arrays must not be empty"));
        }
        Ok(())
    }
}

struct NonEmptyValues;

impl<'de> DeserializeSeed<'de> for NonEmptyValues {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for NonEmptyValues {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a non-empty glTF array")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut count = 0;
        while seq.next_element_seed(CoreValue)?.is_some() {
            count += 1;
        }
        if count == 0 {
            return Err(A::Error::custom("glTF arrays must not be empty"));
        }
        Ok(())
    }
}

struct CoreValue;

impl<'de> DeserializeSeed<'de> for CoreValue {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for CoreValue {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a non-null glTF core value")
    }

    fn visit_map<M>(self, mut map: M) -> Result<(), M::Error>
    where
        M: MapAccess<'de>,
    {
        while map.next_key::<IgnoredAny>()?.is_some() {
            map.next_value_seed(CoreValue)?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element_seed(CoreValue)?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E>
    where
        E: serde::de::Error,
    {
        Err(E::custom("glTF core properties must not be null"))
    }

    fn visit_none<E>(self) -> Result<(), E>
    where
        E: serde::de::Error,
    {
        Err(E::custom("glTF core properties must not be null"))
    }
}

impl<'de> DeserializeSeed<'de> for ExtensionScanner<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ExtensionScanner<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a glTF core object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<(), M::Error>
    where
        M: MapAccess<'de>,
    {
        let declarations = self.declarations;
        let mut has_scene_nodes = false;
        while let Some(key) = map.next_key::<String>()? {
            if self.object == CoreObject::Root && key == "extensionsUsed" {
                let names = map.next_value::<Vec<String>>()?;
                if names.is_empty() {
                    return Err(M::Error::custom("extensionsUsed must not be empty"));
                }
                for name in names {
                    if !declarations.used.insert(name) {
                        return Err(M::Error::custom("duplicate extensionsUsed entry"));
                    }
                }
                continue;
            }
            if self.object == CoreObject::Root && key == "extensionsRequired" {
                let names = map.next_value::<Vec<String>>()?;
                if names.is_empty() {
                    return Err(M::Error::custom("extensionsRequired must not be empty"));
                }
                for name in names {
                    if !declarations.required.insert(name) {
                        return Err(M::Error::custom("duplicate extensionsRequired entry"));
                    }
                }
                continue;
            }
            if key == "extensions" {
                map.next_value_seed(ExtensionSet {
                    declarations: &mut *declarations,
                })?;
                continue;
            }
            if key == "extras" {
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            let Some(field) = core_field(self.object, &key) else {
                map.next_value::<IgnoredAny>()?;
                continue;
            };
            match field {
                CoreField::Value if self.object == CoreObject::Scene && key == "nodes" => {
                    if has_scene_nodes {
                        return Err(M::Error::custom("duplicate scene.nodes property"));
                    }
                    has_scene_nodes = true;
                    map.next_value_seed(SceneRoots)?;
                }
                CoreField::NonEmptyArray => map.next_value_seed(NonEmptyValues)?,
                CoreField::Value => map.next_value_seed(CoreValue)?,
                CoreField::Object(object) => map.next_value_seed(ExtensionScanner {
                    declarations: &mut *declarations,
                    object,
                })?,
                CoreField::Objects(object) => map.next_value_seed(ObjectArray {
                    declarations: &mut *declarations,
                    object,
                })?,
            }
        }
        Ok(())
    }
}

fn validate_extension_declarations(json: &[u8], document: &gltf::Gltf) -> Result<(), ImportError> {
    let mut declarations = ExtensionDeclarations::default();
    let mut parser = serde_json::Deserializer::from_slice(json);
    ExtensionScanner {
        declarations: &mut declarations,
        object: CoreObject::Root,
    }
    .deserialize(&mut parser)
    .map_err(|_| ImportError::Source("invalid glTF metadata"))?;
    parser
        .end()
        .map_err(|_| ImportError::Source("invalid glTF metadata"))?;
    if let Some(lights) = document
        .document
        .as_json()
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.khr_lights_punctual.as_ref())
    {
        for light in &lights.lights {
            let Some(extensions) = &light.extensions else {
                continue;
            };
            let mut parser = serde_json::Deserializer::from_str(extensions.get());
            ExtensionSet {
                declarations: &mut declarations,
            }
            .deserialize(&mut parser)
            .map_err(|_| ImportError::Source("invalid glTF light extensions"))?;
            parser
                .end()
                .map_err(|_| ImportError::Source("invalid glTF light extensions"))?;
        }
    }
    if declarations
        .required
        .iter()
        .any(|name| !declarations.used.contains(name))
        || declarations
            .found
            .iter()
            .any(|name| !declarations.used.contains(name))
    {
        return Err(ImportError::Source(
            "extensionsRequired and extension payloads must be listed in extensionsUsed",
        ));
    }
    if declarations.required.iter().any(|name| {
        !matches!(
            name.as_str(),
            "KHR_lights_punctual" | "KHR_materials_emissive_strength" | "KHR_materials_unlit"
        )
    }) {
        return Err(ImportError::Source(
            "a required glTF extension is not fully supported",
        ));
    }
    Ok(())
}

fn from_gltf(gltf: gltf::Gltf, base: Option<&Path>) -> Result<GltfAsset, ImportError> {
    validate_asset_version(&gltf.document)?;
    validate_document_shape(&gltf.document)?;
    validate_punctual_lights(&gltf.document)?;
    let buffers = import_buffers(&gltf.document, base, gltf.blob)?;
    validate_accessor_ranges(&gltf.document, &buffers)?;
    let used_images = referenced_images(&gltf.document);
    let mut images = Vec::new();
    let mut image_sources: HashMap<usize, GltfImage> = HashMap::new();
    for image in gltf.document.images() {
        for color_space in [ImageColorSpace::Linear, ImageColorSpace::Srgb] {
            if !used_images.contains(&(image.index(), color_space)) {
                continue;
            }
            let source = if let Some(source) = image_sources.get(&image.index()) {
                source.clone()
            } else {
                let source =
                    import_image(image.index(), color_space, image.source(), base, &buffers)?;
                image_sources.insert(image.index(), source.clone());
                source
            };
            images.push(GltfImage {
                color_space,
                ..source
            });
        }
    }
    Ok(GltfAsset {
        document: gltf.document,
        buffers,
        images,
    })
}

fn parse_version(version: &str) -> Option<(u32, u32)> {
    let (major, minor) = version.split_once('.')?;
    if minor.contains('.') {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn validate_asset_version(document: &gltf::Document) -> Result<(), ImportError> {
    let asset = &document.as_json().asset;
    if !parse_version(&asset.version).is_some_and(|(major, _)| major == 2)
        || asset
            .min_version
            .as_deref()
            .and_then(parse_version)
            .is_some_and(|minimum| minimum > (2, 0))
    {
        return Err(ImportError::Source("glTF asset version is not supported"));
    }
    if asset
        .min_version
        .as_deref()
        .is_some_and(|version| parse_version(version).is_none())
    {
        return Err(ImportError::Source("glTF asset minimum version is invalid"));
    }
    Ok(())
}

struct NodeHierarchy {
    parents: Vec<Option<usize>>,
    roots: Vec<usize>,
    entry: Vec<usize>,
    exit: Vec<usize>,
}

impl NodeHierarchy {
    fn from_document(document: &gltf::Document) -> Result<Self, ImportError> {
        let nodes = &document.as_json().nodes;
        if nodes.len() > MAX_SCENE_NODES {
            return Err(ImportError::Scene("document exceeds the 65,536 node limit"));
        }

        let mut parents = vec![None; nodes.len()];
        for (parent, node) in nodes.iter().enumerate() {
            for child in node.children.iter().flatten() {
                let Some(slot) = parents.get_mut(child.value()) else {
                    return Err(ImportError::Scene("node references a missing child"));
                };
                if slot.replace(parent).is_some() {
                    return Err(ImportError::Scene("node has more than one parent"));
                }
            }
        }

        let mut hierarchy = Self {
            parents,
            roots: vec![usize::MAX; nodes.len()],
            entry: vec![usize::MAX; nodes.len()],
            exit: vec![0; nodes.len()],
        };
        let mut visited = 0;
        let mut stack = Vec::new();
        for root in 0..nodes.len() {
            if hierarchy.parents[root].is_some() {
                continue;
            }
            stack.push((root, false));
            while let Some((index, leaving)) = stack.pop() {
                if leaving {
                    hierarchy.exit[index] = visited;
                    continue;
                }
                if hierarchy.entry[index] != usize::MAX {
                    return Err(ImportError::Scene("node hierarchy contains a cycle"));
                }
                hierarchy.roots[index] = root;
                hierarchy.entry[index] = visited;
                visited += 1;
                stack.push((index, true));
                if let Some(children) = &nodes[index].children {
                    stack.extend(children.iter().rev().map(|child| (child.value(), false)));
                }
            }
        }
        if visited != nodes.len() {
            return Err(ImportError::Scene("node hierarchy contains a cycle"));
        }
        for scene in document.scenes() {
            for node in scene.nodes() {
                let Some(parent) = hierarchy.parents.get(node.index()) else {
                    return Err(ImportError::Scene("scene references a missing node"));
                };
                if parent.is_some() {
                    return Err(ImportError::Scene("scene nodes must be hierarchy roots"));
                }
            }
        }
        Ok(hierarchy)
    }

    fn is_ancestor(&self, ancestor: usize, node: usize) -> bool {
        let (Some(&ancestor_root), Some(&node_root)) =
            (self.roots.get(ancestor), self.roots.get(node))
        else {
            return false;
        };
        ancestor_root == node_root
            && self.entry[ancestor] <= self.entry[node]
            && self.entry[node] < self.exit[ancestor]
    }
}

fn validate_skin_hierarchies(
    document: &gltf::Document,
    hierarchy: &NodeHierarchy,
) -> Result<Vec<usize>, ImportError> {
    let mut skin_roots = Vec::new();
    for skin in document.skins() {
        let mut joints = HashSet::new();
        let mut common_root = None;
        for joint in skin.joints() {
            let index = joint.index();
            if !joints.insert(index) {
                return Err(ImportError::Scene("skin joints must be unique"));
            }
            let Some(&root) = hierarchy.roots.get(index) else {
                return Err(ImportError::Scene("skin references a missing joint"));
            };
            if common_root.is_some_and(|common_root| common_root != root) {
                return Err(ImportError::Scene(
                    "skin joints must share a common hierarchy root",
                ));
            }
            common_root = Some(root);
        }
        let Some(common_root) = common_root else {
            return Err(ImportError::Scene("skin joints must not be empty"));
        };
        if skin.skeleton().is_some_and(|skeleton| {
            joints
                .iter()
                .any(|&joint| !hierarchy.is_ancestor(skeleton.index(), joint))
        }) {
            return Err(ImportError::Scene(
                "skin skeleton must be the common root or one of its ancestors",
            ));
        }
        skin_roots.push(common_root);
    }
    Ok(skin_roots)
}

fn validate_skin_scene_membership(
    document: &gltf::Document,
    hierarchy: &NodeHierarchy,
    skin_roots: &[usize],
) -> Result<(), ImportError> {
    let mut scenes_by_root = vec![Vec::new(); hierarchy.roots.len()];
    for (scene_index, scene) in document.scenes().enumerate() {
        for root in scene.nodes() {
            scenes_by_root[root.index()].push(scene_index);
        }
    }
    for (node_index, node) in document.as_json().nodes.iter().enumerate() {
        let Some(skin_index) = node.skin.as_ref().map(|index| index.value()) else {
            continue;
        };
        let Some(&skin_root) = skin_roots.get(skin_index) else {
            return Err(ImportError::Scene("node references a missing skin"));
        };
        let mesh_root = hierarchy.roots[node_index];
        if scenes_by_root[mesh_root]
            .iter()
            .any(|scene| scenes_by_root[skin_root].binary_search(scene).is_err())
        {
            return Err(ImportError::Scene(
                "skin joints must belong to the same scene as the skinned node",
            ));
        }
    }
    Ok(())
}

fn validate_document_shape(document: &gltf::Document) -> Result<(), ImportError> {
    let json = document.as_json();
    validate_material_extensions(document)?;
    let hierarchy = NodeHierarchy::from_document(document)?;
    let skin_roots = validate_skin_hierarchies(document, &hierarchy)?;
    validate_skin_scene_membership(document, &hierarchy, &skin_roots)?;
    validate_image_sources(document)?;
    if json.nodes.iter().any(|node| {
        node.matrix.is_some()
            && (node.translation.is_some() || node.rotation.is_some() || node.scale.is_some())
    }) {
        return Err(ImportError::Scene(
            "node must use either matrix or TRS, not both",
        ));
    }
    if json
        .meshes
        .iter()
        .any(|mesh| mesh.primitives.is_empty() || mesh.weights.as_ref().is_some_and(Vec::is_empty))
        || json.nodes.iter().any(|node| {
            (node.skin.is_some() || node.weights.is_some()) && node.mesh.is_none()
                || node.weights.as_ref().is_some_and(Vec::is_empty)
        })
        || json.skins.iter().any(|skin| skin.joints.is_empty())
    {
        return Err(ImportError::Scene(
            "glTF contains an empty or incomplete object",
        ));
    }

    for mesh in document.meshes() {
        mesh_morph_target_count(&mesh)?;
    }
    if json
        .animations
        .iter()
        .flat_map(|animation| &animation.channels)
        .any(|channel| channel.target.node.value() >= json.nodes.len())
    {
        return Err(ImportError::Scene("animation targets a missing node"));
    }
    if json.animations.iter().any(|animation| {
        animation
            .channels
            .iter()
            .any(|channel| channel.sampler.value() >= animation.samplers.len())
            || animation.samplers.iter().any(|sampler| {
                sampler.input.value() >= json.accessors.len()
                    || sampler.output.value() >= json.accessors.len()
            })
    }) {
        return Err(ImportError::Scene(
            "animation references a missing sampler or accessor",
        ));
    }
    if json.animations.iter().any(|animation| {
        animation.samplers.iter().any(|sampler| {
            let accessor = &json.accessors[sampler.input.value()];
            accessor.min.is_none() || accessor.max.is_none()
        })
    }) {
        return Err(ImportError::Scene(
            "animation input accessor must define min and max bounds",
        ));
    }
    if json
        .animations
        .iter()
        .flat_map(|animation| &animation.channels)
        .any(|channel| json.nodes[channel.target.node.value()].matrix.is_some())
    {
        return Err(ImportError::Scene(
            "animation targets a node with a matrix transform",
        ));
    }
    for node in document.nodes() {
        if !node_transform_is_trs(&node.transform()) {
            return Err(ImportError::Scene(
                "node transform is not finite or cannot be decomposed to TRS",
            ));
        }
        let mesh = node.mesh();
        let weights = node.weights();
        let skin = node.skin();
        if let Some(mesh) = mesh {
            let count = mesh_morph_target_count(&mesh)?;
            if weights.is_some_and(|weights| {
                weights.len() != count || weights.iter().any(|weight| !weight.is_finite())
            }) {
                return Err(ImportError::Scene(
                    "node morph weights do not match its mesh targets",
                ));
            }
            if skin.is_some()
                && mesh.primitives().any(|primitive| {
                    primitive.get(&Semantic::Joints(0)).is_none()
                        || primitive.get(&Semantic::Weights(0)).is_none()
                })
            {
                return Err(ImportError::Scene(
                    "skinned nodes require JOINTS_0 and WEIGHTS_0",
                ));
            }
        }
    }
    Ok(())
}

fn validate_material_extensions(document: &gltf::Document) -> Result<(), ImportError> {
    for material in document.materials() {
        let unlit = material.unlit();
        let specular = material.extension_value("KHR_materials_specular").is_some();
        let specular_glossiness = material
            .extension_value("KHR_materials_pbrSpecularGlossiness")
            .is_some();
        let transmission = material.transmission();
        if (unlit && (material.emissive_strength().is_some() || specular))
            || (specular && specular_glossiness)
            || (transmission.is_some() && (unlit || specular_glossiness))
        {
            return Err(ImportError::Source("incompatible material extensions"));
        }
        let Some(transmission) = transmission else {
            continue;
        };
        let factor = transmission.transmission_factor();
        if !factor.is_finite() || !(0.0..=1.0).contains(&factor) {
            return Err(ImportError::Source(
                "transmissionFactor must be finite and in 0..=1",
            ));
        }
    }
    Ok(())
}

fn validate_punctual_lights(document: &gltf::Document) -> Result<(), ImportError> {
    let extension = document
        .as_json()
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.khr_lights_punctual.as_ref());
    if extension.is_some_and(|extension| extension.lights.is_empty()) {
        return Err(ImportError::Source(
            "KHR_lights_punctual must define at least one light",
        ));
    }
    let source_lights = extension.map_or(&[][..], |extension| extension.lights.as_slice());
    for (source, light) in source_lights
        .iter()
        .zip(document.lights().into_iter().flatten())
    {
        let color = light.color();
        if color
            .iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            || !light.intensity().is_finite()
            || light.intensity() < 0.0
            || light
                .range()
                .is_some_and(|range| !range.is_finite() || range <= 0.0)
        {
            return Err(ImportError::Source("invalid KHR_lights_punctual values"));
        }
        if let Some(spot) = &source.spot {
            let inner = spot.inner_cone_angle;
            let outer = spot.outer_cone_angle;
            if !inner.is_finite()
                || !outer.is_finite()
                || inner < 0.0
                || inner >= std::f32::consts::FRAC_PI_2
                || outer <= 0.0
                || outer > std::f32::consts::FRAC_PI_2
                || inner >= outer
            {
                return Err(ImportError::Source("invalid KHR_lights_punctual spot cone"));
            }
        }
    }
    Ok(())
}

fn validate_image_sources(document: &gltf::Document) -> Result<(), ImportError> {
    for image in &document.as_json().images {
        if image.uri.is_some() == image.buffer_view.is_some() {
            return Err(ImportError::Image(
                "image must define exactly one of uri or bufferView",
            ));
        }
        if image.buffer_view.is_some() && image.mime_type.is_none() {
            return Err(ImportError::Image("bufferView image must define mimeType"));
        }
        if image
            .mime_type
            .as_ref()
            .is_some_and(|mime| !matches!(mime.0.as_str(), "image/jpeg" | "image/png"))
        {
            return Err(ImportError::Image("only PNG and JPEG images are supported"));
        }
    }
    Ok(())
}

fn node_transform_is_trs(transform: &gltf::scene::Transform) -> bool {
    match transform {
        gltf::scene::Transform::Matrix { matrix } => {
            if matrix.iter().flatten().any(|value| !value.is_finite())
                || matrix[0][3] != 0.0
                || matrix[1][3] != 0.0
                || matrix[2][3] != 0.0
                || matrix[3][3] != 1.0
            {
                return false;
            }
            let columns = std::array::from_fn::<_, 3, _>(|column| {
                std::array::from_fn::<_, 3, _>(|row| matrix[column][row] as f64)
            });
            for a in 0..3 {
                for b in a + 1..3 {
                    let dot = (0..3).map(|i| columns[a][i] * columns[b][i]).sum::<f64>();
                    let scale = (0..3)
                        .map(|i| columns[a][i] * columns[a][i])
                        .sum::<f64>()
                        .sqrt()
                        * (0..3)
                            .map(|i| columns[b][i] * columns[b][i])
                            .sum::<f64>()
                            .sqrt();
                    if dot.abs() > 1e-5 * scale {
                        return false;
                    }
                }
            }
            true
        }
        gltf::scene::Transform::Decomposed {
            translation,
            rotation,
            scale,
        } => {
            translation
                .iter()
                .chain(scale)
                .all(|value| value.is_finite())
                && rotation.iter().all(|value| value.is_finite())
                && ((rotation.iter().map(|value| value * value).sum::<f32>() - 1.0).abs() <= 1e-4)
        }
    }
}

fn validate_accessor_ranges(
    document: &gltf::Document,
    buffers: &[Vec<u8>],
) -> Result<(), ImportError> {
    if document
        .as_json()
        .accessors
        .iter()
        .any(|accessor| accessor.buffer_view.is_none() && accessor.byte_offset.is_some())
    {
        return Err(ImportError::Buffer(
            "accessor byteOffset requires a bufferView",
        ));
    }
    validate_vertex_accessor_layout(document)?;
    let index_accessors: HashSet<_> = document
        .meshes()
        .flat_map(|mesh| mesh.primitives())
        .filter_map(|primitive| primitive.indices())
        .map(|accessor| accessor.index())
        .collect();
    for view in document.views() {
        if view.length() == 0 {
            return Err(ImportError::Buffer(
                "buffer view byteLength must be positive",
            ));
        }
        buffer_view_data(&view, buffers)?;
        if view
            .stride()
            .is_some_and(|stride| !(4..=252).contains(&stride) || stride % 4 != 0)
        {
            return Err(ImportError::Buffer("buffer view byteStride is invalid"));
        }
    }
    for accessor in document.accessors() {
        if accessor.data_type() == DataType::U32 && !index_accessors.contains(&accessor.index()) {
            return Err(ImportError::Buffer(
                "UNSIGNED_INT accessor must be used for primitive indices",
            ));
        }
        let count = accessor.count();
        let (size, last_size) = accessor_element_sizes(&accessor);
        if count == 0 || size == 0 || last_size == 0 {
            return Err(ImportError::Buffer(
                "accessor count or element size is invalid",
            ));
        }
        if let Some(view) = accessor.view() {
            let bytes = buffer_view_data(&view, buffers)?;
            let stride = view.stride().unwrap_or(size);
            let absolute_offset = view
                .offset()
                .checked_add(accessor.offset())
                .ok_or(ImportError::Buffer("accessor offset overflows"))?;
            let component_size = accessor.data_type().size();
            let matrix = matches!(
                accessor.dimensions(),
                Dimensions::Mat2 | Dimensions::Mat3 | Dimensions::Mat4
            ) && component_size < 4;
            if accessor.offset() % component_size != 0
                || absolute_offset % component_size != 0
                || stride % component_size != 0
                || (matrix && absolute_offset % 4 != 0)
            {
                return Err(ImportError::Buffer("accessor data is misaligned"));
            }
            if !accessor_range_fits(
                accessor.offset(),
                count,
                stride,
                size,
                last_size,
                bytes.len(),
            ) {
                return Err(ImportError::Buffer("accessor data exceeds its buffer view"));
            }
        }
        if let Some(sparse) = accessor.sparse() {
            let sparse_count = sparse.count();
            if sparse_count == 0 || sparse_count > count {
                return Err(ImportError::Buffer("sparse accessor count is invalid"));
            }
            let indices = sparse.indices();
            let view = indices.view();
            if view.stride().is_some() || view.target().is_some() {
                return Err(ImportError::Buffer(
                    "sparse index buffer view cannot have byteStride or target",
                ));
            }
            let bytes = buffer_view_data(&view, buffers)?;
            let index_type = indices.index_type();
            let index_size = index_type.size();
            let absolute_offset = view
                .offset()
                .checked_add(indices.offset())
                .ok_or(ImportError::Buffer("sparse index offset overflows"))?;
            if indices.offset() % index_size != 0 || absolute_offset % index_size != 0 {
                return Err(ImportError::Buffer("sparse indices are misaligned"));
            }
            if !accessor_range_fits(
                indices.offset(),
                sparse_count,
                index_size,
                index_size,
                index_size,
                bytes.len(),
            ) {
                return Err(ImportError::Buffer(
                    "sparse accessor indices exceed their buffer view",
                ));
            }
            let end = indices.offset() + sparse_count * index_size;
            let encoded = &bytes[indices.offset()..end];
            let mut previous = None;
            for chunk in encoded.chunks_exact(index_size) {
                let index = match index_type {
                    gltf::accessor::sparse::IndexType::U8 => chunk[0] as u32,
                    gltf::accessor::sparse::IndexType::U16 => {
                        u16::from_le_bytes(chunk.try_into().expect("two-byte index")) as u32
                    }
                    gltf::accessor::sparse::IndexType::U32 => {
                        u32::from_le_bytes(chunk.try_into().expect("four-byte index"))
                    }
                };
                if index as usize >= count || previous.is_some_and(|previous| previous >= index) {
                    return Err(ImportError::Buffer(
                        "sparse indices must increase within the accessor range",
                    ));
                }
                previous = Some(index);
            }
            let values = sparse.values();
            let view = values.view();
            if view.stride().is_some() || view.target().is_some() {
                return Err(ImportError::Buffer(
                    "sparse value buffer view cannot have byteStride or target",
                ));
            }
            let bytes = buffer_view_data(&view, buffers)?;
            let absolute_offset = view
                .offset()
                .checked_add(values.offset())
                .ok_or(ImportError::Buffer("sparse value offset overflows"))?;
            if values.offset() % accessor.data_type().size() != 0
                || absolute_offset % accessor.data_type().size() != 0
                || (matches!(
                    accessor.dimensions(),
                    Dimensions::Mat2 | Dimensions::Mat3 | Dimensions::Mat4
                ) && accessor.data_type().size() < 4
                    && absolute_offset % 4 != 0)
            {
                return Err(ImportError::Buffer("sparse values are misaligned"));
            }
            if !accessor_range_fits(
                values.offset(),
                sparse_count,
                size,
                size,
                last_size,
                bytes.len(),
            ) {
                return Err(ImportError::Buffer(
                    "sparse accessor values exceed their buffer view",
                ));
            }
        }
        validate_accessor_bounds(&accessor, buffers)?;
    }
    validate_animations(document, buffers)?;
    Ok(())
}

fn validate_animations(document: &gltf::Document, buffers: &[Vec<u8>]) -> Result<(), ImportError> {
    let json = document.as_json();
    for animation in &json.animations {
        for sampler in &animation.samplers {
            let input =
                document
                    .accessors()
                    .nth(sampler.input.value())
                    .ok_or(ImportError::Scene(
                        "animation references a missing accessor",
                    ))?;
            if !accessor_format(&input, Dimensions::Scalar, DataType::F32, false) {
                return Err(ImportError::Scene(
                    "animation input must be an unnormalized float SCALAR accessor",
                ));
            }
            let interpolation = match sampler.interpolation.as_ref() {
                gltf::json::validation::Checked::Valid(value) => *value,
                gltf::json::validation::Checked::Invalid => {
                    return Err(ImportError::Scene("animation interpolation is invalid"));
                }
            };
            if interpolation == gltf::animation::Interpolation::CubicSpline && input.count() < 2 {
                return Err(ImportError::Scene(
                    "CUBICSPLINE animation requires at least two keyframes",
                ));
            }
            validate_animation_times(&input, buffers)?;
        }

        let mut targets = HashSet::new();
        for channel in &animation.channels {
            let node_index = channel.target.node.value();
            let node = json
                .nodes
                .get(node_index)
                .ok_or(ImportError::Scene("animation targets a missing node"))?;
            let property = match channel.target.path.as_ref() {
                gltf::json::validation::Checked::Valid(value) => *value,
                gltf::json::validation::Checked::Invalid => {
                    return Err(ImportError::Scene("animation target path is invalid"));
                }
            };
            if !targets.insert((node_index, property as u8)) {
                return Err(ImportError::Scene(
                    "animation contains duplicate channel targets",
                ));
            }
            let sampler = animation
                .samplers
                .get(channel.sampler.value())
                .ok_or(ImportError::Scene("animation references a missing sampler"))?;
            let input =
                document
                    .accessors()
                    .nth(sampler.input.value())
                    .ok_or(ImportError::Scene(
                        "animation references a missing accessor",
                    ))?;
            let output =
                document
                    .accessors()
                    .nth(sampler.output.value())
                    .ok_or(ImportError::Scene(
                        "animation references a missing accessor",
                    ))?;
            let interpolation = match sampler.interpolation.as_ref() {
                gltf::json::validation::Checked::Valid(value) => *value,
                gltf::json::validation::Checked::Invalid => {
                    return Err(ImportError::Scene("animation interpolation is invalid"));
                }
            };
            let multiplier = if interpolation == gltf::animation::Interpolation::CubicSpline {
                3
            } else {
                1
            };
            let target_count =
                match property {
                    gltf::animation::Property::Translation | gltf::animation::Property::Scale => {
                        if !accessor_format(&output, Dimensions::Vec3, DataType::F32, false) {
                            return Err(ImportError::Scene(
                                "translation and scale outputs must be float VEC3 accessors",
                            ));
                        }
                        1
                    }
                    gltf::animation::Property::Rotation => {
                        if !animation_value_accessor(&output, Dimensions::Vec4) {
                            return Err(ImportError::Scene(
                                "rotation output accessor type is invalid",
                            ));
                        }
                        1
                    }
                    gltf::animation::Property::MorphTargetWeights => {
                        if !animation_value_accessor(&output, Dimensions::Scalar) {
                            return Err(ImportError::Scene(
                                "weights output accessor type is invalid",
                            ));
                        }
                        let mesh_index = node.mesh.as_ref().map(|mesh| mesh.value()).ok_or(
                            ImportError::Scene("weights animation target must have morph targets"),
                        )?;
                        let mesh = json
                            .meshes
                            .get(mesh_index)
                            .ok_or(ImportError::Scene("weights animation mesh is missing"))?;
                        let count = mesh
                            .primitives
                            .first()
                            .and_then(|primitive| primitive.targets.as_ref())
                            .map_or(0, Vec::len);
                        if count == 0 {
                            return Err(ImportError::Scene(
                                "weights animation target must have morph targets",
                            ));
                        }
                        count
                    }
                };
            let expected = input
                .count()
                .checked_mul(multiplier)
                .and_then(|count| count.checked_mul(target_count))
                .ok_or(ImportError::Scene("animation output count overflows"))?;
            if output.count() != expected {
                return Err(ImportError::Scene(
                    "animation output count does not match its sampler",
                ));
            }
        }
    }
    Ok(())
}

fn animation_value_accessor(accessor: &gltf::Accessor<'_>, dimensions: Dimensions) -> bool {
    accessor_format(accessor, dimensions, DataType::F32, false)
        || [DataType::I8, DataType::U8, DataType::I16, DataType::U16]
            .into_iter()
            .any(|data_type| accessor_format(accessor, dimensions, data_type, true))
}

fn validate_animation_times(
    accessor: &gltf::Accessor<'_>,
    buffers: &[Vec<u8>],
) -> Result<(), ImportError> {
    let fail = || ImportError::Scene("animation times must be nonnegative and strictly increasing");
    if accessor.view().is_none() {
        let Some(sparse) = accessor.sparse() else {
            return if accessor.count() == 1 {
                Ok(())
            } else {
                Err(fail())
            };
        };
        if sparse.count() < accessor.count() {
            if sparse.count() + 1 != accessor.count() {
                return Err(fail());
            }
            let indices = sparse.indices();
            let index_bytes = buffer_view_data(&indices.view(), buffers)?;
            let values = sparse.values();
            let value_bytes = buffer_view_data(&values.view(), buffers)?;
            let mut previous = 0.0_f32;
            for position in 0..sparse.count() {
                if sparse_index_value(&indices, index_bytes, position)? != position + 1 {
                    return Err(fail());
                }
                let start = values.offset() + position * std::mem::size_of::<f32>();
                let time = f32::from_le_bytes(
                    value_bytes[start..start + 4]
                        .try_into()
                        .expect("validated four-byte timestamp"),
                );
                if !time.is_finite() || time <= previous {
                    return Err(fail());
                }
                previous = time;
            }
            return Ok(());
        }
    }

    let data = |buffer: gltf::Buffer<'_>| buffers.get(buffer.index()).map(Vec::as_slice);
    let times = gltf::accessor::Iter::<f32>::new(accessor.clone(), data).ok_or_else(fail)?;
    let mut previous = None;
    let mut count = 0;
    for time in times {
        if !time.is_finite() || time < 0.0 || previous.is_some_and(|previous| previous >= time) {
            return Err(fail());
        }
        previous = Some(time);
        count += 1;
    }
    if count != accessor.count() {
        return Err(fail());
    }
    Ok(())
}

fn validate_accessor_bounds(
    accessor: &gltf::Accessor<'_>,
    buffers: &[Vec<u8>],
) -> Result<(), ImportError> {
    let components = accessor.dimensions().multiplicity();
    let parse_bound = |value: Option<gltf::json::Value>| {
        value
            .map(|value| {
                let values = value
                    .as_array()
                    .filter(|values| values.len() == components)
                    .ok_or(ImportError::Buffer("accessor bounds have an invalid shape"))?;
                values
                    .iter()
                    .map(|value| {
                        let value = value
                            .as_f64()
                            .ok_or(ImportError::Buffer("accessor bounds contain a non-number"))?;
                        if !value.is_finite() {
                            return Err(ImportError::Buffer(
                                "accessor bounds contain a non-finite value",
                            ));
                        }
                        let value = match accessor.data_type() {
                            DataType::F32 => (value as f32) as f64,
                            data_type => {
                                let (minimum, maximum) = match data_type {
                                    DataType::I8 => (i8::MIN as f64, i8::MAX as f64),
                                    DataType::U8 => (0.0, u8::MAX as f64),
                                    DataType::I16 => (i16::MIN as f64, i16::MAX as f64),
                                    DataType::U16 => (0.0, u16::MAX as f64),
                                    DataType::U32 => (0.0, u32::MAX as f64),
                                    DataType::F32 => unreachable!(),
                                };
                                if value.fract() != 0.0 {
                                    return Err(ImportError::Buffer(
                                        "integer accessor bounds contain a fraction",
                                    ));
                                }
                                if !(minimum..=maximum).contains(&value) {
                                    return Err(ImportError::Buffer(
                                        "integer accessor bounds exceed their component type",
                                    ));
                                }
                                value
                            }
                        };
                        if !value.is_finite() {
                            return Err(ImportError::Buffer(
                                "accessor bounds contain a non-finite value",
                            ));
                        }
                        Ok(value)
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
    };
    let min = parse_bound(accessor.min())?;
    let max = parse_bound(accessor.max())?;
    if min.is_none() && max.is_none() && accessor.data_type() != DataType::F32 {
        return Ok(());
    }

    let Some(view) = accessor.view() else {
        let Some(sparse) = accessor.sparse() else {
            return Ok(());
        };
        return validate_sparse_bounds(
            accessor,
            buffers,
            min.as_deref(),
            max.as_deref(),
            sparse.count() < accessor.count(),
        );
    };

    let data = buffer_view_data(&view, buffers)?;
    let sparse = accessor.sparse();
    let sparse_indices = sparse
        .as_ref()
        .map(|sparse| {
            let indices = sparse.indices();
            let view = indices.view();
            let index_bytes = buffer_view_data(&view, buffers)?;
            Ok::<_, ImportError>((indices, index_bytes, sparse.count()))
        })
        .transpose()?;
    let sparse_values = sparse
        .as_ref()
        .map(|sparse| {
            let values = sparse.values();
            Ok::<_, ImportError>((buffer_view_data(&values.view(), buffers)?, values.offset()))
        })
        .transpose()?;
    let (size, _) = accessor_element_sizes(accessor);
    let stride = view.stride().unwrap_or(size);
    let (mut min_values, mut max_values) = bounds_accumulators(components);
    let mut sparse_position = 0;
    for element in 0..accessor.count() {
        let is_sparse = if let Some((indices, bytes, count)) = &sparse_indices {
            if sparse_position < *count
                && sparse_index_value(indices, bytes, sparse_position)? == element
            {
                sparse_position += 1;
                true
            } else {
                false
            }
        } else {
            false
        };
        let (bytes, offset) = if is_sparse {
            let (bytes, start) = sparse_values.as_ref().expect("sparse value data exists");
            (*bytes, start + (sparse_position - 1) * size)
        } else {
            (data, accessor.offset() + element * stride)
        };
        accumulate_accessor_element(accessor, bytes, offset, &mut min_values, &mut max_values)?;
    }
    check_accessor_bounds(&min_values, &max_values, min.as_deref(), max.as_deref())
}

fn validate_sparse_bounds(
    accessor: &gltf::Accessor<'_>,
    buffers: &[Vec<u8>],
    min: Option<&[f64]>,
    max: Option<&[f64]>,
    has_implicit_zeros: bool,
) -> Result<(), ImportError> {
    let sparse = accessor.sparse().expect("sparse accessor");
    let values = sparse.values();
    let bytes = buffer_view_data(&values.view(), buffers)?;
    let (size, _) = accessor_element_sizes(accessor);
    let mut min_values = vec![f64::INFINITY; accessor.dimensions().multiplicity()];
    let mut max_values = vec![f64::NEG_INFINITY; min_values.len()];
    if has_implicit_zeros {
        min_values.fill(0.0);
        max_values.fill(0.0);
    }
    for element in 0..sparse.count() {
        accumulate_accessor_element(
            accessor,
            bytes,
            values.offset() + element * size,
            &mut min_values,
            &mut max_values,
        )?;
    }
    check_accessor_bounds(&min_values, &max_values, min, max)
}

fn sparse_index_value(
    indices: &gltf::accessor::sparse::Indices<'_>,
    bytes: &[u8],
    position: usize,
) -> Result<usize, ImportError> {
    let offset = indices.offset() + position * indices.index_type().size();
    let bytes = bytes
        .get(offset..offset + indices.index_type().size())
        .ok_or(ImportError::Buffer(
            "sparse accessor indices exceed their buffer view",
        ))?;
    Ok(match indices.index_type() {
        gltf::accessor::sparse::IndexType::U8 => bytes[0] as usize,
        gltf::accessor::sparse::IndexType::U16 => {
            u16::from_le_bytes(bytes.try_into().expect("two-byte index")) as usize
        }
        gltf::accessor::sparse::IndexType::U32 => {
            u32::from_le_bytes(bytes.try_into().expect("four-byte index")) as usize
        }
    })
}

fn accumulate_accessor_element(
    accessor: &gltf::Accessor<'_>,
    bytes: &[u8],
    offset: usize,
    min: &mut [f64],
    max: &mut [f64],
) -> Result<(), ImportError> {
    let component_size = accessor.data_type().size();
    for component in 0..min.len() {
        let start =
            offset + accessor_component_offset(accessor.dimensions(), component_size, component);
        let end = start + component_size;
        let bytes = bytes.get(start..end).ok_or(ImportError::Buffer(
            "accessor bounds exceed their buffer view",
        ))?;
        let value = match accessor.data_type() {
            DataType::I8 => bytes[0] as i8 as f64,
            DataType::U8 => bytes[0] as f64,
            DataType::I16 => i16::from_le_bytes(bytes.try_into().expect("two-byte value")) as f64,
            DataType::U16 => u16::from_le_bytes(bytes.try_into().expect("two-byte value")) as f64,
            DataType::U32 => u32::from_le_bytes(bytes.try_into().expect("four-byte value")) as f64,
            DataType::F32 => f32::from_le_bytes(bytes.try_into().expect("four-byte value")) as f64,
        };
        if !value.is_finite() {
            return Err(ImportError::Buffer(
                "accessor data contains a non-finite value",
            ));
        }
        min[component] = min[component].min(value);
        max[component] = max[component].max(value);
    }
    Ok(())
}

fn accessor_component_offset(dimensions: Dimensions, component_size: usize, index: usize) -> usize {
    let rows = match dimensions {
        Dimensions::Mat2 => 2,
        Dimensions::Mat3 => 3,
        Dimensions::Mat4 => 4,
        _ => return index * component_size,
    };
    let column_stride = (rows * component_size).div_ceil(4) * 4;
    (index / rows) * column_stride + (index % rows) * component_size
}

fn bounds_accumulators(components: usize) -> (Vec<f64>, Vec<f64>) {
    (
        vec![f64::INFINITY; components],
        vec![f64::NEG_INFINITY; components],
    )
}

fn check_accessor_bounds(
    actual_min: &[f64],
    actual_max: &[f64],
    expected_min: Option<&[f64]>,
    expected_max: Option<&[f64]>,
) -> Result<(), ImportError> {
    if expected_min.is_some_and(|values| values != actual_min)
        || expected_max.is_some_and(|values| values != actual_max)
    {
        return Err(ImportError::Buffer("accessor bounds do not match its data"));
    }
    Ok(())
}

fn validate_vertex_accessor_layout(document: &gltf::Document) -> Result<(), ImportError> {
    let mut vertex_accessors = HashSet::new();
    let mut accessor_kinds = HashMap::<usize, BufferViewKind>::new();
    let mut view_kinds = HashMap::<usize, BufferViewKind>::new();
    let mut vertex_view_accessors = HashMap::<usize, (usize, bool)>::new();
    let mut register = |accessor: gltf::Accessor<'_>| -> Result<(), ImportError> {
        if !register_kind(
            &mut accessor_kinds,
            accessor.index(),
            BufferViewKind::VertexAttribute,
        ) {
            return Err(ImportError::Buffer(
                "accessor is used for incompatible glTF data kinds",
            ));
        }
        if vertex_accessors.insert(accessor.index())
            && let Some(view) = accessor.view()
        {
            let owner = vertex_view_accessors
                .entry(view.index())
                .or_insert((accessor.index(), false));
            owner.1 |= owner.0 != accessor.index();
            if !register_kind(
                &mut view_kinds,
                view.index(),
                BufferViewKind::VertexAttribute,
            ) {
                return Err(ImportError::Buffer(
                    "buffer view contains multiple glTF data kinds",
                ));
            }
            let (size, _) = accessor_element_sizes(&accessor);
            if accessor.offset() % 4 != 0
                || view.stride().is_some_and(|stride| stride % 4 != 0)
                || (view.stride().is_none() && size % 4 != 0)
            {
                return Err(ImportError::Buffer(
                    "vertex attribute accessor is not four-byte aligned",
                ));
            }
        }
        Ok(())
    };
    for mesh in document.meshes() {
        for primitive in mesh.primitives() {
            for (_, accessor) in primitive.attributes() {
                register(accessor)?;
            }
            for target in primitive.morph_targets() {
                for accessor in [target.positions(), target.normals(), target.tangents()]
                    .into_iter()
                    .flatten()
                {
                    register(accessor)?;
                }
            }
        }
    }

    for mesh in document.meshes() {
        for primitive in mesh.primitives() {
            if let Some(accessor) = primitive.indices() {
                if !register_kind(&mut accessor_kinds, accessor.index(), BufferViewKind::Index) {
                    return Err(ImportError::Buffer(
                        "accessor is used for incompatible glTF data kinds",
                    ));
                }
                if let Some(view) = accessor.view()
                    && !register_kind(&mut view_kinds, view.index(), BufferViewKind::Index)
                {
                    return Err(ImportError::Buffer(
                        "buffer view contains multiple glTF data kinds",
                    ));
                }
            }
        }
    }
    for skin in document.skins() {
        if let Some(accessor) = skin.inverse_bind_matrices() {
            if !register_kind(
                &mut accessor_kinds,
                accessor.index(),
                BufferViewKind::InverseBindMatrices,
            ) {
                return Err(ImportError::Buffer(
                    "accessor is used for incompatible glTF data kinds",
                ));
            }
            if let Some(view) = accessor.view()
                && !register_kind(
                    &mut view_kinds,
                    view.index(),
                    BufferViewKind::InverseBindMatrices,
                )
            {
                return Err(ImportError::Buffer(
                    "buffer view contains multiple glTF data kinds",
                ));
            }
        }
    }

    for accessor in document.accessors() {
        let kind = accessor_kinds
            .get(&accessor.index())
            .copied()
            .unwrap_or(BufferViewKind::Other);
        if let Some(view) = accessor.view() {
            if !register_kind(&mut view_kinds, view.index(), kind) {
                return Err(ImportError::Buffer(
                    "buffer view contains multiple glTF data kinds",
                ));
            }
            if kind != BufferViewKind::VertexAttribute && view.stride().is_some() {
                return Err(ImportError::Buffer(
                    "non-vertex buffer view defines byteStride",
                ));
            }
        }
        if let Some(sparse) = accessor.sparse() {
            if !register_kind(&mut view_kinds, sparse.values().view().index(), kind) {
                return Err(ImportError::Buffer(
                    "buffer view contains multiple glTF data kinds",
                ));
            }
        }
    }
    let image_views: HashSet<_> = document
        .as_json()
        .images
        .iter()
        .filter_map(|image| image.buffer_view.as_ref().map(|view| view.value()))
        .collect();
    for (index, view) in document.views().enumerate() {
        if image_views.contains(&index) {
            if view.stride().is_some() {
                return Err(ImportError::Buffer(
                    "image buffer view cannot define byteStride",
                ));
            }
            if !register_kind(&mut view_kinds, index, BufferViewKind::Image) {
                return Err(ImportError::Buffer(
                    "buffer view contains multiple glTF data kinds",
                ));
            }
        }
    }
    if document.accessors().any(|accessor| {
        accessor.sparse().is_some_and(|sparse| {
            view_kinds
                .get(&sparse.indices().view().index())
                .is_some_and(|kind| *kind != BufferViewKind::Other)
        })
    }) {
        return Err(ImportError::Buffer(
            "buffer view contains multiple glTF data kinds",
        ));
    }
    for view in document.views() {
        if view.stride().is_some()
            && view_kinds
                .get(&view.index())
                .is_some_and(|kind| *kind != BufferViewKind::VertexAttribute)
        {
            return Err(ImportError::Buffer(
                "non-vertex buffer view defines byteStride",
            ));
        }
        if vertex_view_accessors
            .get(&view.index())
            .is_some_and(|(_, shared)| *shared)
            && view.stride().is_none()
        {
            return Err(ImportError::Buffer(
                "buffer view shared by accessors requires byteStride",
            ));
        }
    }
    Ok(())
}

fn buffer_view_data<'a>(
    view: &gltf::buffer::View<'_>,
    buffers: &'a [Vec<u8>],
) -> Result<&'a [u8], ImportError> {
    let buffer = buffers
        .get(view.buffer().index())
        .ok_or(ImportError::Buffer(
            "buffer view references a missing buffer",
        ))?;
    let end = view
        .offset()
        .checked_add(view.length())
        .filter(|&end| end <= buffer.len())
        .ok_or(ImportError::Buffer("buffer view exceeds its buffer"))?;
    Ok(&buffer[view.offset()..end])
}

fn accessor_range_fits(
    offset: usize,
    count: usize,
    stride: usize,
    size: usize,
    last_size: usize,
    length: usize,
) -> bool {
    count != 0
        && stride >= size
        && count
            .checked_sub(1)
            .and_then(|last| last.checked_mul(stride))
            .and_then(|last| offset.checked_add(last))
            .and_then(|end| end.checked_add(last_size))
            .is_some_and(|end| end <= length)
}

fn accessor_element_sizes(accessor: &gltf::Accessor<'_>) -> (usize, usize) {
    let component = accessor.data_type().size();
    let (columns, rows) = match accessor.dimensions() {
        Dimensions::Mat2 => (2, 2),
        Dimensions::Mat3 => (3, 3),
        Dimensions::Mat4 => (4, 4),
        dimensions => {
            let size = component * dimensions.multiplicity();
            return (size, size);
        }
    };
    let column_stride = (rows * component).div_ceil(4) * 4;
    (
        columns * column_stride,
        (columns - 1) * column_stride + rows * component,
    )
}

fn import_buffers(
    document: &gltf::Document,
    base: Option<&Path>,
    mut blob: Option<Vec<u8>>,
) -> Result<Vec<Vec<u8>>, ImportError> {
    document
        .buffers()
        .map(|buffer| {
            let expected = buffer.length();
            if expected == 0 {
                return Err(ImportError::Buffer("buffer byteLength must be positive"));
            }
            if expected > MAX_BUFFER_BYTES {
                return Err(ImportError::Buffer("buffer exceeds the 96 MiB limit"));
            }
            let data = match buffer.source() {
                gltf::buffer::Source::Bin => {
                    let mut data = blob
                        .take()
                        .ok_or(ImportError::Buffer("GLB binary chunk is missing"))?;
                    if data.len() < expected || data.len() > expected + 3 {
                        return Err(ImportError::Buffer(
                            "GLB binary chunk does not match its declared length",
                        ));
                    }
                    data.truncate(expected);
                    data
                }
                gltf::buffer::Source::Uri(uri) if is_data_uri(uri) => {
                    let source = parse_data_uri(uri).map_err(ImportError::Buffer)?;
                    if !source.base64
                        || !source.media_type.is_some_and(|mime| {
                            mime.eq_ignore_ascii_case("application/octet-stream")
                                || mime.eq_ignore_ascii_case("application/gltf-buffer")
                        })
                    {
                        return Err(ImportError::Buffer(
                            "buffer data URI must use base64 application/octet-stream or application/gltf-buffer",
                        ));
                    }
                    let mut data = decode_data_uri(source, MAX_BUFFER_BYTES)
                        .map_err(ImportError::Buffer)?;
                    if data.len() < expected || data.len() > MAX_BUFFER_BYTES {
                        return Err(ImportError::Buffer(
                            "buffer URI does not match its declared length or limit",
                        ));
                    }
                    data.truncate(expected);
                    data
                }
                gltf::buffer::Source::Uri(uri) => {
                    let base = base.ok_or(ImportError::Buffer(
                        "import_slice requires embedded buffers",
                    ))?;
                    let path = resource_path(uri, base).map_err(ImportError::Buffer)?;
                    let mut file = File::open(path).map_err(gltf::Error::Io)?;
                    if file.metadata().map_err(gltf::Error::Io)?.len() > MAX_BUFFER_BYTES as u64 {
                        return Err(ImportError::Buffer("buffer exceeds the 96 MiB limit"));
                    }
                    let mut data = Vec::new();
                    file.by_ref()
                        .take(MAX_BUFFER_BYTES as u64 + 1)
                        .read_to_end(&mut data)
                        .map_err(gltf::Error::Io)?;
                    if data.len() < expected || data.len() > MAX_BUFFER_BYTES {
                        return Err(ImportError::Buffer(
                            "buffer file does not match its declared length or 96 MiB limit",
                        ));
                    }
                    data.truncate(expected);
                    data
                }
            };
            Ok(data)
        })
        .collect()
}

fn import_image(
    index: usize,
    color_space: ImageColorSpace,
    source: gltf::image::Source<'_>,
    base: Option<&Path>,
    buffers: &[Vec<u8>],
) -> Result<GltfImage, ImportError> {
    let (encoded, mime_type) = image_source(source, base, buffers)?;
    let format = match mime_type {
        Some("image/png") => ImageFormat::Png,
        Some("image/jpeg") => ImageFormat::Jpeg,
        Some(_) => return Err(ImportError::Image("only PNG and JPEG images are supported")),
        None => image::guess_format(&encoded)?,
    };
    let decoder =
        ImageReader::with_format(std::io::Cursor::new(encoded.as_ref()), format).into_decoder()?;
    let (width, height) = decoder.dimensions();
    if !rgba8_size_within_limit(width, height) {
        return Err(ImportError::Image("decoded image exceeds the 64 MiB limit"));
    }
    if !matches!(
        decoder.color_type(),
        ColorType::L8 | ColorType::La8 | ColorType::Rgb8 | ColorType::Rgba8
    ) {
        return Err(ImportError::Image(
            "only 8-bit PNG/JPEG images are supported",
        ));
    }
    drop(decoder);
    let extent = Extent2d::new(width, height).ok_or(ImportError::Image("image extent is empty"))?;
    let encoded = std::sync::Arc::from(encoded.into_owned());
    Ok(GltfImage {
        image_index: index,
        color_space,
        extent,
        format,
        encoded,
    })
}

fn decode_rgba8(
    encoded: &[u8],
    format: ImageFormat,
    extent: Extent2d,
) -> Result<Vec<u8>, ImportError> {
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_UPLOAD_BYTES as u64);
    let mut reader = ImageReader::with_format(std::io::Cursor::new(encoded), format);
    reader.limits(limits);
    let image = reader.decode()?;
    let (width, height) = image.dimensions();
    if (width, height) != (extent.width(), extent.height()) {
        return Err(ImportError::Image(
            "decoded dimensions changed after inspection",
        ));
    }
    let channels = match image.color() {
        ColorType::L8 => 1,
        ColorType::La8 => 2,
        ColorType::Rgb8 => 3,
        ColorType::Rgba8 => 4,
        _ => {
            return Err(ImportError::Image(
                "only 8-bit PNG/JPEG images are supported",
            ));
        }
    };
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or(ImportError::Image("image dimensions overflow"))?;
    let byte_len = pixels
        .checked_mul(4)
        .ok_or(ImportError::Image("image dimensions overflow"))?;
    let source_pixels = image.into_bytes();
    if source_pixels.len() != pixels * channels {
        return Err(ImportError::Image(
            "decoded byte count does not match extent",
        ));
    }
    let rgba8: Vec<u8> = match channels {
        1 => source_pixels
            .into_iter()
            .flat_map(|gray| [gray, gray, gray, 255])
            .collect(),
        2 => source_pixels
            .chunks_exact(2)
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
            .collect(),
        3 => source_pixels
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        4 => source_pixels,
        _ => unreachable!(),
    };
    if rgba8.len() != byte_len {
        return Err(ImportError::Image(
            "decoded byte count does not match extent",
        ));
    }
    Ok(rgba8)
}

#[derive(Clone, Copy)]
struct DataUri<'a> {
    media_type: Option<&'a str>,
    payload: &'a str,
    base64: bool,
}

fn is_data_uri(uri: &str) -> bool {
    uri.get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

fn parse_data_uri(uri: &str) -> Result<DataUri<'_>, &'static str> {
    let Some((scheme, rest)) = uri.split_once(':') else {
        return Err("invalid data URI");
    };
    if !scheme.eq_ignore_ascii_case("data") {
        return Err("invalid data URI");
    }
    let (header, payload) = rest.split_once(',').ok_or("invalid data URI")?;
    let base64 = header
        .rsplit_once(';')
        .is_some_and(|(_, marker)| marker.eq_ignore_ascii_case("base64"));
    let header = if base64 {
        &header[..header.len() - ";base64".len()]
    } else {
        header
    };
    if !valid_data_uri_header(header) {
        return Err("invalid data URI media type");
    }
    let mut parts = header.split(';');
    let media_type = parts.next().filter(|value| !value.is_empty());
    if media_type.is_some_and(|value| {
        let Some((kind, subtype)) = value.split_once('/') else {
            return true;
        };
        value.matches('/').count() != 1 || !mime_token(kind) || !mime_token(subtype)
    }) || parts.any(|parameter| {
        parameter
            .split_once('=')
            .is_none_or(|(name, value)| !mime_token(name) || value.is_empty())
    }) {
        return Err("invalid data URI media type");
    }
    Ok(DataUri {
        media_type,
        payload,
        base64,
    })
}

fn valid_data_uri_header(header: &str) -> bool {
    let bytes = header.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            if bytes
                .get(index + 1..index + 3)
                .is_none_or(|escape| escape.iter().any(|byte| hex_value(*byte).is_none()))
            {
                return false;
            }
            index += 3;
        } else if mime_token_byte(byte) || matches!(byte, b'/' | b';' | b'=') {
            index += 1;
        } else {
            return false;
        }
    }
    true
}

fn mime_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(mime_token_byte)
}

fn mime_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'..=b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_data_uri(source: DataUri<'_>, limit: usize) -> Result<Vec<u8>, &'static str> {
    let encoded_limit = if source.base64 {
        limit
            .div_ceil(3)
            .checked_mul(4)
            .ok_or("data URI payload exceeds its size limit")?
    } else {
        limit
    };
    if !valid_percent_escapes(source.payload.as_bytes()) {
        return Err("invalid percent escape in data URI");
    }
    let payload = urlencoding::decode_binary(source.payload.as_bytes());
    if payload.len() > encoded_limit {
        return Err("data URI payload exceeds its size limit");
    }
    let bytes = if source.base64 {
        let encoded = std::str::from_utf8(&payload).map_err(|_| "invalid base64 data URI")?;
        base64::decode(encoded).map_err(|_| "invalid base64 data URI")?
    } else {
        payload.into_owned()
    };
    if bytes.len() > limit {
        return Err("data URI payload exceeds its size limit");
    }
    Ok(bytes)
}

fn valid_percent_escapes(input: &[u8]) -> bool {
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            let escape = input
                .get(index + 1..index + 3)
                .is_some_and(|escape| escape.iter().all(|byte| hex_value(*byte).is_some()));
            if !escape {
                return false;
            }
            index += 3;
        } else {
            if !input[index].is_ascii_alphanumeric()
                && !b"-._~:/?#[]@!$&'()*+,;=".contains(&input[index])
            {
                return false;
            }
            index += 1;
        }
    }
    true
}

fn image_source<'a>(
    source: gltf::image::Source<'a>,
    base: Option<&Path>,
    buffers: &'a [Vec<u8>],
) -> Result<(Cow<'a, [u8]>, Option<&'a str>), ImportError> {
    match source {
        gltf::image::Source::View { view, mime_type } => {
            let start = view.offset();
            let end = start
                .checked_add(view.length())
                .ok_or(ImportError::Image("image buffer view range overflows"))?;
            let bytes = buffers
                .get(view.buffer().index())
                .and_then(|buffer| buffer.get(start..end))
                .ok_or(ImportError::Image("image buffer view is out of range"))?;
            if bytes.len() > MAX_IMAGE_SOURCE_BYTES {
                return Err(ImportError::Image("encoded image exceeds the 64 MiB limit"));
            }
            Ok((Cow::Borrowed(bytes), Some(mime_type)))
        }
        gltf::image::Source::Uri { uri, mime_type } if is_data_uri(uri) => {
            let source = parse_data_uri(uri).map_err(ImportError::Image)?;
            let format = match source.media_type {
                Some(mime) if mime.eq_ignore_ascii_case("image/png") => "image/png",
                Some(mime) if mime.eq_ignore_ascii_case("image/jpeg") => "image/jpeg",
                _ => return Err(ImportError::Image("only PNG and JPEG images are supported")),
            };
            if mime_type.is_some_and(|declared| !declared.eq_ignore_ascii_case(format)) {
                return Err(ImportError::Image(
                    "image MIME type does not match its data URI",
                ));
            }
            let data =
                decode_data_uri(source, MAX_IMAGE_SOURCE_BYTES).map_err(ImportError::Image)?;
            Ok((Cow::Owned(data), Some(format)))
        }
        gltf::image::Source::Uri { uri, mime_type } => {
            let base = base.ok_or(ImportError::Image("import_slice requires embedded images"))?;
            let path = resource_path(uri, base).map_err(ImportError::Image)?;
            let file = File::open(path).map_err(gltf::Error::Io)?;
            if file.metadata().map_err(gltf::Error::Io)?.len() > MAX_IMAGE_SOURCE_BYTES as u64 {
                return Err(ImportError::Image("encoded image exceeds the 64 MiB limit"));
            }
            let mut bytes = Vec::new();
            file.take(MAX_IMAGE_SOURCE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(gltf::Error::Io)?;
            if bytes.len() > MAX_IMAGE_SOURCE_BYTES {
                return Err(ImportError::Image("encoded image exceeds the 64 MiB limit"));
            }
            Ok((Cow::Owned(bytes), mime_type))
        }
    }
}

fn resource_path(uri: &str, base: &Path) -> Result<PathBuf, &'static str> {
    let (absolute, uri_path) = if uri
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
    {
        let rest = &uri[5..];
        if let Some(authority_path) = rest.strip_prefix("//") {
            let (authority, path) = authority_path
                .split_once('/')
                .ok_or("file URI must name a local path")?;
            if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
                return Err("remote file URI authorities are unsupported");
            }
            (true, path)
        } else if let Some(path) = rest.strip_prefix('/') {
            (true, path)
        } else {
            return Err("file URI must use an absolute path");
        }
    } else {
        let first_segment = uri.split('/').next().unwrap_or_default();
        if first_segment.contains(':') {
            return Err("unsupported resource URI scheme");
        }
        (false, uri)
    };
    if uri_path.is_empty()
        || uri_path.contains('?')
        || uri_path.contains('#')
        || (!absolute && uri_path.starts_with('/'))
    {
        return Err("resource URI must contain a relative path without query or fragment");
    }

    let mut segments = Vec::new();
    for (index, segment) in uri_path.split('/').enumerate() {
        if !valid_uri_path_segment(segment, absolute)
            || (index == 0 && !absolute && segment.contains(':'))
        {
            return Err("invalid resource URI path");
        }
        let decoded =
            urlencoding::decode(segment).map_err(|_| "invalid URL-encoded resource path")?;
        if decoded.contains('/')
            || decoded.contains('\0')
            || (cfg!(windows) && decoded.contains('\\'))
        {
            return Err("encoded path separators are not supported");
        }
        if cfg!(windows)
            && decoded.contains(':')
            && !(absolute
                && index == 0
                && decoded.len() == 2
                && decoded.as_bytes()[0].is_ascii_alphabetic())
        {
            return Err("resource URI path cannot start with a drive or scheme");
        }
        segments.push(decoded.into_owned());
    }
    let decoded_path = segments.join("/");
    if absolute {
        let local_path = if cfg!(windows) {
            if decoded_path
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
                && decoded_path.as_bytes().get(1) == Some(&b':')
            {
                if decoded_path.len() == 2 {
                    format!("{decoded_path}/")
                } else {
                    decoded_path
                }
            } else {
                format!("/{decoded_path}")
            }
        } else {
            return Ok(PathBuf::from(format!("/{decoded_path}")));
        };
        Ok(PathBuf::from(local_path))
    } else {
        Ok(base.join(decoded_path))
    }
}

fn valid_uri_path_segment(segment: &str, absolute: bool) -> bool {
    let bytes = segment.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            if bytes
                .get(index + 1..index + 3)
                .is_none_or(|escape| escape.iter().any(|byte| hex_value(*byte).is_none()))
            {
                return false;
            }
            let escaped =
                hex_value(bytes[index + 1]).unwrap() * 16 + hex_value(bytes[index + 2]).unwrap();
            if escaped == b'/' || cfg!(windows) && escaped == b'\\' {
                return false;
            }
            index += 3;
        } else if byte >= 0x80
            || byte.is_ascii_alphanumeric()
            || b"-._~".contains(&byte)
            || (absolute && b"!$&'()*+,;=:@".contains(&byte))
        {
            index += 1;
        } else {
            return false;
        }
    }
    true
}

fn rgba8_size_within_limit(width: u32, height: u32) -> bool {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .is_some_and(|bytes| bytes > 0 && bytes <= MAX_IMAGE_UPLOAD_BYTES)
}

fn referenced_images(document: &gltf::Document) -> HashSet<(usize, ImageColorSpace)> {
    let mut images = HashSet::new();
    for material in document.materials() {
        let pbr = material.pbr_metallic_roughness();
        images.extend(
            [pbr.base_color_texture(), material.emissive_texture()]
                .into_iter()
                .flatten()
                .map(|info| (info.texture().source().index(), ImageColorSpace::Srgb)),
        );
        images.extend(
            [
                pbr.metallic_roughness_texture(),
                material
                    .transmission()
                    .and_then(|transmission| transmission.transmission_texture()),
            ]
            .into_iter()
            .flatten()
            .map(|info| (info.texture().source().index(), ImageColorSpace::Linear)),
        );
        if let Some(info) = material.normal_texture() {
            images.insert((info.texture().source().index(), ImageColorSpace::Linear));
        }
        if let Some(info) = material.occlusion_texture() {
            images.insert((info.texture().source().index(), ImageColorSpace::Linear));
        }
    }
    images
}

fn visit_scene_nodes<'a>(
    document: &'a gltf::Document,
    hierarchy: &NodeHierarchy,
    mut visit: impl FnMut(gltf::Node<'a>, Mat4) -> Result<(), ImportError>,
) -> Result<Vec<Option<Mat4>>, ImportError> {
    let scene = document
        .default_scene()
        .or_else(|| document.scenes().next())
        .ok_or(ImportError::Scene("document contains no scene"))?;
    let mut roots: Vec<_> = scene.nodes().collect();
    roots.reverse();
    let mut stack: Vec<_> = roots
        .into_iter()
        .map(|node| (node, Mat4::IDENTITY, 0))
        .collect();
    let mut transforms = vec![None; hierarchy.parents.len()];
    let mut visited = 0;
    while let Some((node, parent, depth)) = stack.pop() {
        if depth > MAX_SCENE_DEPTH || visited == MAX_SCENE_NODES {
            return Err(ImportError::Scene("scene exceeds its node or depth limit"));
        }
        visited += 1;
        let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        if !world.is_finite() || !world.determinant().is_finite() {
            return Err(ImportError::Scene("node transform is non-finite"));
        }
        if transforms[node.index()].replace(world).is_some() {
            return Err(ImportError::Scene(
                "node appears more than once in the scene",
            ));
        }
        let mut children: Vec<_> = node.children().collect();
        children.reverse();
        visit(node, world)?;
        if stack.len() + children.len() + visited > MAX_SCENE_NODES {
            return Err(ImportError::Scene("scene exceeds its node limit"));
        }
        stack.extend(children.into_iter().map(|child| (child, world, depth + 1)));
    }
    Ok(transforms)
}

impl GltfAsset {
    /// Converts each triangle primitive to renderer data and retains core morph/skin inputs.
    pub fn primitives(&self) -> Result<Vec<GltfPrimitive>, ImportError> {
        let mut primitives = Vec::new();
        for mesh in self.document.meshes() {
            mesh_morph_target_count(&mesh)?;
            for primitive in mesh.primitives() {
                primitives.push(convert_primitive(&self.buffers, mesh.index(), primitive)?);
            }
        }
        Ok(primitives)
    }

    /// Expands the default scene (or first scene) into bounded mesh instances.
    pub fn instances(&self) -> Result<Vec<GltfInstance>, ImportError> {
        let hierarchy = NodeHierarchy::from_document(&self.document)?;
        let skin_roots = validate_skin_hierarchies(&self.document, &hierarchy)?;
        validate_skin_scene_membership(&self.document, &hierarchy, &skin_roots)?;
        let mut pending = Vec::new();
        let transforms = visit_scene_nodes(&self.document, &hierarchy, |node, world| {
            if let Some(mesh) = node.mesh() {
                let skin = node.skin();
                let target_count = mesh_morph_target_count(&mesh)?;
                let morph_weights = node.weights().or_else(|| mesh.weights());
                if morph_weights.is_some_and(|weights| {
                    weights.len() != target_count
                        || weights.iter().any(|weight| !weight.is_finite())
                }) {
                    return Err(ImportError::Scene(
                        "node morph weights do not match its mesh targets",
                    ));
                }
                pending.push((
                    node.index(),
                    mesh.index(),
                    world,
                    morph_weights.unwrap_or(&[]).to_vec(),
                    skin,
                ));
            }
            Ok(())
        })?;
        let mut skin_cache: HashMap<usize, Arc<[Mat4]>> = HashMap::new();
        pending
            .into_iter()
            .map(|(node_index, mesh_index, world, morph_weights, skin)| {
                let skin_matrices = match skin {
                    Some(skin) => {
                        if let Some(matrices) = skin_cache.get(&skin.index()) {
                            Arc::clone(matrices)
                        } else {
                            let joint_count = skin.joints().len();
                            if joint_count > MAX_SCENE_NODES {
                                return Err(ImportError::Scene(
                                    "skin exceeds the 65,536 joint limit",
                                ));
                            }
                            let joints: Vec<_> = skin.joints().collect();
                            let buffer = |buffer: gltf::Buffer<'_>| {
                                self.buffers.get(buffer.index()).map(Vec::as_slice)
                            };
                            let inverse_bind: Vec<_> = match skin.inverse_bind_matrices() {
                                None => vec![Mat4::IDENTITY.to_cols_array_2d(); joint_count],
                                Some(accessor) => {
                                    if !accessor_format(
                                        &accessor,
                                        Dimensions::Mat4,
                                        DataType::F32,
                                        false,
                                    ) || accessor.count() < joint_count
                                    {
                                        return Err(ImportError::Scene(
                                            "inverse bind accessor must be float MAT4 per joint",
                                        ));
                                    }
                                    skin.reader(buffer)
                                        .read_inverse_bind_matrices()
                                        .ok_or(ImportError::Scene(
                                            "inverse bind accessor data is unavailable",
                                        ))?
                                        .take(joint_count)
                                        .collect::<Vec<_>>()
                                }
                            };
                            if inverse_bind.len() != joint_count {
                                return Err(ImportError::Scene(
                                    "inverse bind accessor has fewer matrices than skin joints",
                                ));
                            }
                            let matrices = joints
                                .iter()
                                .zip(inverse_bind)
                                .map(|(joint, inverse_bind)| {
                                    if inverse_bind
                                        .iter()
                                        .flatten()
                                        .any(|value| !value.is_finite())
                                    {
                                        return Err(ImportError::Scene(
                                            "skin inverse bind matrix contains a non-finite value",
                                        ));
                                    }
                                    if [
                                        inverse_bind[0][3],
                                        inverse_bind[1][3],
                                        inverse_bind[2][3],
                                        inverse_bind[3][3],
                                    ] != [0.0, 0.0, 0.0, 1.0]
                                    {
                                        return Err(ImportError::Scene(
                                            "skin inverse bind matrix is not affine",
                                        ));
                                    }
                                    let global = transforms
                                        .get(joint.index())
                                        .and_then(|transform| *transform)
                                        .ok_or(ImportError::Scene(
                                            "skin joint is absent from the selected scene",
                                        ))?;
                                    let matrix = global * Mat4::from_cols_array_2d(&inverse_bind);
                                    if !matrix.is_finite() {
                                        return Err(ImportError::Scene(
                                            "skin matrix contains a non-finite value",
                                        ));
                                    }
                                    Ok(matrix)
                                })
                                .collect::<Result<Vec<_>, ImportError>>()?;
                            let matrices: Arc<[Mat4]> = matrices.into();
                            skin_cache.insert(skin.index(), Arc::clone(&matrices));
                            matrices
                        }
                    }
                    None => Arc::from([]),
                };
                Ok(GltfInstance {
                    node_index,
                    mesh_index,
                    world_transform: world.to_cols_array_2d(),
                    morph_weights,
                    skin_matrices,
                })
            })
            .collect()
    }

    /// Expands point, spot, and directional lights from the selected scene.
    pub fn scene_lights(&self) -> Result<Vec<GltfSceneLight>, ImportError> {
        let hierarchy = NodeHierarchy::from_document(&self.document)?;
        let mut lights = Vec::new();
        let mut directional_count = 0;
        let mut punctual_count = 0;
        visit_scene_nodes(&self.document, &hierarchy, |node, world| {
            let Some(source) = node.light() else {
                return Ok(());
            };
            let position = world.transform_point3(Vec3::ZERO);
            if !position.is_finite() {
                return Err(ImportError::Scene("light position is non-finite"));
            }
            let color = source.color();
            let intensity = source.intensity();
            let range = source.range();
            let light = match source.kind() {
                gltf::khr_lights_punctual::Kind::Directional => {
                    directional_count += 1;
                    if directional_count > MAX_DIRECTIONAL_LIGHTS {
                        return Err(ImportError::Scene(
                            "scene exceeds the 16 directional light limit",
                        ));
                    }
                    let emission_direction =
                        world
                            .transform_vector3(Vec3::NEG_Z)
                            .try_normalize()
                            .ok_or(ImportError::Scene("directional light direction is invalid"))?;
                    GltfSceneLight::Directional {
                        node_index: node.index(),
                        light: DirectionalLight {
                            direction_to_light: (-emission_direction).to_array(),
                            radiance: color.map(|channel| channel * intensity),
                            shadow: None,
                        },
                    }
                }
                gltf::khr_lights_punctual::Kind::Point => {
                    punctual_count += 1;
                    if punctual_count > MAX_PUNCTUAL_LIGHTS {
                        return Err(ImportError::Scene(
                            "scene exceeds the 16 point and spot light limit",
                        ));
                    }
                    GltfSceneLight::Punctual {
                        node_index: node.index(),
                        light: PunctualLight::Point {
                            position: position.to_array(),
                            color,
                            intensity,
                            range,
                        },
                    }
                }
                gltf::khr_lights_punctual::Kind::Spot {
                    inner_cone_angle,
                    outer_cone_angle,
                } => {
                    punctual_count += 1;
                    if punctual_count > MAX_PUNCTUAL_LIGHTS {
                        return Err(ImportError::Scene(
                            "scene exceeds the 16 point and spot light limit",
                        ));
                    }
                    let direction = world
                        .transform_vector3(Vec3::NEG_Z)
                        .try_normalize()
                        .ok_or(ImportError::Scene("spot light direction is invalid"))?
                        .to_array();
                    GltfSceneLight::Punctual {
                        node_index: node.index(),
                        light: PunctualLight::Spot {
                            position: position.to_array(),
                            direction,
                            color,
                            intensity,
                            range,
                            inner_cone_angle,
                            outer_cone_angle,
                        },
                    }
                }
            };
            lights.push(light);
            Ok(())
        })?;
        Ok(lights)
    }

    /// Expands static default poses into renderer-ready mesh instances.
    pub fn scene_primitives(&self) -> Result<Vec<GltfScenePrimitive>, ImportError> {
        let primitives = self.primitives()?;
        let mut scene_primitives = Vec::new();
        for instance in self.instances()? {
            for primitive in primitives
                .iter()
                .filter(|primitive| primitive.mesh_index == instance.mesh_index)
            {
                let skinned =
                    primitive.skin_weights.is_some() && !instance.skin_matrices.is_empty();
                if !instance.skin_matrices.is_empty() && !skinned {
                    return Err(ImportError::Primitive {
                        mesh: primitive.mesh_index,
                        primitive: primitive.primitive_index,
                        reason: "skinned nodes require JOINTS_0 and WEIGHTS_0",
                    });
                }
                if !skinned
                    && Mat4::from_cols_array_2d(&instance.world_transform).determinant() == 0.0
                {
                    return Err(ImportError::Scene(
                        "singular mesh transforms are unsupported by the renderer",
                    ));
                }
                let mut mesh = deform_primitive(primitive, &instance, skinned)?;
                if primitive.material.normal_texture.is_some() && !primitive.has_tangents {
                    mesh = generate_tangents(mesh)?;
                }
                scene_primitives.push(GltfScenePrimitive {
                    node_index: instance.node_index,
                    mesh_index: primitive.mesh_index,
                    primitive_index: primitive.primitive_index,
                    material_index: primitive.material_index,
                    model_transform: if skinned {
                        Mat4::IDENTITY.to_cols_array_2d()
                    } else {
                        instance.world_transform
                    },
                    mesh,
                    material: primitive.material,
                });
            }
        }
        Ok(scene_primitives)
    }
}

fn deform_primitive(
    primitive: &GltfPrimitive,
    instance: &GltfInstance,
    apply_skin: bool,
) -> Result<MeshData, ImportError> {
    if !primitive.morph_targets.is_empty()
        && !instance.morph_weights.is_empty()
        && instance.morph_weights.len() != primitive.morph_targets.len()
    {
        return Err(ImportError::Scene(
            "morph weight count does not match mesh targets",
        ));
    }
    if !apply_skin
        && !primitive.flat_normals
        && (primitive.morph_targets.is_empty()
            || instance.morph_weights.iter().all(|weight| *weight == 0.0))
    {
        return Ok(primitive.mesh.clone());
    }

    let mut vertices = primitive.mesh.vertices().to_vec();
    let mut tangent_deltas = (!primitive.flat_normals
        && primitive
            .morph_targets
            .iter()
            .any(|target| target.tangents.is_some()))
    .then(|| vec![Vec3::ZERO; vertices.len()]);
    for (weight, target) in instance.morph_weights.iter().zip(&primitive.morph_targets) {
        if !weight.is_finite() {
            return Err(ImportError::Scene("morph weight is non-finite"));
        }
        for (index, vertex) in vertices.iter_mut().enumerate() {
            if let Some(positions) = &target.positions {
                vertex.position = add_scaled(vertex.position, positions[index], *weight);
            }
            if let Some(normals) = &target.normals {
                vertex.normal = add_scaled(vertex.normal, normals[index], *weight);
            }
            if let (Some(total), Some(deltas)) = (&mut tangent_deltas, &target.tangents) {
                total[index] += Vec3::from_array(deltas[index]) * *weight;
            }
        }
    }
    if primitive.flat_normals {
        set_flat_normals(&mut vertices, primitive.mesh.indices())
            .ok_or(ImportError::Scene("cannot calculate flat normals"))?;
    } else {
        for vertex in &mut vertices {
            vertex.normal = normalize(vertex.normal)
                .ok_or(ImportError::Scene("morphed normal is zero or invalid"))?;
        }
    }
    if let Some(tangent_deltas) = tangent_deltas {
        for (vertex, delta) in vertices.iter_mut().zip(tangent_deltas) {
            if delta == Vec3::ZERO {
                continue;
            }
            if vertex.tangent[3] == 0 {
                return Err(ImportError::Scene(
                    "tangent morph target has no base tangent",
                ));
            }
            let normal = Vec3::from_array(vertex.normal);
            let tangent = Vec3::new(
                vertex.tangent[0] as f32,
                vertex.tangent[1] as f32,
                vertex.tangent[2] as f32,
            ) / 127.0
                + delta;
            let tangent = (tangent - normal * tangent.dot(normal))
                .try_normalize()
                .ok_or(ImportError::Scene("morphed tangent is zero or invalid"))?;
            vertex.tangent[..3].copy_from_slice(&tangent.to_array().map(pack_snorm8));
        }
    }

    if apply_skin {
        let skin = primitive.skin_weights.as_ref().ok_or(ImportError::Scene(
            "skinned primitive lacks JOINTS_0 or WEIGHTS_0",
        ))?;
        let joints = &instance.skin_matrices;
        for (vertex, (indices, weights)) in vertices
            .iter_mut()
            .zip(skin.joints.iter().zip(&skin.weights))
        {
            let mut position = Vec3::ZERO;
            let mut normal = Vec3::ZERO;
            let mut tangent = Vec3::ZERO;
            let weight_sum = weights.iter().sum::<f32>();
            if !weight_sum.is_finite() || weight_sum <= 0.0 {
                return Err(ImportError::Scene(
                    "skinned vertex has no positive joint weight",
                ));
            }
            for (index, (&joint, &weight)) in indices.iter().zip(weights).enumerate() {
                let matrix = joints
                    .get(joint as usize)
                    .ok_or(ImportError::Scene("vertex joint index exceeds skin joints"))?;
                if weight == 0.0 {
                    continue;
                }
                if indices[..index].iter().zip(&weights[..index]).any(
                    |(previous, previous_weight)| *previous == joint && *previous_weight != 0.0,
                ) {
                    return Err(ImportError::Scene(
                        "skinned vertex repeats a joint with nonzero weight",
                    ));
                }
                position += matrix.transform_point3(Vec3::from_array(vertex.position)) * weight;
                normal += matrix.transform_vector3(Vec3::from_array(vertex.normal)) * weight;
                if vertex.tangent[3] != 0 {
                    let source_tangent = Vec3::new(
                        vertex.tangent[0] as f32,
                        vertex.tangent[1] as f32,
                        vertex.tangent[2] as f32,
                    ) / 127.0;
                    tangent += matrix.transform_vector3(source_tangent) * weight;
                }
            }
            vertex.position = position.to_array();
            vertex.normal = normal
                .try_normalize()
                .ok_or(ImportError::Scene("skinned normal is zero or invalid"))?
                .to_array();
            if vertex.tangent[3] != 0 {
                let tangent = (tangent - normal.normalize() * tangent.dot(normal.normalize()))
                    .try_normalize()
                    .ok_or(ImportError::Scene("skinned tangent is zero or invalid"))?;
                vertex.tangent[..3].copy_from_slice(&tangent.to_array().map(pack_snorm8));
            }
        }
    }

    MeshData::new(vertices, primitive.mesh.indices().to_vec())
        .map_err(|_| ImportError::Scene("morphed or skinned mesh contains invalid vertex data"))
}

fn add_scaled(base: [f32; 3], delta: [f32; 3], weight: f32) -> [f32; 3] {
    Vec3::from_array(delta)
        .mul_add(Vec3::splat(weight), Vec3::from_array(base))
        .to_array()
}

struct MikkGeometry<'a> {
    vertices: &'a [Vertex],
    indices: &'a [u32],
    tangents: Vec<[f32; 4]>,
}

impl Geometry for MikkGeometry<'_> {
    fn num_faces(&self) -> usize {
        self.indices.len() / 3
    }

    fn num_vertices_of_face(&self, _: usize) -> usize {
        3
    }

    fn position(&self, face: usize, vert: usize) -> [f32; 3] {
        self.vertices[self.indices[face * 3 + vert] as usize].position
    }

    fn normal(&self, face: usize, vert: usize) -> [f32; 3] {
        self.vertices[self.indices[face * 3 + vert] as usize].normal
    }

    fn tex_coord(&self, face: usize, vert: usize) -> [f32; 2] {
        self.vertices[self.indices[face * 3 + vert] as usize].uv_0
    }

    fn set_tangent_encoded(&mut self, tangent: [f32; 4], face: usize, vert: usize) {
        self.tangents[face * 3 + vert] = tangent;
    }
}

fn generate_tangents(mesh: MeshData) -> Result<MeshData, ImportError> {
    let mut geometry = MikkGeometry {
        vertices: mesh.vertices(),
        indices: mesh.indices(),
        tangents: vec![[0.0; 4]; mesh.indices().len()],
    };
    if !mikktspace::generate_tangents(&mut geometry) {
        return Err(ImportError::Scene("MikkTSpace tangent generation failed"));
    }

    let mut vertices = Vec::new();
    let mut indices = Vec::with_capacity(mesh.indices().len());
    let mut remap = HashMap::new();
    for (corner, &source_index) in mesh.indices().iter().enumerate() {
        let tangent = geometry.tangents[corner];
        if tangent.iter().any(|value| !value.is_finite())
            || Vec3::from_array(tangent[..3].try_into().unwrap()).length_squared() < 1e-12
        {
            return Err(ImportError::Scene("MikkTSpace produced an invalid tangent"));
        }
        let packed = tangent.map(pack_snorm8);
        let index = *remap.entry((source_index, packed)).or_insert_with(|| {
            let mut vertex = mesh.vertices()[source_index as usize];
            vertex.tangent = packed;
            vertices.push(vertex);
            vertices.len() as u32 - 1
        });
        indices.push(index);
    }
    MeshData::new(vertices, indices)
        .map_err(|_| ImportError::Scene("MikkTSpace mesh exceeds the upload limit"))
}

fn pack_snorm8(value: f32) -> i8 {
    (value.clamp(-1.0, 1.0) * 127.0).round() as i8
}

fn accessor_format(
    accessor: &gltf::Accessor<'_>,
    dimensions: Dimensions,
    data_type: DataType,
    normalized: bool,
) -> bool {
    accessor.dimensions() == dimensions
        && accessor.data_type() == data_type
        && accessor.normalized() == normalized
}

fn position_bounds_match(accessor: &gltf::Accessor<'_>, positions: &[[f32; 3]]) -> bool {
    if accessor.view().is_none() && accessor.sparse().is_none() {
        return true;
    }
    let min = accessor
        .min()
        .and_then(|value| gltf::json::deserialize::from_value::<[f32; 3]>(value).ok());
    let max = accessor
        .max()
        .and_then(|value| gltf::json::deserialize::from_value::<[f32; 3]>(value).ok());
    let (Some(min), Some(max)) = (min, max) else {
        return false;
    };
    let mut actual_min = [f32::INFINITY; 3];
    let mut actual_max = [f32::NEG_INFINITY; 3];
    for position in positions {
        for axis in 0..3 {
            actual_min[axis] = actual_min[axis].min(position[axis]);
            actual_max[axis] = actual_max[axis].max(position[axis]);
        }
    }
    min == actual_min && max == actual_max
}

fn mesh_morph_target_count(mesh: &gltf::Mesh<'_>) -> Result<usize, ImportError> {
    let mut count = None;
    for primitive in mesh.primitives() {
        let targets = primitive.morph_targets().len();
        if count.is_some_and(|count| count != targets) {
            return Err(ImportError::Scene(
                "mesh primitives have different morph target counts",
            ));
        }
        count = Some(targets);
    }
    let count = count.unwrap_or(0);
    if mesh.weights().is_some_and(|weights| {
        weights.len() != count || weights.iter().any(|weight| !weight.is_finite())
    }) {
        return Err(ImportError::Scene(
            "mesh morph weights do not match its targets",
        ));
    }
    Ok(count)
}

fn set_flat_normals(vertices: &mut [Vertex], indices: &[u32]) -> Option<()> {
    for triangle in indices.chunks_exact(3) {
        let a = Vec3::from_array(vertices[triangle[0] as usize].position);
        let b = Vec3::from_array(vertices[triangle[1] as usize].position);
        let c = Vec3::from_array(vertices[triangle[2] as usize].position);
        let normal = normalize((b - a).cross(c - a).to_array())?;
        for &index in triangle {
            vertices[index as usize].normal = normal;
        }
    }
    Some(())
}

fn convert_primitive(
    buffers: &[Vec<u8>],
    mesh: usize,
    primitive: gltf::Primitive<'_>,
) -> Result<GltfPrimitive, ImportError> {
    let index = primitive.index();
    let fail = |reason| ImportError::Primitive {
        mesh,
        primitive: index,
        reason,
    };
    if primitive.mode() != gltf::mesh::Mode::Triangles {
        return Err(fail("only triangle-list primitives are supported"));
    }
    for (semantic, accessor) in primitive.attributes() {
        let float = |dimensions| accessor_format(&accessor, dimensions, DataType::F32, false);
        let normalized =
            |dimensions, data_type| accessor_format(&accessor, dimensions, data_type, true);
        let valid = match semantic {
            Semantic::Positions => float(Dimensions::Vec3),
            Semantic::Normals => float(Dimensions::Vec3),
            Semantic::Tangents => float(Dimensions::Vec4),
            Semantic::TexCoords(_) => {
                float(Dimensions::Vec2)
                    || normalized(Dimensions::Vec2, DataType::U8)
                    || normalized(Dimensions::Vec2, DataType::U16)
            }
            Semantic::Colors(0) => {
                [Dimensions::Vec3, Dimensions::Vec4]
                    .into_iter()
                    .any(|dimensions| {
                        float(dimensions)
                            || normalized(dimensions, DataType::U8)
                            || normalized(dimensions, DataType::U16)
                    })
            }
            Semantic::Joints(0) => {
                !accessor.normalized()
                    && accessor.dimensions() == Dimensions::Vec4
                    && matches!(accessor.data_type(), DataType::U8 | DataType::U16)
            }
            Semantic::Weights(0) => {
                float(Dimensions::Vec4)
                    || normalized(Dimensions::Vec4, DataType::U8)
                    || normalized(Dimensions::Vec4, DataType::U16)
            }
            Semantic::Extras(_) => accessor.data_type() != DataType::U32,
            _ => false,
        };
        if !valid {
            return Err(fail("vertex accessor type or semantic is unsupported"));
        }
    }
    if primitive.attributes().any(|(semantic, _)| {
        matches!(semantic, Semantic::Joints(set) | Semantic::Weights(set) if set > 0)
    }) {
        return Err(fail("only JOINTS_0 and WEIGHTS_0 are supported"));
    }

    let position_accessor = primitive
        .get(&Semantic::Positions)
        .ok_or_else(|| fail("POSITION is required"))?;
    if position_accessor.min().is_none() || position_accessor.max().is_none() {
        return Err(fail("POSITION accessor must define min and max bounds"));
    }
    let vertex_count = primitive
        .get(&Semantic::Positions)
        .map(|accessor| accessor.count())
        .ok_or_else(|| fail("POSITION is required"))?;
    let index_accessor = primitive.indices();
    if index_accessor.as_ref().is_some_and(|accessor| {
        accessor.dimensions() != Dimensions::Scalar
            || accessor.normalized()
            || !matches!(
                accessor.data_type(),
                DataType::U8 | DataType::U16 | DataType::U32
            )
    }) {
        return Err(fail("index accessor type is invalid"));
    }
    let index_count = index_accessor
        .as_ref()
        .map_or(vertex_count, gltf::Accessor::count);
    if vertex_count < 3 || index_count < 3 || index_count % 3 != 0 {
        return Err(fail("indices must describe in-range triangles"));
    }
    if !mesh_size_within_limit(vertex_count, index_count) {
        return Err(fail("mesh exceeds the 64 MiB upload limit"));
    }
    if primitive
        .attributes()
        .any(|(_, accessor)| accessor.count() != vertex_count)
    {
        return Err(fail("vertex attribute count does not match POSITION"));
    }

    let gltf_material = primitive.material();
    let material_index = gltf_material.index();
    let pbr = gltf_material.pbr_metallic_roughness();
    let info_binding = |info: gltf::texture::Info<'_>, color_space| {
        texture_binding(info.texture(), info.tex_coord(), color_space, fail)
    };
    let base_color_texture = pbr
        .base_color_texture()
        .map(|info| info_binding(info, ImageColorSpace::Srgb))
        .transpose()?;
    let metallic_roughness_texture = pbr
        .metallic_roughness_texture()
        .map(|info| info_binding(info, ImageColorSpace::Linear))
        .transpose()?;
    let normal_texture = gltf_material
        .normal_texture()
        .map(|info| {
            texture_binding(
                info.texture(),
                info.tex_coord(),
                ImageColorSpace::Linear,
                fail,
            )
        })
        .transpose()?;
    let occlusion_texture = gltf_material
        .occlusion_texture()
        .map(|info| {
            texture_binding(
                info.texture(),
                info.tex_coord(),
                ImageColorSpace::Linear,
                fail,
            )
        })
        .transpose()?;
    let emissive_texture = gltf_material
        .emissive_texture()
        .map(|info| info_binding(info, ImageColorSpace::Srgb))
        .transpose()?;
    let transmission = gltf_material.transmission();
    let transmission_factor = transmission
        .as_ref()
        .map_or(0.0, |extension| extension.transmission_factor());
    let transmission_texture = transmission
        .and_then(|extension| extension.transmission_texture())
        .map(|info| info_binding(info, ImageColorSpace::Linear))
        .transpose()?;
    let alpha_mode = match gltf_material.alpha_mode() {
        GltfAlphaMode::Opaque => AlphaMode::Opaque,
        GltfAlphaMode::Mask => AlphaMode::Mask,
        GltfAlphaMode::Blend => AlphaMode::Blend,
    };
    let textures = [
        base_color_texture,
        metallic_roughness_texture,
        normal_texture,
        occlusion_texture,
        emissive_texture,
        transmission_texture,
    ];
    if textures.iter().any(Option::is_some) && primitive.get(&Semantic::TexCoords(0)).is_none() {
        return Err(fail("material textures require TEXCOORD_0"));
    }
    let normal_accessor = primitive.get(&Semantic::Normals);
    let declared_tangent_accessor = primitive.get(&Semantic::Tangents);
    let tangent_accessor = normal_accessor
        .as_ref()
        .and(declared_tangent_accessor.as_ref());
    let expands_by_corner =
        normal_accessor.is_none() || (normal_texture.is_some() && tangent_accessor.is_none());
    let expanded_vertex_count = if expands_by_corner {
        index_count
    } else {
        vertex_count
    };
    if !mesh_size_within_limit(vertex_count.max(expanded_vertex_count), index_count) {
        return Err(fail("expanded mesh exceeds the 64 MiB upload limit"));
    }

    let morph_targets = primitive.morph_targets();
    let mut morph_bytes = 0usize;
    for target in morph_targets {
        for (accessor, has_base_attribute, is_position, is_tangent) in [
            (target.positions(), true, true, false),
            (target.normals(), normal_accessor.is_some(), false, false),
            (
                target.tangents(),
                declared_tangent_accessor.is_some(),
                false,
                true,
            ),
        ] {
            let Some(accessor) = accessor else { continue };
            if !has_base_attribute {
                return Err(fail("morph target modifies a missing vertex attribute"));
            }
            if !accessor_format(&accessor, Dimensions::Vec3, DataType::F32, false)
                || accessor.count() != vertex_count
            {
                return Err(fail("morph accessor type or count is invalid"));
            }
            if is_position && (accessor.min().is_none() || accessor.max().is_none()) {
                return Err(fail(
                    "POSITION morph accessor must define min and max bounds",
                ));
            }
            if is_tangent && normal_accessor.is_none() {
                continue;
            }
            let count = if is_position && normal_accessor.is_none() {
                expanded_vertex_count
            } else {
                accessor.count()
            };
            morph_bytes = morph_bytes
                .checked_add(
                    count
                        .checked_mul(3 * std::mem::size_of::<f32>())
                        .ok_or_else(|| fail("morph data size overflows"))?,
                )
                .ok_or_else(|| fail("morph data size overflows"))?;
            if morph_bytes > MAX_PRIMITIVE_MORPH_BYTES {
                return Err(fail("primitive morph data exceeds the 96 MiB limit"));
            }
        }
    }

    let material = GltfMaterial {
        base_color_factor: pbr.base_color_factor(),
        metallic_factor: pbr.metallic_factor(),
        roughness_factor: pbr.roughness_factor(),
        emissive_factor: gltf_material.emissive_factor(),
        emissive_strength: gltf_material.emissive_strength().unwrap_or(1.0),
        alpha_mode,
        alpha_cutoff: gltf_material.alpha_cutoff().unwrap_or(0.5),
        transmission_factor,
        double_sided: gltf_material.double_sided(),
        unlit: gltf_material.unlit(),
        normal_scale: gltf_material
            .normal_texture()
            .map_or(1.0, |info| info.scale()),
        occlusion_strength: gltf_material
            .occlusion_texture()
            .map_or(1.0, |info| info.strength()),
        specular_factor: 1.0,
        specular_color_factor: [1.0; 3],
        base_color_texture,
        metallic_roughness_texture,
        normal_texture,
        occlusion_texture,
        emissive_texture,
        transmission_texture,
    };
    material.factors().validate().map_err(fail)?;

    let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(Vec::as_slice));
    if primitive
        .get(&Semantic::Weights(0))
        .is_some_and(|accessor| accessor.normalized())
    {
        let valid = match reader
            .read_weights(0)
            .ok_or_else(|| fail("WEIGHTS_0 accessor data could not be read"))?
        {
            gltf::mesh::util::ReadWeights::U8(mut weights) => weights
                .all(|weight| weight.into_iter().map(u32::from).sum::<u32>() == u8::MAX as u32),
            gltf::mesh::util::ReadWeights::U16(mut weights) => weights
                .all(|weight| weight.into_iter().map(u32::from).sum::<u32>() == u16::MAX as u32),
            gltf::mesh::util::ReadWeights::F32(_) => false,
        };
        if !valid {
            return Err(fail(
                "normalized WEIGHTS_0 integers must sum to their component maximum",
            ));
        }
    }
    let positions = zero_unbacked(
        &position_accessor,
        reader.read_positions().map(Iterator::collect),
    )
    .ok_or_else(|| fail("POSITION data could not be read"))?;
    if positions.len() < 3 || positions.iter().flatten().any(|value| !value.is_finite()) {
        return Err(fail("POSITION must contain at least three finite vertices"));
    }
    if !position_bounds_match(&position_accessor, &positions) {
        return Err(fail("POSITION accessor bounds do not match its data"));
    }
    let mut indices: Vec<u32> = match index_accessor.as_ref() {
        Some(accessor) => zero_unbacked(
            accessor,
            reader
                .read_indices()
                .map(|indices| indices.into_u32().collect()),
        )
        .ok_or_else(|| fail("index accessor data could not be read"))?,
        None => (0..positions.len())
            .map(|index| u32::try_from(index).map_err(|_| fail("too many vertices")))
            .collect::<Result<_, _>>()?,
    };
    if indices.len() < 3
        || indices.len() % 3 != 0
        || indices
            .iter()
            .any(|&index| index as usize >= positions.len())
    {
        return Err(fail("indices must describe in-range triangles"));
    }
    if index_accessor
        .as_ref()
        .is_some_and(|accessor| match accessor.data_type() {
            DataType::U8 => indices.contains(&u8::MAX.into()),
            DataType::U16 => indices.contains(&u16::MAX.into()),
            DataType::U32 => indices.contains(&u32::MAX),
            _ => false,
        })
    {
        return Err(fail("index accessor uses the reserved maximum value"));
    }

    let colors: Vec<[f32; 4]> = match primitive.get(&Semantic::Colors(0)) {
        Some(accessor) => zero_unbacked(
            &accessor,
            reader.read_colors(0).map(|values| {
                values
                    .into_rgba_f32()
                    .map(|color| color.map(|value| value.clamp(0.0, 1.0)))
                    .collect()
            }),
        )
        .ok_or_else(|| fail("COLOR_0 accessor data could not be read"))?,
        None => vec![[1.0; 4]; positions.len()],
    };
    if colors.len() != positions.len() || colors.iter().flatten().any(|value| !value.is_finite()) {
        return Err(fail("COLOR_0 count or values are invalid"));
    }

    let tex_coords: Vec<[f32; 2]> = match primitive.get(&Semantic::TexCoords(0)) {
        Some(accessor) => zero_unbacked(
            &accessor,
            reader
                .read_tex_coords(0)
                .map(|values| values.into_f32().collect()),
        )
        .ok_or_else(|| fail("TEXCOORD_0 accessor data could not be read"))?,
        None => vec![[0.0; 2]; positions.len()],
    };
    if tex_coords.len() != positions.len()
        || tex_coords.iter().flatten().any(|value| !value.is_finite())
    {
        return Err(fail("TEXCOORD_0 count or values are invalid"));
    }

    let (normals, flat_normals) = match normal_accessor.as_ref() {
        Some(accessor) => (
            zero_unbacked(accessor, reader.read_normals().map(Iterator::collect))
                .ok_or_else(|| fail("NORMAL accessor data could not be read"))?
                .into_iter()
                .map(|normal| {
                    normalize(normal)
                        .ok_or_else(|| fail("NORMAL contains a zero or invalid vector"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            false,
        ),
        None => (vec![[0.0; 3]; positions.len()], true),
    };
    if normals.len() != positions.len() {
        return Err(fail("NORMAL count does not match POSITION"));
    }
    let tangents = if flat_normals {
        vec![[0; 4]; positions.len()]
    } else if let Some(accessor) = tangent_accessor.as_ref() {
        zero_unbacked(accessor, reader.read_tangents().map(Iterator::collect))
            .ok_or_else(|| fail("TANGENT accessor data could not be read"))?
            .into_iter()
            .map(|tangent| {
                if !tangent.iter().all(|value| value.is_finite())
                    || (tangent[3] != -1.0 && tangent[3] != 1.0)
                {
                    return Err(fail("TANGENT contains invalid values"));
                }
                let direction = normalize(tangent[..3].try_into().unwrap())
                    .ok_or_else(|| fail("TANGENT contains a zero or invalid vector"))?;
                Ok([
                    pack_snorm8(direction[0]),
                    pack_snorm8(direction[1]),
                    pack_snorm8(direction[2]),
                    pack_snorm8(tangent[3]),
                ])
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![[0; 4]; positions.len()]
    };
    let has_tangents = !flat_normals && tangent_accessor.is_some();

    let target_accessors: Vec<_> = primitive.morph_targets().collect();
    let target_data = reader.read_morph_targets();
    let mut morph_targets = Vec::new();
    for (target, (position_deltas, normal_deltas, tangent_deltas)) in
        target_accessors.into_iter().zip(target_data)
    {
        let positions = target
            .positions()
            .map(|accessor| {
                zero_unbacked(&accessor, position_deltas.map(Iterator::collect))
                    .ok_or_else(|| fail("morph POSITION accessor data could not be read"))
            })
            .transpose()?;
        let normals = target
            .normals()
            .map(|accessor| {
                zero_unbacked(&accessor, normal_deltas.map(Iterator::collect))
                    .ok_or_else(|| fail("morph NORMAL accessor data could not be read"))
            })
            .transpose()?;
        let tangents = if normal_accessor.is_some() {
            target
                .tangents()
                .map(|accessor| {
                    zero_unbacked(&accessor, tangent_deltas.map(Iterator::collect))
                        .ok_or_else(|| fail("morph TANGENT accessor data could not be read"))
                })
                .transpose()?
        } else {
            None
        };
        if [positions.as_ref(), normals.as_ref(), tangents.as_ref()]
            .into_iter()
            .flatten()
            .any(|values| {
                values.len() != vertex_count
                    || values.iter().flatten().any(|value| !value.is_finite())
            })
        {
            return Err(fail("morph target count or values are invalid"));
        }
        if let (Some(accessor), Some(values)) = (target.positions(), positions.as_ref()) {
            if !position_bounds_match(&accessor, values) {
                return Err(fail("POSITION morph accessor bounds do not match its data"));
            }
        }
        morph_targets.push(GltfMorphTarget {
            positions,
            normals,
            tangents,
        });
    }

    let joints = primitive
        .get(&Semantic::Joints(0))
        .map(|accessor| {
            zero_unbacked(
                &accessor,
                reader
                    .read_joints(0)
                    .map(|values| values.into_u16().collect()),
            )
            .ok_or_else(|| fail("JOINTS_0 accessor data could not be read"))
        })
        .transpose()?;
    let weights = primitive
        .get(&Semantic::Weights(0))
        .map(|accessor| {
            zero_unbacked(
                &accessor,
                reader
                    .read_weights(0)
                    .map(|values| values.into_f32().collect()),
            )
            .ok_or_else(|| fail("WEIGHTS_0 accessor data could not be read"))
        })
        .transpose()?;
    let mut skin_weights = match (joints, weights) {
        (None, None) => None,
        (Some(joints), Some(weights))
            if joints.len() == vertex_count
                && weights.len() == vertex_count
                && weights
                    .iter()
                    .flatten()
                    .all(|weight| weight.is_finite() && *weight >= 0.0) =>
        {
            Some(GltfSkinWeights { joints, weights })
        }
        _ => {
            return Err(fail(
                "JOINTS_0 and WEIGHTS_0 must be paired finite vertex data",
            ));
        }
    };

    let mut vertices = positions
        .into_iter()
        .zip(normals)
        .zip(colors)
        .zip(tex_coords)
        .zip(tangents)
        .map(|((((position, normal), color_0), uv_0), tangent)| Vertex {
            position,
            normal,
            uv_0,
            color_0,
            tangent,
        })
        .collect::<Vec<_>>();
    if flat_normals {
        let source_indices = indices;
        vertices = source_indices
            .iter()
            .map(|&index| vertices[index as usize])
            .collect();
        for target in &mut morph_targets {
            if let Some(deltas) = &mut target.positions {
                *deltas = source_indices
                    .iter()
                    .map(|&index| deltas[index as usize])
                    .collect();
            }
            if let Some(deltas) = &mut target.normals {
                *deltas = source_indices
                    .iter()
                    .map(|&index| deltas[index as usize])
                    .collect();
            }
        }
        if let Some(weights) = &mut skin_weights {
            weights.joints = source_indices
                .iter()
                .map(|&index| weights.joints[index as usize])
                .collect();
            weights.weights = source_indices
                .iter()
                .map(|&index| weights.weights[index as usize])
                .collect();
        }
        indices = (0..vertices.len())
            .map(|index| u32::try_from(index).map_err(|_| fail("too many expanded vertices")))
            .collect::<Result<_, _>>()?;
        set_flat_normals(&mut vertices, &indices)
            .ok_or_else(|| fail("cannot calculate flat normals from degenerate geometry"))?;
    }
    Ok(GltfPrimitive {
        mesh_index: mesh,
        primitive_index: index,
        material_index,
        mesh: MeshData::new(vertices, indices).map_err(fail)?,
        material,
        flat_normals,
        has_tangents,
        morph_targets,
        skin_weights,
    })
}

fn texture_binding(
    texture: gltf::texture::Texture<'_>,
    tex_coord: u32,
    color_space: ImageColorSpace,
    fail: impl Fn(&'static str) -> ImportError,
) -> Result<GltfTexture, ImportError> {
    if tex_coord != 0 {
        return Err(fail("only TEXCOORD_0 textures are supported"));
    }
    let sampler = texture.sampler();
    let (min_filter, mipmap_mode) = match sampler
        .min_filter()
        .unwrap_or(gltf::texture::MinFilter::LinearMipmapLinear)
    {
        gltf::texture::MinFilter::Nearest => (TextureFilter::Nearest, MipmapMode::None),
        gltf::texture::MinFilter::Linear => (TextureFilter::Linear, MipmapMode::None),
        gltf::texture::MinFilter::NearestMipmapNearest => {
            (TextureFilter::Nearest, MipmapMode::Nearest)
        }
        gltf::texture::MinFilter::LinearMipmapNearest => {
            (TextureFilter::Linear, MipmapMode::Nearest)
        }
        gltf::texture::MinFilter::NearestMipmapLinear => {
            (TextureFilter::Nearest, MipmapMode::Linear)
        }
        gltf::texture::MinFilter::LinearMipmapLinear => (TextureFilter::Linear, MipmapMode::Linear),
    };
    let wrap = |mode| match mode {
        gltf::texture::WrappingMode::ClampToEdge => TextureWrap::ClampToEdge,
        gltf::texture::WrappingMode::MirroredRepeat => TextureWrap::MirroredRepeat,
        gltf::texture::WrappingMode::Repeat => TextureWrap::Repeat,
    };
    Ok(GltfTexture {
        image_index: texture.source().index(),
        color_space,
        sampler: TextureSampler {
            min_filter,
            mipmap_mode,
            mag_filter: match sampler
                .mag_filter()
                .unwrap_or(gltf::texture::MagFilter::Linear)
            {
                gltf::texture::MagFilter::Nearest => TextureFilter::Nearest,
                gltf::texture::MagFilter::Linear => TextureFilter::Linear,
            },
            wrap_u: wrap(sampler.wrap_s()),
            wrap_v: wrap(sampler.wrap_t()),
        },
    })
}

fn normalize(vector: [f32; 3]) -> Option<[f32; 3]> {
    let vector = Vec3::from_array(vector);
    if !vector.is_finite() {
        return None;
    }
    let scale = vector.abs().max_element();
    if scale == 0.0 {
        return None;
    }
    Some((vector / scale).normalize().to_array())
}

fn mesh_size_within_limit(vertex_count: usize, index_count: usize) -> bool {
    vertex_count
        .checked_mul(std::mem::size_of::<Vertex>())
        .and_then(|vertices| {
            index_count
                .checked_mul(std::mem::size_of::<u32>())
                .and_then(|indices| vertices.checked_add(indices))
        })
        .is_some_and(|bytes| bytes <= MAX_MESH_UPLOAD_BYTES)
}

fn zero_unbacked<T: Clone + Default>(
    accessor: &gltf::Accessor<'_>,
    values: Option<Vec<T>>,
) -> Option<Vec<T>> {
    values.or_else(|| {
        (accessor.view().is_none() && accessor.sparse().is_none())
            .then(|| vec![T::default(); accessor.count()])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_environment_sh_matches_lambertian_irradiance() {
        let (width, height) = (256, 128);
        let rgb = vec![0.5_f32; width * height * 3];
        let sh = diffuse_irradiance_sh(&rgb, width as u32, height as u32).unwrap();
        for normal in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] {
            let basis = sh_basis(normal);
            for channel in 0..3 {
                let irradiance: f64 = sh
                    .iter()
                    .enumerate()
                    .map(|(index, coefficient)| f64::from(coefficient[channel]) * basis[index])
                    .sum();
                assert!(
                    (irradiance - std::f64::consts::PI * 0.5).abs() < 3e-5,
                    "normal {normal:?} produced irradiance {irradiance}"
                );
            }
        }
    }

    #[test]
    fn finite_hdr_radiance_cannot_overflow_diffuse_sh() {
        assert!(diffuse_irradiance_sh(&[1.0e38; 3], 1, 1).is_err());
    }

    #[test]
    fn environment_hdr_contract_rejects_unsupported_metadata() {
        let mut metadata = HdrMetadata {
            width: 2,
            height: 1,
            orientation: ((1, 0), (0, 1)),
            exposure: None,
            color_correction: None,
            pixel_aspect_ratio: None,
            custom_attributes: vec![("FORMAT".into(), "32-bit_rle_rgbe".into())],
        };
        assert!(validate_environment_hdr_metadata(&metadata).is_ok());

        metadata.exposure = Some(2.0);
        assert!(validate_environment_hdr_metadata(&metadata).is_err());
        metadata.exposure = None;
        metadata.color_correction = Some((1.0, 2.0, 1.0));
        assert!(validate_environment_hdr_metadata(&metadata).is_err());
        metadata.color_correction = None;
        metadata.pixel_aspect_ratio = Some(2.0);
        assert!(validate_environment_hdr_metadata(&metadata).is_err());
        metadata.pixel_aspect_ratio = None;
        metadata.orientation = ((0, 1), (-1, 0));
        assert!(validate_environment_hdr_metadata(&metadata).is_err());
        metadata.orientation = ((1, 0), (0, 1));
        metadata.custom_attributes.clear();
        assert!(validate_environment_hdr_metadata(&metadata).is_err());
    }

    #[test]
    fn imports_builtin_hdr_environment_and_computes_diffuse_sh() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/assets/aloe_farm_shade_house_1k.hdr");
        let environment = import_environment(path).unwrap();
        assert_eq!(
            (environment.extent.width(), environment.extent.height()),
            (1024, 512)
        );
        assert!(
            environment.diffuse_irradiance_sh[0]
                .iter()
                .any(|value| *value > 1.0)
        );
        assert!(
            environment
                .upload(EnvironmentId::new(1).unwrap())
                .validate()
                .is_ok()
        );
    }

    fn minimal_json() -> Vec<u8> {
        let mut json = br#"{"asset":{"version":"2.0"}}"#.to_vec();
        while json.len() % 4 != 0 {
            json.push(b' ');
        }
        json
    }

    fn glb(json: &[u8], bin: &[u8]) -> Vec<u8> {
        let mut json = json.to_vec();
        while json.len() % 4 != 0 {
            json.push(b' ');
        }
        let mut bin = bin.to_vec();
        while bin.len() % 4 != 0 {
            bin.push(0);
        }
        let length = 12 + 8 + json.len() + 8 + bin.len();
        let mut data = vec![0; length];
        data[..4].copy_from_slice(b"glTF");
        data[4..8].copy_from_slice(&2_u32.to_le_bytes());
        data[8..12].copy_from_slice(&(length as u32).to_le_bytes());
        data[12..16].copy_from_slice(&(json.len() as u32).to_le_bytes());
        data[16..20].copy_from_slice(b"JSON");
        data[20..20 + json.len()].copy_from_slice(&json);
        let offset = 20 + json.len();
        data[offset..offset + 4].copy_from_slice(&(bin.len() as u32).to_le_bytes());
        data[offset + 4..offset + 8].copy_from_slice(b"BIN\0");
        data[offset + 8..].copy_from_slice(&bin);
        data
    }

    fn validate_extensions(json: &[u8]) -> Result<(), ImportError> {
        let document = gltf::Gltf::from_slice_without_validation(&minimal_json())?;
        validate_extension_declarations(json, &document)
    }

    #[test]
    fn imports_khr_punctual_lights_from_meshless_scene_nodes() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "extensionsUsed":["KHR_lights_punctual","EXT_light_note"],
            "extensionsRequired":["KHR_lights_punctual"],
            "extensions":{"KHR_lights_punctual":{"lights":[
                {"type":"point","color":[0.5,0.6,0.7],"intensity":4,"range":10,
                 "extensions":{"EXT_light_note":{}}},
                {"type":"spot","spot":{"innerConeAngle":0.1,"outerConeAngle":0.5},"intensity":3},
                {"type":"directional","intensity":2}
            ]}},
            "nodes":[
                {"translation":[1,0,0],"children":[1,2,3,4]},
                {"translation":[0,2,0],"extensions":{"KHR_lights_punctual":{"light":0}}},
                {"translation":[0,0,3],"rotation":[0.70710677,0,0,0.70710677],"extensions":{"KHR_lights_punctual":{"light":1}}},
                {"rotation":[0,0.70710677,0,0.70710677],"extensions":{"KHR_lights_punctual":{"light":2}}},
                {"translation":[0,-2,0],"extensions":{"KHR_lights_punctual":{"light":0}}}
            ],
            "scenes":[{"nodes":[0]}],"scene":0
        }"#;
        let asset = import_slice(source).unwrap();
        let lights = asset.scene_lights().unwrap();
        assert_eq!(lights.len(), 4);
        assert!(matches!(
            lights[0],
            GltfSceneLight::Punctual {
                node_index: 1,
                light: PunctualLight::Point {
                    position: [1.0, 2.0, 0.0],
                    color: [0.5, 0.6, 0.7],
                    intensity: 4.0,
                    range: Some(10.0),
                },
            }
        ));
        let GltfSceneLight::Punctual {
            node_index: 2,
            light:
                PunctualLight::Spot {
                    position,
                    direction,
                    inner_cone_angle,
                    outer_cone_angle,
                    ..
                },
        } = lights[1]
        else {
            panic!("expected imported spot light")
        };
        assert_eq!(position, [1.0, 0.0, 3.0]);
        assert!(direction[1] > 0.9999 && direction[0].abs() < 1e-5 && direction[2].abs() < 1e-5);
        assert_eq!([inner_cone_angle, outer_cone_angle], [0.1, 0.5]);
        let GltfSceneLight::Directional {
            node_index: 3,
            light,
        } = lights[2]
        else {
            panic!("expected imported directional light")
        };
        assert!(light.direction_to_light[0] > 0.9999);
        assert!(
            light.direction_to_light[1].abs() < 1e-5 && light.direction_to_light[2].abs() < 1e-5
        );
        assert_eq!(light.radiance, [2.0; 3]);
        assert_eq!(light.shadow, None);
        assert!(matches!(
            lights[3],
            GltfSceneLight::Punctual { node_index: 4, .. }
        ));
    }

    #[test]
    fn punctual_light_extensions_and_values_are_validated() {
        let source = br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_lights_punctual"],"extensions":{"KHR_lights_punctual":{"lights":[{"type":"point","extensions":{"EXT_unlisted":{}}}]}},"nodes":[{"extensions":{"KHR_lights_punctual":{"light":0}}}],"scenes":[{"nodes":[0]}]}"#;
        assert!(matches!(
            import_slice(source),
            Err(ImportError::Source(
                "extensionsRequired and extension payloads must be listed in extensionsUsed"
            ))
        ));

        for light in [
            r#"{"type":"point","color":[1.1,1,1]}"#,
            r#"{"type":"point","intensity":-1}"#,
            r#"{"type":"point","range":0}"#,
            r#"{"type":"spot"}"#,
            r#"{"type":"spot","spot":{"innerConeAngle":0.5,"outerConeAngle":0.4}}"#,
        ] {
            let source = format!(
                r#"{{"asset":{{"version":"2.0"}},"extensionsUsed":["KHR_lights_punctual"],"extensions":{{"KHR_lights_punctual":{{"lights":[{light}]}}}},"nodes":[{{"extensions":{{"KHR_lights_punctual":{{"light":0}}}}}}],"scenes":[{{"nodes":[0]}}]}}"#
            );
            assert!(import_slice(source.as_bytes()).is_err(), "{light}");
        }

        let bad_reference = br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_lights_punctual"],"extensions":{"KHR_lights_punctual":{"lights":[{"type":"point"}]}},"nodes":[{"extensions":{"KHR_lights_punctual":{"light":1}}}],"scenes":[{"nodes":[0]}]}"#;
        assert!(import_slice(bad_reference).is_err());

        let extra_spot_properties = br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_lights_punctual"],"extensions":{"KHR_lights_punctual":{"lights":[{"type":"point","spot":{"innerConeAngle":0.2,"outerConeAngle":0.6}}]}},"nodes":[{"extensions":{"KHR_lights_punctual":{"light":0}}}],"scenes":[{"nodes":[0]}]}"#;
        let asset = import_slice(extra_spot_properties).unwrap();
        assert!(matches!(
            asset.scene_lights().unwrap().as_slice(),
            [GltfSceneLight::Punctual {
                light: PunctualLight::Point { .. },
                ..
            }]
        ));
    }

    #[test]
    fn mesh_upload_budget_is_checked_without_overflow() {
        assert!(mesh_size_within_limit(3, 3));
        assert!(!mesh_size_within_limit(usize::MAX, 3));
        assert!(!mesh_size_within_limit(MAX_MESH_UPLOAD_BYTES, 0));
    }

    #[test]
    fn accessor_ranges_reject_short_views_and_overflow() {
        assert!(accessor_range_fits(0, 3, 12, 12, 12, 36));
        assert!(!accessor_range_fits(0, 3, 12, 12, 12, 35));
        assert!(accessor_range_fits(0, 1, 12, 12, 11, 11));
        assert!(!accessor_range_fits(0, 2, 12, 12, 11, 22));
        assert!(!accessor_range_fits(0, usize::MAX, usize::MAX, 64, 64, 96));
    }

    #[test]
    fn matrix_columns_require_four_byte_alignment() {
        let uri = base64::encode([0; 26]);
        let base = format!(
            r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":26,"uri":"data:application/octet-stream;base64,{uri}"}}],"bufferViews":[{{"buffer":0,"byteOffset":2,"byteLength":24}}],"accessors":[{{"bufferView":0,"componentType":5123,"count":1,"type":"MAT3"}}]}}"#
        );
        assert!(matches!(
            import_slice(base.as_bytes()),
            Err(ImportError::Buffer("accessor data is misaligned"))
        ));

        let sparse = format!(
            r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":26,"uri":"data:application/octet-stream;base64,{uri}"}}],"bufferViews":[{{"buffer":0,"byteLength":1}},{{"buffer":0,"byteOffset":2,"byteLength":24}}],"accessors":[{{"componentType":5123,"count":1,"type":"MAT3","sparse":{{"count":1,"indices":{{"bufferView":0,"componentType":5121}},"values":{{"bufferView":1}}}}}}]}}"#
        );
        assert!(matches!(
            import_slice(sparse.as_bytes()),
            Err(ImportError::Buffer("sparse values are misaligned"))
        ));
    }

    #[test]
    fn unstrided_buffer_views_may_be_shared_by_nonvertex_accessors() {
        let mut binary = Vec::new();
        for value in [0.0_f32, 1.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0] {
            binary.extend_from_slice(&value.to_le_bytes());
        }
        let uri = base64::encode(binary);
        let source = format!(
            r#"{{
                "asset":{{"version":"2.0"}},
                "buffers":[{{"byteLength":32,"uri":"data:application/octet-stream;base64,{uri}"}}],
                "bufferViews":[{{"buffer":0,"byteLength":32}}],
                "accessors":[
                    {{"bufferView":0,"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1]}},
                    {{"bufferView":0,"byteOffset":8,"componentType":5126,"count":2,"type":"VEC3"}}
                ],
                "nodes":[{{}}],
                "animations":[{{"samplers":[{{"input":0,"output":1}}],"channels":[{{"sampler":0,"target":{{"node":0,"path":"translation"}}}}]}}]
            }}"#
        );
        assert!(import_slice(source.as_bytes()).is_ok());
    }

    #[test]
    fn ignores_valid_application_specific_vertex_attributes() {
        let source = include_str!("../../../examples/assets/triangle.gltf").replace(
            "\"POSITION\": 0, \"TEXCOORD_0\": 1",
            "\"POSITION\": 0, \"TEXCOORD_0\": 1, \"_TEMPERATURE\": 1",
        );
        let asset = import_slice(source.as_bytes()).unwrap();
        assert_eq!(asset.primitives().unwrap().len(), 1);
    }

    #[test]
    fn zero_initialized_accessors_keep_their_declared_values() {
        let mut binary = Vec::new();
        for value in [
            0.0_f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, // positions
            0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, // normals
        ] {
            binary.extend_from_slice(&value.to_le_bytes());
        }
        let uri = base64::encode(binary);
        let source = format!(
            r#"{{
                "asset":{{"version":"2.0"}},
                "buffers":[{{"byteLength":72,"uri":"data:application/octet-stream;base64,{uri}"}}],
                "bufferViews":[{{"buffer":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":36}}],
                "accessors":[
                    {{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
                    {{"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"}},
                    {{"componentType":5121,"count":3,"type":"SCALAR"}},
                    {{"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[0,0,0]}}
                ],
                "meshes":[
                    {{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1}},"indices":2}}]}},
                    {{"primitives":[{{"attributes":{{"POSITION":3,"NORMAL":1}}}}]}}
                ]
            }}"#
        );
        let asset = import_slice(source.as_bytes()).unwrap();
        let primitives = asset.primitives().unwrap();
        assert_eq!(primitives[0].mesh.indices(), [0, 0, 0]);
        assert_eq!(primitives[1].mesh.vertices()[0].position, [0.0; 3]);
    }

    #[test]
    fn rejects_accessor_offset_without_buffer_view() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":8,"uri":"data:application/octet-stream;base64,AAAAAAAAAAA="}],
            "bufferViews":[{"buffer":0,"byteLength":2},{"buffer":0,"byteOffset":4,"byteLength":4}],
            "accessors":[{"byteOffset":0,"componentType":5126,"count":3,"type":"SCALAR","sparse":{"count":1,"indices":{"bufferView":0,"componentType":5121},"values":{"bufferView":1}}}]
        }"#;
        assert!(matches!(
            import_slice(source),
            Err(ImportError::Buffer(
                "accessor byteOffset requires a bufferView"
            ))
        ));
    }

    #[test]
    fn rejects_unsigned_int_accessors_unless_they_are_indices() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "accessors":[{"componentType":5125,"count":1,"type":"SCALAR"}]
        }"#;
        assert!(matches!(
            import_slice(source),
            Err(ImportError::Buffer(
                "UNSIGNED_INT accessor must be used for primitive indices"
            ))
        ));
    }

    #[test]
    fn rejects_zero_length_buffers_and_buffer_views() {
        let buffer = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":0,"uri":"data:application/octet-stream;base64,"}]}"#;
        assert!(matches!(
            import_slice(buffer),
            Err(ImportError::Buffer("buffer byteLength must be positive"))
        ));

        let view = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":1,"uri":"data:application/octet-stream;base64,AA=="}],"bufferViews":[{"buffer":0,"byteLength":0}]}"#;
        assert!(matches!(
            import_slice(view),
            Err(ImportError::Buffer(
                "buffer view byteLength must be positive"
            ))
        ));
    }

    #[test]
    fn rejects_buffer_views_shared_by_indices_and_inverse_bind_matrices() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":100,"uri":"data:application/octet-stream;base64,AA=="}],
            "bufferViews":[{"buffer":0,"byteLength":64},{"buffer":0,"byteOffset":64,"byteLength":36}],
            "accessors":[
                {"bufferView":0,"componentType":5123,"count":3,"type":"SCALAR"},
                {"bufferView":0,"componentType":5126,"count":1,"type":"MAT4"},
                {"bufferView":1,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}
            ],
            "meshes":[{"primitives":[{"attributes":{"POSITION":2},"indices":0}]}],
            "skins":[{"joints":[0],"inverseBindMatrices":1}],
            "nodes":[{"mesh":0,"skin":0}],
            "scenes":[{"nodes":[0]}]
        }"#;
        let document = gltf::Gltf::from_slice(source).unwrap();
        assert!(matches!(
            validate_vertex_accessor_layout(&document.document),
            Err(ImportError::Buffer(
                "buffer view contains multiple glTF data kinds"
            ))
        ));
    }

    #[test]
    fn rejects_images_sharing_sparse_attribute_index_views() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":40,"uri":"data:application/octet-stream;base64,AA=="}],
            "bufferViews":[{"buffer":0,"byteLength":4},{"buffer":0,"byteOffset":4,"byteLength":36}],
            "accessors":[{"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0],"sparse":{"count":1,"indices":{"bufferView":0,"componentType":5121},"values":{"bufferView":1}}}],
            "meshes":[{"primitives":[{"attributes":{"POSITION":0}}]}],
            "images":[{"bufferView":0,"mimeType":"image/png"}]
        }"#;
        let document = gltf::Gltf::from_slice(source).unwrap();
        assert!(matches!(
            validate_vertex_accessor_layout(&document.document),
            Err(ImportError::Buffer(
                "buffer view contains multiple glTF data kinds"
            ))
        ));
    }

    #[test]
    fn image_buffer_views_reject_stride_but_accept_target_hints() {
        let source = r#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":4,"uri":"data:application/octet-stream;base64,AAAAAA=="}],
            "bufferViews":[{"buffer":0,"byteLength":4,"target":34962}],
            "images":[{"bufferView":0,"mimeType":"image/png"}]
        }"#;
        let document = gltf::Gltf::from_slice(source.as_bytes()).unwrap();
        assert!(validate_vertex_accessor_layout(&document.document).is_ok());

        let source = source.replace("\"target\":34962", "\"byteStride\":4");
        let document = gltf::Gltf::from_slice(source.as_bytes()).unwrap();
        assert!(matches!(
            validate_vertex_accessor_layout(&document.document),
            Err(ImportError::Buffer(
                "image buffer view cannot define byteStride"
            ))
        ));
    }

    #[test]
    fn rejects_incomplete_or_conflicting_image_sources_without_panicking() {
        let sources = [
            r#"{"asset":{"version":"2.0"},"images":[{}]}"#,
            r#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":1,"uri":"data:application/octet-stream;base64,AA=="}],"bufferViews":[{"buffer":0,"byteLength":1}],"images":[{"uri":"missing.png","bufferView":0,"mimeType":"image/png"}]}"#,
            r#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":1,"uri":"data:application/octet-stream;base64,AA=="}],"bufferViews":[{"buffer":0,"byteLength":1}],"images":[{"bufferView":0}]}"#,
            r#"{"asset":{"version":"2.0"},"images":[{"uri":"data:image/png;base64,AA==","mimeType":"image/webp"}]}"#,
        ];
        for source in sources {
            assert!(matches!(
                import_slice(source.as_bytes()),
                Err(ImportError::Image(_))
            ));
        }
    }

    #[test]
    fn data_uris_decode_rfc2397_payloads_and_check_declared_mime() {
        let uri = "DATA:IMAGE/PNG;charset=utf-8,%89PNG%0D%0A";
        let source = parse_data_uri(uri).unwrap();
        assert_eq!(source.media_type, Some("IMAGE/PNG"));
        assert!(!source.base64);
        assert_eq!(decode_data_uri(source, 16).unwrap(), b"\x89PNG\r\n");

        let uri = "DaTa:application/gltf-buffer;BASE64,AQID";
        let source = parse_data_uri(uri).unwrap();
        assert!(source.base64);
        assert_eq!(decode_data_uri(source, 4).unwrap(), [1, 2, 3]);
        assert!(parse_data_uri("data:image/png,%GG").is_ok());
        assert!(decode_data_uri(parse_data_uri("data:image/png,%GG").unwrap(), 4).is_err());
        assert!(decode_data_uri(parse_data_uri("data:image/png,AQID").unwrap(), 2).is_err());

        let (bytes, mime) = image_source(
            gltf::image::Source::Uri {
                uri: "DATA:IMAGE/PNG;charset=utf-8,%89PNG%0D%0A",
                mime_type: None,
            },
            None,
            &[],
        )
        .unwrap();
        assert_eq!(mime, Some("image/png"));
        assert_eq!(bytes.as_ref(), b"\x89PNG\r\n");

        let image_uri = "data:image/jpeg;base64,AQID";
        assert!(matches!(
            image_source(
                gltf::image::Source::Uri {
                    uri: image_uri,
                    mime_type: Some("image/png"),
                },
                None,
                &[]
            ),
            Err(ImportError::Image(
                "image MIME type does not match its data URI"
            ))
        ));
    }

    #[test]
    fn buffer_data_uris_require_a_supported_base64_media_type() {
        let valid = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":1,"uri":"DATA:APPLICATION/GLTF-BUFFER;BASE64,AA=="}]}"#;
        assert!(import_slice(valid).is_ok());

        for uri in [
            "data:text/plain;base64,AA==",
            "data:application/octet-stream,%00",
        ] {
            let source = format!(
                r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":1,"uri":"{uri}"}}]}}"#
            );
            assert!(matches!(
                import_slice(source.as_bytes()),
                Err(ImportError::Buffer(_))
            ));
        }
    }

    #[test]
    fn resource_uris_validate_and_decode_each_path_segment() {
        let base = Path::new("models");
        assert_eq!(
            resource_path("textures/my%20image.png", base).unwrap(),
            PathBuf::from("models/textures/my image.png")
        );
        assert_eq!(
            resource_path("textures/a%2Bb.png", base).unwrap(),
            PathBuf::from("models/textures/a+b.png")
        );
        assert_eq!(
            resource_path("textures/高解像度.png", base).unwrap(),
            PathBuf::from("models/textures/高解像度.png")
        );
        for uri in [
            "textures/%GG.png",
            "textures%2Fsecret.bin",
            "textures/a+b.png",
            "textures/file.png?download=1",
            "http://example.invalid/file.bin",
        ] {
            assert!(resource_path(uri, base).is_err(), "accepted {uri}");
        }
        if cfg!(windows) {
            assert!(resource_path("textures%5Csecret.bin", base).is_err());
            assert_eq!(
                resource_path("file:///C:/models/a%20b.glb", base).unwrap(),
                PathBuf::from("C:/models/a b.glb")
            );
        } else {
            assert_eq!(
                resource_path("textures%5Csecret.bin", base).unwrap(),
                PathBuf::from("models/textures\\secret.bin")
            );
            assert_eq!(
                resource_path("%3Aname.bin", base).unwrap(),
                PathBuf::from("models/:name.bin")
            );
        }
    }

    #[test]
    fn transmission_factor_range_uses_exact_decimal_values() {
        for value in ["-0", "0.0", "1.0", "1e0", "10e-1", "0.99999999999999999"] {
            assert!(
                json_number_in_unit_interval(value),
                "accepted value {value}"
            );
        }
        for value in ["-1e-100", "1.00000001", "1.0000000000000001", "2"] {
            assert!(
                !json_number_in_unit_interval(value),
                "rejected value {value}"
            );
        }
    }

    fn animation_source(
        times: [f32; 2],
        output_type: &str,
        output_count: usize,
        interpolation: Option<&str>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        for time in times {
            bytes.extend_from_slice(&time.to_le_bytes());
        }
        for value in 0..output_count * 3 {
            bytes.extend_from_slice(&(value as f32).to_le_bytes());
        }
        let uri = base64::encode(&bytes);
        let interpolation = interpolation
            .map(|value| format!(",\"interpolation\":\"{value}\""))
            .unwrap_or_default();
        format!(
            r#"{{
                "asset":{{"version":"2.0"}},
                "buffers":[{{"byteLength":{},"uri":"data:application/octet-stream;base64,{uri}"}}],
                "bufferViews":[{{"buffer":0,"byteLength":8}},{{"buffer":0,"byteOffset":8,"byteLength":{}}}],
                "accessors":[
                    {{"bufferView":0,"componentType":5126,"count":2,"type":"SCALAR","min":[{}],"max":[{}]}},
                    {{"bufferView":1,"componentType":5126,"count":{output_count},"type":"{output_type}"}}
                ],
                "nodes":[{{}}],
                "animations":[{{"samplers":[{{"input":0,"output":1{interpolation}}}],"channels":[{{"sampler":0,"target":{{"node":0,"path":"translation"}}}}]}}]
            }}"#,
            bytes.len(),
            bytes.len() - 8,
            times[0].min(times[1]),
            times[0].max(times[1]),
        )
        .into_bytes()
    }

    fn weighted_triangle(component_type: u32, valid_sum: bool) -> String {
        let width = if component_type == 5121 { 1 } else { 2 };
        let maximum = if width == 1 { u8::MAX as u16 } else { u16::MAX };
        let mut bytes = vec![0; 12];
        for _ in 0..3 {
            let first = if valid_sum { maximum } else { 1 };
            if width == 1 {
                bytes.extend_from_slice(&[first as u8, 0, 0, 0]);
            } else {
                bytes.extend_from_slice(&first.to_le_bytes());
                bytes.extend_from_slice(&[0; 6]);
            }
        }

        let mut source: serde_json::Value =
            serde_json::from_str(include_str!("../../../examples/assets/triangle.gltf")).unwrap();
        source["buffers"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "byteLength": bytes.len(),
                "uri": format!("data:application/octet-stream;base64,{}", base64::encode(bytes))
            }));
        source["bufferViews"].as_array_mut().unwrap().extend([
            serde_json::json!({"buffer": 1, "byteLength": 12}),
            serde_json::json!({
                "buffer": 1,
                "byteOffset": 12,
                "byteLength": width * 12
            }),
        ]);
        source["accessors"].as_array_mut().unwrap().extend([
            serde_json::json!({
                "bufferView": 3,
                "componentType": 5121,
                "count": 3,
                "type": "VEC4"
            }),
            serde_json::json!({
                "bufferView": 4,
                "componentType": component_type,
                "count": 3,
                "type": "VEC4",
                "normalized": true
            }),
        ]);
        let attributes = &mut source["meshes"][0]["primitives"][0]["attributes"];
        attributes["JOINTS_0"] = serde_json::json!(3);
        attributes["WEIGHTS_0"] = serde_json::json!(4);
        source.to_string()
    }

    #[test]
    fn normalized_integer_skin_weights_must_sum_before_normalization() {
        for component_type in [5121, 5123] {
            let valid = weighted_triangle(component_type, true);
            assert!(import_slice(valid.as_bytes()).unwrap().primitives().is_ok());

            let invalid = weighted_triangle(component_type, false);
            assert!(matches!(
                import_slice(invalid.as_bytes()).unwrap().primitives(),
                Err(ImportError::Primitive {
                    reason: "normalized WEIGHTS_0 integers must sum to their component maximum",
                    ..
                })
            ));
        }
    }

    #[test]
    fn indexed_attribute_sets_must_be_canonical_and_consecutive() {
        let source = include_str!("../../../examples/assets/triangle.gltf");
        for invalid in [
            source.replace("\"TEXCOORD_0\": 1", "\"TEXCOORD_01\": 1"),
            source.replace("\"TEXCOORD_0\": 1", "\"TEXCOORD_0\": 1, \"TEXCOORD_2\": 1"),
        ] {
            assert!(matches!(
                import_slice(invalid.as_bytes()),
                Err(ImportError::Primitive {
                    reason: "indexed attribute sets must start at zero and be consecutive",
                    ..
                })
            ));
        }
        let consecutive =
            source.replace("\"TEXCOORD_0\": 1", "\"TEXCOORD_0\": 1, \"TEXCOORD_1\": 1");
        assert!(import_slice(consecutive.as_bytes()).is_ok());
    }

    #[test]
    fn animation_samplers_validate_times_output_shape_and_cubic_counts() {
        assert!(import_slice(&animation_source([0.0, 1.0], "VEC3", 2, None)).is_ok());
        assert!(matches!(
            import_slice(&animation_source([1.0, 0.0], "VEC3", 2, None)),
            Err(ImportError::Scene(
                "animation times must be nonnegative and strictly increasing"
            ))
        ));
        assert!(matches!(
            import_slice(&animation_source([0.0, 1.0], "VEC3", 1, None)),
            Err(ImportError::Scene(
                "animation output count does not match its sampler"
            ))
        ));
        assert!(
            import_slice(&animation_source(
                [0.0, 1.0],
                "VEC3",
                6,
                Some("CUBICSPLINE")
            ))
            .is_ok()
        );
        assert!(matches!(
            import_slice(&animation_source(
                [0.0, 1.0],
                "VEC3",
                2,
                Some("CUBICSPLINE")
            )),
            Err(ImportError::Scene(
                "animation output count does not match its sampler"
            ))
        ));
        assert!(matches!(
            import_slice(&animation_source([0.0, 1.0], "SCALAR", 2, None)),
            Err(ImportError::Scene(
                "translation and scale outputs must be float VEC3 accessors"
            ))
        ));
    }

    #[test]
    fn ignores_extension_animation_channels_without_core_nodes() {
        let source = String::from_utf8(animation_source([0.0, 1.0], "VEC3", 2, None))
            .unwrap()
            .replace(
                "\"asset\":{\"version\":\"2.0\"}",
                "\"asset\":{\"version\":\"2.0\"},\"extensionsUsed\":[\"KHR_animation_pointer\"]",
            )
            .replace(
                "\"node\":0,\"path\":\"translation\"",
                "\"path\":\"pointer\",\"extensions\":{\"KHR_animation_pointer\":{\"pointer\":\"/nodes/0/translation\"}}",
            );
        let asset = import_slice(source.as_bytes()).unwrap();
        assert!(asset.document.as_json().animations[0].channels.is_empty());

        let required = source.replace(
            "\"extensionsUsed\":[\"KHR_animation_pointer\"]",
            "\"extensionsUsed\":[\"KHR_animation_pointer\"],\"extensionsRequired\":[\"KHR_animation_pointer\"]",
        );
        assert!(matches!(
            import_slice(required.as_bytes()),
            Err(ImportError::Gltf(gltf::Error::Validation(_)))
        ));
    }

    #[test]
    fn validates_node_less_animation_channel_envelopes_before_ignoring_them() {
        let source = String::from_utf8(animation_source([0.0, 1.0], "VEC3", 2, None))
            .unwrap()
            .replace(
                "\"asset\":{\"version\":\"2.0\"}",
                "\"asset\":{\"version\":\"2.0\"},\"extensionsUsed\":[\"KHR_animation_pointer\"]",
            )
            .replace(
                "\"node\":0,\"path\":\"translation\"",
                "\"path\":\"pointer\",\"extensions\":{\"KHR_animation_pointer\":{\"pointer\":\"/nodes/0/translation\"}}",
            );
        for invalid in [
            source.replace("\"sampler\":0,", ""),
            source.replace("\"sampler\":0,", "\"sampler\":7,"),
            source.replace("\"path\":\"pointer\",", ""),
            source.replace(
                "\"extensions\":{\"KHR_animation_pointer\":{\"pointer\":\"/nodes/0/translation\"}}",
                "\"extensions\":[]",
            ),
        ] {
            assert!(import_slice(invalid.as_bytes()).is_err());
        }
    }

    #[test]
    fn ignores_unimplemented_details_of_node_less_animation_channels() {
        let source = String::from_utf8(animation_source([0.0, 1.0], "VEC3", 2, None))
            .unwrap()
            .replace(
                "\"asset\":{\"version\":\"2.0\"}",
                "\"asset\":{\"version\":\"2.0\"},\"extensionsUsed\":[\"KHR_animation_pointer\"]",
            )
            .replace(
                "\"node\":0,\"path\":\"translation\"",
                "\"path\":\"pointer\",\"extensions\":{\"KHR_animation_pointer\":{\"pointer\":\"/nodes/0/translation\"}}",
            );
        for ignored in [
            source.replace("\"path\":\"pointer\",", "\"path\":\"future-path\","),
            source.replace("/nodes/0/translation", "not-a-json-pointer"),
            source.replace("\"pointer\":\"/nodes/0/translation\"", "\"pointer\":7"),
        ] {
            let asset = import_slice(ignored.as_bytes()).unwrap();
            assert!(asset.document.as_json().animations[0].channels.is_empty());
        }
    }

    #[test]
    fn rejects_empty_animation_channel_and_sampler_arrays() {
        for field in ["channels", "samplers"] {
            let mut source: serde_json::Value =
                serde_json::from_slice(&animation_source([0.0, 1.0], "VEC3", 2, None)).unwrap();
            source["animations"][0][field] = serde_json::json!([]);
            assert!(matches!(
                import_slice(source.to_string().as_bytes()),
                Err(ImportError::Source(
                    "animations must contain at least one channel and sampler"
                ))
            ));
        }
    }

    #[test]
    fn rejects_unsupported_texture_coordinate_morph_targets() {
        let source = include_str!("../../../examples/assets/triangle.gltf").replace(
            "\"material\": 0 }",
            "\"material\": 0, \"targets\": [{\"TEXCOORD_0\":1}] }",
        );
        assert!(matches!(
            import_slice(source.as_bytes()),
            Err(ImportError::Primitive {
                reason: "texture-coordinate and color morph targets are not supported",
                ..
            })
        ));
    }

    #[test]
    fn accessor_bounds_include_sparse_substitutions_and_allow_unbacked_values() {
        let bytes = base64::encode([1, 0, 0, 0, 0, 0, 0, 64]);
        let sparse = format!(
            r#"{{
                "asset":{{"version":"2.0"}},
                "buffers":[{{"byteLength":8,"uri":"data:application/octet-stream;base64,{bytes}"}}],
                "bufferViews":[{{"buffer":0,"byteLength":1}},{{"buffer":0,"byteOffset":4,"byteLength":4}}],
                "accessors":[{{"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1],"sparse":{{"count":1,"indices":{{"bufferView":0,"componentType":5121}},"values":{{"bufferView":1}}}}}}]
            }}"#
        );
        assert!(matches!(
            import_slice(sparse.as_bytes()),
            Err(ImportError::Buffer("accessor bounds do not match its data"))
        ));

        let unbacked = br#"{"asset":{"version":"2.0"},"accessors":[{"componentType":5126,"count":1,"type":"VEC3","min":[9,8,7],"max":[3,2,1]}]}"#;
        assert!(import_slice(unbacked).is_ok());
    }

    #[test]
    fn accepts_sparse_indices_sharing_generic_sparse_value_storage() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":8,"uri":"data:application/octet-stream;base64,AAAAAAAAgD8="}],
            "bufferViews":[{"buffer":0,"byteLength":8}],
            "accessors":[{"componentType":5126,"count":1,"type":"SCALAR","sparse":{"count":1,"indices":{"bufferView":0,"componentType":5121},"values":{"bufferView":0,"byteOffset":4}}}]
        }"#;
        assert!(import_slice(source).is_ok());
    }

    #[test]
    fn accepts_sparse_views_shared_by_distinct_accessors() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":8,"uri":"data:application/octet-stream;base64,AAAAAAAAgD8="}],
            "bufferViews":[{"buffer":0,"byteLength":1},{"buffer":0,"byteOffset":4,"byteLength":4}],
            "accessors":[
                {"componentType":5126,"count":1,"type":"SCALAR","sparse":{"count":1,"indices":{"bufferView":0,"componentType":5121},"values":{"bufferView":1}}},
                {"componentType":5126,"count":1,"type":"SCALAR","sparse":{"count":1,"indices":{"bufferView":0,"componentType":5121},"values":{"bufferView":1}}}
            ]
        }"#;
        assert!(import_slice(source).is_ok());
    }

    #[test]
    fn accepts_nonvertex_accessors_sharing_a_buffer_view() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":8,"uri":"data:application/octet-stream;base64,AAAAAAAAAAA="}],
            "bufferViews":[{"buffer":0,"byteLength":8}],
            "accessors":[
                {"bufferView":0,"componentType":5126,"count":1,"type":"SCALAR"},
                {"bufferView":0,"byteOffset":4,"componentType":5126,"count":1,"type":"SCALAR"}
            ]
        }"#;
        assert!(import_slice(source).is_ok());
    }

    #[test]
    fn distinct_vertex_accessors_sharing_a_view_require_byte_stride() {
        let source = include_str!("../../../examples/assets/triangle.gltf")
            .replace("\"byteLength\": 36", "\"byteLength\": 60")
            .replace(
                "\"bufferView\": 1, \"componentType\": 5126",
                "\"bufferView\": 0, \"byteOffset\": 36, \"componentType\": 5126",
            );
        assert!(matches!(
            import_slice(source.as_bytes()),
            Err(ImportError::Buffer(
                "buffer view shared by accessors requires byteStride"
            ))
        ));
    }

    #[test]
    fn accepts_unused_buffer_view_with_byte_stride() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":4,"uri":"data:application/octet-stream;base64,AAAAAA=="}],
            "bufferViews":[{"buffer":0,"byteLength":4,"byteStride":4}]
        }"#;
        assert!(import_slice(source).is_ok());
    }

    #[test]
    fn import_checks_accessor_ranges_before_reading_data() {
        let source = r#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":4,"uri":"data:application/octet-stream;base64,AAAAAA=="}],
            "bufferViews":[{"buffer":0,"byteLength":4}],
            "accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,1]}]
        }"#;
        assert!(matches!(
            import_slice(source.as_bytes()),
            Err(ImportError::Buffer("accessor data exceeds its buffer view"))
        ));
    }

    #[test]
    fn rejects_sparse_indices_that_are_unsorted_or_out_of_range() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":12}],
            "bufferViews":[{"buffer":0,"byteLength":2},{"buffer":0,"byteOffset":4,"byteLength":8}],
            "accessors":[{"componentType":5126,"count":3,"type":"SCALAR","sparse":{"count":2,"indices":{"bufferView":0,"componentType":5121},"values":{"bufferView":1}}}]
        }"#;
        let mut bin = vec![1, 1, 0, 0];
        bin.extend_from_slice(&1.0_f32.to_le_bytes());
        bin.extend_from_slice(&2.0_f32.to_le_bytes());
        assert!(matches!(
            import_slice(&glb(source, &bin)),
            Err(ImportError::Buffer(
                "sparse indices must increase within the accessor range"
            ))
        ));

        bin[1] = 3;
        assert!(matches!(
            import_slice(&glb(source, &bin)),
            Err(ImportError::Buffer(
                "sparse indices must increase within the accessor range"
            ))
        ));
    }

    #[test]
    fn rejects_meshes_with_inconsistent_morph_target_counts() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":36,"uri":"data:application/octet-stream;base64,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}],
            "bufferViews":[{"buffer":0,"byteLength":36}],
            "accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,1]}],
            "meshes":[{"primitives":[
                {"attributes":{"POSITION":0}},
                {"attributes":{"POSITION":0},"targets":[{"POSITION":0}]}
            ]}]
        }"#;
        assert!(matches!(
            import_slice(source),
            Err(ImportError::Scene(
                "mesh primitives have different morph target counts"
            ))
        ));
    }

    #[test]
    fn rejects_empty_morph_target_objects_and_arrays() {
        let source = include_str!("../../../examples/assets/triangle.gltf").replace(
            "\"indices\": 2, \"material\": 0 }",
            "\"indices\": 2, \"material\": 0, \"targets\": [{}] }",
        );
        assert!(matches!(
            import_slice(source.as_bytes()),
            Err(ImportError::Primitive {
                reason: "morph targets must contain at least one attribute",
                ..
            })
        ));

        let empty = source.replace("\"targets\": [{}]", "\"targets\": []");
        assert!(matches!(
            import_slice(empty.as_bytes()),
            Err(ImportError::Primitive {
                reason: "morph target arrays must not be empty",
                ..
            })
        ));
    }

    #[test]
    fn rejects_null_core_properties_and_morph_semantics_that_would_be_dropped() {
        let base = || {
            serde_json::from_str::<serde_json::Value>(include_str!(
                "../../../examples/assets/triangle.gltf"
            ))
            .unwrap()
        };
        let mut invalid = Vec::new();

        let mut source = base();
        source["meshes"][0]["primitives"][0]["targets"] = serde_json::json!(null);
        invalid.push(source);

        let mut source = base();
        source["meshes"][0]["primitives"][0]["targets"] = serde_json::json!([{"POSITION": null}]);
        invalid.push(source);

        let mut source = base();
        source["meshes"][0]["primitives"][0]["targets"] = serde_json::json!([{"JOINTS_0": 0}]);
        invalid.push(source);

        let mut source = base();
        source["nodes"][0]["matrix"] = serde_json::json!(null);
        invalid.push(source);

        let mut source = base();
        source["images"][0]["uri"] = serde_json::json!(null);
        source["images"][0]["bufferView"] = serde_json::json!(0);
        source["images"][0]["mimeType"] = serde_json::json!("image/png");
        invalid.push(source);

        let mut source = base();
        source["buffers"][0]["uri"] = serde_json::json!(null);
        invalid.push(source);

        for source in invalid {
            assert!(import_slice(source.to_string().as_bytes()).is_err());
        }

        let mut source = base();
        source["extras"] = serde_json::json!(null);
        assert!(import_slice(source.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn clamps_vertex_colors_to_the_gltf_range() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "buffers":[{"byteLength":84}],
            "bufferViews":[{"buffer":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":48}],
            "accessors":[
                {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},
                {"bufferView":1,"componentType":5126,"count":3,"type":"VEC4"}
            ],
            "meshes":[{"primitives":[{"attributes":{"POSITION":0,"COLOR_0":1}}]}]
        }"#;
        let mut bin = Vec::new();
        let values: [f32; 21] = [
            0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, -1.0, 2.0, 0.5, 3.0, -1.0, 2.0, 0.5, 3.0,
            -1.0, 2.0, 0.5, 3.0,
        ];
        for value in values {
            bin.extend_from_slice(&value.to_le_bytes());
        }
        let asset = import_slice(&glb(source, &bin)).unwrap();
        assert_eq!(
            asset.primitives().unwrap()[0].mesh.vertices()[0].color_0,
            [0.0, 1.0, 0.5, 1.0]
        );
    }

    #[test]
    fn rejects_skinned_nodes_without_skin_attributes() {
        let source = include_str!("../../../examples/assets/triangle.gltf")
            .replace(
                "  \"nodes\": [",
                "  \"skins\": [{\"joints\":[2]}],\n  \"nodes\": [",
            )
            .replace(
                "{ \"mesh\": 0, \"translation\": [-0.45, 0, 0],",
                "{ \"mesh\": 0, \"skin\": 0, \"translation\": [-0.45, 0, 0],",
            )
            .replace(
                "    { \"mesh\": 0, \"translation\": [0.45, 0, 0], \"scale\": [-0.55, 0.55, 1] }\n  ],",
                "    { \"mesh\": 0, \"translation\": [0.45, 0, 0], \"scale\": [-0.55, 0.55, 1] },\n    {}\n  ],",
            )
            .replace("\"nodes\": [0, 1]", "\"nodes\": [0, 1, 2]");
        assert!(matches!(
            import_slice(source.as_bytes()),
            Err(ImportError::Scene(
                "skinned nodes require JOINTS_0 and WEIGHTS_0"
            ))
        ));
    }

    #[test]
    fn validates_skin_common_roots_and_skeleton_ancestors() {
        let mut valid: serde_json::Value =
            serde_json::from_str(&weighted_triangle(5121, true)).unwrap();
        let bind_matrices = [
            Mat4::IDENTITY,
            Mat4::from_translation(Vec3::new(0.0, -3.0, 0.0)),
            Mat4::from_translation(Vec3::new(-1.0, 0.0, 0.0)),
            Mat4::from_scale(Vec3::splat(2.0)),
        ];
        let mut bind_bytes = Vec::new();
        for matrix in bind_matrices {
            for value in matrix.to_cols_array() {
                bind_bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        valid["buffers"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "byteLength": bind_bytes.len(),
                "uri": format!(
                    "data:application/octet-stream;base64,{}",
                    base64::encode(bind_bytes)
                )
            }));
        valid["bufferViews"].as_array_mut().unwrap().extend([
            serde_json::json!({"buffer": 2, "byteLength": 128}),
            serde_json::json!({"buffer": 2, "byteOffset": 128, "byteLength": 128}),
        ]);
        valid["accessors"].as_array_mut().unwrap().extend([
            serde_json::json!({
                "bufferView": 5,
                "componentType": 5126,
                "count": 2,
                "type": "MAT4"
            }),
            serde_json::json!({
                "bufferView": 6,
                "componentType": 5126,
                "count": 2,
                "type": "MAT4"
            }),
        ]);
        valid["nodes"] = serde_json::json!([
            {"mesh": 0, "skin": 0, "translation": [10, 0, 0]},
            {"mesh": 0, "skin": 0, "scale": [2, 2, 2]},
            {"translation": [0, 10, 0], "children": [3, 4]},
            {"translation": [1, 0, 0]},
            {"translation": [0, 2, 0]},
            {"mesh": 0, "skin": 1, "translation": [0, 0, 20]}
        ]);
        valid["scenes"][0]["nodes"] = serde_json::json!([0, 1, 2, 5]);
        valid["skins"] = serde_json::json!([
            {"joints": [3, 4], "skeleton": 2, "inverseBindMatrices": 5},
            {"joints": [3, 4], "skeleton": 2, "inverseBindMatrices": 6}
        ]);
        let asset = import_slice(valid.to_string().as_bytes()).unwrap();
        let instances = asset.instances().unwrap();
        assert_eq!(instances.len(), 3);
        assert_eq!(instances[0].skin_matrices.len(), 2);
        assert!(Arc::ptr_eq(
            &instances[0].skin_matrices,
            &instances[1].skin_matrices
        ));
        assert_ne!(instances[0].world_transform, instances[1].world_transform);
        assert_eq!(
            instances[0].skin_matrices[0],
            Mat4::from_translation(Vec3::new(1.0, 10.0, 0.0))
        );
        assert_eq!(
            instances[0].skin_matrices[1],
            Mat4::from_translation(Vec3::new(0.0, 9.0, 0.0))
        );
        assert!(!Arc::ptr_eq(
            &instances[0].skin_matrices,
            &instances[2].skin_matrices
        ));
        assert_eq!(
            instances[2].skin_matrices[0],
            Mat4::from_translation(Vec3::new(0.0, 10.0, 0.0))
        );
        assert_eq!(
            instances[2].skin_matrices[1],
            Mat4::from_translation(Vec3::new(0.0, 12.0, 0.0)) * Mat4::from_scale(Vec3::splat(2.0))
        );

        let mut missing_root = valid.clone();
        missing_root["scenes"][0]["nodes"] = serde_json::json!([0, 1]);
        assert!(matches!(
            import_slice(missing_root.to_string().as_bytes()),
            Err(ImportError::Scene(
                "skin joints must belong to the same scene as the skinned node"
            ))
        ));

        let mut other_scene_missing_root = valid.clone();
        other_scene_missing_root["scenes"] =
            serde_json::json!([{"nodes": [0, 1, 2]}, {"nodes": [0]}]);
        assert!(matches!(
            import_slice(other_scene_missing_root.to_string().as_bytes()),
            Err(ImportError::Scene(
                "skin joints must belong to the same scene as the skinned node"
            ))
        ));

        let mut duplicate_joint = valid.clone();
        duplicate_joint["skins"][0]["joints"] = serde_json::json!([3, 3]);
        assert!(matches!(
            import_slice(duplicate_joint.to_string().as_bytes()),
            Err(ImportError::Scene("skin joints must be unique"))
        ));

        let mut separate_roots = valid.clone();
        separate_roots["nodes"] = serde_json::json!([
            {"mesh": 0, "skin": 0},
            {"mesh": 0},
            {},
            {}
        ]);
        separate_roots["scenes"][0]["nodes"] = serde_json::json!([0, 1, 2, 3]);
        separate_roots["skins"] = serde_json::json!([{"joints": [2, 3]}]);
        assert!(matches!(
            import_slice(separate_roots.to_string().as_bytes()),
            Err(ImportError::Scene(
                "skin joints must share a common hierarchy root"
            ))
        ));

        let mut unrelated_skeleton = valid;
        unrelated_skeleton["nodes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({}));
        unrelated_skeleton["scenes"][0]["nodes"] = serde_json::json!([0, 1, 2, 5]);
        unrelated_skeleton["skins"][0]["skeleton"] = serde_json::json!(5);
        assert!(matches!(
            import_slice(unrelated_skeleton.to_string().as_bytes()),
            Err(ImportError::Scene(
                "skin skeleton must be the common root or one of its ancestors"
            ))
        ));
    }

    #[test]
    fn buffer_limit_is_checked_before_resolving_data_source() {
        let source = format!(
            r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":{},"uri":"data:application/octet-stream;base64,"}}]}}"#,
            MAX_BUFFER_BYTES + 1
        );
        let document = gltf::Gltf::from_slice(source.as_bytes()).unwrap().document;
        assert!(matches!(
            import_buffers(&document, None, None),
            Err(ImportError::Buffer("buffer exceeds the 96 MiB limit"))
        ));
    }

    #[test]
    fn glb_file_limit_is_checked_before_reading_its_header() {
        let mut prefix = b"glTF".to_vec();
        prefix.extend_from_slice(&[0; 8]);
        assert!(matches!(
            load_document(&mut std::io::Cursor::new(prefix), MAX_GLB_BYTES + 1),
            Err(ImportError::Source("GLB exceeds the 96 MiB file limit"))
        ));
    }

    #[test]
    fn glb_document_limit_is_checked_before_reading_json() {
        let chunk_length = MAX_DOCUMENT_BYTES + 4;
        let declared = 20 + chunk_length;
        let mut glb = vec![0; 20];
        glb[..4].copy_from_slice(b"glTF");
        glb[4..8].copy_from_slice(&2_u32.to_le_bytes());
        glb[8..12].copy_from_slice(&(declared as u32).to_le_bytes());
        glb[12..16].copy_from_slice(&(chunk_length as u32).to_le_bytes());
        glb[16..20].copy_from_slice(b"JSON");
        assert!(matches!(
            load_document(&mut std::io::Cursor::new(glb), declared as u64),
            Err(ImportError::Source("GLB JSON exceeds the 64 MiB limit"))
        ));
    }

    #[test]
    fn standalone_json_limit_is_checked_before_parsing() {
        assert!(matches!(
            load_document(
                &mut std::io::Cursor::new(b"{}  "),
                MAX_DOCUMENT_BYTES as u64 + 1,
            ),
            Err(ImportError::Source("glTF JSON exceeds the 64 MiB limit"))
        ));
    }

    #[test]
    fn glb_ignores_unknown_chunks_without_loading_them() {
        let json = minimal_json();
        let declared = 12 + 8 + json.len() + 8 + 4;
        let mut glb = vec![0; declared];
        glb[..4].copy_from_slice(b"glTF");
        glb[4..8].copy_from_slice(&2_u32.to_le_bytes());
        glb[8..12].copy_from_slice(&(declared as u32).to_le_bytes());
        glb[12..16].copy_from_slice(&(json.len() as u32).to_le_bytes());
        glb[16..20].copy_from_slice(b"JSON");
        glb[20..20 + json.len()].copy_from_slice(&json);
        let extra = 20 + json.len();
        glb[extra..extra + 4].copy_from_slice(&4_u32.to_le_bytes());
        glb[extra + 4..extra + 8].copy_from_slice(b"VEND");
        assert!(import_slice(&glb).unwrap().buffers.is_empty());
    }

    #[test]
    fn glb_rejects_invalid_versions_order_and_chunk_lengths() {
        let valid = glb(&minimal_json(), &[]);
        let json_length = u32::from_le_bytes(valid[12..16].try_into().unwrap()) as usize;
        let json_end = 20 + json_length;

        let mut wrong_version = valid.clone();
        wrong_version[4..8].copy_from_slice(&1_u32.to_le_bytes());
        assert!(matches!(
            import_slice(&wrong_version),
            Err(ImportError::Source("unsupported GLB version"))
        ));

        let mut wrong_first_chunk = valid.clone();
        wrong_first_chunk[16..20].copy_from_slice(b"BIN\0");
        assert!(matches!(
            import_slice(&wrong_first_chunk),
            Err(ImportError::Source("GLB must start with a JSON chunk"))
        ));

        let mut unaligned_chunk = valid.clone();
        unaligned_chunk[12..16].copy_from_slice(&((json_length - 1) as u32).to_le_bytes());
        assert!(matches!(
            import_slice(&unaligned_chunk),
            Err(ImportError::Source(
                "GLB chunk length is not four-byte aligned"
            ))
        ));

        let mut misplaced_bin = valid.clone();
        misplaced_bin.splice(
            json_end..json_end,
            [4, 0, 0, 0, b'V', b'E', b'N', b'D', 0, 0, 0, 0],
        );
        let declared = misplaced_bin.len() as u32;
        misplaced_bin[8..12].copy_from_slice(&declared.to_le_bytes());
        assert!(matches!(
            import_slice(&misplaced_bin),
            Err(ImportError::Source("GLB BIN chunk is not second"))
        ));

        let mut duplicate_json = valid.clone();
        let mut chunk = Vec::from((json_length as u32).to_le_bytes());
        chunk.extend_from_slice(b"JSON");
        chunk.extend_from_slice(&valid[20..json_end]);
        duplicate_json.splice(json_end..json_end, chunk);
        let declared = duplicate_json.len() as u32;
        duplicate_json[8..12].copy_from_slice(&declared.to_le_bytes());
        assert!(matches!(
            import_slice(&duplicate_json),
            Err(ImportError::Source("GLB has multiple JSON chunks"))
        ));

        let mut invalid_total_length = valid;
        let length = u32::from_le_bytes(invalid_total_length[8..12].try_into().unwrap());
        invalid_total_length[8..12].copy_from_slice(&(length - 1).to_le_bytes());
        assert!(matches!(
            import_slice(&invalid_total_length),
            Err(ImportError::Source("invalid GLB length"))
        ));
    }

    #[test]
    fn glb_imports_its_binary_chunk() {
        let json = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":4}]}"#;
        let asset = import_slice(&glb(json, &[1, 2, 3, 4])).unwrap();
        assert_eq!(&*asset.buffers[0], &[1, 2, 3, 4]);
    }

    #[test]
    fn glb_requires_a_bin_chunk_for_an_implicit_buffer() {
        let json = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":4}]}"#;
        let mut data = glb(json, &[]);
        data.truncate(data.len() - 8);
        let length = data.len() as u32;
        data[8..12].copy_from_slice(&length.to_le_bytes());
        assert!(matches!(
            import_slice(&data),
            Err(ImportError::Buffer("GLB binary chunk is missing"))
        ));
    }

    #[test]
    fn glb_accepts_json_trailing_whitespace() {
        let mut json = br#"{"asset":{"version":"2.0"}}"#.to_vec();
        json.extend_from_slice(b"\t\r\n");
        assert!(import_slice(&glb(&json, &[])).is_ok());
    }

    #[test]
    fn glb_requires_zero_binary_padding_and_ignores_unreferenced_bin_chunk() {
        let json = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":5}]}"#;
        let mut data = glb(json, &[1, 2, 3, 4, 5]);
        *data.last_mut().unwrap() = 1;
        assert!(matches!(
            import_slice(&data),
            Err(ImportError::Buffer("GLB binary padding must be zero"))
        ));

        assert!(
            import_slice(&glb(&minimal_json(), &[1, 2, 3, 4]))
                .unwrap()
                .buffers
                .is_empty()
        );
    }

    #[test]
    fn decoded_image_budget_is_checked_from_dimensions() {
        assert!(rgba8_size_within_limit(4096, 4096));
        assert!(!rgba8_size_within_limit(4097, 4096));
        assert!(!rgba8_size_within_limit(u32::MAX, u32::MAX));
    }

    #[test]
    fn imports_triangle_attributes_and_material_factors() {
        let gltf = r#"{
                "asset": {"version":"2.0"},
                "extensionsUsed": ["KHR_materials_emissive_strength"],
                "buffers": [{"byteLength":114,"uri":"data:application/octet-stream;base64,AAAAAAAAAAAAAAAAAACAPwAAAAAAAAAAAAAAAAAAgD8AAAAAAAAAAAAAAAAAAIA/AAAAAAAAAAAAAIA/AACAPwAAAAAAAAAAAACAPwAAAAAAAIA/AAAAAAAAgD8AAAAAAAAAAAAAgD8AAIA/AAABAAIA"}],
                "bufferViews": [
                    {"buffer":0,"byteOffset":0,"byteLength":36},
                    {"buffer":0,"byteOffset":36,"byteLength":24},
                    {"buffer":0,"byteOffset":60,"byteLength":48},
                    {"buffer":0,"byteOffset":108,"byteLength":6}
                ],
                "accessors": [
                    {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},
                    {"bufferView":1,"componentType":5126,"count":3,"type":"VEC2"},
                    {"bufferView":2,"componentType":5126,"count":3,"type":"VEC4"},
                    {"bufferView":3,"componentType":5123,"count":3,"type":"SCALAR","min":[0],"max":[2]}
                ],
                "materials": [{
                    "pbrMetallicRoughness": {"baseColorFactor":[0.25,0.5,0.75,1],"metallicFactor":0.4,"roughnessFactor":0.6},
                    "emissiveFactor":[0.1,0.2,0.3],
                    "extensions":{"KHR_materials_emissive_strength":{"emissiveStrength":2}}
                }],
                "meshes": [{"primitives":[{"attributes":{"POSITION":0,"TEXCOORD_0":1,"COLOR_0":2},"indices":3,"material":0}]}],
                "nodes": [
                    {"translation":[1,0,0],"children":[1]},
                    {"mesh":0,"translation":[0,2,0]},
                    {"mesh":0,"translation":[4,0,0]}
                ],
                "scenes": [{"nodes":[0,2]}],
                "scene": 0
            }"#;
        let asset = import_slice(gltf.as_bytes()).unwrap();
        let primitives = asset.primitives().unwrap();

        assert_eq!(primitives.len(), 1);
        let primitive = &primitives[0];
        assert_eq!(primitive.mesh.vertices().len(), 3);
        assert_eq!(primitive.mesh.indices(), [0, 1, 2]);
        assert_eq!(primitive.mesh.vertices()[0].normal, [0.0, 0.0, 1.0]);
        assert_eq!(primitive.mesh.vertices()[1].color_0, [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(
            primitive.material.renderer_data(&HashMap::new()).unwrap(),
            MaterialData {
                base_color_factor: [0.25, 0.5, 0.75, 1.0],
                metallic_factor: 0.4,
                roughness_factor: 0.6,
                emissive_factor: [0.1, 0.2, 0.3],
                emissive_strength: 2.0,
                base_color_texture: None,
                metallic_roughness_texture: None,
                ..MaterialData::default()
            }
        );
        let instances = asset.instances().unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!((instances[0].node_index, instances[0].mesh_index), (1, 0));
        assert_eq!(instances[0].world_transform[3][0], 1.0);
        assert_eq!(instances[0].world_transform[3][1], 2.0);
        assert_eq!((instances[1].node_index, instances[1].mesh_index), (2, 0));
        assert_eq!(instances[1].world_transform[3][0], 4.0);
    }

    #[test]
    fn imports_file_based_gltf_source() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/assets/triangle.gltf");
        let asset = import(path).unwrap();
        assert_eq!(asset.primitives().unwrap().len(), 1);
    }

    #[test]
    fn rejects_position_bounds_that_disagree_with_accessor_data() {
        let source = include_str!("../../../examples/assets/triangle.gltf")
            .replace("\"max\": [1, 1, 0]", "\"max\": [1, 1, 1]");
        assert!(matches!(
            import_slice(source.as_bytes()),
            Err(ImportError::Buffer("accessor bounds do not match its data"))
        ));
    }

    #[test]
    fn rejects_non_finite_float_accessors_without_declared_bounds() {
        let uri = base64::encode(f32::INFINITY.to_le_bytes());
        let source = format!(
            r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":4,"uri":"data:application/octet-stream;base64,{uri}"}}],"bufferViews":[{{"buffer":0,"byteLength":4}}],"accessors":[{{"bufferView":0,"componentType":5126,"count":1,"type":"SCALAR"}}]}}"#
        );
        assert!(matches!(
            import_slice(source.as_bytes()),
            Err(ImportError::Buffer(
                "accessor data contains a non-finite value"
            ))
        ));

        let mut bytes = [0; 8];
        bytes[0] = 0;
        bytes[4..].copy_from_slice(&f32::NAN.to_le_bytes());
        let uri = base64::encode(bytes);
        let source = format!(
            r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":8,"uri":"data:application/octet-stream;base64,{uri}"}}],"bufferViews":[{{"buffer":0,"byteLength":1}},{{"buffer":0,"byteOffset":4,"byteLength":4}}],"accessors":[{{"componentType":5126,"count":1,"type":"SCALAR","sparse":{{"count":1,"indices":{{"bufferView":0,"componentType":5121}},"values":{{"bufferView":1}}}}}}]}}"#
        );
        assert!(matches!(
            import_slice(source.as_bytes()),
            Err(ImportError::Buffer(
                "accessor data contains a non-finite value"
            ))
        ));
    }

    #[test]
    fn rejects_declared_bounds_that_disagree_for_indices_too() {
        let source = include_str!("../../../examples/assets/triangle.gltf")
            .replace("\"max\": [2]", "\"max\": [1]");
        assert!(matches!(
            import_slice(source.as_bytes()),
            Err(ImportError::Buffer("accessor bounds do not match its data"))
        ));
    }

    #[test]
    fn embedded_gltf_fixture_preserves_shared_mesh_instances() {
        let asset = import_slice(include_bytes!("../../../examples/assets/triangle.gltf")).unwrap();
        let primitives = asset.primitives().unwrap();
        let instances = asset.instances().unwrap();
        assert_eq!(primitives.len(), 1);
        assert_eq!(asset.images.len(), 2);
        assert!(
            asset
                .images
                .iter()
                .any(|image| image.color_space == ImageColorSpace::Linear)
        );
        assert!(
            asset
                .images
                .iter()
                .any(|image| image.color_space == ImageColorSpace::Srgb)
        );
        assert_eq!(asset.images[0].image_index, 0);
        assert_eq!(asset.images[0].extent.width(), 2);
        assert!(std::sync::Arc::ptr_eq(
            &asset.images[0].encoded,
            &asset.images[1].encoded
        ));
        let image = asset.images[0].upload(ImageId::new(1).unwrap()).unwrap();
        assert_eq!(
            &*image.rgba8,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255
            ]
        );
        assert_eq!(primitives[0].mesh.indices(), [0, 1, 2]);
        assert_eq!(primitives[0].mesh.vertices()[1].uv_0, [1.0, 0.0]);
        let base = primitives[0].material.base_color_texture.unwrap();
        let metallic_roughness = primitives[0].material.metallic_roughness_texture.unwrap();
        assert_eq!(base.image_index, metallic_roughness.image_index);
        assert_eq!(base.sampler, metallic_roughness.sampler);
        assert_eq!(base.sampler.min_filter, TextureFilter::Nearest);
        assert_eq!(base.sampler.mipmap_mode, MipmapMode::Nearest);
        assert_eq!(base.sampler.wrap_u, TextureWrap::MirroredRepeat);
        assert_eq!(base.sampler.wrap_v, TextureWrap::ClampToEdge);
        let image_id = ImageId::new(7).unwrap();
        let data_id = ImageId::new(8).unwrap();
        let bound = primitives[0]
            .material
            .renderer_data(&HashMap::from([
                ((0, ImageColorSpace::Srgb), image_id),
                ((0, ImageColorSpace::Linear), data_id),
            ]))
            .unwrap();
        assert_eq!(bound.base_color_texture.unwrap().image, image_id);
        assert_eq!(bound.metallic_roughness_texture.unwrap().image, data_id);
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].mesh_index, 0);
        assert_eq!(instances[0].world_transform[3][0], -0.45);
        assert_eq!(instances[1].mesh_index, 0);
        assert_eq!(instances[1].world_transform[0][0], -0.55);
    }

    #[test]
    fn normal_textures_are_imported_as_linear_data() {
        let gltf = include_str!("../../../examples/assets/triangle.gltf").replace(
            r#""pbrMetallicRoughness": {
        "baseColorFactor": [1, 1, 1, 1],
        "baseColorTexture": { "index": 0 },
        "metallicFactor": 0.1,
        "roughnessFactor": 0.8,
        "metallicRoughnessTexture": { "index": 0 }
      }"#,
            r#""pbrMetallicRoughness": { "baseColorFactor": [1, 1, 1, 1] },
      "normalTexture": { "index": 0 }"#,
        );
        assert!(!gltf.contains("metallicRoughnessTexture"));
        let asset = import_slice(gltf.as_bytes()).unwrap();
        assert_eq!(asset.images.len(), 1);
        assert_eq!(asset.images[0].color_space, ImageColorSpace::Linear);
        let material = asset.primitives().unwrap()[0].material;
        assert!(material.base_color_texture.is_none());
        assert_eq!(
            material.normal_texture.unwrap().color_space,
            ImageColorSpace::Linear
        );
    }

    #[test]
    fn rejects_unknown_required_extensions() {
        let gltf = br#"{
            "asset":{"version":"2.0"},
            "extensionsUsed":["EXT_unknown"],
            "extensionsRequired":["EXT_unknown"]
        }"#;
        assert!(matches!(
            import_slice(gltf),
            Err(ImportError::Gltf(gltf::Error::Validation(_)))
        ));
    }

    #[test]
    fn extension_sets_and_payloads_must_be_objects() {
        let valid = br#"{"asset":{"version":"2.0"},"extensionsUsed":["EXT_shape"],"extensions":{"EXT_shape":{"value":null}}}"#;
        assert!(validate_extensions(valid).is_ok());

        for invalid in [
            br#"{"asset":{"version":"2.0"},"extensions":[]}"#.as_slice(),
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["EXT_shape"],"extensions":{"EXT_shape":null}}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["EXT_shape"],"extensions":{"EXT_shape":"value"}}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["EXT_shape"],"extensions":{"EXT_shape":[]}}"#,
        ] {
            assert!(matches!(
                validate_extensions(invalid),
                Err(ImportError::Source("invalid glTF metadata"))
            ));
        }
    }

    #[test]
    fn ignores_nulls_in_unknown_properties_but_rejects_them_in_core_fields() {
        for valid in [
            br#"{"asset":{"version":"2.0"},"vendorData":null}"#.as_slice(),
            br#"{"asset":{"version":"2.0","vendorData":{"nested":null}}}"#,
            br#"{"asset":{"version":"2.0"},"nodes":[{"vendorData":{"nested":null}}]}"#,
            br#"{"asset":{"version":"2.0"},"extras":null}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["EXT_shape"],"extensions":{"EXT_shape":{"nested":null}}}"#,
        ] {
            assert!(validate_extensions(valid).is_ok());
            assert!(parse_document(valid).is_ok());
        }

        for invalid in [
            br#"{"asset":{"version":null}}"#.as_slice(),
            br#"{"asset":{"version":"2.0"},"nodes":[{"mesh":null}]}"#,
        ] {
            assert!(matches!(
                validate_extensions(invalid),
                Err(ImportError::Source("invalid glTF metadata"))
            ));
            assert!(parse_document(invalid).is_err());
        }
    }

    #[test]
    fn integer_accessor_bounds_fit_the_component_type_with_or_without_storage() {
        let cases = [
            (
                5120,
                i8::MIN as i64,
                i8::MAX as i64,
                [i8::MIN.to_le_bytes(), i8::MAX.to_le_bytes()].concat(),
            ),
            (5121, 0, u8::MAX as i64, vec![0, u8::MAX]),
            (
                5122,
                i16::MIN as i64,
                i16::MAX as i64,
                [i16::MIN.to_le_bytes(), i16::MAX.to_le_bytes()].concat(),
            ),
            (
                5123,
                0,
                u16::MAX as i64,
                [0_u16.to_le_bytes(), u16::MAX.to_le_bytes()].concat(),
            ),
            (
                5125,
                0,
                u32::MAX as i64,
                [0_u32.to_le_bytes(), u32::MAX.to_le_bytes()].concat(),
            ),
        ];

        for (component_type, minimum, maximum, bytes) in cases {
            let unbacked = format!(
                r#"{{"asset":{{"version":"2.0"}},"accessors":[{{"componentType":{component_type},"count":1,"type":"SCALAR","min":[{minimum}],"max":[{maximum}]}}]}}"#
            );
            let document = parse_document(unbacked.as_bytes()).unwrap();
            let accessor = document.document.accessors().next().unwrap();
            assert!(validate_accessor_bounds(&accessor, &[]).is_ok());

            let stored = format!(
                r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":{},"uri":"data:application/octet-stream;base64,{}"}}],"bufferViews":[{{"buffer":0,"byteLength":{}}}],"accessors":[{{"bufferView":0,"componentType":{component_type},"count":2,"type":"SCALAR","min":[{minimum}],"max":[{maximum}]}}]}}"#,
                bytes.len(),
                base64::encode(&bytes),
                bytes.len()
            );
            let document = parse_document(stored.as_bytes()).unwrap();
            let accessor = document.document.accessors().next().unwrap();
            assert!(validate_accessor_bounds(&accessor, &[bytes]).is_ok());

            for (minimum, maximum) in [
                (minimum.saturating_sub(1), maximum),
                (minimum, maximum.saturating_add(1)),
            ] {
                let invalid = format!(
                    r#"{{"asset":{{"version":"2.0"}},"accessors":[{{"componentType":{component_type},"count":1,"type":"SCALAR","min":[{minimum}],"max":[{maximum}]}}]}}"#
                );
                assert!(parse_document(invalid.as_bytes()).is_err());
            }
        }

        for source in [
            br#"{"asset":{"version":"2.0"},"accessors":[{"componentType":5121,"count":1,"type":"SCALAR","min":[256],"max":[256]}]}"#.as_slice(),
            br#"{"asset":{"version":"2.0"},"accessors":[{"componentType":5121,"count":1,"type":"SCALAR","min":[255.00000000000000001],"max":[255.00000000000000001]}]}"#,
        ] {
            assert!(parse_document(source).is_err());
            assert!(import_slice(source).is_err());
        }

        let stored = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":1,"uri":"data:application/octet-stream;base64,AA=="}],"bufferViews":[{"buffer":0,"byteLength":1}],"accessors":[{"bufferView":0,"componentType":5121,"count":1,"type":"SCALAR","min":[256],"max":[256]}]}"#;
        assert!(parse_document(stored).is_err());
        assert!(import_slice(stored).is_err());
    }

    #[test]
    fn scene_roots_must_be_nonempty_and_unique() {
        for valid in [
            br#"{"asset":{"version":"2.0"},"nodes":[{}],"scenes":[{"nodes":[0.0]}]}"#.as_slice(),
            br#"{"asset":{"version":"2.0"},"nodes":[{}],"scenes":[{"nodes":[-0]}]}"#,
            br#"{"asset":{"version":"2.0"},"nodes":[{},{}],"scenes":[{"nodes":[1.0]}]}"#,
            br#"{"asset":{"version":"2.0"},"nodes":[{},{}],"scenes":[{"nodes":[1e0]}]}"#,
        ] {
            assert!(validate_extensions(valid).is_ok());
            assert!(import_slice(valid).is_ok());
        }

        for invalid in [
            br#"{"asset":{"version":"2.0"},"scenes":[{"nodes":[]}] }"#.as_slice(),
            br#"{"asset":{"version":"2.0"},"nodes":[{}],"scenes":[{"nodes":[0,0]}]}"#,
            br#"{"asset":{"version":"2.0"},"nodes":[{},{}],"scenes":[{"nodes":[1.0000000000000000000000000001]}]}"#,
        ] {
            assert!(matches!(
                validate_extensions(invalid),
                Err(ImportError::Source("invalid glTF metadata"))
            ));
        }
    }

    #[test]
    fn schema_nonempty_arrays_reject_empty_core_and_light_lists() {
        for property in [
            "accessors",
            "animations",
            "buffers",
            "bufferViews",
            "cameras",
            "images",
            "materials",
            "meshes",
            "nodes",
            "samplers",
            "scenes",
            "skins",
            "textures",
        ] {
            let mut source = serde_json::json!({"asset":{"version":"2.0"}});
            source[property] = serde_json::json!([]);
            assert!(
                matches!(
                    import_slice(source.to_string().as_bytes()),
                    Err(ImportError::Source("invalid glTF metadata"))
                ),
                "{property} must not be empty when present"
            );
        }

        let empty_children = br#"{"asset":{"version":"2.0"},"nodes":[{"children":[]}] }"#;
        assert!(matches!(
            import_slice(empty_children),
            Err(ImportError::Source("invalid glTF metadata"))
        ));

        let empty_lights = br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_lights_punctual"],"extensions":{"KHR_lights_punctual":{"lights":[]}}}"#;
        assert!(matches!(
            import_slice(empty_lights),
            Err(ImportError::Source(
                "KHR_lights_punctual must define at least one light"
            ))
        ));
    }

    #[test]
    fn scene_roots_cannot_reference_missing_nodes() {
        let invalid = br#"{"asset":{"version":"2.0"},"scenes":[{"nodes":[0]}]}"#;
        assert!(
            import_slice(invalid)
                .and_then(|asset| asset.instances().map(|_| ()))
                .is_err()
        );
    }

    #[test]
    fn extension_declarations_must_not_repeat_names() {
        for invalid in [
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["EXT_x","EXT_x"]}"#.as_slice(),
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["EXT_x"],"extensionsRequired":["EXT_x","EXT_x"]}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":[]}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["EXT_x"],"extensionsRequired":[]}"#,
        ] {
            assert!(matches!(
                validate_extensions(invalid),
                Err(ImportError::Source("invalid glTF metadata"))
            ));
        }
    }

    #[test]
    fn imports_negative_normal_texture_scale() {
        let mut source: serde_json::Value =
            serde_json::from_str(include_str!("../../../examples/assets/triangle.gltf")).unwrap();
        source["materials"][0]["normalTexture"] = serde_json::json!({"index": 0, "scale": -1.0});
        let material = import_slice(source.to_string().as_bytes())
            .unwrap()
            .primitives()
            .unwrap()
            .remove(0)
            .material;
        assert_eq!(material.normal_scale, -1.0);
    }

    #[test]
    fn imports_transmission_factor_and_linear_texture() {
        let mut source: serde_json::Value =
            serde_json::from_str(include_str!("../../../examples/assets/triangle.gltf")).unwrap();
        source["extensionsUsed"] = serde_json::json!(["KHR_materials_transmission"]);
        source["materials"][0]["extensions"] = serde_json::json!({
            "KHR_materials_transmission": {
                "transmissionFactor": 0.75,
                "transmissionTexture": {"index": 0}
            }
        });
        let asset = import_slice(&serde_json::to_vec(&source).unwrap()).unwrap();
        let material = asset.primitives().unwrap().remove(0).material;
        let texture = material.transmission_texture.unwrap();
        assert_eq!(material.transmission_factor, 0.75);
        assert_eq!(texture.color_space, ImageColorSpace::Linear);
        assert_eq!(texture.sampler.mag_filter, TextureFilter::Nearest);

        let image_ids = asset
            .images
            .iter()
            .enumerate()
            .map(|(index, image)| {
                (
                    (image.image_index, image.color_space),
                    ImageId::new(index as u64 + 1).unwrap(),
                )
            })
            .collect();
        let data = material.renderer_data(&image_ids).unwrap();
        assert_eq!(data.transmission_factor, 0.75);
        assert_eq!(
            data.transmission_texture.unwrap().image,
            ImageId::new(1).unwrap()
        );
    }

    #[test]
    fn transmission_defaults_to_zero_and_checks_extension_constraints() {
        let defaulted = br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission"],"materials":[{"extensions":{"KHR_materials_transmission":{}}}]}"#;
        let asset = import_slice(defaulted).unwrap();
        assert_eq!(
            asset
                .document
                .materials()
                .next()
                .unwrap()
                .transmission()
                .unwrap()
                .transmission_factor(),
            0.0
        );

        for invalid in [
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission"],"materials":[{"extensions":{"KHR_materials_transmission":{"transmissionFactor":1.01}}}]}"#.as_slice(),
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission"],"materials":[{"extensions":{"KHR_materials_transmission":{"transmissionFactor":1.00000001}}}]}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission"],"materials":[{"extensions":{"KHR_materials_transmission":{"transmissionFactor":-1e-100}}}]}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission"],"materials":[{"extensions":{"KHR_materials_transmission":{"transmissionFactor":null}}}]}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission"],"materials":[{"extensions":{"KHR_materials_transmission":{"transmissionTexture":null}}}]}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission"],"materials":[{"extensions":{"KHR_materials_transmission":{"transmissionTexture":[]}}}]}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission","KHR_materials_unlit"],"materials":[{"extensions":{"KHR_materials_transmission":{},"KHR_materials_unlit":{}}}]}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission","KHR_materials_pbrSpecularGlossiness"],"materials":[{"extensions":{"KHR_materials_transmission":{},"KHR_materials_pbrSpecularGlossiness":{}}}]}"#,
        ] {
            assert!(import_slice(invalid).is_err());
        }
    }

    #[test]
    fn rejects_material_extension_exclusions() {
        for invalid in [
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_unlit","KHR_materials_emissive_strength"],"materials":[{"extensions":{"KHR_materials_unlit":{},"KHR_materials_emissive_strength":{}}}]}"#.as_slice(),
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_unlit","KHR_materials_specular"],"materials":[{"extensions":{"KHR_materials_unlit":{},"KHR_materials_specular":{}}}]}"#,
            br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_specular","KHR_materials_pbrSpecularGlossiness"],"materials":[{"extensions":{"KHR_materials_specular":{},"KHR_materials_pbrSpecularGlossiness":{}}}]}"#,
        ] {
            assert!(matches!(
                import_slice(invalid),
                Err(ImportError::Source("incompatible material extensions"))
            ));
        }
    }

    #[test]
    fn required_transmission_is_rejected_until_its_material_rendering_is_supported() {
        let invalid = br#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_transmission"],"extensionsRequired":["KHR_materials_transmission"],"materials":[{"extensions":{"KHR_materials_transmission":{}}}]}"#;
        assert!(matches!(
            import_slice(invalid),
            Err(ImportError::Source(
                "a required glTF extension is not fully supported"
            ))
        ));
    }

    #[test]
    fn ignores_optional_extension_fallbacks_and_rejects_required_unsupported_data() {
        let source = include_str!("../../../examples/assets/triangle.gltf");
        let optional = source
            .replace(
                "\"asset\": { \"version\": \"2.0\" },",
                "\"asset\": { \"version\": \"2.0\" },\n  \"extensionsUsed\": [\"EXT_optional\", \"KHR_materials_specular\"],\n  \"extras\": {\"extensions\": {\"EXT_in_extras\": {}}},",
            )
            .replace(
                "\"pbrMetallicRoughness\": {",
                "\"extensions\": {\"EXT_optional\": {}, \"KHR_materials_specular\": {\"specularFactor\": 0.2, \"specularTexture\": {\"index\": 0}}},\n      \"pbrMetallicRoughness\": {",
            )
            .replace(
                "\"baseColorTexture\": { \"index\": 0 },",
                "\"baseColorTexture\": { \"index\": 0, \"extensions\": {\"EXT_optional\": {}} },",
            )
            .replace(
                "\"textures\": [{ \"sampler\": 0, \"source\": 0 }],",
                "\"textures\": [{ \"sampler\": 0, \"source\": 0, \"extensions\": {\"EXT_optional\": {}} }],",
            )
            .replace(
                "\"material\": 0 }",
                "\"material\": 0, \"extensions\": {\"EXT_optional\": {}} }",
            );
        let asset = import_slice(optional.as_bytes()).unwrap();
        let material = asset.primitives().unwrap().remove(0).material;
        assert_eq!(material.specular_factor, 1.0);
        assert!(material.base_color_texture.is_some());

        let required = optional.replace(
            "\"extensionsUsed\": [\"EXT_optional\", \"KHR_materials_specular\"],",
            "\"extensionsUsed\": [\"EXT_optional\", \"KHR_materials_specular\"],\n  \"extensionsRequired\": [\"KHR_materials_specular\"],",
        );
        assert!(matches!(
            import_slice(required.as_bytes()),
            Err(ImportError::Gltf(gltf::Error::Validation(_)))
        ));

        let unlisted = optional.replace(
            "\"extensionsUsed\": [\"EXT_optional\", \"KHR_materials_specular\"],",
            "",
        );
        assert!(matches!(
            import_slice(unlisted.as_bytes()),
            Err(ImportError::Source(
                "extensionsRequired and extension payloads must be listed in extensionsUsed"
            ))
        ));
        assert!(matches!(
            import_slice(
                br#"{"asset":{"version":"2.0"},"extensionsRequired":["KHR_materials_unlit"]}"#
            ),
            Err(ImportError::Source(
                "extensionsRequired and extension payloads must be listed in extensionsUsed"
            ))
        ));
    }

    #[test]
    fn rejects_sheared_node_matrices_but_accepts_trs_matrices() {
        let sheared = br#"{
            "asset":{"version":"2.0"},
            "nodes":[{"matrix":[1,0,0,0,0.5,1,0,0,0,0,1,0,0,0,0,1]}]
        }"#;
        assert!(matches!(
            import_slice(sheared),
            Err(ImportError::Scene(
                "node transform is not finite or cannot be decomposed to TRS"
            ))
        ));

        let trs = br#"{
            "asset":{"version":"2.0"},
            "nodes":[{"matrix":[2,0,0,0,0,3,0,0,0,0,-4,0,1,2,3,1]}]
        }"#;
        assert!(import_slice(trs).is_ok());
    }

    #[test]
    fn rejects_nodes_that_specify_matrix_and_trs_together() {
        let mut source: serde_json::Value =
            serde_json::from_str(include_str!("../../../examples/assets/triangle.gltf")).unwrap();
        source["nodes"][0]["matrix"] =
            serde_json::json!([1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]);
        assert!(matches!(
            import_slice(source.to_string().as_bytes()),
            Err(ImportError::Scene(
                "node must use either matrix or TRS, not both"
            ))
        ));
    }

    #[test]
    fn validates_all_node_trees_and_scene_roots() {
        let invalid = [
            r#"{"asset":{"version":"2.0"},"nodes":[{"children":[2]},{"children":[2]},{}],"scenes":[{"nodes":[0,1]}]}"#,
            r#"{"asset":{"version":"2.0"},"nodes":[{"children":[1]},{"children":[0]}],"scenes":[{"nodes":[]}]}"#,
            r#"{"asset":{"version":"2.0"},"nodes":[{"children":[1]},{}],"scenes":[{"nodes":[1]}]}"#,
        ];
        for source in invalid {
            let document = gltf::Gltf::from_slice_without_validation(source.as_bytes())
                .unwrap()
                .document;
            assert!(NodeHierarchy::from_document(&document).is_err());
        }

        let valid = r#"{"asset":{"version":"2.0"},"nodes":[{"children":[1,2]},{},{}],"scenes":[{"nodes":[0]}]}"#;
        let document = gltf::Gltf::from_slice_without_validation(valid.as_bytes())
            .unwrap()
            .document;
        let hierarchy = NodeHierarchy::from_document(&document).unwrap();
        assert!(hierarchy.is_ancestor(0, 1));
        assert!(!hierarchy.is_ancestor(1, 2));
    }

    #[test]
    fn rejects_animation_targets_that_use_matrix_transforms() {
        let source = br#"{
            "asset":{"version":"2.0"},
            "accessors":[{"componentType":5126,"count":1,"type":"SCALAR","min":[0],"max":[0]},{"componentType":5126,"count":1,"type":"VEC3"}],
            "nodes":[{"matrix":[1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1]}],
            "animations":[{"samplers":[{"input":0,"output":1}],"channels":[{"sampler":0,"target":{"node":0,"path":"translation"}}]}]
        }"#;
        let document = gltf::Gltf::from_slice_without_validation(source)
            .unwrap()
            .document;
        assert!(matches!(
            validate_document_shape(&document),
            Err(ImportError::Scene(
                "animation targets a node with a matrix transform"
            ))
        ));
    }

    #[test]
    fn rejects_morph_weight_animation_on_matrix_transform_nodes() {
        let mut source: serde_json::Value =
            serde_json::from_str(include_str!("../../../examples/assets/triangle.gltf")).unwrap();
        source["nodes"][0] = serde_json::json!({
            "matrix": [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1],
            "mesh": 0
        });
        source["meshes"][0]["primitives"][0]["targets"] = serde_json::json!([{"POSITION": 0}]);
        source["accessors"].as_array_mut().unwrap().extend([
            serde_json::json!({
                "componentType": 5126,
                "count": 1,
                "type": "SCALAR",
                "min": [0],
                "max": [0]
            }),
            serde_json::json!({"componentType": 5126, "count": 1, "type": "SCALAR"}),
        ]);
        source["animations"] = serde_json::json!([{
            "samplers": [{"input": 3, "output": 4}],
            "channels": [{"sampler": 0, "target": {"node": 0, "path": "weights"}}]
        }]);
        assert!(matches!(
            import_slice(source.to_string().as_bytes()),
            Err(ImportError::Scene(
                "animation targets a node with a matrix transform"
            ))
        ));
    }

    #[test]
    fn rejects_animation_targets_with_missing_nodes_without_panicking() {
        let source = br#"{"asset":{"version":"2.0"},"accessors":[{"componentType":5126,"count":1,"type":"SCALAR","min":[0],"max":[0]},{"componentType":5126,"count":1,"type":"VEC3"}],"nodes":[],"animations":[{"samplers":[{"input":0,"output":1}],"channels":[{"sampler":0,"target":{"node":0,"path":"translation"}}]}]}"#;
        let document = gltf::Gltf::from_slice_without_validation(source)
            .unwrap()
            .document;
        assert!(matches!(
            validate_document_shape(&document),
            Err(ImportError::Scene("animation targets a missing node"))
        ));
    }

    #[test]
    fn counts_unique_vertex_accessors_when_meshes_reuse_them() {
        let source = include_str!("../../../examples/assets/triangle.gltf").replace(
            "{ \"attributes\": {\"POSITION\": 0, \"TEXCOORD_0\": 1}, \"indices\": 2, \"material\": 0 }",
            "{ \"attributes\": {\"POSITION\": 0, \"TEXCOORD_0\": 1}, \"indices\": 2, \"material\": 0 }, { \"attributes\": {\"POSITION\": 0, \"TEXCOORD_0\": 1}, \"indices\": 2, \"material\": 0 }",
        );
        let document = gltf::Gltf::from_slice(source.as_bytes()).unwrap();
        assert!(validate_vertex_accessor_layout(&document.document).is_ok());
    }

    #[test]
    fn checks_the_json_asset_version_separately_from_glb_version() {
        let old_version = gltf::Gltf::from_slice(br#"{"asset":{"version":"1.0"}}"#).unwrap();
        assert!(matches!(
            validate_asset_version(&old_version.document),
            Err(ImportError::Source("glTF asset version is not supported"))
        ));

        let new_minimum =
            gltf::Gltf::from_slice(br#"{"asset":{"version":"2.3","minVersion":"2.1"}}"#).unwrap();
        assert!(matches!(
            validate_asset_version(&new_minimum.document),
            Err(ImportError::Source("glTF asset version is not supported"))
        ));

        let forward_compatible =
            gltf::Gltf::from_slice(br#"{"asset":{"version":"2.3","minVersion":"2.0"}}"#).unwrap();
        assert!(validate_asset_version(&forward_compatible.document).is_ok());
    }

    #[test]
    fn accepts_schema_integers_encoded_as_zero_fraction_numbers() {
        for byte_length in ["4.0", "40e-1", "0.4e1"] {
            let source = format!(
                r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":{byte_length},"uri":"data:application/octet-stream;base64,AAAAAA=="}}]}}"#
            );
            let result = import_slice(source.as_bytes());
            assert!(result.is_ok(), "{byte_length}: {:?}", result.err());
        }
    }

    #[test]
    fn accepts_negative_zero_for_nonnegative_integer_fields() {
        let source = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":1,"uri":"data:application/octet-stream;base64,AA=="}],"bufferViews":[{"buffer":0,"byteOffset":-0.0,"byteLength":1}]}"#;
        assert!(import_slice(source).is_ok());
    }

    #[test]
    fn normalizes_only_integral_json_number_tokens() {
        let source = br#"{"length":4.0,"exponent":1e2,"fraction":4.25,"rounds":999999999999999.9,"almost_one":1.00000000000000001,"negative":-1.0,"negative_zero":-0.0,"large":9007199254740992,"large_decimal":9007199254740992.0,"max":18446744073709551615.0,"text":"5.0"}"#;
        assert_eq!(
            normalize_integer_json_numbers(source).as_deref(),
            Some(br#"{"length":4,"exponent":100,"fraction":4.25,"rounds":999999999999999.9,"almost_one":1.00000000000000001,"negative":-1.0,"negative_zero":0,"large":9007199254740992,"large_decimal":9007199254740992,"max":18446744073709551615,"text":"5.0"}"#.as_slice())
        );
        assert!(integer_json_number(b"1.").is_none());
        assert!(integer_json_number(b"1.5").is_none());
        assert!(integer_json_number(b"1e").is_none());
        assert_eq!(
            integer_json_number(b"-0.0").as_deref(),
            Some(b"0".as_slice())
        );
        assert_eq!(
            integer_json_number(b"9007199254740992.0").as_deref(),
            Some(b"9007199254740992".as_slice())
        );
        assert_eq!(
            integer_json_number(b"9007199254740993.0").as_deref(),
            Some(b"9007199254740993".as_slice())
        );
        assert_eq!(
            integer_json_number(b"18446744073709551615.0").as_deref(),
            Some(b"18446744073709551615".as_slice())
        );
        assert!(integer_json_number(b"18446744073709551616.0").is_none());
        assert_eq!(
            integer_json_number(b"-0e999999999999999999999999").as_deref(),
            Some(b"0".as_slice())
        );
    }

    #[test]
    fn accepts_large_integer_spelling_for_zero_initialized_accessor() {
        let source = br#"{"asset":{"version":"2.0"},"accessors":[{"componentType":5126,"count":9007199254740992.0,"type":"SCALAR"}]}"#;
        assert!(import_slice(source).is_ok());
    }

    #[test]
    fn accepts_zero_exponent_in_integer_json_field() {
        let source = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":1,"uri":"data:application/octet-stream;base64,AA=="}],"bufferViews":[{"buffer":0,"byteOffset":0e999999999999999999999999,"byteLength":1}]}"#;
        assert!(import_slice(source).is_ok());
    }
}
