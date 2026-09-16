//! Per-device state and the [`DeviceHooks`] implementation: `vkCreateSwapchainKHR`/
//! `vkDestroySwapchainKHR` track swapchains, `vkGetDeviceQueue`/`vkGetDeviceQueue2`
//! learn which queue family a queue belongs to, `vkQueuePresentKHR` is where the real
//! capture/transport/write-back round trip (`crate::capture::run`) happens now.

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::sync::{Arc, Mutex};

use ash::vk;
use vulkan_layer::{DeviceHooks, DeviceInfo, LayerResult, LayerVulkanCommand as VulkanCommand};

use crate::capture;
use crate::shm::ShmClient;
use crate::swapchain::{self, SwapchainState};

/// The one swapchain (across every device in this process) allowed to drive the
/// shared-memory channel. A process can present more than one swapchain -- the game
/// window and the Steam overlay, or, mid-resize, the old and new windows at once.
/// Routing all of them through one channel would make the helper rebuild its feature on
/// every size switch, and could hand one swapchain another's answer; the largest by
/// area is assumed to be the game, and the rest present untouched.
struct Primary {
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    area: u64,
}

static PRIMARY: Mutex<Option<Primary>> = Mutex::new(None);

fn claim_primary(device: vk::Device, swapchain: vk::SwapchainKHR, width: u32, height: u32) -> bool {
    let mut guard = PRIMARY.lock().unwrap();
    let area = u64::from(width) * u64::from(height);
    match &*guard {
        Some(p) if p.device == device && p.swapchain == swapchain => true,
        Some(p) if area <= p.area => false,
        _ => {
            *guard = Some(Primary { device, swapchain, area });
            true
        }
    }
}

fn release_primary(device: vk::Device, swapchain: vk::SwapchainKHR) {
    let mut guard = PRIMARY.lock().unwrap();
    if matches!(&*guard, Some(p) if p.device == device && p.swapchain == swapchain) {
        *guard = None;
    }
}

/// Resolves one function pointer through the next layer/driver's `vkGetDeviceProcAddr`,
/// or `None` if it isn't there. That's not a failure worth panicking over: a device
/// that never enabled `VK_KHR_swapchain` (a compute-only device, or any device an app
/// simply never presents from) legitimately has no `vkCreateSwapchainKHR` to resolve,
/// and such a device will also never have an app call it -- so a missing pointer here
/// just means this device's hooks quietly do nothing, not that anything is wrong. This
/// was caught by `examples/smoke.rs` creating a device with no extensions enabled at
/// all: the first version of this function panicked on exactly that, which would have
/// crashed every plain compute app the layer got loaded into.
///
/// Transmuting the result to `F` is sound exactly as far as the caller names the right
/// `PFN_vk*` type for `name` -- the same contract the equivalent C cast upstream's own
/// `next_dpa` calls carry.
///
/// # Safety
/// `get_proc` must be a valid `vkGetDeviceProcAddr` for `device`, and `F` must be the
/// PFN type matching `name`.
unsafe fn resolve<F: Copy>(get_proc: vk::PFN_vkGetDeviceProcAddr, device: vk::Device, name: &CStr) -> Option<F> {
    let p = unsafe { get_proc(device, name.as_ptr()) }?;
    // SAFETY: forwarded from the caller's own safety contract.
    Some(unsafe { std::mem::transmute_copy::<_, F>(&p) })
}

pub struct NeuralForgeDeviceInfo {
    _loader_data: crate::loader_data::Registration,
    device: Arc<ash::Device>,
    /// `None` only in the hypothetical case `create_device_info`'s own doc comment
    /// notes (a device created against an instance from before this layer loaded,
    /// which never happens for an implicit layer) -- capture is simply skipped
    /// (present passes through unmodified) whenever it is, rather than panicking.
    instance: Option<Arc<ash::Instance>>,
    surface_caps: Option<vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR>,
    physical_device: vk::PhysicalDevice,
    next_create_swapchain_khr: Option<vk::PFN_vkCreateSwapchainKHR>,
    next_destroy_swapchain_khr: Option<vk::PFN_vkDestroySwapchainKHR>,
    next_queue_present_khr: Option<vk::PFN_vkQueuePresentKHR>,
    next_get_swapchain_images_khr: Option<vk::PFN_vkGetSwapchainImagesKHR>,
    next_get_device_queue: Option<vk::PFN_vkGetDeviceQueue>,
    next_get_device_queue2: Option<vk::PFN_vkGetDeviceQueue2>,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    swapchains: HashMap<vk::SwapchainKHR, SwapchainState>,
    shm: ShmClient,
    /// Which queue family a `VkQueue` handle belongs to -- learned by observing the
    /// app's own `vkGetDeviceQueue`/`vkGetDeviceQueue2` calls (see those hooks below),
    /// since Vulkan has no query that answers this for a handle after the fact. Needed
    /// to build a command pool for whatever queue `queue_present_khr` hands us.
    queue_families: HashMap<vk::Queue, u32>,
    capture: Option<capture::CaptureResources>,
    /// The Phase 2 non-blocking capture pipeline (`ASYNC_CAPTURE_DESIGN.md`) `capture::run`
    /// uses for its own hot-path captures. Kept separate from `capture` above (which
    /// stays the single synchronous resource `run_sync` and `run`'s CPU-only
    /// write-back fallback still use) rather than sharing one resource type across
    /// both purposes -- a slot mid-flight for one would otherwise have to be safe to
    /// borrow for the other's completely different, fully-synchronous contract.
    capture_pipeline: Option<capture::CapturePipeline>,
    /// The Phase 3 zero-copy capture path (`EXTERNAL_MEMORY_HOST_DESIGN.md`) --
    /// mutually exclusive with `capture_pipeline` above, never both active for the
    /// same device. One per protocol v3 wire slot (`PROTOCOL_V3_DESIGN.md`), each
    /// importing that slot's own disjoint proxy region -- no write-write hazard
    /// between them (unlike two of the same slot, which `DirectCapture`'s own doc
    /// comment still explains). `capture::run` decides `DirectCapture` vs.
    /// `CapturePipeline` once per call, cheaply, from `external_memory_host` below
    /// plus a live alignment query -- so this stays populated (or not) correctly even
    /// if that decision's answer could somehow change mid-process, though in practice
    /// it never does.
    direct_capture: [Option<capture::DirectCapture>; 2],
    /// Whether `NeuralForgeInstanceHooks::create_device` (crate::lib) got
    /// `VK_EXT_external_memory_host` added to this device's own creation -- set once,
    /// at construction, from a side channel only that hook can populate (see its own
    /// doc comment for why `create_info` here would always say "no" regardless of
    /// what was actually enabled). `false` is the common case (no ordinary game
    /// requests this extension on its own); `capture::run` treats it as "keep using
    /// the staging-buffer path", not an error.
    external_memory_host: bool,
    gpu_compose: Option<crate::composition::gpu::GpuCompose>,
    /// Reused across frames by `capture::run` for its own pre-edit frame snapshot,
    /// instead of a fresh `frame_bytes`-sized heap allocation every single present
    /// call -- see that function's own doc comment on why the snapshot exists at all.
    /// At 4K RGBA8 that's a ~31.6MiB allocation avoided every frame; measured on
    /// `lordnikon` (2026-09-10) at ~78ms per fresh allocation+copy, a real, if not
    /// fully explained (a `perf stat` on the same machine at the same time showed the
    /// process 97% backend-bound with an IPC of 0.1 -- a severe memory-subsystem
    /// stall this allocation likely aggravates without being its root cause), cost.
    original_scratch: Vec<u8>,
    /// The pipelined redesign's own persistent state -- see `capture::run`'s own doc
    /// comment for why a round trip's original frame has to outlive the present call
    /// that sent it, across however many present calls it takes the helper to answer.
    /// One per protocol v3 wire slot: each slot's in-flight request has its own,
    /// completely independent original frame and dims.
    inflight: [capture::Inflight; 2],
    /// A single disabled-state evaluation reserves the helper's images and NGX
    /// feature before the game's working set fills available VRAM (see
    /// `capture::run`'s own doc comment on why) -- one flag for the whole process,
    /// not per-slot: it only ever uses wire slot 0.
    bootstrap_complete: bool,
    /// Reused across frames the same way `original_scratch` is, for the answer bytes
    /// `capture::run` reads back once a round trip resolves.
    answer_scratch: Vec<u8>,
    /// The original frame paired with whichever wire slot most recently produced the
    /// answer currently held in `last_answer` -- not per-slot, since only one answer
    /// is ever the "currently presented" one at a time (see `capture::Inflight`'s own
    /// doc comment for why this moved out of the per-slot array).
    raw_answer_base: Vec<u8>,
    /// Monotonic identity for `raw_answer_base`/`last_answer` together -- GPU
    /// composition uploads only when this changes.
    raw_answer_generation: u64,
    last_answer: Vec<u8>,
    hotkey: crate::hotkey::Poller,
    /// Passive transfer observations for swapchains which could not be admitted at
    /// creation.  This is diagnostic-only: it never changes a game command buffer.
    observed_swapchain_writes: HashSet<vk::Image>,
    /// Keyed by the *source* image (the game's own internal render target the render
    /// tap reads from), never by a swapchain image -- see `prune_orphaned_tap_source`'s
    /// doc comment for why every insertion here has to be paired with eventual removal,
    /// not left to grow for the process's whole lifetime.
    tapped_source_layouts: HashMap<vk::Image, vk::ImageLayout>,
    tap_sources_by_destination: HashMap<vk::Image, vk::Image>,
}

/// Removes `source`'s `tapped_source_layouts` entry once nothing in
/// `tap_sources_by_destination` still points at it.
///
/// Real bug, found 2026-09-16 after a live GTA session ran into a GPU-level hang
/// (`Xid 109 CTX_SWITCH_TIMEOUT`) during genuinely long real play, never reproduced by
/// any short test: `tapped_source_layouts` was insert-only -- `observe_swapchain_write`
/// added an entry for every distinct source image the render tap ever observed, for
/// the whole life of the process, and nothing ever removed one. `destroy_swapchain_khr`
/// already cleaned up `tap_sources_by_destination` (keyed by the *destination*
/// swapchain image, which really is bounded by swapchain lifetime), but never touched
/// `tapped_source_layouts` at all.
///
/// The real danger isn't just unbounded growth: Vulkan explicitly allows a destroyed
/// image's handle value to be reused for a later, completely unrelated image. Once the
/// game frees one of its own internal render targets and a new allocation happens to
/// reuse that same handle, `cmd_pipeline_barrier`/`cmd_pipeline_barrier2` (which check
/// every barrier's image against this map, for every image in the whole process, not
/// just ones this layer cares about) would silently start updating *our* stale entry
/// to track the new, unrelated resource's layout -- and if `tap_sources_by_destination`
/// still pointed some live swapchain's destination at that same stale handle, the
/// present hook could then issue capture/composition GPU commands against an image the
/// game is concurrently using for something else entirely, under completely wrong
/// layout assumptions. That kind of concurrent, layout-incoherent access is exactly the
/// class of thing that can wedge a GPU's scheduler -- a plausible, concrete mechanism
/// for a real hang, not merely a memory leak, and one that only needed enough real
/// playtime for a handle to actually get reused, which is why it never showed up in
/// `vkcube` or any short synthetic test.
fn prune_orphaned_tap_source(state: &mut State, source: vk::Image) {
    if !state.tap_sources_by_destination.values().any(|&src| src == source) {
        state.tapped_source_layouts.remove(&source);
    }
}

type CleanupState = (Arc<ash::Device>, Arc<Mutex<State>>);
static CLEANUP: once_cell::sync::Lazy<Mutex<HashMap<vk::Device, CleanupState>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// Called only from vkDestroyDevice, before downstream device destruction.
/// Vulkan requires the caller to externally synchronize this device AND all its
/// queues here. That host-side contract makes a teardown-only device wait valid;
/// this is deliberately not done in a swapchain resize or per-frame hook.
/// # Safety
/// The caller must meet vkDestroyDevice's external synchronization requirements.
pub(crate) unsafe fn destroy_private_resources(handle: vk::Device) {
    let owned = CLEANUP.lock().unwrap().remove(&handle);
    if let Some((device, state)) = owned {
        let mut state = state.lock().unwrap();
        if state.capture.is_some() || state.capture_pipeline.is_some() || state.direct_capture.iter().any(Option::is_some) || state.gpu_compose.is_some() {
            match unsafe { device.device_wait_idle() } {
                Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST) => {
                    unsafe { capture::destroy(state.capture.take(), &device); }
                    unsafe { capture::destroy_pipeline(state.capture_pipeline.take(), &device); }
                    for slot in &mut state.direct_capture {
                        unsafe { capture::destroy_direct_capture(slot.take(), &device); }
                    }
                    if let Some(compose) = state.gpu_compose.take() {
                        unsafe { compose.destroy(&device); }
                    }
                }
                Err(error) => crate::log!("[layer] teardown wait failed: {:?}; cannot safely free pending resources", error),
            }
        }
        state.swapchains.clear();
        let mut primary = PRIMARY.lock().unwrap();
        if primary.as_ref().is_some_and(|p| p.device == handle) { *primary = None; }
        crate::log!("[layer] private device teardown complete {:?}", handle);
        crate::logging::flush();
    }
}

impl NeuralForgeDeviceInfo {
    pub fn new(
        instance: Option<Arc<ash::Instance>>,
        surface_caps: Option<vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR>,
        physical_device: vk::PhysicalDevice,
        device: Arc<ash::Device>,
        next_get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
        create_info: &vk::DeviceCreateInfo,
    ) -> Self {
        let handle = device.handle();
        // SAFETY: `next_get_device_proc_addr` is the next layer/driver's own
        // `vkGetDeviceProcAddr`, handed to us by the layer framework for exactly this
        // device; each name below matches the `PFN_vk*` type requested.
        let (create, destroy, present, get_images, get_queue, get_queue2) = unsafe {
            (
                resolve::<vk::PFN_vkCreateSwapchainKHR>(
                    next_get_device_proc_addr,
                    handle,
                    c"vkCreateSwapchainKHR",
                ),
                resolve::<vk::PFN_vkDestroySwapchainKHR>(
                    next_get_device_proc_addr,
                    handle,
                    c"vkDestroySwapchainKHR",
                ),
                resolve::<vk::PFN_vkQueuePresentKHR>(
                    next_get_device_proc_addr,
                    handle,
                    c"vkQueuePresentKHR",
                ),
                resolve::<vk::PFN_vkGetSwapchainImagesKHR>(
                    next_get_device_proc_addr,
                    handle,
                    c"vkGetSwapchainImagesKHR",
                ),
                resolve::<vk::PFN_vkGetDeviceQueue>(next_get_device_proc_addr, handle, c"vkGetDeviceQueue"),
                resolve::<vk::PFN_vkGetDeviceQueue2>(next_get_device_proc_addr, handle, c"vkGetDeviceQueue2"),
            )
        };
        // Set by `NeuralForgeInstanceHooks::create_device` (crate::lib) before this
        // device existed at all -- see that function's own doc comment for why this
        // is the only way to learn it here, since `create_info` below always reflects
        // the app's *original*, un-injected request regardless of what actually got
        // enabled. Stored on `State` below; `capture::run` reads it every call to
        // decide between `DirectCapture` and `CapturePipeline` (see
        // EXTERNAL_MEMORY_HOST_DESIGN.md).
        let external_memory_host = crate::take_external_memory_host_enabled(handle);
        crate::log!(
            "[layer] hooked device {:?} (swapchain support: {}, external_memory_host: {})",
            handle,
            create.is_some() && destroy.is_some() && present.is_some(),
            external_memory_host
        );
        // Explicit flush: a one-time-per-device event, not the per-frame hot path
        // `logging::log`'s modulo-64 throttle exists for -- worth the syscall so this
        // milestone survives a process killed by a signal before its normal exit path
        // (confirmed missing this session: a `timeout`-killed `vkcube` lost every log
        // line after this one, including real per-frame activity, purely because
        // nothing forced a flush past this first, coincidentally-flushed call).
        crate::logging::flush();
        let state = Arc::new(Mutex::new(State { external_memory_host, ..State::default() }));
        CLEANUP.lock().unwrap().insert(handle, (device.clone(), state.clone()));
        Self {
            // SAFETY: create_info is the loader chain for this newly created device.
            _loader_data: unsafe { crate::loader_data::register(handle, create_info) },
            device,
            instance,
            surface_caps,
            physical_device,
            next_create_swapchain_khr: create,
            next_destroy_swapchain_khr: destroy,
            next_queue_present_khr: present,
            next_get_swapchain_images_khr: get_images,
            next_get_device_queue: get_queue,
            next_get_device_queue2: get_queue2,
            state,
        }
    }

    /// The images backing `swapchain`, in the order the loader hands out indices for
    /// `VkPresentInfoKHR::pImageIndices` -- cached once at creation (see
    /// `create_swapchain_khr`) since the list never changes for a swapchain's lifetime.
    fn fetch_swapchain_images(&self, swapchain: vk::SwapchainKHR) -> Vec<vk::Image> {
        let Some(get_images) = self.next_get_swapchain_images_khr else { return Vec::new() };
        let handle = self.device.handle();
        let mut count = 0u32;
        // SAFETY: `get_images` was resolved from the next layer/driver's own proc-addr
        // table; the two-call enumeration pattern (count, then fill) is exactly what
        // the Vulkan spec requires for this function.
        if unsafe { get_images(handle, swapchain, &mut count, std::ptr::null_mut()) } != vk::Result::SUCCESS {
            return Vec::new();
        }
        let mut images = vec![vk::Image::null(); count as usize];
        // SAFETY: `images` has exactly `count` elements, matching what the first call
        // just reported.
        if unsafe { get_images(handle, swapchain, &mut count, images.as_mut_ptr()) } != vk::Result::SUCCESS {
            return Vec::new();
        }
        images
    }

    /// Records the first application transfer into each known swapchain image. A
    /// transfer command itself proves that the source image has the relevant read
    /// usage, making it a candidate for a later render-tap design. This hook is
    /// intentionally observational and always forwards the application command.
    fn observe_swapchain_write(
        &self, kind: &str, src: vk::Image, src_layout: vk::ImageLayout,
        dst: vk::Image, dst_layout: vk::ImageLayout, region_count: usize,
    ) {
        let mut state = self.state.lock().unwrap();
        let known = state.swapchains.values().any(|swapchain| swapchain.images.contains(&dst));
        if known && state.observed_swapchain_writes.insert(dst) {
            crate::log!("[layer] observed game {} into swapchain: src={:?} {:?} dst={:?} {:?} regions={}",
                kind, src, src_layout, dst, dst_layout, region_count);
            crate::logging::flush();
        }
        if known {
            state.tapped_source_layouts.insert(src, src_layout);
            // A destination normally keeps the same source for its whole life (the
            // game doesn't usually re-target its own blit/copy calls frame to frame),
            // but if it ever does, the old source needs the same orphan check
            // `destroy_swapchain_khr` already does -- otherwise a source that's no
            // longer referenced by anything would sit in `tapped_source_layouts`
            // forever, the same unbounded-growth/stale-handle hazard
            // `prune_orphaned_tap_source`'s own doc comment explains.
            if let Some(previous_source) = state.tap_sources_by_destination.insert(dst, src) {
                if previous_source != src {
                    prune_orphaned_tap_source(&mut state, previous_source);
                }
            }
        }
    }
}

impl DeviceInfo for NeuralForgeDeviceInfo {
    type HooksType = Self;
    type HooksRefType<'a> = &'a Self;

    fn hooked_commands() -> &'static [VulkanCommand] {
        &[
            VulkanCommand::CreateSwapchainKhr,
            VulkanCommand::DestroySwapchainKhr,
            VulkanCommand::QueuePresentKhr,
            VulkanCommand::GetDeviceQueue,
            VulkanCommand::GetDeviceQueue2,
            VulkanCommand::CmdCopyImage,
            VulkanCommand::CmdBlitImage,
            VulkanCommand::CmdPipelineBarrier,
            VulkanCommand::CmdPipelineBarrier2,
            VulkanCommand::DestroyImage,
        ]
    }

    fn hooks(&self) -> Self::HooksRefType<'_> {
        self
    }
}

impl DeviceHooks for NeuralForgeDeviceInfo {
    fn create_swapchain_khr(
        &self,
        create_info: &vk::SwapchainCreateInfoKHR,
        allocator: Option<&vk::AllocationCallbacks>,
    ) -> LayerResult<ash::prelude::VkResult<vk::SwapchainKHR>> {
        // No `VK_KHR_swapchain` on this device -- see `resolve()`'s doc comment. An app
        // that enabled the extension would never let this be `None`; let the framework's
        // own next-in-chain dispatch handle it exactly as if we weren't here.
        let Some(next_create) = self.next_create_swapchain_khr else {
            return LayerResult::Unhandled;
        };
        let eligible = crate::layer_enabled() && crate::ownership::eligible()
            && swapchain::is_supported_format(create_info.image_format)
            && create_info.image_extent.width <= neuralforge_protocol::MAX_W
            && create_info.image_extent.height <= neuralforge_protocol::MAX_H
            && swapchain::is_plausible_game_size(create_info.image_extent.width, create_info.image_extent.height);
        let adjusted = if eligible {
            self.instance.as_deref().and_then(|instance|
                self.surface_caps.and_then(|query| crate::surface_usage::prepare(instance, query, self.physical_device, create_info)))
        } else { None };
        let pass_through = adjusted.is_none();
        if pass_through && eligible {
            crate::log!("[layer] capture admission declined for {}x{} fmt={:?} usage={:?}",
                create_info.image_extent.width, create_info.image_extent.height,
                create_info.image_format, create_info.image_usage);
        }
        let mut swapchain = vk::SwapchainKHR::null();
        let alloc_ptr = allocator.map_or(std::ptr::null(), std::ptr::from_ref);
        // SAFETY: only image_usage changes in a private copy with verified support.
        // Forward exactly once: failed creation also retires oldSwapchain.
        let result = unsafe { next_create(self.device.handle(), adjusted.as_ref().unwrap_or(create_info), alloc_ptr, &mut swapchain) };
        if result != vk::Result::SUCCESS {
            return LayerResult::Handled(Err(result));
        }
        let hdr_kind = swapchain::detect_hdr_kind(create_info.image_format, create_info.image_color_space);
        // Cache even a pass-through swapchain's images. This lets the passive command
        // diagnostics identify a legal render-to-swapchain transfer without touching
        // the application's creation or recording path.
        let images = self.fetch_swapchain_images(swapchain);
        let state = SwapchainState {
            format: create_info.image_format,
            width: create_info.image_extent.width,
            height: create_info.image_extent.height,
            hdr_kind,
            pass_through,
            images,
        };
        crate::log!(
            "[layer] swapchain {:?} {}x{} fmt={:?} hdr={} pass_through={} images={}",
            swapchain,
            state.width,
            state.height,
            state.format,
            state.hdr_kind,
            state.pass_through,
            state.images.len()
        );
        let mut layer_state = self.state.lock().unwrap();
        layer_state.swapchains.insert(swapchain, state);
        if !pass_through {
            if let Some(instance) = self.instance.as_deref() {
                layer_state.shm.prepare_motion_resources(instance, self.physical_device,
                    create_info.image_extent.width, create_info.image_extent.height,
                    swapchain::proxy_format_for(create_info.image_format));
            }
        }
        // Explicit flush, same reasoning as `new()`'s -- a one-time-per-swapchain
        // milestone, not the per-frame hot path.
        crate::logging::flush();
        LayerResult::Handled(Ok(swapchain))
    }

    fn get_device_queue(&self, queue_family_index: u32, queue_index: u32) -> LayerResult<vk::Queue> {
        let Some(next) = self.next_get_device_queue else { return LayerResult::Unhandled };
        let mut queue = vk::Queue::null();
        // SAFETY: `next` was resolved from the next layer/driver's own proc-addr
        // table; `queue_family_index`/`queue_index` are the caller's own, forwarded
        // unchanged.
        unsafe { next(self.device.handle(), queue_family_index, queue_index, &mut queue) };
        self.state.lock().unwrap().queue_families.insert(queue, queue_family_index);
        LayerResult::Handled(queue)
    }

    fn get_device_queue2(&self, queue_info: &vk::DeviceQueueInfo2) -> LayerResult<vk::Queue> {
        let Some(next) = self.next_get_device_queue2 else { return LayerResult::Unhandled };
        let mut queue = vk::Queue::null();
        // SAFETY: `next` was resolved from the next layer/driver's own proc-addr
        // table; `queue_info` is valid for the duration of this call (handed to us by
        // the loader for exactly this call).
        unsafe { next(self.device.handle(), queue_info, &mut queue) };
        self.state.lock().unwrap().queue_families.insert(queue, queue_info.queue_family_index);
        LayerResult::Handled(queue)
    }

    fn cmd_copy_image(
        &self, _command_buffer: vk::CommandBuffer, src: vk::Image, src_layout: vk::ImageLayout,
        dst: vk::Image, dst_layout: vk::ImageLayout, regions: &[vk::ImageCopy],
    ) -> LayerResult<()> {
        self.observe_swapchain_write("copy", src, src_layout, dst, dst_layout, regions.len());
        LayerResult::Unhandled
    }

    fn cmd_blit_image(
        &self, _command_buffer: vk::CommandBuffer, src: vk::Image, src_layout: vk::ImageLayout,
        dst: vk::Image, dst_layout: vk::ImageLayout, regions: &[vk::ImageBlit], _filter: vk::Filter,
    ) -> LayerResult<()> {
        self.observe_swapchain_write("blit", src, src_layout, dst, dst_layout, regions.len());
        LayerResult::Unhandled
    }

    fn cmd_pipeline_barrier(
        &self, _command_buffer: vk::CommandBuffer, _src_stage: vk::PipelineStageFlags,
        _dst_stage: vk::PipelineStageFlags, _dependency: vk::DependencyFlags,
        _memory: &[vk::MemoryBarrier], _buffers: &[vk::BufferMemoryBarrier],
        images: &[vk::ImageMemoryBarrier],
    ) -> LayerResult<()> {
        let mut state = self.state.lock().unwrap();
        for barrier in images {
            if let Some(layout) = state.tapped_source_layouts.get_mut(&barrier.image) {
                *layout = barrier.new_layout;
                crate::log!("[layer] tracked GTA render source {:?}: {:?} -> {:?}",
                    barrier.image, barrier.old_layout, barrier.new_layout);
            }
        }
        LayerResult::Unhandled
    }

    fn cmd_pipeline_barrier2(
        &self, _command_buffer: vk::CommandBuffer, info: &vk::DependencyInfo,
    ) -> LayerResult<()> {
        // SAFETY: the layer framework validated `info` for this application call;
        // the count/pointer pair is valid for the hook's duration.
        let images = unsafe { std::slice::from_raw_parts(info.p_image_memory_barriers,
            info.image_memory_barrier_count as usize) };
        let mut state = self.state.lock().unwrap();
        for barrier in images {
            if let Some(layout) = state.tapped_source_layouts.get_mut(&barrier.image) {
                *layout = barrier.new_layout;
                crate::log!("[layer] tracked GTA render source {:?}: {:?} -> {:?} (sync2)",
                    barrier.image, barrier.old_layout, barrier.new_layout);
            }
        }
        LayerResult::Unhandled
    }

    fn destroy_swapchain_khr(
        &self,
        swapchain: vk::SwapchainKHR,
        allocator: Option<&vk::AllocationCallbacks>,
    ) -> LayerResult<()> {
        let Some(next_destroy) = self.next_destroy_swapchain_khr else {
            return LayerResult::Unhandled;
        };
        release_primary(self.device.handle(), swapchain);
        {
            let mut state = self.state.lock().unwrap();
            if let Some(old) = state.swapchains.remove(&swapchain) {
                for image in &old.images {
                    state.observed_swapchain_writes.remove(image);
                    if let Some(source) = state.tap_sources_by_destination.remove(image) {
                        prune_orphaned_tap_source(&mut state, source);
                    }
                }
                if let Some(gpu) = &mut state.gpu_compose { gpu.retire_present_images(&old.images); }
            }
        }
        let alloc_ptr = allocator.map_or(std::ptr::null(), std::ptr::from_ref);
        // SAFETY: same contract as `create_swapchain_khr` above.
        unsafe { next_destroy(self.device.handle(), swapchain, alloc_ptr) };
        LayerResult::Handled(())
    }

    /// The source-side half of the tap-source lifetime fix -- see
    /// `prune_orphaned_tap_source`'s doc comment for the destination-side half and the
    /// real bug behind both. Pruning only when a *destination* mapping goes away leaves
    /// the most dangerous case open: the game destroys one of its own render targets
    /// (a tap source) while the swapchain that referenced it lives on, the driver hands
    /// that same handle value to a later, unrelated image, and both maps here still
    /// name it as a live source in a known layout. Dropping it the moment the game
    /// destroys it closes that window at the only point it can actually be closed.
    /// Purely observational -- the app's own destroy is always forwarded unchanged.
    fn destroy_image(&self, image: vk::Image, _allocator: Option<&vk::AllocationCallbacks>) -> LayerResult<()> {
        let mut state = self.state.lock().unwrap();
        if state.tapped_source_layouts.remove(&image).is_some() {
            state.tap_sources_by_destination.retain(|_, src| *src != image);
        }
        LayerResult::Unhandled
    }

    fn queue_present_khr(
        &self,
        queue: vk::Queue,
        present_info: &vk::PresentInfoKHR,
    ) -> LayerResult<ash::prelude::VkResult<()>> {
        let Some(next_present) = self.next_queue_present_khr else {
            return LayerResult::Unhandled;
        };
        // Set by `capture::run` only when `composition::gpu::GpuCompose::dispatch_into_image_async`
        // wrote this frame's composited result asynchronously -- see that function's
        // own doc comment. When `Some`, the real present call below *must* wait on it,
        // or the presentation engine could display the image before the GPU work that
        // writes it has actually finished (a real, visible corruption bug, not a style
        // preference).
        let mut wait_semaphore: Option<vk::Semaphore> = None;
        if crate::layer_enabled() {
            // SAFETY: `p_swapchains`/`p_image_indices`/`swapchain_count` are a valid,
            // parallel pair of slices for the duration of this call -- part of the
            // `VkPresentInfoKHR` the loader just handed us.
            let (swapchains, image_indices) = unsafe {
                (
                    std::slice::from_raw_parts(present_info.p_swapchains, present_info.swapchain_count as usize),
                    std::slice::from_raw_parts(present_info.p_image_indices, present_info.swapchain_count as usize),
                )
            };
            let mut state = self.state.lock().unwrap();
            for (&sc, &image_index) in swapchains.iter().zip(image_indices) {
                let Some(sw) = state.swapchains.get(&sc) else { continue };
                let Some(&image) = sw.images.get(image_index as usize) else { break };
                let tap = state.tap_sources_by_destination.get(&image).and_then(|source|
                    state.tapped_source_layouts.get(source).map(|layout| (*source, *layout)));
                if sw.pass_through && !matches!(tap, Some((_, vk::ImageLayout::GENERAL))) {
                    continue;
                }
                if !claim_primary(self.device.handle(), sc, sw.width, sw.height) {
                    break;
                }
                let Some(&queue_family) = state.queue_families.get(&queue) else {
                    // We've never seen this queue via a hooked `vkGetDeviceQueue`/
                    // `vkGetDeviceQueue2` call (e.g. an app using `VK_KHR_synchronization2`
                    // queue submission paths this layer doesn't intercept) -- no family
                    // to build a command pool on, so fail open rather than guess one.
                    break;
                };
                let width = sw.width;
                let height = sw.height;
                let proxy_format = swapchain::proxy_format_for(sw.format);
                let bgr_order = swapchain::is_bgr_order(sw.format);
                let (capture_image, capture_layout) = tap.unwrap_or((image, vk::ImageLayout::PRESENT_SRC_KHR));
                let State { shm, capture, capture_pipeline, direct_capture, external_memory_host, gpu_compose, original_scratch, inflight, bootstrap_complete, answer_scratch, raw_answer_base, raw_answer_generation, last_answer, hotkey, .. } = &mut *state;
                shm.poll_toggle_hotkey(hotkey);
                if shm.model_known_unavailable() {
                    // The helper has permanently disabled itself for this session
                    // (see `ngx::ensure_feature`'s one-shot design) -- nothing will
                    // ever evaluate a captured frame, so paying for the capture
                    // itself (a full image<->buffer round trip plus a whole-frame
                    // `memcpy`, every single present call) is pure waste. Skip
                    // straight to a real no-op present, matching what "fail-open"
                    // should actually cost: nothing.
                    break;
                }
                if let Some(instance) = &self.instance {
                    // SAFETY: `queue` is the same queue this present call was made on,
                    // externally synchronized for its duration by the same Vulkan rule
                    // that lets the caller call `vkQueuePresentKHR` on it at all right
                    // after this returns -- exactly this function's own safety
                    // contract. `image` is one of `sc`'s own images, currently
                    // `PRESENT_SRC_KHR` per `vkQueuePresentKHR`'s precondition on every
                    // image it's about to present.
                    unsafe {
                        wait_semaphore = capture::run(
                            &self.device,
                            instance,
                            self.physical_device,
                            queue,
                            queue_family,
                            capture_image,
                            capture_layout,
                            image,
                            width,
                            height,
                            proxy_format,
                            bgr_order,
                            capture,
                            capture_pipeline,
                            direct_capture,
                            *external_memory_host,
                            gpu_compose,
                            shm,
                            original_scratch,
                            inflight,
                            bootstrap_complete,
                            answer_scratch,
                            raw_answer_base,
                            raw_answer_generation,
                            last_answer,
                        );
                    }
                }
                break;
            }
        }

        // SAFETY: `present_info` is valid for the duration of this call; `next_present`
        // was resolved from the next layer/driver's own proc-addr table.
        let result = if let Some(sem) = wait_semaphore {
            // Combine whatever wait semaphores the app itself already provided with
            // our own -- never replace them, `capture::run`'s own compute work is an
            // *additional* dependency the present must wait on, not a substitute for
            // whatever the app was already correctly synchronizing against (its own
            // rendering-complete semaphore, most commonly).
            let mut combined: Vec<vk::Semaphore> = Vec::with_capacity(present_info.wait_semaphore_count as usize + 1);
            if present_info.wait_semaphore_count > 0 {
                // SAFETY: `p_wait_semaphores` is a valid slice of `wait_semaphore_count`
                // elements per `present_info`'s own contract, valid for this call's
                // duration.
                combined.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(present_info.p_wait_semaphores, present_info.wait_semaphore_count as usize)
                });
            }
            combined.push(sem);
            // Copies every other field (`p_next`, `swapchain_count`, `p_swapchains`,
            // `p_image_indices`, `p_results`) unchanged from the app's own
            // `present_info` -- only the wait-semaphore list is actually different.
            let modified_info = vk::PresentInfoKHR { wait_semaphore_count: combined.len() as u32, p_wait_semaphores: combined.as_ptr(), ..*present_info };
            // SAFETY: `modified_info` is valid for the duration of this call --
            // `combined` (which it borrows from) outlives it; `next_present` was
            // resolved from the next layer/driver's own proc-addr table.
            unsafe { next_present(queue, &modified_info) }
        } else {
            // SAFETY: `present_info` is valid for the duration of this call;
            // `next_present` was resolved from the next layer/driver's own
            // proc-addr table.
            unsafe { next_present(queue, present_info) }
        };
        LayerResult::Handled(result.result())
    }
}

#[cfg(test)]
mod tap_source_lifetime_tests {
    use super::*;
    use ash::vk::Handle;

    fn image(raw: u64) -> vk::Image {
        vk::Image::from_raw(raw)
    }

    /// The exact scenario `prune_orphaned_tap_source`'s own doc comment describes:
    /// once nothing in `tap_sources_by_destination` points at a source any more, its
    /// `tapped_source_layouts` entry must actually go, not sit there for the rest of
    /// the process's life -- real 2026-09-16 bug, this guards against reintroducing it.
    #[test]
    fn prune_orphaned_tap_source_removes_a_truly_unreferenced_source() {
        let mut state = State::default();
        let source = image(1);
        state.tapped_source_layouts.insert(source, vk::ImageLayout::GENERAL);
        // No entry in `tap_sources_by_destination` points at `source` at all.
        prune_orphaned_tap_source(&mut state, source);
        assert!(!state.tapped_source_layouts.contains_key(&source));
    }

    #[test]
    fn prune_orphaned_tap_source_keeps_a_source_still_referenced_elsewhere() {
        let mut state = State::default();
        let source = image(1);
        state.tapped_source_layouts.insert(source, vk::ImageLayout::GENERAL);
        // A second, still-live destination also reads from this same source image --
        // pruning must not remove it out from under that live reference.
        state.tap_sources_by_destination.insert(image(2), source);
        prune_orphaned_tap_source(&mut state, source);
        assert!(state.tapped_source_layouts.contains_key(&source));
    }

    /// Simulates `destroy_swapchain_khr`'s own cleanup loop directly against `State`
    /// (its real trait method needs a live Vulkan device this test has no reason to
    /// stand up) -- proves a destroyed swapchain's own destination images no longer
    /// leave their source orphaned in `tapped_source_layouts` once nothing else
    /// references it, the actual leak this session found via a real GPU hang.
    #[test]
    fn destroying_the_only_swapchain_referencing_a_source_prunes_it() {
        let mut state = State::default();
        let source = image(10);
        let destination = image(20);
        state.tapped_source_layouts.insert(source, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        state.tap_sources_by_destination.insert(destination, source);

        // What `destroy_swapchain_khr` does for each of the destroyed swapchain's own
        // images.
        if let Some(removed_source) = state.tap_sources_by_destination.remove(&destination) {
            prune_orphaned_tap_source(&mut state, removed_source);
        }

        assert!(!state.tap_sources_by_destination.contains_key(&destination));
        assert!(!state.tapped_source_layouts.contains_key(&source));
    }

    /// Two live swapchains sharing one source image (a real, normal case -- e.g. the
    /// same off-screen render target blitted into two different swapchains): only
    /// destroying *both* destinations should prune the shared source.
    #[test]
    fn a_source_shared_by_two_destinations_survives_until_both_are_gone() {
        let mut state = State::default();
        let source = image(10);
        state.tapped_source_layouts.insert(source, vk::ImageLayout::GENERAL);
        state.tap_sources_by_destination.insert(image(20), source);
        state.tap_sources_by_destination.insert(image(21), source);

        state.tap_sources_by_destination.remove(&image(20));
        prune_orphaned_tap_source(&mut state, source);
        assert!(state.tapped_source_layouts.contains_key(&source), "still referenced by image(21)");

        state.tap_sources_by_destination.remove(&image(21));
        prune_orphaned_tap_source(&mut state, source);
        assert!(!state.tapped_source_layouts.contains_key(&source));
    }

    /// `observe_swapchain_write`'s own re-target case: a destination that starts
    /// pointing at one source and later points at a different one must orphan-check
    /// the *old* source, the same way losing the destination entirely does.
    #[test]
    fn retargeting_a_destination_to_a_new_source_prunes_the_old_one_if_unreferenced() {
        let mut state = State::default();
        let destination = image(20);
        let old_source = image(1);
        let new_source = image(2);
        state.tapped_source_layouts.insert(old_source, vk::ImageLayout::GENERAL);
        state.tap_sources_by_destination.insert(destination, old_source);

        // What `observe_swapchain_write` does when a destination's source changes.
        state.tapped_source_layouts.insert(new_source, vk::ImageLayout::GENERAL);
        if let Some(previous_source) = state.tap_sources_by_destination.insert(destination, new_source) {
            if previous_source != new_source {
                prune_orphaned_tap_source(&mut state, previous_source);
            }
        }

        assert!(!state.tapped_source_layouts.contains_key(&old_source));
        assert!(state.tapped_source_layouts.contains_key(&new_source));
    }

    /// `destroy_image`'s own logic against `State`: the game destroying a *source*
    /// image (its own render target) while the swapchain that reads from it is still
    /// alive must drop both the layout entry and every destination mapping naming it
    /// -- the handle-reuse hazard `prune_orphaned_tap_source`'s doc comment describes,
    /// which destination-side pruning alone can never catch.
    #[test]
    fn destroying_a_source_image_drops_its_layout_and_every_mapping_to_it() {
        let mut state = State::default();
        let source = image(10);
        let unrelated_source = image(11);
        state.tapped_source_layouts.insert(source, vk::ImageLayout::GENERAL);
        state.tapped_source_layouts.insert(unrelated_source, vk::ImageLayout::GENERAL);
        state.tap_sources_by_destination.insert(image(20), source);
        state.tap_sources_by_destination.insert(image(21), source);
        state.tap_sources_by_destination.insert(image(22), unrelated_source);

        // What `destroy_image` does when the game frees `source`.
        if state.tapped_source_layouts.remove(&source).is_some() {
            state.tap_sources_by_destination.retain(|_, src| *src != source);
        }

        assert!(!state.tapped_source_layouts.contains_key(&source));
        assert!(!state.tap_sources_by_destination.contains_key(&image(20)));
        assert!(!state.tap_sources_by_destination.contains_key(&image(21)));
        // The unrelated source and its own destination are untouched.
        assert!(state.tapped_source_layouts.contains_key(&unrelated_source));
        assert_eq!(state.tap_sources_by_destination.get(&image(22)), Some(&unrelated_source));
    }
}
