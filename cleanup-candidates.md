# Cleanup candidates requiring review

Generated 2026-11-15. These paths were retained because they contain source code, review logs, validation shaders, package assets, or the active Release build. Review before deleting. Paths are relative to the X173GE root.

## Current build output

- `target` — the current interactive viewer is running from this tree; it also contains saved PNG/BMP review images. Close the viewer and move those images before cleaning the directory.

## Source and validation outputs

- `.codex-target/asset-compare` — source crate and Cargo manifests.
- `.codex-target/asset-inspect` — source crate and Cargo manifests.
- `.codex-target/asset-smoke` — source crate and Cargo manifests.
- `.codex-target/asset-compare-target` — generated SPIR-V files alongside Cargo output.
- `.codex-target/shader-validation-current` — validated SPIR-V files.
- `.codex-target/reverse-z-shaders` — empty directory; removal was rejected by automatic review.

## Human review logs

- `.codex-target/human-review-ambient-environment`
- `.codex-target/human-review-current`
- `.codex-target/human-review-fog-ao`
- `.codex-target/human-review-global-light`
- `.codex-target/human-review-global-light-labeled`
- `.codex-target/human-review-origin`

## Package and fixture staging

- `.codex-target/ao-calibration-package-high-quality`
- `.codex-target/ao-calibration-package-standard`
- `.codex-target/final-size-high-quality`
- `.codex-target/final-size-standard`
- `.codex-target/ibl-20261111-high-quality`
- `.codex-target/ibl-20261111-standard`
- `.codex-target/package-high-quality-20261111`
- `.codex-target/package-high-quality-ready-20261111`
- `.codex-target/package-ibl-review-20261111-high-quality`
- `.codex-target/package-ibl-review-20261111-standard`
- `.codex-target/package-source-current-20261111-high-quality`
- `.codex-target/package-source-current-20261111-standard`
- `.codex-target/package-standard-20261111`
- `.codex-target/package-standard-ready-20261111`
- `.codex-target/ssr-subpixel-package`
