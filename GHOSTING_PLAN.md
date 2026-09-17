# Ghosting: what upstream does differently, and the plan to fix it

Status: **in progress** (last updated 2026-09-17, overnight session). Alex reviewed and
approved: order 1→2→3→5→4, Quality as the default mode, `working_scale` 0.75, attempt
step 4. Step 1 was attempted and hit a real, measured obstacle (§1a) that changes its
scope; step 4 was investigated and also turned out larger than estimated (§1b). Nothing
new was deployed to lordnikon this session beyond what v0.1.65 already had — seeing both
steps 1 and 4 need real Vulkan/cross-process surgery in the project's most crash-prone
area, with Alex asleep and unable to catch a live problem, the call was to stop and hand
back a corrected, evidence-based plan rather than gamble on lordnikon overnight.

### 1a. Step 1 (`working_scale`) — the CPU approach is measured non-viable

The natural-looking implementation — capture at full resolution as today, resample the
bytes down to `working_scale × (w,h)` on the CPU before sending them over SHM, and
resample the model's smaller answer back up before compositing — was built and tested
(`composition::downscale::resample_rgba8`, a real, tested, separable resize reusing the
project's existing Lanczos/Catmull-Rom/Mitchell-Netravali/Kaiser kernels, which until now
were themselves dead code, computed but never called by anything).

Measured at GTA's real resolution (2560×1440 ↔ 1920×1080, release build,
`composition::downscale::tests::real_resolution_resample_timing`):

```
down (2560x1440 -> 1920x1080): 315 ms
up   (1920x1080 -> 2560x1440): 546 ms
```

That is far worse than the 87 ms PCIe-BAR bug this same session fixed, and this call
would sit inline in `capture::run` on the game's present thread. **Not wired in; not
deployed.** The resample function itself is correct and kept (unit-tested: identity
resize is byte-exact, flat-color resize doesn't drift, dimensions are always exact,
a downscale→upscale roundtrip stays close to the original) — it is real groundwork, just
not usable as-is on the hot path.

**The actual fix needs to be on the GPU**: `vkCmdBlitImage` (`VK_FILTER_LINEAR`, a
hardware-implemented resize unit) inserted into `CapturePipeline`'s existing
image→buffer copy (shrink before the buffer, on the capture side) and into
`composition::gpu`'s existing buffer→image upload step (grow back up, on the answer
side) — in both cases the *existing*, already-tested compute shader (`compose.comp`)
would keep reading full-resolution images exactly as it does today; only the resource
sizing and one blit call per side would change. That is real, correctness-critical
surgery on `Sized_`/`ComposeSlot` — the exact structures this project's own history
documents as its most crash-prone — and was deliberately not attempted unsupervised
overnight. This is the concrete next step for step 1, not a restart.

### 1b. Step 4 (motion vectors) — larger than "re-enable a stub"

Investigated `crates/layer/src/optical_flow.rs` (the code `prepare_motion_resources`
currently short-circuits past). It already does something more specific than assumed:
it creates its own **private Vulkan device** via a fresh `vkCreateDevice` call, on the
same physical GPU, but that call happens *inside the game's own process*, invoked from
`prepare_motion_resources` at the exact moment (per that function's own comment) "during
a live game's swapchain transition" — which is the documented driver-crash trigger.

So simply removing the early `return` would very plausibly reproduce the original
crash: the problem was never "motion vectors are unstable," it was "creating a second
Vulkan device from within the game's process, mid-swapchain-transition, crashes this
driver." A private device on the same physical GPU is not automatically safe just
because it's a separate logical device — it is still contending for the same GSP
firmware queue from within the process the game's own device transition is disrupting.

The safer design from §3's plan — computing flow **in the Windows helper process**
instead — is a real, different architecture, not a small change: the helper is a
separate OS process (under Wine/Proton) with its own independent Vulkan device already
set up for NGX/DXVK-NVAPI. Moving flow computation there means:
- A new `VK_NV_optical_flow` session on the helper's *own* device (need to confirm the
  extension is exposed through DXVK-NVAPI's Vulkan implementation under Wine — not
  verified yet).
- The layer sending two consecutive captured frames (or the helper keeping its own
  Frame[N-1]) across the existing SHM transport instead of one.
- New protocol/SHM wiring so the helper's own computed flow reaches `DLSSNR.MVec`
  without a round trip back through the layer.

This is genuinely comparable in size to step 1, not a quick toggle, and it touches a
second process's Vulkan setup this project has comparatively little live-tested
experience with. Correctly identifying *why* the existing code crashes (a same-process,
mid-transition second device — not "motion vectors are inherently unsafe") is the real
deliverable from tonight's look at this: it means the helper-side design is still the
right target, now for a verified reason instead of an assumed one.

## 5a. Updated recommendation for Alex

Both step 1 and step 4, done properly, are real Vulkan/cross-process features in the
project's riskiest area — not something to land unsupervised overnight, and not
something to rush now that the honest scope is known. Suggested next session, with you
available to test live on lordnikon:
1. Build the GPU-blit version of step 1 (§1a) — bounded, mechanical, and the existing
   compose shader test harness (`gpu_dispatch_matches_the_cpu_reference`) extends
   naturally to cover it.
2. Re-measure the objective number (`capture_hot_path_cost_per_present` plus the
   helper's own `eval=` timing) before touching step 2's synchronous mode at all — step
   2's expected fps math (§4's table) depends on step 1's real result, not the estimate.
3. Revisit step 4 with the helper-side design once 1–3 are landed and validated, since
   it is now understood to be its own project-sized piece of work, not a quick unstub.

The original status line below (proposal for review, 2026-09-16) is kept for history.

## 1. Where things stand tonight

- The fps collapse is fixed (v0.1.64, capture readback buffer moved out of the PCIe BAR:
  87 ms → 5.7 ms per present). In-game: 120s fps, enhancement visibly applied.
- Ghosting remains. Three compositor motion-mask variants were tried live against GTA's
  built-in benchmark:
  - v1 (threshold 0.006..0.045, squared): "a little bit less ghosting".
  - **v2 (0.005..0.032, cubed): "still ghosting but closer to upstream"** — this is what
    is deployed on lordnikon now and ships as v0.1.65.
  - v3 (threshold scaled by the model's own edit strength): "visuals are worse" — reverted.
- Conclusion: the mask is a band-aid. The ghost is structural, and no threshold fixes it —
  v3 showed that pushing the mask harder only removes the enhancement during motion.

## 2. What was learned

### 2.1 The ghost's mechanism in NeuralForge

`capture::run` sends frame N to the model. The answer comes back **~26 ms later** (helper
log tonight at 2560x1440: eval p50 26 ms, p95 28 ms) — that is 3–4 game frames at 120 fps.
Meanwhile the layer keeps presenting. To avoid the flicker of alternating native and
enhanced frames (the v0.1.49/v0.1.50 fix), it re-applies answer(N)'s delta onto frames
N+1…N+3 through a per-pixel motion mask (`compose.comp`, `carry_delta`). Any detail the
model added at frame-N positions lands on content that has since moved. That is the ghost.

Without knowing where the content went, a stale delta can only be shown (ghost) or dropped
(enhancement vanishes while moving). Every mask variant is a point on that line.

### 2.2 What upstream (DLSS5VKLayer) does instead

From its README, issues, release notes and design docs only — no upstream source, shader or
SPIR-V was read (see §6).

1. **Synchronous per frame.** Upstream's present thread *blocks* until the helper returns
   the processed frame for that same frame (issue #13's disassembly: 2 ms spin, then 200 µs
   polls on `seq_resp`; the closing measurement found the wait is the NGX inference itself,
   ~5.8 ms at 1440p). Every displayed frame is the model's answer for that frame; nothing
   stale is ever carried, so it cannot ghost. It is also why upstream's GPU sits at 98%/227W
   in Alex's test: game and model serialise on one GPU.
2. **The model runs at reduced resolution.** ~0.75 scale: a 1920x1080 model for a 2560x1440
   swapchain, 2880x1620 for 4K (issue #13's table). NeuralForge evaluates at the full
   2560x1440. `working_scale` exists in the protocol and GUI but is **not wired into the
   layer** (no reference anywhere in `crates/layer`, confirmed tonight) — it is a no-op.
   26 ms versus ~6 ms is the whole fps difference between the two designs.
3. **Real motion vectors and carried history.** `VK_NV_optical_flow` synthetic motion
   vectors, on by default, `R16G16_SFLOAT` in source-pixel units, current→previous;
   `DLSSNR.Reset` only on the first frame and on CPU-detected scene cuts; `UseAutoMask=1`;
   `Depth` null; zero vectors as the fallback when NVOF is unavailable. NVIDIA describes
   the model as conditioned on "the current rendered frame, engine motion vectors, carried
   temporal state" and "trained for frame-to-frame temporal stability" — the temporal
   stability lives *inside the model*, and it needs motion vectors to work.

   NeuralForge today: motion vectors are stubbed (commit 72d7a55, after a real driver
   crash) **and**, found tonight, the helper sets `DLSSNR.Reset = 1` on *every* evaluate
   whenever the Motion toggle is on (`reset_history = mvec_enabled && motion.is_empty()`,
   `crates/helper/src/main.rs:249`; the config has `set_mvec_enabled=1`). So the model
   gets no temporal state at all: each answer is an independent single-frame answer. That
   is very likely the *flicker* that motivated the held-answer hack in the first place —
   the hack hides a symptom of the missing motion vectors, and creates the ghosting. With
   the toggle off it is the other failure: `Reset = 0` with zero vectors, and the model
   smears its own history instead. Neither is what upstream does.
4. **Other projects reach the same design.** Magpie's DLSS-NR backend (documented in
   upstream's pipeline notes) is per-frame synchronous with fences, reuses a cached output
   for a repeated frame id, and with "input resolution scaling" on it downsamples, runs the
   net at reduced resolution and composites a Lanczos-3 residual back — exactly
   NeuralForge's designed-but-unwired `working_scale` path. dlss5-bridge (ReShade add-on)
   uses NVIDIA Optical Flow for games without motion vectors and warns that approximated
   inputs make "text soften and dense foliage smear" — the known cost of the optical-flow
   route. Alex saw upstream clean on GTA, so it is acceptable there.

### 2.3 Why NeuralForge is at 120 fps and upstream is not

Because NeuralForge never waits. That is the whole trade: async = free fps + ghost;
synchronous = no ghost + fps capped by the model. Upstream's fps is only good because its
model is cheap (0.75 scale, ~6 ms). NeuralForge synchronous *at full resolution* would be
~1/(8 ms + 26 ms) ≈ 30 fps — the fps collapse again, from a different cause. So the
resolution scale is a prerequisite, not a nicety.

## 3. Plan (proposed order)

### Step 1 — Wire the model resolution scale (prerequisite)

Capture at full resolution, downscale the proxy to `working_scale × (w, h)` with the
existing supersampling filter (Lanczos3 default), send *that* to the model, and let the
compositor's existing transfer-ratio path (`proxy ≠ original` — the case it was designed
for and currently never exercises) carry the enhancement back onto the full-resolution
original. Default 0.75, matching upstream.

- Expected: eval 26 ms → roughly 8–10 ms at 1920x1080 (the helper's timing line will say
  exactly). Slight softness in the enhancement is the known cost; the original frame is
  untouched at full resolution.
- Self-testable: helper eval timing + `capture_hot_path_cost_per_present`. Visual: Alex.
- Risk: low–moderate. The compositor path exists but has been dormant; the protocol's
  per-slot width/height must describe the proxy, not the frame, and the helper's frame
  resources must be sized to it.

### Step 2 — Synchronous "Quality" presentation mode (the ghost fix; upstream's policy)

On the present of frame N: capture, send, **wait** (bounded, e.g. 30 ms; on timeout fail
open and present the native frame), composite answer(N) onto frame N itself, present. No
held-answer carry at all; `carry_delta` unused in this mode.

- Expected fps ≈ 1/(game frame + capture ~7 ms + eval): with Step 1, roughly 50–70 fps at
  1440p; without Step 1, ~30 fps (why Step 1 comes first). Upstream is faster here only
  because its transport is zero-copy (dma-buf); NeuralForge's Phase 4 work closes that
  later.
- Latency: +1 frame of model time, same as upstream.
- Keep the current async path as **"Performance"** mode (max fps, v2 mask, mild ghosting).
  GUI: *Presentation: Quality (no ghosting, lower fps) / Performance (max fps)*.
- Implementation is mostly *removal* (the carry) plus a bounded spin/sleep on `seq_resp`
  like upstream's, using the 2-slot inflight machinery that already exists. The composite
  runs before the present on the same frame (~1.7 ms GPU, measured).
- Later refinement, not needed first: pipelining — present frame N-1 while the model works
  on N — hides the wait at a constant one-frame latency (suggested in upstream's issue
  #13; upstream has not done it either).

### Step 3 — An explicit `Reset` policy (tiny; do with Step 2)

Stop deriving `DLSSNR.Reset` from the grayed-out Motion toggle. In Quality mode with no
motion vectors: `Reset = 1` every frame (independent, deterministic answers; no
self-smear). Document it. Zero risk.

### Step 4 — Real motion vectors and carried history (full upstream parity; hardest)

Re-enable the optical-flow path without the recorded crash (creating the private NVOF
device *during the game's swapchain transition*). Two ways, in preference order:

1. **Compute the flow in the helper**, as upstream does: keep Frame[N-1] in VRAM in the
   helper, run the flow pass there between N-1 and N before `EvaluateFeature`. No private
   device inside the game process at all — the crash class disappears. Costs one extra
   frame in the helper and the flow pass itself (upstream: ~1–2 ms).
2. Keep it in the layer but create the NVOF session lazily on a steady-state present,
   never in the swapchain hook (upstream 0.3.0-3 fixed "Optical Flow queue-family sharing
   and synchronization" in the same area).

Then: `Reset` only on frame 1 and scene cuts (mean-luma threshold ~55 like upstream),
`MVecScale 1.0`, `UseAutoMask=1`, `Depth` null. Payoff: the model's own temporal
stability — less shimmer, and Quality mode matches upstream fully. Risk: the exact driver
crash; every run on lordnikon with `journalctl -k` watched for Xid lines.

### Step 5 — Housekeeping found tonight

- **Layer deploy gap.** AppImage/Gear Lever updates never refresh
  `~/.local/share/neuralforge/lib/neuralforge/libneuralforge_layer.so`, which is what the
  game actually loads. That is why v0.1.64 "did nothing" until the .so was copied by hand.
  The GUI should re-install the layer on launch whenever the bundled hash differs (upstream's
  `install.sh` overwrites in place and its README says to relaunch the game).
- **Steam env gotcha.** Document `NEURALFORGE_DISABLE=1` + a full Steam restart as the way
  to A/B against upstream, and that Steam bakes its launch environment into every game.
- v0.1.65 ships the v2 mask (done tonight).

## 4. Expectations per step

| Step | fps (1440p) | Ghosting | Effort | Risk | Tests |
|---|---|---|---|---|---|
| 1 scale 0.75 | 120s (async unchanged) | unchanged; answers arrive ~3x sooner, so the ghost window shrinks | 1 session | low–mod | eval timing, benchmark, Alex looks for softness |
| 2 Quality mode | ~50–70 | **gone** (nothing stale shown) | 1–2 sessions | moderate (hot path) | benchmark for cost; Alex's eyes + GTA benchmark vs upstream |
| 3 Reset policy | — | — | minutes | none | log line |
| 4 motion vectors | −1–2 ms | gone + less shimmer | several sessions | **high** (driver crash) | journalctl Xid watch, Alex's eyes |
| 5 deploy fix | — | — | 1 session | low | Gear Lever update → game loads new hash |

## 5. Decisions for Alex

1. **Order.** Recommended: 1 → 2 → 3 → 5 → 4. Alternative: Step 2 first at full resolution
   to *see* the ghost-free result quickly, accepting ~30 fps until Step 1 lands.
2. **Default mode** once Step 2 exists: Quality (upstream's look, the reference you compared
   against) or Performance (the 120s)?
3. **Default `working_scale` 0.75** like upstream — fine to trade a little softness in the
   enhancement for the fps? (The original frame stays full resolution either way.)
4. **Step 4.** Go ahead despite the crash history, or stop after Steps 1–3 if it already
   looks as good as upstream?

## 6. Sources and clean-room note

Read for this plan: DLSS5VKLayer's README, issues #13 and #21, release notes 0.2.6-2…0.3.1-1,
and its `frame-hold.md` / `extracted_pipeline_notes.md` / `DEVELOPMENT.md` docs; NVIDIA's
DLSS 5 research page; the READMEs of dlss5-linux, dlss5-bridge, dlss-nr-on-intel,
OptiScaler_DLSSNR forks, dlssnr-patcher, DLSS5VKLayer-Plus. **No upstream C++/shader/SPIR-V
was read**; ATTRIBUTION.md is unchanged and still accurate.

- https://github.com/bmitch87/DLSS5VKLayer (AGPL-3.0) — README: Synthetic Motion Vectors,
  GUI Settings
- https://github.com/bmitch87/DLSS5VKLayer/issues/13 — synchronous present-thread wait,
  per-resolution model timings, the 98% utilisation explanation
- https://github.com/bmitch87/DLSS5VKLayer/issues/21 — Xid 79 on Ada (not our GPU)
- https://research.nvidia.com/labs/adlr/DLSS5/ — model inputs: frame, motion vectors,
  carried temporal state; trained for temporal stability
- https://www.nvidia.com/en-us/geforce/news/dlss-5-3d-guided-neural-rendering/
- https://github.com/NIGos/dlss5-bridge — optical-flow substitute inputs and their cost
- https://github.com/pantsoftime/dlss5-linux — the vklayer route "costs what a post-present
  route costs: synthetic motion vectors from optical flow, no depth, the HUD included"
- https://github.com/Uzbekunknown/dlss-nr-on-intel — independent reimplementation notes
- https://www.phoronix.com/news/DLSS5VKLayer
