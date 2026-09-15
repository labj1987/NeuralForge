# NeuralForge target-machine validation — 2026-09-14

Target: `alex@lordnikon`, RTX 5070, NVIDIA 615.71.09. Upstream package 0.3.0-1
remains installed. At inspection time GTA and the upstream helper were not running.
The saved upstream config retains passes=1, model_resolution=1, motion_enabled=0,
motion_quality=0. No upstream config, package, library, manifest, prefix or Steam
launch option was modified. Upstream config and both layer manifests passed a
before/after SHA-256 comparison.

NeuralForge is installed separately in `~/.local/share/neuralforge`, with its own
config, prefix, helper and `/tmp/neuralforge-1000/shm.bin`. The helper was started
and remains running. Its live settings retain working_scale=1, passes=1,
mvec_enabled=0, mvec_quality=0 and apply_model=1. The required NVIDIA DLLs were
copied by the explicit binary importer; nothing was moved from upstream.

The initial SSH-launched helper later exited while its header still reported RUNNING.
The cause of that exit is not established; header state alone is not a liveness check.
For continued validation it was relaunched with a transient user service,
`neuralforge-validation-helper.service` (oneshot with RemainAfterExit), keeping its
process tree outside the short-lived SSH session. This is not a boot-enabled service.
Verify the actual helper process and advancing counters before any benchmark.

The user service environment also inherited
`VK_INSTANCE_LAYERS=VK_LAYER_NV_dlssnr:VK_LAYER_NV_present`. This caused upstream's
NR layer to load into the compute helper. The supervisor now removes game-rendering
layer selections and activation flags from its child environment, preserving unrelated
layers such as validation and NV_present. The desktop manager's environment is unchanged.
A real child-process test verifies that separation.

Installer updates now replace files atomically. Previously they overwrote files in
place, which is unsafe when a running Wine helper or game has mapped an executable or
library. An integration test holds the old file open across an update and verifies
that it retains the old bytes while new opens see the replacement. No active process
is automatically stopped by the installer.

## Presentation test and dispatch fix

A 120-frame, 1280x720 Wayland `vkcube` smoke test with Khronos validation exits 0
without NeuralForge. With only NeuralForge loaded, the first run aborted with
SIGABRT at the very first `vkResetCommandBuffer` in `capture_pristine`, before
any helper frame was processed. GDB reproduced this and identified the reset call.

The layer allocates its private command buffers below the loader trampoline.
It was missing loader dispatch initialization for those buffers. The fix stores the
loader's device-data callback and initializes every private capture/composition
command buffer immediately after allocation, using Khronos's documented fallback
for an older loader without the callback. Direct-loader GPU tests are unaffected.
Two regression tests cover callback traversal/lifetime and the fallback dispatch slot.
No fence wait, queue timing or model setting was changed.

Reference: [Khronos loader interface, Creating New Dispatchable Objects](https://github.com/KhronosGroup/Vulkan-Loader/blob/main/docs/LoaderLayerInterface.md#creating-new-dispatchable-objects).

After this fix, the same NVIDIA `vkcube` test exits 0. Process maps confirm it loaded
only `libneuralforge_layer.so`, not upstream's `libVkLayer_NV_dlssnr.so`.
This confirms the abort is resolved; it does **not** establish valid end-to-end rendering.

## Correctness follow-up

The follow-up fixes make capture opt in only when the surface explicitly supports
both transfer-source and transfer-destination image usage. The layer queries the
next instance dispatch chain, never retries creation, and forwards the original
creation unchanged when a surface has an extended or unsupported configuration.
This matters because a failed replacement creation can retire an application's old
swapchain.

The layer now frees private capture and composition resources immediately before
the framework forwards `vkDestroyDevice`. This is the one teardown point where
Vulkan requires the application to externally synchronize the device and queues;
no per-frame or resize wait was added. Presentation semaphores are now assigned to
the acquired swapchain image rather than a rotating command-buffer slot. Retired
swapchain semaphores remain alive until device teardown. This follows Khronos's
[swapchain semaphore reuse guidance](https://docs.vulkan.org/guide/latest/swapchain_semaphore_reuse.html).

On `lordnikon`, a 1,800-frame 2560x1440 Wayland `vkcube` run with NeuralForge,
the full model, host SHM, and `NEURALFORGE_DMABUF=0` exited normally in 19.6 seconds.
Khronos validation reported zero errors. The helper reported `model_up=1` and had
processed 192 frames at the time of the status capture. A separate 900-frame run
with synchronization validation enabled also exited normally with zero validation
errors and zero synchronization hazards. Both runs loaded only
`libneuralforge_layer.so`; upstream's NR layer was absent. The upstream config and
both installed upstream layer manifests still match their initial SHA-256 hashes.

Eleven non-fatal validation warnings remain from the pinned layer framework asking
`vkGetDeviceProcAddr` for instance-level commands. They do not come from the capture
path and are not yet resolved. There is still no measured GTA result or Feature 18
throughput claim. Smoke-test elapsed time is not game FPS or a performance result.

## GTA comparison gate

The upstream session was sampled for 61.478 seconds with the documented GTA baseline
and `DLSSNR_DMABUF=0`. Its layer counter advanced 4,540 frames (73.85 layer frames per
second); mean GPU utilization was 94.9%, mean VRAM allocation 5,851 MiB, mean board
power 232.3 W, and peak temperature 75 C. These counters are useful pipeline evidence,
but are not game FPS or a 1%-low result.

For the NeuralForge-only launch, Steam was restarted with `NEURALFORGE_ENABLE=1`,
`NEURALFORGE_TARGET_EXE=GTA5_Enhanced.exe`, `NEURALFORGE_DMABUF=0`, its isolated
implicit-layer path, and a per-session loader disable for `VK_LAYER_NV_dlssnr`.
NeuralForge loaded into the Rockstar processes and passed their swapchains through;
the explicit ownership filter did not let those processes acquire the session.
GTA itself reached 2560x1440, but reported only `TRANSFER_DST | COLOR_ATTACHMENT`
for its swapchain image usage. NeuralForge requires `TRANSFER_SRC` to copy the image
to its host transport. Since the surface capability query did not advertise it, the
layer retained the original swapchain and did no capture, resize, or helper work.
The upstream installation and configuration remain unchanged. A matched NeuralForge
benchmark is blocked until a legal GTA capture path is designed and validated.

A passive transfer probe subsequently found GTA's legal pre-present route: the game
blits `TRANSFER_SRC_OPTIMAL` render images into the `TRANSFER_DST_OPTIMAL` swapchain
images. The layer merely recorded those commands and forwarded them unchanged. The
candidate render-tap design is recorded in `RENDER_TAP_DESIGN.md`; it has not been
enabled for rendering or benchmarked.

The guarded render tap was then validated live. GTA's source images were observed
through Synchronization2 barriers as `GENERAL -> TRANSFER_SRC_OPTIMAL -> GENERAL`.
NeuralForge captures only after the source has returned to `GENERAL`, transitions it
to `TRANSFER_SRC_OPTIMAL` for its private copy, and restores `GENERAL`; the swapchain
remains a `TRANSFER_DST` output. Helper frames advanced from 219 to 336 on first use,
with `model_up=1` and `NEURALFORGE_DMABUF=0` throughout.

An initial 61.413-second NeuralForge interval advanced 475 layer frames (7.73 layer
frames per second), with 34.9% average GPU utilization, 5,235 MiB VRAM, 77.1 W mean
power, and 55 C maximum temperature. This cannot be compared as game FPS or against
the earlier upstream interval because the GTA scene and GPU workload were not held
constant. It does demonstrate that the current fully synchronous host transport is
the next performance bottleneck to instrument and pipeline.

A second, steady-state 61.379-second interval advanced 474 layer frames (7.72 layer
frames per second), with 35.6% average GPU utilization, 5,210 MiB VRAM, 76.9 W mean
power, and 53 C maximum temperature. The repeat confirms the synchronous pipeline
limit is reproducible rather than startup warm-up behavior.

The ownership filter was exercised with two temporary names for the same `vkcube`
binary. A process launched as `explorer.exe` was excluded, created only pass-through
swapchains, and left `helper_frames` unchanged. A process launched as
`GTA5_Enhanced.exe` with `NEURALFORGE_TARGET_EXE=GTA5_Enhanced.exe` acquired the
NeuralForge lease, used a non-pass-through swapchain, and advanced helper frames
from 484 to 530. These are process-filter tests, not a GTA launch. They confirm the
intended launcher exclusion and explicit-target path without touching Steam, GTA, or
the upstream install.

Keep the PR in draft until the review accepts these changes and the matched GTA
benchmark in PHASE1.md has been run. The user's helper, model-resolution and
DMA-BUF constraints remain in force; later optimization features are unimplemented.

## 2026-09-15 — eleven validation warnings: not reproduced; root cause identified

Repository renamed to `labj1987/NeuralForge` on GitHub; local checkout's remote and
directory were updated to match, confirmed against the renamed repository. The Phase 1
PR above is merged.

Attempted to reproduce and fix the eleven non-fatal validation warnings from Phase 1
item 4 before continuing. A freshly built `libneuralforge_layer.so` was deployed
alongside the already-installed one (`~/nf-validate` via `VK_ADD_LAYER_PATH`, not
`VK_LAYER_PATH` -- the latter replaces rather than extends the default search path
and hides the system's own `VK_LAYER_KHRONOS_validation` manifest, which is why an
earlier attempt in this same session saw zero output and turned out not to have
validation loaded at all). With `VK_LAYER_KHRONOS_validation:VK_LAYER_neuralforge_neural`
confirmed active (`VK_LOADER_DEBUG=layer`), a 1280x720 Wayland `vkcube` run against the
currently-installed NeuralForge build produced **zero validation warnings or errors**,
with and without `VK_VALIDATION_FEATURE_ENABLE_BEST_PRACTICES_EXT`, and `VK_LOADER_DEBUG=all`
showed nothing related to `vkGetDeviceProcAddr` beyond expected platform-surface-name
misses (Win32/Android/iOS/etc., irrelevant on this platform).

The likely source, found by reading the pinned `vulkan-layer` framework
(`google/vk-layer-for-rust` at `102d87cd`, `vulkan-layer/src/lib.rs` around its
`create_device` trampoline): it builds this layer's device dispatch table via
`ash::Device::load`, called with the *instance's* `get_instance_proc_addr` slot
replaced by `vkGetDeviceProcAddr` -- a deliberate trick to resolve the whole
`ash::Device` function table generically. The framework's own comment acknowledges
this can make the loader "complain about internal vkGetDeviceProcAddr called for
<function name>" for instance-level commands and calls it benign. This project's own
code (`crates/layer/src/device.rs`) only resolves six clearly device-level commands
itself and is not the source.

This was not reproduced live today, so it is not fixed. Either the specific
validation-layer version here (`1.4.341`) does not flag this pattern, or it only
surfaces under conditions this `vkcube` run did not match (GTA's actual Xwayland
surface path through Proton/winevulkan, rather than native Wayland). Re-verify against
a real GTA session, or against `vkcube` run through Xwayland specifically, before
concluding this needs a fork of the pinned framework -- patching a third-party git
dependency is a real undertaking and should not be started on an unreproduced report.

Also added `scripts/bench.sh` for Phase 1 item 1 (the repeatable native/upstream/
neuralforge benchmark script). It restarts Steam under each mode's environment (an
already-running Steam client does not pick up a new shell's exported vars -- confirmed
the hard way in the 2026-09-14 session above), waits for `GTA5_Enhanced.exe`, and then
**stops and waits for a human to confirm the saved route/scene has been reached**
before starting the timed sample -- it cannot drive the car itself. The matched 3x
benchmark this phase's exit gate requires has still not been run.
