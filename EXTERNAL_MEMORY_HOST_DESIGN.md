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

`State::external_memory_host: bool` carries this into `capture::run`'s own state; `false`
(no real game requests the extension, or the driver lacks it) just means the capture
pipeline keeps using the staging-buffer path it already has -- nothing about capture
correctness depends on this being `true`, today or once it's actually wired into the
import path (next step, not done yet).

## Validation

Real hardware, `lordnikon`, RTX 5070, driver 615.71.09: `vkcube` at both 1280x720 and
2560x1440 (the real GTA render resolution) under
`VK_LAYER_KHRONOS_validation:VK_LAYER_neuralforge_neural` with `VK_LAYER_VALIDATE_SYNC=1`.
Both runs logged `external_memory_host: true` (the NVIDIA driver does advertise the
extension) with zero validation errors or synchronization hazards, and the capture
pipeline kept advancing `layer_frames` normally (474 in 20s at 2560x1440) -- no
regression versus the pre-Phase-3 behavior. Not yet validated: a device where the
extension genuinely isn't available (this driver always has it, so the `Unhandled`
fallback branch for "not supported" is exercised only by code review, not a live run);
GTA itself, which needs a real session.

## Not done yet

This commit only adds the *mechanism* for getting the extension enabled. The capture
pipeline (`CapturePipeline`/`CaptureBuffer` in `capture.rs`) still always allocates its
own staging memory and copies through it -- `State::external_memory_host` is threaded
through but not yet read anywhere. Actually importing the SHM proxy/answer regions as
device memory (querying `minImportedHostPointerAlignment`, aligning the mapping layout
to it, building `CaptureBuffer` from an imported host pointer when available, falling
back to the current allocation when not) is the next step.
