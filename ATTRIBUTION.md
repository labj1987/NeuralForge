# Attribution

**As of 2026-09-17, this project is no longer clean-room with respect to
[DLSS5VKLayer](https://github.com/bmitch87/DLSS5VKLayer) (bmitch87), AGPL-3.0.** Earlier
versions of NeuralForge deliberately avoided reading DLSS5VKLayer's own source (or
anything GPL/AGPL-licensed) specifically to keep this project MIT-licensed. That
boundary produced a worse result — months of re-deriving fixes from behavior and docs
alone, repeatedly landing on techniques upstream had already tried and rejected — for
no benefit anyone actually wanted, so it was dropped. This project's own license
changed to **AGPL-3.0-or-later** to match (see [LICENSE](LICENSE) and
[README.md](README.md#license)), and this file now records what is actually read,
adapted, or taken from DLSS5VKLayer's real source, not just its documented behavior.

## What's taken, and how

| Source | What's taken |
|---|---|
| [DLSS5VKLayer](https://github.com/bmitch87/DLSS5VKLayer) (bmitch87), AGPL-3.0 — specifically `layer_linux/src/composition.cpp`, `composition.h`, and `dlssnr/dlssnr.hlsl` read directly | **The composition strategy**: no temporal accumulator/held-answer carried forward and blended against newer frames. DLSS5VKLayer's own source comment (`dlssnr.hlsl`, in the resolve function) states this directly: a reprojected-history accumulator was built, measured as a dead end twice ("the model re-decides its detail with the framing, so an old answer does not belong to a new frame and reprojecting it only moves where the disagreement lands"), and removed — replaced with re-anchoring the composition to the model's cached answer against the *current* frame, fresh, every single present, with no motion-based suppression of the edit at all. NeuralForge's own `carry_delta`/motion-mask compositor (`compose.comp`, `crates/layer/src/composition/gpu.rs::record_temporal_delta_into_image`, added v0.1.49–v0.1.65) was independently arrived at and is architecturally the same kind of reprojection-with-suppression technique DLSS5VKLayer's own source documents trying and rejecting — this is why it kept ghosting/flickering no matter how the suppression threshold was tuned. Replaced per this finding; see `CHANGELOG.md` for the version this landed in. |
| DLSS5VKLayer, same files | The overall two-leg structure (capture leg encodes a proxy and downloads it; a helper round trip happens in between; a compose leg uploads the answer and recomposes onto the swapchain image) and the general shape of the luminance-ratio transfer (`ratio = originalLuma / proxyLuma` below the proxy's level, a headroom-preserving inverse above it, blended via `lerp` with a saturated strength, hue corrected in OkLab) — NeuralForge already had an equivalent, independently-derived version of this exact ratio math (`composition::gpu::UpgradeToneMap`/the "classic" non-`carry_delta` dispatch path, built earlier from the RenoDX design below) that had simply never been wired into the live per-present hot path. Confirming DLSS5VKLayer's own resolve function uses the same shape of math is what justified routing the hot path through it instead of `carry_delta`, rather than porting new math wholesale. |

## What was previously taken clean-room (still accurate, unaffected by the above)

| Source | What's taken |
|---|---|
| [RenoDX](https://github.com/clshortfuse/renodx) (clshortfuse) — MIT | The color composition design: the two-branch luminance/headroom rule, the OkLab hue-correction step, and the reversible neutral-axis gamut compression. This is RenoDX's own DLSS 5 addon design, MIT-licensed, read and reimplemented directly from RenoDX's own public source. |
| Björn Ottosson ([bottosson.github.io/posts/oklab](https://bottosson.github.io/posts/oklab/)) | The OkLab conversion matrices, published as public reference constants. |
| Public domain / standard color science | sRGB transfer function, SMPTE ST.2084 (PQ) encode/decode, Hunt-Pointer-Estevez LMS conversion — textbook formulas, attributable to no one project. |
| Public domain / standard resampling literature | The Lanczos, Catmull-Rom, Mitchell-Netravali, and Kaiser-windowed-sinc kernels used for the supersampling downscale leg — implemented from their mathematical definitions. |

## What's still not taken

DLSS5VKLayer's own `dlssnr.hlsl` documents (inline, at its "native + edit" transfer
mode) that one specific technique — an additive edit rule and a guard shape for it —
comes from [xenmods/DLSSNR-Cost-Scaler](https://github.com/xenmods/DLSSNR-Cost-Scaler),
MIT, "no code is copied." NeuralForge has not adopted that specific mode
(`transfer == 2`/"native + edit") and this file will be updated if that changes. The
NGX caller-identity spoof (`crates/helper/src/spoof.rs`) remains an independent
reimplementation of the same generic PE import-table-hook technique, not read from
DLSS5VKLayer's C++ — see [README.md's Legal section](README.md#legal) for why that
mechanism carries its own separate legal exposure regardless of either project's
license.

## License texts

This project's own code is AGPL-3.0-or-later — see [LICENSE](LICENSE). DLSS5VKLayer is
AGPL-3.0; adapting its composition strategy and confirming its transfer math directly
is why this project's license now matches. RenoDX's MIT license applies to the design
this project's color composition math is independently derived from; xenmods'
DLSSNR-Cost-Scaler (MIT) is referenced above for provenance only, matching
DLSS5VKLayer's own attribution of it, and no code from it appears in this repository.
