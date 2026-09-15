# Zero-copy host transport: device-extension injection

Phase 3's first goal (`ASYNC_CAPTURE_DESIGN.md`'s natural successor) is importing the
SHM proxy/answer regions directly as Vulkan device memory (`VK_EXT_external_memory_host`),
so a capture's `vkCmdCopyImageToBuffer` writes straight into shared memory instead of a
staging buffer the CPU then copies out of. That needs the extension enabled on
whichever `VkDevice` the capture commands submit against -- the game's own device, not
a private one, since the image being copied is the game's own swapchain/render-tap
source.

## The problem this solves

The game creates its own `VkDevice` by calling `vkCreateDevice` with whatever
extensions it wants; this layer only ever observes that call after the device already
exists (`Layer::create_device_info`, called by the pinned `vulkan-layer` framework with
a live `Arc<ash::Device>` already in hand). No ordinary game requests
`VK_EXT_external_memory_host` on its own -- it exists for exactly this kind of
external-interop use case, not normal rendering -- so without some way to add it, this
layer would either need its own private device (which can't touch the game's images
without a second, much larger external-memory *image* sharing scheme) or stay on the
staging-buffer path indefinitely.

## The fix: `InstanceHooks::create_device`

The pinned framework does have a hook for this, just not on the `DeviceHooks` trait
(everything in `device.rs` implements that one, and it only ever sees an
already-created device). It's on `InstanceHooks`, called *before* the framework's own
default `vkCreateDevice` forwarding -- see `vulkan_layer::Global::create_device` in the
pinned crate (`layer_trait/generated.rs`'s `InstanceHooks::create_device`, invoked from
`lib.rs`'s free-standing `create_device` trampoline). This project had never
implemented `InstanceHooks`/`InstanceInfo` before now (`Layer::InstanceInfo` was
`vulkan_layer::StubInstanceInfo`, a real no-op).

`crates/layer/src/lib.rs`'s new `NeuralForgeInstanceHooks::create_device`:

1. Returns `LayerResult::Unhandled` (the exact same as this hook not existing at all)
   whenever there's no safe extension to add: `VK_EXT_external_memory_host` already
   requested, or `vkEnumerateDeviceExtensionProperties` says the physical device
   doesn't actually support it. This is deliberately the overwhelmingly common return
   value -- the hook only ever takes over to add one specific extension, never to
   change anything else about device creation.
2. Otherwise, builds an extended extension list, resolves the real `vkCreateDevice`
   through `layer_device_link.pfnNextGetInstanceProcAddr` (the same resolution the
   framework's own default path uses), and calls it with the extended list.
3. If that creation is refused for any reason the earlier query didn't predict,
   retries with the exact, byte-identical original request before giving up -- this
   optimization must never be the reason a device creation that would otherwise have
   succeeded now fails.
4. On success, records the created `VkDevice` handle in a short-lived side table
   (`EXTERNAL_MEMORY_HOST_DEVICES`) `NeuralForgeDeviceInfo::new` (`device.rs`) checks
   and clears exactly once. This side channel exists because the framework's own
   subsequent `create_device_info` call is handed the *original*, un-injected
   `VkDeviceCreateInfo` regardless of what a hooked `create_device` actually passed to
   the real driver -- there's no other way for `device.rs` to learn what happened.

`State::external_memory_host: bool` carries this into `capture::run`'s own state,
read fresh every present call (cheap: `vkGetPhysicalDeviceProperties2` is a purely
local query) alongside a live check that the *actual* mmap'd proxy-region pointer
(not just its constant offset within the mapping) is aligned to whatever the driver's
own `minImportedHostPointerAlignment` requires. `false` either way just means the
capture pipeline keeps using the staging-buffer path it already has.

## The actual import path: `DirectCapture`

Implemented: `capture::DirectCapture`, a single capture slot (deliberately not two
like `CapturePipeline` -- see its own doc comment) whose device memory is *imported*
from `ShmClient::proxy_region()`'s live pointer via `VkImportMemoryHostPointerInfoEXT`,
so `vkCmdCopyImageToBuffer` writes straight into the SHM proxy region with no staging
buffer. `run`'s shared `poll_or_submit_capture` helper drives whichever of
`DirectCapture`/`CapturePipeline` is active; for the direct path, `ShmClient::write_proxy`
is never called (its one copy is exactly what importing removes) -- though
`original_scratch`/`inflight.original` still need their own one-copy readback out of
the now-written proxy region, since that Vec has to remain stable across whatever
capture starts next and overwrites the shared, imported memory (see `poll_or_submit_capture`'s
own doc comment). Net effect versus the pre-Phase-3 path: two CPU copies (staging ->
Vec -> SHM) become one (SHM -> Vec) for the capture direction.

Two real bugs found and fixed via real-hardware validation (not caught by this
project's local software ICD, which is more permissive than NVIDIA's driver +
validation layers here):
1. `VkBufferCreateInfo` for a buffer that will be bound to imported memory must chain
   `VkExternalMemoryBufferCreateInfo` with the same handle type used at import time
   (`VUID-vkBindBufferMemory-memory-02985`) -- missing entirely in the first version,
   caught immediately by `VK_LAYER_VALIDATE_SYNC=1` on `lordnikon`.
2. The import's `allocationSize` must be a multiple of `minImportedHostPointerAlignment`
   (`VUID-VkMemoryAllocateInfo-allocationSize-01745`) -- a *test* bug (passing the raw
   pixel byte count instead of the alignment-rounded region size), not the production
   code, but only visible once real hardware reported the real alignment (4096 on this
   NVIDIA driver) instead of the local software ICD's more forgiving behavior.

## Validation

Real hardware, `lordnikon`, RTX 5070, driver 615.71.09, both via `vkcube` and via a
dedicated test copied to and run directly against the real driver:

- `vkcube` at 1280x720 and 2560x1440 (GTA's real render resolution) under
  `VK_LAYER_KHRONOS_validation:VK_LAYER_neuralforge_neural` with `VK_LAYER_VALIDATE_SYNC=1`:
  both logged `external_memory_host: true`, zero validation errors or hazards, capture
  pipeline throughput unregressed (474 layer frames in 20s at 2560x1440). This only
  exercises device creation, not `DirectCapture` itself -- `vkcube` never triggers the
  render tap (see `HARDWARE_VALIDATION.md`), same limitation as Phase 2.
- `capture::tests::direct_capture_writes_straight_into_imported_host_memory` (new):
  builds a device with the extension actually enabled, fills a source image with a
  known, checkable color, captures it through `DirectCapture` into a real `mmap`'d
  host region, and asserts every captured byte matches the source exactly. Passes
  locally (this dev machine's software ICD also supports the extension, useful bonus
  coverage) and on `lordnikon` under full synchronization validation -- the two real
  bugs above were found and fixed via this exact test, on this exact hardware.

**Not yet validated**: a device where the extension genuinely isn't available (both
tested drivers have it, so the `Unhandled`/fallback branches are reviewed, not
exercised live); GTA itself, which needs a real session and is the only way to measure
whether this actually moves layer fps toward upstream's ~74/s on `lordnikon`.

## Not done yet

This commit only adds the *mechanism* for getting the extension enabled. The capture
pipeline (`CapturePipeline`/`CaptureBuffer` in `capture.rs`) still always allocates its
own staging memory and copies through it -- `State::external_memory_host` is threaded
through but not yet read anywhere. Actually importing the SHM proxy/answer regions as
device memory (querying `minImportedHostPointerAlignment`, aligning the mapping layout
to it, building `CaptureBuffer` from an imported host pointer when available, falling
back to the current allocation when not) is the next step.
