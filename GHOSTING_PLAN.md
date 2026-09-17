# Ghosting: what upstream does differently, and the plan to fix it

Status: **proposal for review** (2026-09-16). Nothing below is implemented except the
rollback in §1. Decisions needed are in §5.

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
