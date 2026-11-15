# Bloom pyramid shaders

This is the linear-HDR pyramidal bloom variant described in Jorge Jimenez,
“Next Generation Post Processing in Call of Duty: Advanced Warfare,” the
SIGGRAPH 2014 Advances in Real-Time Rendering course. This is a production
reference, not a research-paper result. The [author’s slides and discussion](https://www.iryoku.com/next-generation-post-processing-in-call-of-duty-advanced-warfare/)
describe the pyramid, normalized firefly weighting, and repeated tent
upsampling.

## Passes

- `downsample_first.frag`: four input taps at UV offsets
  `(-0.5,-0.5)`, `(0.5,-0.5)`, `(-0.5,0.5)`, `(0.5,0.5)` times the inverse
  source extent. Each tap has `w = 1 / (1 + Y)`, where
  `Y = dot(max(rgb, 0), (0.2126, 0.7152, 0.0722))`. Output is the normalized
  weighted average `sum(w * rgb) / sum(w)`.
- `downsample.frag`: the same four tap offsets, each with weight `1/4`.
- `upsample.frag`: coarse-image taps at the 3x3 UV grid
  `(-r,0,r) x (-r,0,r)`, with runtime `radius_uv`. Per-axis weights are `(1,2,1)`;
  each 2D tap uses their product and the sum is normalized by `16`. The
  normalized coarse result is added to the same-level fine image.

Use a linear-filtered, clamp-to-edge sampler for every source. Downsample
passes assume each output level is about half the source extent in each axis;
the upsample radius is configurable in normalized UV coordinates across levels. All
three passes read and write linear HDR values. Composite the accumulated bloom
with a low, separately tuned intensity before tone mapping. There is no
bright-pass threshold in this variant.

The initial `radius_uv = 0.003` preset is not a visually validated target.
Karis weighting suppresses isolated bright samples only on the first level; it
does not guarantee their complete removal. Very large radii can expose
undersampling, tiny or odd-sized levels can repeat clamped edge samples, and
this shader set does not define pyramid allocation, composite intensity, or
temporal stabilization. The composite shader adds runtime `intensity` to the
HDR scene before applying the existing Reinhard tone map.
