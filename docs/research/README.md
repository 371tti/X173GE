# Rendering research notes

These are research records, not claims that every feature is complete or visually validated. Each note separates the original source from later production guidance and records whether its variant is implemented, proposed, or awaiting visual review.

## Notes

- [GTAO](gtao.md) — Activision technical memo, not a peer-reviewed paper.
- [SSR](ssr.md) — JCGT research paper.
- [SMAA](smaa.md) — Eurographics/Computer Graphics Forum paper and author reference implementation.
- [PCSS](pcss.md) — SIGGRAPH sketch.
- [CSM](csm.md) — PSSM paper, GPU Gems chapter, and production presenter notes.
- [Translucent shadows](translucent-shadows.md) — MBOIT I3D paper and supplementary material.
- [Volumetric light](volumetric.md) — Eurographics short paper.
- [Environment lighting](environment-lighting.md) — diffuse SH from HDRI and SSR-only scene reflections; no environment specular fallback.
- [Punctual lights](punctual-lights.md) — ratified Khronos glTF extension and PBRT production reference; not a research paper.
- [Bloom](bloom.md) — SIGGRAPH production course; deliberately not called a research paper.
- [Tone mapping and automatic exposure](tone-mapping.md) — Reinhard log-average exposure, GPU temporal adaptation, and post-curve display-linear contrast.

## Shared validation rule

For each effect, compare the same fixed scene, camera, resolution, exposure, and quality setting against an unfiltered/reference render. Keep scene inputs deterministic. Check both a still frame and camera/object motion where temporal stability is involved. Save GPU timings by pass and report quality regressions with the exact scene and camera. No numeric cost target is claimed by these notes; use the rebuild1 baseline and then X173GE measurements.
