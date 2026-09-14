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

## Remaining measured blockers

The validation log reports:

- `VUID-VkImageMemoryBarrier-oldLayout-01212`: capture transitions an image to/from
  TRANSFER_SRC_OPTIMAL although the swapchain was created with COLOR_ATTACHMENT only.
- `VUID-vkCmdCopyImageToBuffer-srcImage-00186`: the same image lacks TRANSFER_SRC usage.
- `VUID-vkDestroyDevice-device-05137`: NeuralForge capture command buffer, buffer,
  memory, fence and pool remain alive at device destruction.
- Additional `vkGetDeviceProcAddr` warnings from querying instance-level functions.

The helper initialized NGX successfully but reported model_up=0 and helper_frames=0
through this short test. There is no measured Feature 18 throughput or GTA result.
Elapsed smoke-test time must not be interpreted as game FPS or a performance win.

Keep the PR in draft until the Vulkan correctness issues are addressed. First review
supported surface usage and the swapchain interception path, including unsupported
surfaces and creation failure. Then design cleanup at a point where GPU completion
and object lifetime are proven; the pinned layer framework destroys the downstream
device before dropping custom device state, so an ordinary Rust Drop implementation
alone is too late. Do not reintroduce the historical fence changes or assume a wait
in a teardown hook is automatically synchronized with the app's other queues.

After those correctness gates pass, exercise a longer full-model smoke run at the
GTA baseline size, prove launcher exclusion and stable ownership, and only then run
the matched GTA benchmark in PHASE1.md. The user's helper and model-resolution
constraints remain in force. DMA-BUF and later optimization features remain unimplemented.
