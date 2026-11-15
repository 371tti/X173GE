//! Fixed single-queue pass order and access contract for the current renderer.

use ash::vk;

const MAX_PASSES: usize = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pass {
    Shadow,
    GtaoPrepass,
    GtaoHorizon,
    GtaoUpsample,
    Scene,
    Ssr,
    GodRaysMarch,
    GodRaysHorizontal,
    GodRaysVertical,
    GodRaysComposite,
    Exposure,
    BloomDown,
    BloomUp,
    BloomComposite,
    SmaaPost,
    SmaaEdges,
    SmaaWeights,
    Present,
}

#[derive(Clone, Copy)]
enum Image {
    ShadowDepth,
    Mboit,
    GtaoDepth,
    GtaoNormal,
    GtaoRaw,
    Gtao,
    SceneColor,
    SceneDepth,
    SceneNormal,
    Reflectance,
    SsrColor,
    RaysMarch,
    RaysHorizontal,
    RaysScattering,
    RaysColor,
    BloomPyramid,
    BloomResult,
    SmaaPost,
    SmaaEdges,
    SmaaWeights,
    Swapchain,
    ExposureHistory,
    Exposure,
}

impl Image {
    const ALL: [Self; 23] = [
        Self::ShadowDepth,
        Self::Mboit,
        Self::GtaoDepth,
        Self::GtaoNormal,
        Self::GtaoRaw,
        Self::Gtao,
        Self::SceneColor,
        Self::SceneDepth,
        Self::SceneNormal,
        Self::Reflectance,
        Self::SsrColor,
        Self::RaysMarch,
        Self::RaysHorizontal,
        Self::RaysScattering,
        Self::RaysColor,
        Self::BloomPyramid,
        Self::BloomResult,
        Self::SmaaPost,
        Self::SmaaEdges,
        Self::SmaaWeights,
        Self::Swapchain,
        Self::ExposureHistory,
        Self::Exposure,
    ];

    const fn bit(self) -> u64 {
        1 << self as u8
    }

    const fn name(self) -> &'static str {
        match self {
            Self::ShadowDepth => "shadow depth",
            Self::Mboit => "translucent shadow",
            Self::GtaoDepth => "GTAO depth",
            Self::GtaoNormal => "GTAO normal",
            Self::GtaoRaw => "GTAO raw",
            Self::Gtao => "GTAO",
            Self::SceneColor => "scene color",
            Self::SceneDepth => "scene depth",
            Self::SceneNormal => "scene normal",
            Self::Reflectance => "SSR reflectance",
            Self::SsrColor => "SSR color",
            Self::RaysMarch => "God Rays march",
            Self::RaysHorizontal => "God Rays horizontal blur",
            Self::RaysScattering => "God Rays scattering",
            Self::RaysColor => "God Rays color",
            Self::BloomPyramid => "Bloom pyramid",
            Self::BloomResult => "Bloom result",
            Self::SmaaPost => "SMAA input",
            Self::SmaaEdges => "SMAA edges",
            Self::SmaaWeights => "SMAA weights",
            Self::Swapchain => "swapchain image",
            Self::ExposureHistory => "previous exposure",
            Self::Exposure => "adapted exposure",
        }
    }
}

fn images(images: &[Image]) -> u64 {
    images.iter().fold(0, |mask, image| mask | image.bit())
}

#[derive(Clone, Copy)]
pub(super) struct Config {
    pub(super) shadows: bool,
    pub(super) mboit: bool,
    pub(super) gtao: bool,
    pub(super) ssr: bool,
    pub(super) god_rays: bool,
    pub(super) bloom: bool,
    pub(super) smaa: bool,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("frame graph has too many passes")]
    Capacity,
    #[error("frame graph reads {image} before a pass writes it")]
    ReadBeforeWrite { image: &'static str },
    #[error("frame graph expected {expected:?}, received {received:?}")]
    OutOfOrder { expected: Pass, received: Pass },
    #[error("frame graph ended before all declared passes were recorded")]
    Incomplete,
}

pub(super) struct FrameGraph {
    passes: [Option<Pass>; MAX_PASSES],
    count: usize,
    cursor: usize,
    produced: u64,
}

impl FrameGraph {
    pub(super) fn build(config: Config) -> Result<Self, Error> {
        let mboit = config.mboit && config.shadows;
        let mut graph = Self {
            passes: [None; MAX_PASSES],
            count: 0,
            cursor: 0,
            produced: 0,
        };
        graph.produced = Image::ExposureHistory.bit();
        if config.shadows {
            let writes = Image::ShadowDepth.bit() | if mboit { Image::Mboit.bit() } else { 0 };
            graph.add(Pass::Shadow, 0, writes)?;
        }
        if config.gtao {
            graph.add(
                Pass::GtaoPrepass,
                0,
                images(&[Image::GtaoDepth, Image::GtaoNormal]),
            )?;
            graph.add(
                Pass::GtaoHorizon,
                images(&[Image::GtaoDepth, Image::GtaoNormal]),
                Image::GtaoRaw.bit(),
            )?;
            graph.add(
                Pass::GtaoUpsample,
                images(&[Image::GtaoDepth, Image::GtaoNormal, Image::GtaoRaw]),
                Image::Gtao.bit(),
            )?;
        }

        let shadow_reads = if config.shadows {
            Image::ShadowDepth.bit()
        } else {
            0
        } | if mboit { Image::Mboit.bit() } else { 0 };
        let scene_writes = images(&[Image::SceneColor, Image::SceneDepth, Image::SceneNormal])
            | if config.ssr {
                Image::Reflectance.bit()
            } else {
                0
            };
        graph.add(
            Pass::Scene,
            shadow_reads | if config.gtao { Image::Gtao.bit() } else { 0 },
            scene_writes,
        )?;

        let mut current = Image::SceneColor;
        if config.ssr {
            graph.add(
                Pass::Ssr,
                images(&[
                    Image::SceneColor,
                    Image::SceneDepth,
                    Image::SceneNormal,
                    Image::Reflectance,
                ]),
                Image::SsrColor.bit(),
            )?;
            current = Image::SsrColor;
        }
        if config.god_rays {
            graph.add(
                Pass::GodRaysMarch,
                Image::SceneDepth.bit() | shadow_reads,
                Image::RaysMarch.bit(),
            )?;
            graph.add(
                Pass::GodRaysHorizontal,
                Image::RaysMarch.bit(),
                Image::RaysHorizontal.bit(),
            )?;
            graph.add(
                Pass::GodRaysVertical,
                Image::RaysHorizontal.bit(),
                Image::RaysScattering.bit(),
            )?;
            graph.add(
                Pass::GodRaysComposite,
                current.bit() | Image::RaysScattering.bit(),
                Image::RaysColor.bit(),
            )?;
            current = Image::RaysColor;
        }
        graph.add(
            Pass::Exposure,
            current.bit() | Image::ExposureHistory.bit(),
            Image::Exposure.bit(),
        )?;
        if config.bloom {
            graph.add(Pass::BloomDown, current.bit(), Image::BloomPyramid.bit())?;
            graph.add(
                Pass::BloomUp,
                Image::BloomPyramid.bit(),
                Image::BloomResult.bit(),
            )?;
            if config.smaa {
                graph.add(
                    Pass::BloomComposite,
                    current.bit() | Image::BloomResult.bit() | Image::Exposure.bit(),
                    Image::SmaaPost.bit(),
                )?;
                current = Image::SmaaPost;
            }
        }
        if config.smaa {
            if !config.bloom {
                graph.add(
                    Pass::SmaaPost,
                    current.bit() | Image::Exposure.bit(),
                    Image::SmaaPost.bit(),
                )?;
                current = Image::SmaaPost;
            }
            graph.add(
                Pass::SmaaEdges,
                Image::SmaaPost.bit(),
                Image::SmaaEdges.bit(),
            )?;
            graph.add(
                Pass::SmaaWeights,
                Image::SmaaEdges.bit(),
                Image::SmaaWeights.bit(),
            )?;
        }
        let mut reads = current.bit();
        reads |= Image::Exposure.bit();
        if config.smaa {
            reads |= Image::SmaaWeights.bit();
        }
        if config.bloom && !config.smaa {
            reads |= Image::BloomResult.bit();
        }
        graph.add(Pass::Present, reads, Image::Swapchain.bit())?;
        Ok(graph)
    }

    fn add(&mut self, pass: Pass, reads: u64, writes: u64) -> Result<(), Error> {
        if self.count == MAX_PASSES {
            return Err(Error::Capacity);
        }
        if let Some(image) = Image::ALL
            .iter()
            .find(|image| reads & image.bit() != 0 && self.produced & image.bit() == 0)
        {
            return Err(Error::ReadBeforeWrite {
                image: image.name(),
            });
        }
        self.passes[self.count] = Some(pass);
        self.count += 1;
        self.produced |= writes;
        Ok(())
    }

    /// Advances command recording in declared order; disabled passes are skipped.
    pub(super) fn enter(&mut self, pass: Pass) -> Result<bool, Error> {
        let Some(expected) = self.passes.get(self.cursor).copied().flatten() else {
            return Err(Error::Incomplete);
        };
        if expected == pass {
            self.cursor += 1;
            return Ok(true);
        }
        if self.passes[self.cursor..self.count].contains(&Some(pass)) {
            return Err(Error::OutOfOrder {
                expected,
                received: pass,
            });
        }
        Ok(false)
    }

    pub(super) fn finish(&self) -> Result<(), Error> {
        if self.cursor == self.count {
            Ok(())
        } else {
            Err(Error::Incomplete)
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Scope {
    pub(super) stages: vk::PipelineStageFlags,
    pub(super) access: vk::AccessFlags,
}

pub(super) fn sampled_scope() -> Scope {
    Scope {
        stages: vk::PipelineStageFlags::FRAGMENT_SHADER,
        access: vk::AccessFlags::SHADER_READ,
    }
}

pub(super) fn color_write_scope() -> Scope {
    Scope {
        stages: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
        access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
    }
}

pub(super) fn depth_write_scope() -> Scope {
    Scope {
        stages: vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
            | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
        access: vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
    }
}

#[cfg(feature = "gtao")]
pub(super) fn color_depth_write_scope() -> Scope {
    let color = color_write_scope();
    let depth = depth_write_scope();
    Scope {
        stages: color.stages | depth.stages,
        access: color.access | depth.access,
    }
}

pub(super) fn scene_attachment_scope() -> Scope {
    Scope {
        stages: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
            | vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
            | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
        access: vk::AccessFlags::COLOR_ATTACHMENT_READ
            | vk::AccessFlags::COLOR_ATTACHMENT_WRITE
            | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_READ
            | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
    }
}

pub(super) fn previous_scene_scope() -> Scope {
    let sampled = sampled_scope();
    let attachment = scene_attachment_scope();
    Scope {
        stages: sampled.stages | attachment.stages,
        access: sampled.access | attachment.access,
    }
}

pub(super) const COLOR_ATTACHMENT_LAYOUT: vk::ImageLayout =
    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
pub(super) const DEPTH_ATTACHMENT_LAYOUT: vk::ImageLayout =
    vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL;
pub(super) const SAMPLED_COLOR_LAYOUT: vk::ImageLayout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
pub(super) const SAMPLED_DEPTH_LAYOUT: vk::ImageLayout =
    vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL;
pub(super) const PRESENT_LAYOUT: vk::ImageLayout = vk::ImageLayout::PRESENT_SRC_KHR;

/// Generates the external synchronization paired with a pass's attachment access.
pub(super) fn render_pass_dependencies(
    source: Scope,
    attachment: Scope,
    consumer: Scope,
) -> [vk::SubpassDependency; 2] {
    [
        vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(source.stages)
            .dst_stage_mask(attachment.stages)
            .src_access_mask(source.access)
            .dst_access_mask(attachment.access),
        vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(attachment.stages)
            .dst_stage_mask(consumer.stages)
            .src_access_mask(attachment.access)
            .dst_access_mask(consumer.access),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn high_quality() -> Config {
        Config {
            shadows: true,
            mboit: false,
            gtao: true,
            ssr: true,
            god_rays: true,
            bloom: true,
            smaa: true,
        }
    }

    #[test]
    fn high_quality_plan_records_every_effect_in_dependency_order() {
        let mut graph = FrameGraph::build(high_quality()).unwrap();
        for pass in [
            Pass::Shadow,
            Pass::GtaoPrepass,
            Pass::GtaoHorizon,
            Pass::GtaoUpsample,
            Pass::Scene,
            Pass::Ssr,
            Pass::GodRaysMarch,
            Pass::GodRaysHorizontal,
            Pass::GodRaysVertical,
            Pass::GodRaysComposite,
            Pass::Exposure,
            Pass::BloomDown,
            Pass::BloomUp,
            Pass::BloomComposite,
            Pass::SmaaEdges,
            Pass::SmaaWeights,
            Pass::Present,
        ] {
            assert!(graph.enter(pass).unwrap());
        }
        graph.finish().unwrap();
    }

    #[test]
    fn optional_effect_combinations_keep_all_reads_produced() {
        for bits in 0..128 {
            let enabled = |bit| bits & (1 << bit) != 0;
            FrameGraph::build(Config {
                shadows: enabled(0),
                mboit: enabled(1),
                gtao: enabled(2),
                ssr: enabled(3),
                god_rays: enabled(4),
                bloom: enabled(5),
                smaa: enabled(6),
            })
            .unwrap();
        }
    }

    #[test]
    fn disabled_passes_are_skipped_and_present_remains_required() {
        let mut graph = FrameGraph::build(Config {
            shadows: false,
            mboit: false,
            gtao: false,
            ssr: false,
            god_rays: false,
            bloom: false,
            smaa: false,
        })
        .unwrap();
        assert!(!graph.enter(Pass::Shadow).unwrap());
        assert!(graph.enter(Pass::Scene).unwrap());
        assert!(!graph.enter(Pass::Ssr).unwrap());
        assert!(graph.enter(Pass::Exposure).unwrap());
        assert!(graph.enter(Pass::Present).unwrap());
        graph.finish().unwrap();
    }

    #[test]
    fn pass_order_cannot_skip_an_enabled_predecessor() {
        let mut graph = FrameGraph::build(high_quality()).unwrap();
        assert!(matches!(
            graph.enter(Pass::Scene),
            Err(Error::OutOfOrder {
                expected: Pass::Shadow,
                received: Pass::Scene
            })
        ));
    }

    #[test]
    fn missing_resource_producer_is_rejected() {
        let mut graph = FrameGraph {
            passes: [None; MAX_PASSES],
            count: 0,
            cursor: 0,
            produced: 0,
        };
        assert!(matches!(
            graph.add(Pass::Scene, Image::Gtao.bit(), 0),
            Err(Error::ReadBeforeWrite { .. })
        ));
    }

    #[test]
    fn render_pass_scopes_cover_sampled_color_and_scene_hazards() {
        let sampled = sampled_scope();
        #[cfg(any(
            feature = "smaa",
            feature = "bloom",
            feature = "gtao",
            feature = "ssr",
            feature = "god-rays",
            feature = "translucent-shadows"
        ))]
        {
            let color = render_pass_dependencies(sampled, color_write_scope(), sampled);
            assert!(color[0].src_stage_mask == vk::PipelineStageFlags::FRAGMENT_SHADER);
            assert!(color[0].dst_stage_mask == vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT);
            assert!(color[0].dst_access_mask == vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
            assert!(color[1].src_access_mask == vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
            assert!(color[1].dst_access_mask == vk::AccessFlags::SHADER_READ);
        }

        let scene =
            render_pass_dependencies(previous_scene_scope(), scene_attachment_scope(), sampled);
        assert!(
            scene[0]
                .src_stage_mask
                .contains(vk::PipelineStageFlags::FRAGMENT_SHADER)
        );
        assert!(
            scene[0]
                .src_access_mask
                .contains(vk::AccessFlags::SHADER_READ)
        );
        assert!(
            scene[0]
                .dst_access_mask
                .contains(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE)
        );
        assert!(scene[1].dst_stage_mask == vk::PipelineStageFlags::FRAGMENT_SHADER);
        assert!(scene[1].dst_access_mask == vk::AccessFlags::SHADER_READ);
        assert!(SAMPLED_DEPTH_LAYOUT == vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL);
    }
}
