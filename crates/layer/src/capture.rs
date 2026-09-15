//! Milestone 4 phase A/B: capture the image `queue_present_khr` is about to present
//! into the shared-memory proxy region, run the round trip, and copy a result back
//! before the real present call.
//!
//! No `VK_EXT_external_memory_host` import yet -- every byte crosses an explicit CPU
//! `memcpy` between a host-visible/host-coherent staging buffer and the mapping
//! `ShmClient` owns. That is exactly the "staging copy" fallback
//! `crates/helper/src/shm.rs`'s own doc comment already describes as always-correct,
//! just not zero-copy; importing the mapping directly as device memory is a later
//! optimization on top of this, not a prerequisite for it working.
//!
//! Stage 1 (capture into a staging buffer) is one command buffer + one fence,
//! synchronous -- the CPU needs those bytes before it can even start the SHM round
//! trip, so there's no way around blocking on it. What happens after the round trip
//! depends on the settings and what's available: the common case (real GPU compose,
//! no debug dump pending) is `composition::gpu::GpuCompose::dispatch_into_image_async`
//! (2026-09-10) -- non-blocking, its own doc comment covers why that's sound. Every
//! other case (CPU compose, a pending `capture_request`, `RGBA16F`, no GPU available)
//! still falls back to the original synchronous stage-2 write-back below, one more
//! command buffer + fence wait, same as this whole function used to always do.

use ash::vk;

use crate::shm::ShmClient;

pub struct CaptureResources {
    queue_family: u32,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    capacity: vk::DeviceSize,
}

// SAFETY: every field is either a plain Vulkan handle (as `Send`-safe as `ash::Device`
// itself already assumes) or `ptr`, a `vkMapMemory` pointer into memory this struct
// owns exclusively -- never aliased outside the `Mutex<State>` this always lives behind
// in `NeuralForgeDeviceInfo`.
unsafe impl Send for CaptureResources {}

impl CaptureResources {
    /// # Safety
    /// Must not be called while any submitted work referencing these handles might
    /// still be in flight -- callers only ever call this right after a successful
    /// `vkWaitForFences` on `self.fence`, or at device-destruction time.
    unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: forwarded from this function's own contract.
        unsafe {
            device.destroy_fence(self.fence, None);
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
            device.destroy_command_pool(self.pool, None);
        }
    }
}

/// One command pool/buffer + fence + host-coherent staging buffer -- the resource
/// bundle both [`CaptureResources`] (a single one) and [`CapturePipeline`] (two, see
/// its own doc comment) are built from. Pulled out so there is exactly one place that
/// builds/unwinds this specific allocation sequence, not two copies that could drift.
struct CaptureBuffer {
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    capacity: vk::DeviceSize,
}

// SAFETY: same reasoning as `CaptureResources`'s own impl below -- plain Vulkan
// handles plus a `vkMapMemory` pointer into memory this struct owns exclusively.
unsafe impl Send for CaptureBuffer {}

impl CaptureBuffer {
    /// # Safety
    /// Must not be called while any submitted work referencing these handles might
    /// still be in flight -- see every caller's own safety comment for how each
    /// upholds that.
    unsafe fn destroy(&self, device: &ash::Device) {
        // SAFETY: forwarded from this function's own contract.
        unsafe {
            device.destroy_fence(self.fence, None);
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
            device.destroy_command_pool(self.pool, None);
        }
    }
}

fn build_capture_buffer(device: &ash::Device, instance: &ash::Instance, physical_device: vk::PhysicalDevice, queue_family: u32, bytes: vk::DeviceSize) -> Option<CaptureBuffer> {
    let pool_info =
        vk::CommandPoolCreateInfo::builder().queue_family_index(queue_family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    // SAFETY: `device` is the live device this capture serves; `pool_info` is valid.
    let Ok(pool) = (unsafe { device.create_command_pool(&pool_info, None) }) else { return None };

    let alloc_info = vk::CommandBufferAllocateInfo::builder()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    // SAFETY: `pool` was just created above.
    let cmd = match unsafe { crate::loader_data::allocate_commands(device, &alloc_info) } {
        Ok(bufs) => bufs[0],
        Err(_) => {
            // SAFETY: `pool` owns no other resources yet.
            unsafe { device.destroy_command_pool(pool, None) };
            return None;
        }
    };

    let fence_info = vk::FenceCreateInfo::builder().flags(vk::FenceCreateFlags::SIGNALED);
    // SAFETY: starting signaled means the first use's own wait/poll never blocks (or
    // reports pending) on a fence nothing has submitted work against yet.
    let fence = match unsafe { device.create_fence(&fence_info, None) } {
        Ok(f) => f,
        Err(_) => {
            // SAFETY: `pool` owns no other resources yet; freeing it also frees `cmd`.
            unsafe { device.destroy_command_pool(pool, None) };
            return None;
        }
    };

    let buf_info = vk::BufferCreateInfo::builder()
        .size(bytes)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: `buf_info` is valid.
    let buffer = match unsafe { device.create_buffer(&buf_info, None) } {
        Ok(b) => b,
        Err(_) => {
            // SAFETY: neither `fence` nor `pool` owns `buffer` (it doesn't exist).
            unsafe {
                device.destroy_fence(fence, None);
                device.destroy_command_pool(pool, None);
            }
            return None;
        }
    };
    // SAFETY: `buffer` was just created and is not yet bound to memory.
    let reqs = unsafe { device.get_buffer_memory_requirements(buffer) };
    // SAFETY: `physical_device` is the device this capture serves; `instance` is its
    // owning instance (stored once at `vkCreateInstance`, see `crate::CURRENT_INSTANCE`).
    let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    // The Vulkan spec guarantees at least one memory type with both bits set, so this
    // failing would mean a spec-non-compliant driver, not a real device limitation --
    // still handled as a plain "skip capture" rather than assumed impossible.
    let Some(type_index) = (0..mem_props.memory_type_count)
        .find(|&i| (reqs.memory_type_bits & (1 << i)) != 0 && mem_props.memory_types[i as usize].property_flags.contains(wanted))
    else {
        // SAFETY: `buffer` has no memory bound yet; nothing else owns `fence`/`pool`.
        unsafe {
            device.destroy_buffer(buffer, None);
            device.destroy_fence(fence, None);
            device.destroy_command_pool(pool, None);
        }
        return None;
    };

    let alloc = vk::MemoryAllocateInfo::builder().allocation_size(reqs.size).memory_type_index(type_index);
    // SAFETY: `alloc` is valid; `type_index` was just confirmed to satisfy `reqs`.
    let memory = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(_) => {
            // SAFETY: same reasoning as the branch above.
            unsafe {
                device.destroy_buffer(buffer, None);
                device.destroy_fence(fence, None);
                device.destroy_command_pool(pool, None);
            }
            return None;
        }
    };
    // SAFETY: `buffer`/`memory` were each just created above, sized/typed to satisfy
    // each other by construction.
    if unsafe { device.bind_buffer_memory(buffer, memory, 0) }.is_err() {
        // SAFETY: `memory` is not yet bound to anything that would make freeing it
        // unsound; `buffer` has no memory bound.
        unsafe {
            device.free_memory(memory, None);
            device.destroy_buffer(buffer, None);
            device.destroy_fence(fence, None);
            device.destroy_command_pool(pool, None);
        }
        return None;
    }
    // SAFETY: `memory` is `HOST_VISIBLE` by the type selection above; mapping the
    // whole allocation is always in bounds.
    let ptr = match unsafe { device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) } {
        Ok(p) => p.cast::<u8>(),
        Err(_) => {
            // SAFETY: same reasoning as the bind-failure branch above.
            unsafe {
                device.free_memory(memory, None);
                device.destroy_buffer(buffer, None);
                device.destroy_fence(fence, None);
                device.destroy_command_pool(pool, None);
            }
            return None;
        }
    };

    Some(CaptureBuffer { pool, cmd, fence, buffer, memory, ptr, capacity: reqs.size })
}

/// Builds (or rebuilds, if the queue family changed or `bytes` grew past what's
/// already allocated) the resources capture needs. `existing` is left `None` on any
/// failure -- every caller treats that as "skip capture this frame, present
/// unmodified", never a reason to stop trying on a later frame.
fn ensure(
    existing: &mut Option<CaptureResources>,
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
    bytes: vk::DeviceSize,
) -> bool {
    if let Some(r) = existing {
        if r.queue_family == queue_family && r.capacity >= bytes {
            return true;
        }
        // SAFETY: called between frames, never while `r.fence` might still be
        // unsignaled from an in-flight submission -- `queue_present_khr` only reaches
        // here after the previous frame's own capture fully completed.
        unsafe { r.destroy(device) };
        *existing = None;
    }
    let Some(b) = build_capture_buffer(device, instance, physical_device, queue_family, bytes) else { return false };
    *existing = Some(CaptureResources { queue_family, pool: b.pool, cmd: b.cmd, fence: b.fence, buffer: b.buffer, memory: b.memory, ptr: b.ptr, capacity: b.capacity });
    true
}

fn subresource() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::builder()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
        .build()
}

fn barrier(image: vk::Image, old: vk::ImageLayout, new: vk::ImageLayout, src: vk::AccessFlags, dst: vk::AccessFlags) -> vk::ImageMemoryBarrier {
    vk::ImageMemoryBarrier::builder()
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(subresource())
        .src_access_mask(src)
        .dst_access_mask(dst)
        .build()
}

/// What [`run`] is carrying forward from the round trip it most recently *sent*,
/// across as many present calls as the helper takes to answer it. `original` holds
/// the exact pixels captured at send time -- needed again once the answer finally
/// arrives, since composition combines the two -- alongside the dimensions/format
/// that capture was taken at, so a resolution change mid-flight is detected (and the
/// stale pair discarded) rather than composited against a mismatched frame size.
#[derive(Default)]
pub struct Inflight {
    original: Vec<u8>,
    dims: Option<(u32, u32, u32)>,
    /// A single disabled-state evaluation reserves the helper's images and NGX
    /// feature before the game's working set fills available VRAM.  It never writes
    /// a result to the swapchain, so starting the game with NR off stays visually
    /// and functionally off.
    bootstrap_complete: bool,
    /// Monotonic identity for the held raw helper result.  GPU slots use this to
    /// upload only when a newly evaluated answer arrives.
    raw_answer_generation: u64,
    raw_answer_base: Vec<u8>,
}

/// Real per-frame NR compute (a helper round trip through a Wine-hosted process, plus
/// whatever GPU work either side does) does not run at anywhere close to swapchain
/// present rate -- measured on real hardware (`lordnikon`, 2026-09-10, see
/// `CLAUDE.md`) at roughly 100-150ms end to end even once every other bottleneck
/// found that same session was fixed. [`run_sync`] (this crate's entire capture path
/// before this) called that round trip, and blocked waiting for it, from *inside*
/// every single present call -- meaning the game's own presentation rate could never
/// exceed the round trip's, even though the actual GPU compute involved is only a
/// few milliseconds. That coupling, not any single slow operation, was the real
/// cause of a reported ~2.8 fps at 4K with NR on, confirmed by removing this
/// project's layer entirely and watching the same game return to 99% GPU utilization
/// and a normal framerate.
///
/// This function decouples the two: it captures and sends a new frame only when no
/// round trip is currently in flight, checks on any in-flight one *without blocking*
/// (see [`ShmClient::poll_async_request`]), and applies whatever answer arrives to
/// whichever frame happens to be current at that moment -- not necessarily the one
/// that was captured alongside it. Every other frame (which, once the pipeline is
/// running, is most of them) touches `image` not at all and returns `None`
/// immediately, at effectively zero cost. The tradeoff this accepts, deliberately,
/// per Alex's own explicit authorization ("do it if it gives us the most frames when
/// NR is on"): the visible NR enhancement updates at whatever rate the round trip
/// actually achieves, not every frame, and is very occasionally composited against a
/// slightly newer frame than the one it was computed from (a few frames of temporal
/// staleness at most, bounded by the round trip's own duration) -- a real quality
/// tradeoff, not a free lunch, but one that keeps the game's own rendering and
/// presentation running at its true native rate instead of being held hostage by a
/// cross-process IPC round trip on every single frame.
///
/// Ordering inside a single call matters and is deliberate: capturing a new frame
/// (when due) always happens *before* compositing an answer that arrived this same
/// frame, because compositing overwrites `image` -- capturing after that would
/// capture this function's own composited output instead of the game's real
/// rendering, feeding a corrupted "original" into the next cycle.
///
/// Polls whatever capture is already in flight -- [`DirectCapture`] when `use_direct`,
/// otherwise [`CapturePipeline`] -- and, if one just completed, gets its bytes into
/// `original_scratch` and the SHM proxy region, then returns `true`. If nothing
/// completed this call, tries to submit a new capture into whichever strategy is
/// active instead (`false` either way). Shared by both of `run`'s capture call sites
/// (the disabled-bootstrap one-shot and the main enabled path below) -- they differ
/// only in what they do with a successful result afterward (`inflight` bookkeeping),
/// not in how a capture gets started or consumed.
///
/// For [`CapturePipeline`], "gets its bytes into `original_scratch` and the SHM proxy
/// region" means what it always has: copy staging memory into `original_scratch`, then
/// [`ShmClient::write_proxy`] copies that into the proxy region. For [`DirectCapture`],
/// the GPU write already landed the bytes in the proxy region directly -- no
/// `write_proxy` call needed, that copy is exactly what importing the region as device
/// memory removes -- but `original_scratch` still needs its own stable copy (read back
/// out of the now-written proxy region), because `inflight.original`/`prepare_motion`
/// need bytes that survive whatever capture starts next and overwrites that region,
/// which the live proxy region itself can't provide once it's shared, imported memory.
#[allow(clippy::too_many_arguments)]
fn poll_or_submit_capture(
    use_direct: bool,
    pipeline: &mut Option<CapturePipeline>,
    direct: &mut Option<DirectCapture>,
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family: u32,
    capture_image: vk::Image,
    capture_layout: vk::ImageLayout,
    width: u32,
    height: u32,
    proxy_format: u32,
    frame_bytes: u64,
    shm: &mut ShmClient,
    original_scratch: &mut Vec<u8>,
) -> bool {
    if use_direct {
        let Some((host_ptr, capacity)) = shm.proxy_region() else { return false };
        // SAFETY: `host_ptr`/`capacity` describe `shm`'s own live proxy region, valid
        // for as long as `shm` stays open (the life of this process, since the
        // mapping is never unmapped -- see `neuralforge_protocol::mapping::Mapping::header`'s
        // own doc comment on the equivalent GUI/CLI mapping); nothing else writes to
        // it except through `ShmClient::write_proxy`, which this branch never calls.
        if !unsafe {
            ensure_direct_capture(direct, device, instance, physical_device, queue_family, host_ptr, capacity as vk::DeviceSize)
        } {
            return false;
        }
        let d = direct.as_mut().expect("just ensured above");
        if let Some(_dims) = poll_direct_capture(d, device) {
            let n = capacity.min(frame_bytes as usize);
            original_scratch.clear();
            // SAFETY: `host_ptr` is `shm`'s own live proxy region, valid for at least
            // `capacity` bytes; `poll_direct_capture` returning `Some` just confirmed
            // this slot's fence signaled, making the GPU's writes to it visible to the
            // CPU (host-coherent memory backs every capture buffer in this module,
            // imported or not).
            original_scratch.extend_from_slice(unsafe { std::slice::from_raw_parts(host_ptr, n) });
            shm.set_frame_info(width, height, proxy_format);
            return true;
        }
        submit_direct_capture(d, device, queue, capture_image, capture_layout, width, height, proxy_format);
        false
    } else {
        if !ensure_pipeline(pipeline, device, instance, physical_device, queue_family, frame_bytes) {
            return false;
        }
        let p = pipeline.as_mut().expect("just ensured above");
        if poll_pipeline_capture(p, device, original_scratch).is_some() {
            shm.set_frame_info(width, height, proxy_format);
            shm.write_proxy(original_scratch);
            return true;
        }
        submit_pipeline_capture(p, device, queue, capture_image, capture_layout, width, height, proxy_format);
        false
    }
}

/// # Safety
/// Same contract as [`run_sync`]: `queue` must be the same queue `image`'s
/// presentation was requested on, with no concurrent use of it from another thread
/// for the duration of this call.
#[allow(clippy::too_many_arguments)]
pub unsafe fn run(
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family: u32,
    capture_image: vk::Image,
    capture_layout: vk::ImageLayout,
    image: vk::Image,
    width: u32,
    height: u32,
    proxy_format: u32,
    bgr_order: bool,
    resources: &mut Option<CaptureResources>,
    pipeline: &mut Option<CapturePipeline>,
    direct: &mut Option<DirectCapture>,
    external_memory_host: bool,
    gpu_compose: &mut Option<crate::composition::gpu::GpuCompose>,
    shm: &mut ShmClient,
    original_scratch: &mut Vec<u8>,
    inflight: &mut Inflight,
    answer_scratch: &mut Vec<u8>,
    last_answer: &mut Vec<u8>,
) -> Option<vk::Semaphore> {
    let pipeline_start = std::time::Instant::now();
    // `composition_settings()` (and everything else below) only ever reads through an
    // already-open mapping -- nothing about it opens one. Every real path that DOES
    // open the mapping (`try_round_trip`/`begin_async_request`) lives later in this
    // same function, gated behind the `composition_settings()` check right below.
    // Real bug, found and fixed 2026-09-11 via a live `vkcube` bisection on
    // `lordnikon`: on a brand-new process the mapping is never open yet, so this used
    // to return `None` here on literally every single frame, forever -- this function
    // was being called every present call (confirmed real, not theoretical) but never
    // actually captured or sent a single frame, because it always bailed out before
    // ever reaching the code that would open the mapping in the first place.
    // `ShmClient::open` is cheap to call unconditionally (an immediate no-op once
    // already open, see its own early return), so there's no real cost to calling it
    // here up front instead of leaving each caller to remember to.
    shm.open();
    // Decided fresh each call rather than cached: `vkGetPhysicalDeviceProperties2` is
    // a cheap, purely local query (the driver already has this value on hand, no real
    // round trip), so there's no need for a whole extra piece of per-device state just
    // to memoize something this inexpensive. Checked against the *actual* runtime
    // proxy-region pointer, not just its constant offset within the mapping --
    // `mmap`'s returned address alignment is not something this project can assume
    // beyond the page size POSIX guarantees, and a wrong guess here would silently
    // stop `poll_or_submit_capture` from ever completing a capture again for the rest
    // of this device's life (`ensure_direct_capture` failing is the only signal, and
    // this function has no per-call fallback to `CapturePipeline` once `use_direct` is
    // decided) rather than fail open onto the always-correct staging-buffer path.
    let use_direct = external_memory_host
        && shm.proxy_region().is_some_and(|(ptr, capacity)| {
            min_imported_host_pointer_alignment(instance, physical_device).is_some_and(|alignment| {
                let alignment = alignment as usize;
                alignment != 0 && (ptr as usize) % alignment == 0 && capacity % alignment == 0
            })
        });
    let Some(settings) = shm.composition_settings() else { return None };
    // `debug_view`'s compare/split views and a pending `capture_request`'s dump both
    // need *this* frame's own original and answer, not whatever the async pipeline
    // below happens to have on hand -- same-frame correctness matters more than
    // throughput for either, and both are rare, deliberately-triggered cases (a
    // developer toggling a debug view, or a one-shot dump request), not the normal
    // per-frame path this function otherwise replaces.
    if capture_image != image && (settings.debug_view != 0 || shm.capture_request_pending()) { return None; }
    if settings.debug_view != 0 || shm.capture_request_pending() {
        return unsafe {
            run_sync(
                device,
                instance,
                physical_device,
                queue,
                queue_family,
                image,
                width,
                height,
                proxy_format,
                bgr_order,
                resources,
                gpu_compose,
                shm,
                original_scratch,
                last_answer,
            )
        };
    }
    // "Off keeps the whole pass running... and simply presents the clean frame" --
    // `ShmHeader::apply_model`'s own doc comment -- and the model being permanently
    // unavailable is the same "nothing will ever consume a captured frame" case
    // `ShmClient::model_known_unavailable`'s own doc comment already covers. Either
    // way, paying for a capture+round-trip cycle nobody will use is pure waste;
    // skip the whole pipeline and let the caller present `image` untouched.
    // `neural_enabled` (the GUI's own "Enabled" toggle, `ShmHeader::enabled`) is
    // included here too -- a real bug, found 2026-09-11: `ShmHeader::neural_enabled()`
    // existed and the GUI wrote to it, but nothing in this crate ever read it back,
    // so turning "Enabled" off in the GUI had no effect on anything real at all.
    let bytes_per_pixel = neuralforge_protocol::enums::proxy_format::bytes_per_pixel(proxy_format) as u64;
    let frame_bytes = u64::from(width) * u64::from(height) * bytes_per_pixel;
    if frame_bytes == 0 || frame_bytes as usize > neuralforge_protocol::MAX_FRAME {
        return None;
    }

    let disabled = !settings.apply_model || !settings.neural_enabled || shm.model_known_unavailable();
    // The helper cannot allocate its NGX images until it knows the game's actual
    // swapchain dimensions.  Waiting until the user enables NR often means GTA has
    // already consumed nearly all VRAM, making that first allocation fail forever.
    // Send exactly one frame while disabled to create the feature early, then discard
    // its answer.  This is deliberately one request per process, never a hidden
    // rendering loop while NR is switched off.
    if disabled {
        if inflight.bootstrap_complete {
            return None;
        }
        if shm.has_pending_request() {
            if shm.poll_async_request() == Some(true) {
                inflight.bootstrap_complete = true;
            }
            return None;
        }
        // Same non-blocking capture strategy as the enabled path below, used here too
        // so this one-shot warm-up capture never costs a blocking fence wait either --
        // it just takes a call or two longer to land (irrelevant for a once-per-process
        // bootstrap) instead of stalling the present it happens on.
        if poll_or_submit_capture(
            use_direct, pipeline, direct, device, instance, physical_device, queue, queue_family,
            capture_image, capture_layout, width, height, proxy_format, frame_bytes, shm, original_scratch,
        ) {
            shm.prepare_motion(instance, physical_device, width, height, proxy_format, original_scratch);
            if shm.begin_async_request() {
                inflight.dims = Some((width, height, proxy_format));
            }
        }
        return None;
    }

    // Poll whatever was sent on some earlier frame *before* touching anything else --
    // `inflight`'s current contents correspond to it, and must be read (below) before
    // a new capture this same frame (if one happens) is allowed to replace them.
    let mut have_answer = false;
    if shm.has_pending_request() {
        if shm.poll_async_request() == Some(true) {
            answer_scratch.resize(frame_bytes as usize, 0);
            shm.read_answer(answer_scratch);
            // Preserve the exact game frame supplied to the model before the next
            // request replaces `inflight.original`; the temporal GPU path uses it
            // to carry only the model's enhancement delta onto current frames.
            inflight.raw_answer_base.clear();
            inflight.raw_answer_base.extend_from_slice(&inflight.original);
            have_answer = true;
        }
    }

    // Non-blocking capture (`ASYNC_CAPTURE_DESIGN.md`, `EXTERNAL_MEMORY_HOST_DESIGN.md`):
    // poll whatever capture is already in flight -- never a queue/fence wait -- before
    // deciding whether to submit a new one. Same wire-protocol constraint as before:
    // only start a new round trip (and only bother keeping a just-finished capture's
    // bytes at all) when nothing is already outstanding. A capture that finishes while
    // a request is *already* in flight is still polled here (freeing its slot for
    // reuse) but its bytes are simply not consumed -- the same bounded
    // temporal-staleness tradeoff `run`'s own doc comment already accepts, not a new
    // one. `poll_or_submit_capture` never submits in the same call it successfully
    // polls, so a single check here (rather than the two separate ones a poll-then-
    // maybe-submit split would need) already correctly skips submitting a redundant
    // capture on the same call a round trip just started.
    if !shm.has_pending_request() {
        if poll_or_submit_capture(
            use_direct, pipeline, direct, device, instance, physical_device, queue, queue_family,
            capture_image, capture_layout, width, height, proxy_format, frame_bytes, shm, original_scratch,
        ) {
            shm.prepare_motion(instance, physical_device, width, height, proxy_format, original_scratch);
            if shm.begin_async_request() {
                std::mem::swap(&mut inflight.original, original_scratch);
                inflight.dims = Some((width, height, proxy_format));
            }
        }
    }

    if have_answer && inflight.dims == Some((width, height, proxy_format)) && neuralforge_protocol::enums::proxy_format::is_8bit(proxy_format) {
        // Retain the model's raw answer for continuous re-presentation below --
        // deliberately *not* run through `composition::gpu`/`composition::apply`'s
        // tone-map compositor. That compositor's `UpgradeToneMap` targets `original`'s
        // own luminance exactly whenever `original <= proxy`; this pipeline's `proxy
        // == original` (no real downscaled proxy exists yet -- see this crate's other
        // doc comments) makes that true on every pixel, which doesn't just dilute the
        // model's edit but actively fights it: a *stronger* raw answer gets *more*
        // aggressively cancelled by the same ratio-based rescale, confirmed by direct
        // measurement on `lordnikon` 2026-09-12 (maxing every tuning parameter nearly
        // doubled the raw model's own delta from original, then the compositor's
        // output delta *dropped* below the unmodified baseline). No tuning knob fixes
        // that; it's this pipeline's proxy/original conflation actively working
        // against the model's answer, not merely muting it.
        last_answer.clear();
        last_answer.extend_from_slice(answer_scratch);
        inflight.raw_answer_generation = inflight.raw_answer_generation.wrapping_add(1).max(1);
    }

    // Re-present the most recently retained answer on *every* call, not only the
    // rare one a round trip happens to resolve on. Compositing only on that rare
    // frame (`image` left completely untouched every other frame, this function's
    // very first design) alternates "native" and "one processed frame" -- the
    // flicker this project has fought since v0.1.49/v0.1.50 (see this crate's other
    // doc comments) -- independently of whether that processed frame is stale.
    // Re-blitting the same held answer every frame instead removes the alternation:
    // displayed content is always "the model's edit," refreshed at the round trip's
    // own cadence rather than toggling against untouched frames in between. This
    // does not fix temporal staleness during fast motion (the tradeoff a real
    // downscaled-proxy-plus-motion-vector pipeline would remove) -- see this
    // crate's own doc comment on motion vectors being disabled -- but it removes
    // the *alternation* specifically, a separate and, per tonight's live testing,
    // apparently the dominant source of what got called "flicker".
    if last_answer.is_empty() {
        return None;
    }
    // Cache each helper answer in device-local memory once, then copy that cached
    // frame to every presented swapchain image.  The prior path re-uploaded two 4K
    // CPU buffers and ran the compose shader on every present, which made the counter
    // read in the 50s while frame pacing felt like the teens.  This leaves only one
    // device-local transfer on ordinary presents and preserves the no-flicker held
    // answer policy.
    if gpu_compose.is_none() {
        *gpu_compose = crate::composition::gpu::GpuCompose::new(device, queue_family);
    }
    if let Some(gpu) = gpu_compose {
        if let Some(sem) = gpu.present_temporal_delta_async(
            device,
            instance,
            physical_device,
            queue,
            width,
            height,
            &inflight.raw_answer_base,
            last_answer,
            inflight.raw_answer_generation,
            bgr_order,
            image,
        ) {
            shm.publish_frame_timing(pipeline_start.elapsed(), true);
            return Some(sem);
        }
    }
    // No GPU compose available at all (`GpuCompose::new` failed) -- last resort: the
    // same CPU-visible write-back every other fallback path in this module already
    // uses, blocking cost and all.
    if !ensure(resources, device, instance, physical_device, queue_family, frame_bytes) {
        return None;
    }
    let r = resources.as_ref().expect("just ensured above");
    write_bytes_to_image(device, r, queue, image, width, height, last_answer);
    shm.publish_frame_timing(pipeline_start.elapsed(), true);
    None
}

/// Records "copy `image` (in `initial_layout`) into `buffer`, restore `initial_layout`"
/// into `cmd` -- reset, begin, both barriers, the copy, end. Does not submit or wait;
/// [`submit_pipeline_capture`] (the only caller now that the old fully-synchronous
/// single-shot capture path is gone -- see `ASYNC_CAPTURE_DESIGN.md`) does that
/// itself, deliberately without waiting. Pulled out on its own so a future second
/// caller shares the exact same recorded commands rather than a copy that could
/// drift apart -- not, today, because there already is one.
fn record_capture_commands(device: &ash::Device, cmd: vk::CommandBuffer, image: vk::Image, initial_layout: vk::ImageLayout, buffer: vk::Buffer, width: u32, height: u32) -> bool {
    // SAFETY: `cmd` was allocated from a pool created with `RESET_COMMAND_BUFFER`.
    if unsafe { device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()) }.is_err() {
        return false;
    }
    let begin_info = vk::CommandBufferBeginInfo::builder().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: `cmd` was just reset above.
    if unsafe { device.begin_command_buffer(cmd, &begin_info) }.is_err() {
        return false;
    }
    let to_transfer_src = barrier(
        image,
        initial_layout,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::AccessFlags::empty(),
        vk::AccessFlags::TRANSFER_READ,
    );
    // SAFETY: `cmd` is in the recording state; `image` is the caller's own, currently
    // `initial_layout` per every caller's own contract on the image it passes in.
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_transfer_src],
        );
    }
    let region = vk::BufferImageCopy::builder()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(
            vk::ImageSubresourceLayers::builder()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .mip_level(0)
                .base_array_layer(0)
                .layer_count(1)
                .build(),
        )
        .image_offset(vk::Offset3D::default())
        .image_extent(vk::Extent3D { width, height, depth: 1 })
        .build();
    // SAFETY: `image` was just transitioned to `TRANSFER_SRC_OPTIMAL` above; `buffer`
    // is sized to at least `width*height*bytes_per_pixel` by whichever caller built it.
    unsafe {
        device.cmd_copy_image_to_buffer(cmd, image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buffer, &[region]);
    }
    // Restore `image` to exactly the layout this function found it in -- nothing is
    // guaranteed to touch `image` again this same frame, so leaving it in
    // `TRANSFER_DST_OPTIMAL` (a layout only valid mid-way through an image<->buffer
    // round trip) would be a real bug the moment the real present call, or the game's
    // own next use of a render-tap source, ran against it instead.
    let to_present = barrier(
        image,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        initial_layout,
        vk::AccessFlags::TRANSFER_READ,
        vk::AccessFlags::empty(),
    );
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_present],
        );
    }
    unsafe { device.end_command_buffer(cmd) }.is_ok()
}

/// Phase 2 (`ASYNC_CAPTURE_DESIGN.md`): two [`CaptureBuffer`] slots so a new capture
/// submission never has to wait on the previous one's fence first. `run` only ever
/// calls [`poll_pipeline_capture`] (non-blocking: did an earlier submission finish?)
/// and [`submit_pipeline_capture`] (non-blocking: start a new one if a slot is free)
/// against this -- no queue wait, no fence wait, on the per-present path. The one
/// place this module *does* block on a pipeline fence is [`ensure_pipeline`]'s resize
/// path, which is not on that path.
pub struct CapturePipeline {
    queue_family: u32,
    slots: [PipelineSlot; 2],
}

struct PipelineSlot {
    buf: CaptureBuffer,
    /// `Some((width, height, proxy_format))` for a submission whose fence has not yet
    /// been confirmed signaled by [`poll_pipeline_capture`]. Nothing may reset or
    /// reuse `buf.cmd`/`buf.buffer`/`buf.memory` while this is `Some` -- the exact
    /// invariant whose violation caused the 2026-09-12 UB regression documented in
    /// this project's history (`docs/history/development-before-neuralforge.md`).
    pending: Option<(u32, u32, u32)>,
}

/// Builds both slots if `existing` is `None`; rebuilds both (same capacity/queue-
/// family-change trigger as [`ensure`]) if either is undersized or the queue family
/// changed. Unlike [`ensure`], a slot reaching this function may legitimately still
/// be `pending` -- draining it is this function's job, not a precondition callers
/// have to uphold, since the entire point of the pipeline is that callers never wait
/// on a pending slot themselves.
fn ensure_pipeline(
    existing: &mut Option<CapturePipeline>,
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
    bytes: vk::DeviceSize,
) -> bool {
    if let Some(p) = existing.as_ref() {
        if p.queue_family == queue_family && p.slots.iter().all(|s| s.buf.capacity >= bytes) {
            return true;
        }
    } else {
        return build_pipeline(existing, device, instance, physical_device, queue_family, bytes);
    }
    // A rebuild is needed (capacity grew or the queue family changed -- the same two
    // triggers `ensure` already has). Resize/teardown is rare and not latency
    // sensitive, so this is the one place in the pipeline that takes a real, blocking
    // (unbounded, like every other fence wait this project keeps unbounded rather
    // than guessing a safe timeout) wait -- never on the steady-state per-frame path.
    let p = existing.as_ref().expect("checked above");
    for slot in &p.slots {
        if slot.pending.is_some() {
            // SAFETY: `slot.buf.fence` is this slot's own fence; waiting for it here,
            // before any destroy below touches the resources it guards, is exactly
            // what makes that destroy sound -- the "drain before rebuilding on a live
            // device" this pipeline's own design doc calls for.
            if unsafe { device.wait_for_fences(&[slot.buf.fence], true, u64::MAX) }.is_err() {
                // A real device error, not a timeout (there is no timeout above).
                // Leave the existing pipeline exactly as it was rather than guess it's
                // safe to destroy -- next frame's `ensure_pipeline` call tries again.
                return false;
            }
        }
    }
    let p = existing.take().expect("checked above");
    for slot in &p.slots {
        // SAFETY: every slot's fence was just confirmed signaled above.
        unsafe { slot.buf.destroy(device) };
    }
    build_pipeline(existing, device, instance, physical_device, queue_family, bytes)
}

fn build_pipeline(
    existing: &mut Option<CapturePipeline>,
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
    bytes: vk::DeviceSize,
) -> bool {
    let Some(a) = build_capture_buffer(device, instance, physical_device, queue_family, bytes) else { return false };
    let Some(b) = build_capture_buffer(device, instance, physical_device, queue_family, bytes) else {
        // SAFETY: `a` was just built above; nothing has submitted work against it yet.
        unsafe { a.destroy(device) };
        return false;
    };
    *existing = Some(CapturePipeline {
        queue_family,
        slots: [PipelineSlot { buf: a, pending: None }, PipelineSlot { buf: b, pending: None }],
    });
    true
}

/// Non-blocking: checks every slot for a submission whose fence has actually signaled
/// (`vkGetFenceStatus`, never `vkWaitForFences`) and, for the first one found, copies
/// its bytes into `out` and frees the slot. Returns that submission's own
/// `(width, height, proxy_format)` -- the caller needs it to detect a resolution
/// change against whatever it was expecting, same as every other dims check in this
/// module. `None` (leaving `out` untouched) if nothing is signaled yet, or if a slot's
/// fence reported a real error (left `pending` forever rather than guessed safe to
/// reuse -- fail-closed on that one slot, not a reason to stop trying the other).
fn poll_pipeline_capture(pipeline: &mut CapturePipeline, device: &ash::Device, out: &mut Vec<u8>) -> Option<(u32, u32, u32)> {
    for slot in &mut pipeline.slots {
        let dims = slot.pending?;
        // SAFETY: `slot.buf.fence` belongs to this slot; a status query never touches
        // command-buffer/buffer/memory state, so it's sound to call regardless of
        // whether the submission this fence guards has actually completed yet.
        match unsafe { device.get_fence_status(slot.buf.fence) } {
            Ok(true) => {
                let (width, height, proxy_format) = dims;
                let bytes_per_pixel = neuralforge_protocol::enums::proxy_format::bytes_per_pixel(proxy_format) as u64;
                let frame_bytes = (u64::from(width) * u64::from(height) * bytes_per_pixel) as usize;
                // SAFETY: `slot.buf.ptr` is a live host-coherent mapping of at least
                // `frame_bytes` bytes (`submit_pipeline_capture` only ever submits
                // into a slot `build_capture_buffer` already sized for this exact
                // `frame_bytes`); the fence just confirmed signaled means the GPU's
                // writes are visible to the CPU with no explicit flush/invalidate
                // needed (host-coherent memory, same as every other read of a
                // `CaptureBuffer::ptr` in this module).
                let captured = unsafe { std::slice::from_raw_parts(slot.buf.ptr, frame_bytes) };
                out.clear();
                out.extend_from_slice(captured);
                slot.pending = None;
                return Some(dims);
            }
            Ok(false) => {} // still in flight -- leave `pending`, check again next call
            Err(_) => {}     // real device error -- leave `pending`; never guess reuse is safe
        }
    }
    None
}

/// Non-blocking: records and submits a new capture into whichever slot is free
/// (`pending: None`), if any. `false` (no new capture this frame) if both slots are
/// still pending or recording/submission itself failed -- the caller already treats
/// that as "skip capture this frame", the same fail-open discipline as every other
/// path in this module. Never waits, never touches a slot that is still `pending`.
#[allow(clippy::too_many_arguments)]
fn submit_pipeline_capture(
    pipeline: &mut CapturePipeline,
    device: &ash::Device,
    queue: vk::Queue,
    image: vk::Image,
    initial_layout: vk::ImageLayout,
    width: u32,
    height: u32,
    proxy_format: u32,
) -> bool {
    let Some(slot) = pipeline.slots.iter_mut().find(|s| s.pending.is_none()) else { return false };
    if !record_capture_commands(device, slot.buf.cmd, image, initial_layout, slot.buf.buffer, width, height) {
        return false;
    }
    // SAFETY: `slot.buf.fence` is `pending: None` here -- either never used yet
    // (starts signaled, see `build_capture_buffer`) or its previous signal was
    // already confirmed and consumed by `poll_pipeline_capture` -- so resetting it
    // now cannot race an in-flight wait on it.
    if unsafe { device.reset_fences(&[slot.buf.fence]) }.is_err() {
        return false;
    }
    let submit = vk::SubmitInfo::builder().command_buffers(std::slice::from_ref(&slot.buf.cmd)).build();
    // SAFETY: `slot.buf.cmd` was just recorded and ended by `record_capture_commands`
    // above. Deliberately not waited on here -- the entire point of this function:
    // the caller's present call returns immediately, and a later
    // `poll_pipeline_capture` call picks up the result once this fence actually
    // signals, exactly like `vkQueuePresentKHR` itself never waits on the work it
    // submits either.
    if unsafe { device.queue_submit(queue, &[submit], slot.buf.fence) }.is_err() {
        return false;
    }
    slot.pending = Some((width, height, proxy_format));
    true
}

/// # Safety
/// Must only be called at device-destruction time, with no submitted work referencing
/// these handles still in flight -- same contract as [`destroy`].
pub unsafe fn destroy_pipeline(pipeline: Option<CapturePipeline>, device: &ash::Device) {
    if let Some(p) = pipeline {
        for slot in &p.slots {
            // SAFETY: forwarded from this function's own contract.
            unsafe { slot.buf.destroy(device) };
        }
    }
}

/// Whether, and by how much, this device's driver requires host pointers imported via
/// `VK_EXT_external_memory_host` to be aligned (`VkPhysicalDeviceExternalMemoryHostPropertiesEXT::minImportedHostPointerAlignment`).
/// `None` if the query itself fails -- treated as "don't attempt import", the same
/// fail-open discipline as every other capability check in this module.
fn min_imported_host_pointer_alignment(instance: &ash::Instance, physical_device: vk::PhysicalDevice) -> Option<vk::DeviceSize> {
    let mut ext_props = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
    let mut props2 = vk::PhysicalDeviceProperties2::builder().push_next(&mut ext_props);
    // SAFETY: `physical_device` belongs to `instance`; `props2` is a freshly built,
    // valid out-parameter with the EXT struct chained into its `pNext`.
    unsafe { instance.get_physical_device_properties2(physical_device, &mut props2) };
    (ext_props.min_imported_host_pointer_alignment > 0).then_some(ext_props.min_imported_host_pointer_alignment)
}

/// The Phase 3 zero-copy capture path (`EXTERNAL_MEMORY_HOST_DESIGN.md`): a single
/// [`CaptureBuffer`] whose device memory is *imported* directly from the live SHM
/// proxy region (`ShmClient::proxy_region`), so `vkCmdCopyImageToBuffer` writes
/// straight into shared memory -- no staging buffer, no CPU copy on the way there.
///
/// Deliberately one slot, not two like [`CapturePipeline`]: the imported memory *is*
/// the one shared proxy region every caller of `ShmClient::write_proxy` ultimately
/// writes to. Two of these submitted concurrently would be two unsynchronized GPU
/// writes to the exact same destination bytes -- a real write-write hazard, not just
/// wasted work -- so there is no second slot to hide capture latency behind here. The
/// tradeoff for a direct write is capping this at one in-flight capture at a time;
/// [`run`] only ever uses one of [`DirectCapture`] or [`CapturePipeline`] for a given
/// device, never both, precisely so nothing else can also be writing to the same
/// region through the other path.
pub struct DirectCapture {
    buf: CaptureBuffer,
    /// Same meaning as `PipelineSlot::pending`, for this capture's own single slot.
    pending: Option<(u32, u32, u32)>,
}

/// Builds (or rebuilds, on a capacity/queue-family change) the one slot
/// [`DirectCapture`] needs, importing `host_ptr`/`capacity` (the live SHM proxy
/// region) rather than allocating fresh device memory. `false` on any failure --
/// every caller already treats that as "fall back to `CapturePipeline`", never a
/// reason to stop trying on a later frame.
///
/// # Safety
/// `host_ptr` must be valid for `capacity` bytes for as long as `existing` holds
/// `Some` afterward, and nothing outside the resulting `DirectCapture`'s own command
/// buffer may write to those bytes while a capture against them is in flight.
unsafe fn ensure_direct_capture(
    existing: &mut Option<DirectCapture>,
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
    host_ptr: *mut u8,
    capacity: vk::DeviceSize,
) -> bool {
    if let Some(d) = existing.as_ref() {
        if d.buf.capacity >= capacity && d.buf.ptr == host_ptr {
            return true;
        }
        // A resize/pointer change (the mapping is only ever established once per
        // process in practice, but this mirrors `ensure_pipeline`'s own rebuild
        // discipline rather than assuming that) needs the same drain-before-destroy
        // care: never touch a slot that might still be in flight.
        if d.pending.is_some() {
            // SAFETY: `d.buf.fence` is this slot's own fence; waiting for it before
            // any destroy below touches the resources it guards is exactly what makes
            // that destroy sound. Unbounded, like every other rebuild wait in this
            // module -- rare, not latency sensitive, and there is no timeout to guess.
            if unsafe { device.wait_for_fences(&[d.buf.fence], true, u64::MAX) }.is_err() {
                return false;
            }
        }
        let d = existing.take().expect("checked above");
        // SAFETY: the fence was just confirmed signaled above (or was never pending).
        unsafe { d.buf.destroy(device) };
    }
    // SAFETY: forwarded from this function's own contract.
    let Some(buf) = (unsafe { build_imported_capture_buffer(device, instance, physical_device, queue_family, host_ptr, capacity) }) else {
        return false;
    };
    *existing = Some(DirectCapture { buf, pending: None });
    true
}

/// Non-blocking, mirrors [`poll_pipeline_capture`] -- except there is nothing to copy
/// out: a signaled fence here means the bytes are already sitting in the SHM proxy
/// region this slot's memory was imported from. Returns the completed submission's own
/// `(width, height, proxy_format)`, or `None` if nothing is signaled yet (or the fence
/// reported a real error, left `pending` forever rather than guessed safe to reuse).
fn poll_direct_capture(direct: &mut DirectCapture, device: &ash::Device) -> Option<(u32, u32, u32)> {
    let dims = direct.pending?;
    // SAFETY: `direct.buf.fence` belongs to this slot; a status query never touches
    // command-buffer/buffer/memory state, so it's sound regardless of whether the
    // submission this fence guards has actually completed yet.
    match unsafe { device.get_fence_status(direct.buf.fence) } {
        Ok(true) => {
            direct.pending = None;
            Some(dims)
        }
        Ok(false) => None,
        Err(_) => None, // real device error -- leave `pending`; never guess reuse is safe
    }
}

/// Non-blocking, mirrors [`submit_pipeline_capture`] -- records and submits a new
/// capture into the one slot, if it's free (`pending: None`). `false` if it's still
/// pending or recording/submission itself failed.
#[allow(clippy::too_many_arguments)]
fn submit_direct_capture(
    direct: &mut DirectCapture,
    device: &ash::Device,
    queue: vk::Queue,
    image: vk::Image,
    initial_layout: vk::ImageLayout,
    width: u32,
    height: u32,
    proxy_format: u32,
) -> bool {
    if direct.pending.is_some() {
        return false;
    }
    if !record_capture_commands(device, direct.buf.cmd, image, initial_layout, direct.buf.buffer, width, height) {
        return false;
    }
    // SAFETY: `direct.buf.fence` is `pending: None` here -- either never used yet
    // (starts signaled) or its previous signal was already confirmed and consumed by
    // `poll_direct_capture` -- so resetting it now cannot race an in-flight wait.
    if unsafe { device.reset_fences(&[direct.buf.fence]) }.is_err() {
        return false;
    }
    let submit = vk::SubmitInfo::builder().command_buffers(std::slice::from_ref(&direct.buf.cmd)).build();
    // SAFETY: `direct.buf.cmd` was just recorded and ended above. Deliberately not
    // waited on -- same reasoning as `submit_pipeline_capture`.
    if unsafe { device.queue_submit(queue, &[submit], direct.buf.fence) }.is_err() {
        return false;
    }
    direct.pending = Some((width, height, proxy_format));
    true
}

/// # Safety
/// Must only be called at device-destruction time, with no submitted work referencing
/// these handles still in flight -- same contract as [`destroy`]. Never unmaps or
/// otherwise touches the imported host pointer itself -- that memory belongs to
/// `ShmClient`, not this buffer, exactly like every other Vulkan-object-only destroy
/// in this module.
pub unsafe fn destroy_direct_capture(direct: Option<DirectCapture>, device: &ash::Device) {
    if let Some(d) = direct {
        // SAFETY: forwarded from this function's own contract.
        unsafe { d.buf.destroy(device) };
    }
}

/// Builds a [`CaptureBuffer`] whose device memory is *imported* from `host_ptr`
/// (`VK_EXT_external_memory_host`) rather than freshly allocated -- `ptr` in the
/// result is `host_ptr` itself, not a separate `vkMapMemory` mapping, so a capture
/// submitted against it writes straight into whatever `host_ptr` already points at.
/// `None` on any failure, exactly like [`build_capture_buffer`].
///
/// # Safety
/// `host_ptr` must be valid for `bytes` bytes and already aligned/sized to whatever
/// `min_imported_host_pointer_alignment` the caller queried -- this function does not
/// re-check either, only the driver does (at `vkAllocateMemory`, where a violation is
/// a validation error, not proactively caught here).
unsafe fn build_imported_capture_buffer(
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
    host_ptr: *mut u8,
    bytes: vk::DeviceSize,
) -> Option<CaptureBuffer> {
    let pool_info =
        vk::CommandPoolCreateInfo::builder().queue_family_index(queue_family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    // SAFETY: `device` is the live device this capture serves; `pool_info` is valid.
    let Ok(pool) = (unsafe { device.create_command_pool(&pool_info, None) }) else { return None };
    let alloc_info = vk::CommandBufferAllocateInfo::builder()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    // SAFETY: `pool` was just created above.
    let cmd = match unsafe { crate::loader_data::allocate_commands(device, &alloc_info) } {
        Ok(bufs) => bufs[0],
        Err(_) => {
            // SAFETY: `pool` owns no other resources yet.
            unsafe { device.destroy_command_pool(pool, None) };
            return None;
        }
    };
    let fence_info = vk::FenceCreateInfo::builder().flags(vk::FenceCreateFlags::SIGNALED);
    // SAFETY: starting signaled means the first use's own poll never reports pending
    // for a fence nothing has submitted work against yet.
    let fence = match unsafe { device.create_fence(&fence_info, None) } {
        Ok(f) => f,
        Err(_) => {
            // SAFETY: `pool` owns no other resources yet; freeing it also frees `cmd`.
            unsafe { device.destroy_command_pool(pool, None) };
            return None;
        }
    };

    // Imported host memory has its own compatibility query -- separate from (and not
    // necessarily the same memory-type set as) `build_capture_buffer`'s own
    // HOST_VISIBLE|HOST_COHERENT search for a fresh allocation.
    // SAFETY: `instance`/`device` are live for the duration of this call.
    let ext_fn = unsafe {
        vk::ExtExternalMemoryHostFn::load(|name| std::mem::transmute(instance.get_device_proc_addr(device.handle(), name.as_ptr())))
    };
    let mut host_props = vk::MemoryHostPointerPropertiesEXT::default();
    // SAFETY: `device` is live; `host_ptr` is valid for `bytes` bytes per this
    // function's own contract.
    let query_result = unsafe {
        (ext_fn.get_memory_host_pointer_properties_ext)(
            device.handle(),
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
            host_ptr.cast(),
            &mut host_props,
        )
    };
    if query_result != vk::Result::SUCCESS {
        // SAFETY: neither `fence` nor `pool` owns any other resource yet.
        unsafe {
            device.destroy_fence(fence, None);
            device.destroy_command_pool(pool, None);
        }
        return None;
    }

    // A buffer that will be bound to *imported* memory must declare that up front:
    // VUID-vkBindBufferMemory-memory-02985 requires the external handle type used at
    // import time to already be set in the buffer's own `VkExternalMemoryBufferCreateInfo`
    // at creation -- found live, on real hardware, via `VK_LAYER_VALIDATE_SYNC=1`
    // (see EXTERNAL_MEMORY_HOST_DESIGN.md), not caught by the local software ICD this
    // crate's tests otherwise run against.
    let mut external_info = vk::ExternalMemoryBufferCreateInfo::builder().handle_types(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
    let buf_info = vk::BufferCreateInfo::builder()
        .size(bytes)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .push_next(&mut external_info);
    // SAFETY: `buf_info` is valid.
    let buffer = match unsafe { device.create_buffer(&buf_info, None) } {
        Ok(b) => b,
        Err(_) => {
            // SAFETY: neither `fence` nor `pool` owns `buffer` (it doesn't exist).
            unsafe {
                device.destroy_fence(fence, None);
                device.destroy_command_pool(pool, None);
            }
            return None;
        }
    };
    // SAFETY: `buffer` was just created and is not yet bound to memory.
    let reqs = unsafe { device.get_buffer_memory_requirements(buffer) };
    // SAFETY: `physical_device` is the device this capture serves; `instance` is its
    // owning instance.
    let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    // Only a memory type both the buffer itself (`reqs`) and the imported pointer
    // (`host_props`) agree on is actually usable here.
    let compatible = reqs.memory_type_bits & host_props.memory_type_bits;
    let Some(type_index) = (0..mem_props.memory_type_count)
        .find(|&i| (compatible & (1 << i)) != 0 && mem_props.memory_types[i as usize].property_flags.contains(wanted))
    else {
        // SAFETY: `buffer` has no memory bound yet; nothing else owns `fence`/`pool`.
        unsafe {
            device.destroy_buffer(buffer, None);
            device.destroy_fence(fence, None);
            device.destroy_command_pool(pool, None);
        }
        return None;
    };

    let mut import_info =
        vk::ImportMemoryHostPointerInfoEXT::builder().handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT).host_pointer(host_ptr.cast());
    let alloc = vk::MemoryAllocateInfo::builder().allocation_size(bytes).memory_type_index(type_index).push_next(&mut import_info);
    // SAFETY: `alloc` is valid; `type_index` was just confirmed to satisfy both
    // `reqs` and `host_props` above; `host_ptr`/`bytes` satisfy this function's own
    // safety contract on alignment/size.
    let memory = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(_) => {
            // SAFETY: same reasoning as the branch above.
            unsafe {
                device.destroy_buffer(buffer, None);
                device.destroy_fence(fence, None);
                device.destroy_command_pool(pool, None);
            }
            return None;
        }
    };
    // SAFETY: `buffer`/`memory` were each just created above, sized/typed to satisfy
    // each other by construction.
    if unsafe { device.bind_buffer_memory(buffer, memory, 0) }.is_err() {
        // SAFETY: `memory` is not yet bound to anything that would make freeing it
        // unsound; `buffer` has no memory bound.
        unsafe {
            device.free_memory(memory, None);
            device.destroy_buffer(buffer, None);
            device.destroy_fence(fence, None);
            device.destroy_command_pool(pool, None);
        }
        return None;
    }

    // No `vkMapMemory` here, unlike `build_capture_buffer`: `host_ptr` already *is*
    // the address this imported memory refers to -- that is the entire point of a
    // host-pointer import, and re-mapping it would be redundant at best.
    Some(CaptureBuffer { pool, cmd, fence, buffer, memory, ptr: host_ptr, capacity: bytes })
}

/// Writes `bytes` (exactly `width*height*4` `RGBA8` bytes) into `image` via
/// `r`'s own staging buffer -- the CPU-composited last resort when no GPU compose
/// path is available at all. Fully synchronous; `image` assumed/left `PRESENT_SRC_KHR`,
/// same contract [`run_sync`]'s own stage 2 relies on. Best-effort: does nothing
/// observable on failure beyond leaving `image` unpresented-to this frame, same
/// fail-open discipline as every other stage in this module.
fn write_bytes_to_image(device: &ash::Device, r: &CaptureResources, queue: vk::Queue, image: vk::Image, width: u32, height: u32, bytes: &[u8]) {
    let frame_bytes = u64::from(width) * u64::from(height) * 4;
    if bytes.len() as u64 != frame_bytes {
        return;
    }
    // SAFETY: `r.ptr` is a live mapping of at least `frame_bytes` bytes -- the same
    // invariant `run_sync`'s own stage 1/2 already rely on.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), r.ptr, bytes.len()) };
    // SAFETY: `r.cmd` was allocated with `RESET_COMMAND_BUFFER`.
    if unsafe { device.reset_command_buffer(r.cmd, vk::CommandBufferResetFlags::empty()) }.is_err() {
        return;
    }
    let begin_info = vk::CommandBufferBeginInfo::builder().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    if unsafe { device.begin_command_buffer(r.cmd, &begin_info) }.is_err() {
        return;
    }
    let to_dst = barrier(
        image,
        vk::ImageLayout::PRESENT_SRC_KHR,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::AccessFlags::empty(),
        vk::AccessFlags::TRANSFER_WRITE,
    );
    unsafe {
        device.cmd_pipeline_barrier(r.cmd, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[to_dst]);
    }
    let region = vk::BufferImageCopy::builder()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(vk::ImageSubresourceLayers::builder().aspect_mask(vk::ImageAspectFlags::COLOR).mip_level(0).base_array_layer(0).layer_count(1).build())
        .image_offset(vk::Offset3D::default())
        .image_extent(vk::Extent3D { width, height, depth: 1 })
        .build();
    unsafe {
        device.cmd_copy_buffer_to_image(r.cmd, r.buffer, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
    }
    let to_present = barrier(image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::PRESENT_SRC_KHR, vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::empty());
    unsafe {
        device.cmd_pipeline_barrier(r.cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::ALL_COMMANDS, vk::DependencyFlags::empty(), &[], &[], &[to_present]);
    }
    if unsafe { device.end_command_buffer(r.cmd) }.is_err() {
        return;
    }
    if unsafe { device.reset_fences(&[r.fence]) }.is_err() {
        return;
    }
    let submit = vk::SubmitInfo::builder().command_buffers(std::slice::from_ref(&r.cmd)).build();
    if unsafe { device.queue_submit(queue, &[submit], r.fence) }.is_err() {
        return;
    }
    let _ = unsafe { device.wait_for_fences(&[r.fence], true, u64::MAX) };
}

/// Captures `image` into the proxy region, runs the shared-memory round trip, and
/// copies a result back into `image` before the caller's own present call. `resources`
/// is the per-device slot `queue_present_khr` owns (lazily built/rebuilt here).
///
/// Fails open on any error: returns without having touched `image` at all (still in
/// whatever layout the caller found it in, `PRESENT_SRC_KHR`) if anything along the way
/// doesn't work, so the caller can always fall back to presenting unmodified.
///
/// Returns `Some(semaphore)` when (and only when)
/// `composition::gpu::GpuCompose::dispatch_into_image_async` was used: `image` is
/// already fully written with the composited result, but the GPU work that wrote it
/// is not guaranteed *complete* yet (that is the entire point of the "async" in its
/// name -- this function never blocks on it). The caller **must** add that semaphore
/// to the real present call's own wait-semaphore list before presenting `image` --
/// otherwise the presentation engine could display `image` before the compute work
/// finishes writing it, a real, visible corruption/tearing bug, not merely a style
/// preference. `None` in every other case means `image` is already fully complete and
/// correctly laid out (`PRESENT_SRC_KHR`) -- safe to present with no extra wait.
///
/// # Safety
/// `queue` must be the same queue `image`'s presentation was requested on, with no
/// concurrent use of it from another thread for the duration of this call (the same
/// external-synchronization requirement `vkQueuePresentKHR` itself already places on
/// its own `queue` argument, which is what makes submitting here, from inside the
/// present hook, sound without any additional locking).
/// The old, fully-synchronous, one-frame-at-a-time path: capture *this* frame, block
/// on the helper round trip for *this* frame's own answer (up to a real timeout
/// budget), composite, write back -- all within the same present call. Kept
/// unchanged and still used for the two cases that genuinely need same-frame
/// correctness: a pending `capture_request` (its dump must show *this* frame's real
/// before/after, not some other frame's) and any non-zero `debug_view` (the
/// compare/split views are meaningless if original and answer come from different
/// moments). See [`run`]'s own doc comment for why every other case no longer goes
/// through here.
#[allow(clippy::too_many_arguments)]
unsafe fn run_sync(
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family: u32,
    image: vk::Image,
    width: u32,
    height: u32,
    proxy_format: u32,
    bgr_order: bool,
    resources: &mut Option<CaptureResources>,
    gpu_compose: &mut Option<crate::composition::gpu::GpuCompose>,
    shm: &mut ShmClient,
    original_scratch: &mut Vec<u8>,
    _last_answer: &mut Vec<u8>,
) -> Option<vk::Semaphore> {
    let bytes_per_pixel = neuralforge_protocol::enums::proxy_format::bytes_per_pixel(proxy_format) as u64;
    let frame_bytes = u64::from(width) * u64::from(height) * bytes_per_pixel;
    if frame_bytes == 0 || frame_bytes as usize > neuralforge_protocol::MAX_FRAME {
        return None;
    }
    if !ensure(resources, device, instance, physical_device, queue_family, frame_bytes) {
        return None;
    }
    let r = resources.as_ref().expect("just ensured above");

    // Stage 1: image -> staging buffer.
    // SAFETY: `r.cmd` was allocated from `r.pool`, created with
    // `RESET_COMMAND_BUFFER`; resetting before every `begin_command_buffer` is exactly
    // what that flag exists to allow.
    if unsafe { device.reset_command_buffer(r.cmd, vk::CommandBufferResetFlags::empty()) }.is_err() {
        return None;
    }
    let begin_info = vk::CommandBufferBeginInfo::builder().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: `r.cmd` was just reset above.
    if unsafe { device.begin_command_buffer(r.cmd, &begin_info) }.is_err() {
        return None;
    }
    let to_transfer_src = barrier(
        image,
        vk::ImageLayout::PRESENT_SRC_KHR,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::AccessFlags::empty(),
        vk::AccessFlags::TRANSFER_READ,
    );
    // SAFETY: `r.cmd` is in the recording state; `image` is the caller's own,
    // currently-`PRESENT_SRC_KHR` swapchain image per `vkQueuePresentKHR`'s contract.
    unsafe {
        device.cmd_pipeline_barrier(
            r.cmd,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_transfer_src],
        );
    }
    let copy_out = vk::BufferImageCopy::builder()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(
            vk::ImageSubresourceLayers::builder()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .mip_level(0)
                .base_array_layer(0)
                .layer_count(1)
                .build(),
        )
        .image_offset(vk::Offset3D::default())
        .image_extent(vk::Extent3D { width, height, depth: 1 })
        .build();
    // SAFETY: `image` was just transitioned to `TRANSFER_SRC_OPTIMAL` above; `r.buffer`
    // was sized to at least `frame_bytes` by `ensure`.
    unsafe {
        device.cmd_copy_image_to_buffer(r.cmd, image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, r.buffer, &[copy_out]);
    }
    let to_transfer_dst = barrier(
        image,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::AccessFlags::TRANSFER_READ,
        vk::AccessFlags::TRANSFER_WRITE,
    );
    // SAFETY: same reasoning as the first barrier above, transitioning for the
    // write-back this same command buffer will record in stage 2.
    unsafe {
        device.cmd_pipeline_barrier(
            r.cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_transfer_dst],
        );
    }
    if unsafe { device.end_command_buffer(r.cmd) }.is_err() {
        return None;
    }
    // SAFETY: `r.fence` starts signaled (see `ensure`) or was reset+waited-on by the
    // previous call to this function; `queue` is the caller's, externally synchronized
    // for the duration of this call per this function's own safety contract.
    if unsafe { device.reset_fences(&[r.fence]) }.is_err() {
        return None;
    }
    let submit = vk::SubmitInfo::builder().command_buffers(std::slice::from_ref(&r.cmd)).build();
    let t_stage1_start = std::time::Instant::now();
    // SAFETY: `r.cmd` was just recorded and ended above.
    if unsafe { device.queue_submit(queue, &[submit], r.fence) }.is_err() {
        return None;
    }
    // SAFETY: `r.fence` was just submitted against above.
    if unsafe { device.wait_for_fences(&[r.fence], true, u64::MAX) }.is_err() {
        return None;
    }
    let t_stage1 = t_stage1_start.elapsed();

    // CPU side: the captured bytes are now in `r.ptr` (host-coherent, no explicit
    // flush/invalidate needed). Hand them to the helper, then, if it actually
    // answered, overwrite `r.ptr` in place with that answer -- stage 2 below copies
    // whatever is sitting in `r.ptr` back into `image`, so this is what makes the
    // helper's answer (a real NGX evaluation, or the helper's own proxy-echo fallback
    // when the model isn't ready -- `neuralforge_helper::main`'s per-frame loop guarantees
    // the answer region is always the same size/format as the proxy either way)
    // actually reach the screen. A helper that never answers (not running, or the
    // round trip timed out) leaves `r.ptr` untouched -- it still holds the bytes
    // stage 1 just captured, so stage 2 below presents those unmodified, same fail-open
    // behavior as every other error path in this function.
    // SAFETY: `r.ptr` is a live mapping of at least `frame_bytes` bytes (the memory
    // type/size `ensure` just built or confirmed already satisfies this call's own
    // `frame_bytes`).
    let captured = unsafe { std::slice::from_raw_parts(r.ptr, frame_bytes as usize) };
    // Composition (below) needs the pre-edit frame after `read_answer` has already
    // overwritten `r.ptr` in place, so it has to be copied out now, before that
    // happens -- one extra `frame_bytes`-sized allocation/copy per frame, on top of
    // the two Vulkan transfers this function already does; not yet worth avoiding
    // ahead of proving the composition path correct at all.
    let t_snapshot_start = std::time::Instant::now();
    original_scratch.clear();
    original_scratch.extend_from_slice(captured);
    let original: &[u8] = original_scratch.as_slice();
    let t_snapshot = t_snapshot_start.elapsed();
    let t_write_proxy_start = std::time::Instant::now();
    shm.set_frame_info(width, height, proxy_format);
    shm.write_proxy(captured);
    shm.prepare_motion(instance, physical_device, width, height, proxy_format, captured);
    let t_write_proxy = t_write_proxy_start.elapsed();
    let t_roundtrip_start = std::time::Instant::now();
    let answered = shm.try_round_trip();
    let t_roundtrip = t_roundtrip_start.elapsed();
    let t_compose_start = std::time::Instant::now();
    // `Some(sem)` only when `composition::gpu::GpuCompose::dispatch_into_image_async`
    // already wrote the fully composited result straight into `image` itself, on the
    // GPU's own timeline -- skips the capture_request dump (nothing useful to dump:
    // `r.ptr` still holds the *raw* answer, not the composited result, on this path)
    // and stage 2 (there is nothing left for it to do) below, returning early instead.
    // The caller (`device.rs`) must chain `sem` into the real present call -- see this
    // function's own doc comment and `dispatch_into_image_async`'s for why.
    let mut composed_async: Option<vk::Semaphore> = None;
    if answered {
        // SAFETY: same reasoning as the read above; `ShmClient::read_answer` never
        // writes past the slice's length, which is exactly `frame_bytes` here.
        let answer_dst = unsafe { std::slice::from_raw_parts_mut(r.ptr, frame_bytes as usize) };
        shm.read_answer(answer_dst);
        // Only `RGBA8` is handled -- `RGBA16F` still passes the helper's raw answer
        // through untouched (see `composition::apply`'s own doc comment for why, and
        // `neuralforge_protocol::enums::proxy_format` for the format codes).
        if neuralforge_protocol::enums::proxy_format::is_8bit(proxy_format) {
            if let Some(settings) = shm.composition_settings() {
                if settings.apply_model && settings.neural_enabled {
                    // GPU dispatch (`composition::gpu`) only implements the normal
                    // composited case (`compose.comp` has no concept of `debug_view`
                    // at all) -- fails open to the CPU reference
                    // (`composition::apply::apply_rgba8`, which every mode already
                    // handles) whenever the GPU path isn't applicable, isn't
                    // available, or fails, same fail-open discipline as every other
                    // stage in this function.
                    let mut composed_sync = false;
                    // The helper's model output is the intended display-referred
                    // neural result. The legacy tone-map compositor was built for
                    // a clipped, downscaled proxy, but this pipeline feeds it the
                    // full original frame; it therefore collapses most of the model
                    // edit back toward the source image. Present the raw model result
                    // for normal rendering until that proxy pipeline exists.
                    if settings.debug_view == 0 {
                        if gpu_compose.is_none() {
                            *gpu_compose = crate::composition::gpu::GpuCompose::new(device, queue_family);
                        }
                        // Try the fast, non-blocking path first -- but only when
                        // nothing on the CPU needs to see the result afterward. A
                        // pending `capture_request` does (its dump needs real bytes
                        // in `r.ptr`), so that specific, rare, deliberately-triggered
                        // case still goes through the slower, fully-synchronous
                        // CPU-visible `dispatch` below, same as before this path
                        // existed.
                        // The first neural frame after enabling the feature can
                        // race the application's present transition on NVIDIA
                        // drivers. Keep composition CPU-visible until the
                        // async handoff is proven safe for live games.
                        if false && !shm.capture_request_pending() {
                            if let Some(gpu) = gpu_compose {
                                composed_async = gpu.dispatch_into_image_async(
                                    device,
                                    instance,
                                    physical_device,
                                    queue,
                                    width,
                                    height,
                                    &original,
                                    answer_dst,
                                    settings.colour_strength,
                                    settings.transfer_strength,
                                    settings.max_ratio,
                                    bgr_order,
                                    image,
                                );
                            }
                        }
                        if composed_async.is_none() {
                            if let Some(gpu) = gpu_compose {
                                composed_sync = gpu.dispatch(
                                    device,
                                    instance,
                                    physical_device,
                                    queue,
                                    width,
                                    height,
                                    &original,
                                    answer_dst,
                                    settings.colour_strength,
                                    settings.transfer_strength,
                                    settings.max_ratio,
                                    bgr_order,
                                );
                            }
                        }
                    }
                    if composed_async.is_none() && !composed_sync {
                        crate::composition::apply::apply_rgba8(
                            &original,
                            answer_dst,
                            settings.colour_strength,
                            settings.transfer_strength,
                            settings.max_ratio,
                            settings.debug_view,
                            bgr_order,
                        );
                    }
                } else {
                    // "Off keeps the whole pass running... and simply presents the
                    // clean frame" -- ShmHeader::apply_model's own doc comment.
                    answer_dst.copy_from_slice(&original);
                }
            }
        }
    }
    crate::log!(
        "[capture] {}x{} {} bytes -> proxy; round trip answered={} composed_async={}",
        width,
        height,
        frame_bytes,
        answered,
        composed_async.is_some()
    );
    if let Some(sem) = composed_async {
        // `image` is already fully written (on the GPU's own timeline -- not
        // necessarily *complete* yet, that's the entire point) and back in
        // `PRESENT_SRC_KHR`. Nothing left to do this frame except hand `sem` up to
        // the caller so the real present call waits on it.
        crate::log!(
            "[capture] timing stage1={:?} snapshot={:?} write_proxy={:?} roundtrip={:?} compose(async-dispatch-only)={:?} stage2=skipped total={:?}",
            t_stage1,
            t_snapshot,
            t_write_proxy,
            t_roundtrip,
            t_compose_start.elapsed(),
            t_stage1_start.elapsed(),
        );
        shm.publish_frame_timing(t_stage1_start.elapsed(), true);
        return Some(sem);
    }

    // Real `ShmHeader::capture_request` support: dump this frame's original and
    // final (post-composition, if any ran above) bytes to disk. Checked regardless of
    // `answered`/`proxy_format` so a request during a fail-open frame still produces a
    // (identical) matched pair rather than silently doing nothing -- `write_pair`
    // itself is the only place that would need to special-case a format it can't
    // encode, and today it always gets `RGBA8` bytes either way.
    if shm.take_capture_request() && neuralforge_protocol::enums::proxy_format::is_8bit(proxy_format) {
        // SAFETY: same reasoning as every other read of `r.ptr` in this function --
        // still a live mapping of at least `frame_bytes` bytes, and stage 2 below
        // hasn't started overwriting it yet.
        let current = unsafe { std::slice::from_raw_parts(r.ptr, frame_bytes as usize) };
        crate::dump::write_pair(&original, current, width, height, bgr_order);
    }

    // Stage 2: staging buffer (now holding the answer, if there was one -- otherwise
    // still the captured bytes) -> image.
    // SAFETY: `r.cmd` was ended above; the pool it came from allows re-recording.
    if unsafe { device.reset_command_buffer(r.cmd, vk::CommandBufferResetFlags::empty()) }.is_err() {
        return None;
    }
    // SAFETY: `r.cmd` was just reset.
    if unsafe { device.begin_command_buffer(r.cmd, &begin_info) }.is_err() {
        return None;
    }
    let copy_in = copy_out;
    // SAFETY: `image` is currently `TRANSFER_DST_OPTIMAL` from stage 1's own final
    // barrier; `r.buffer` (same host-coherent memory as `r.ptr`, which the CPU-side
    // block above may have just overwritten with the answer) holds exactly
    // `frame_bytes` valid bytes either way, matching `copy_in`'s own extent.
    unsafe {
        device.cmd_copy_buffer_to_image(r.cmd, r.buffer, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[copy_in]);
    }
    let to_present = barrier(
        image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::PRESENT_SRC_KHR,
        vk::AccessFlags::TRANSFER_WRITE,
        vk::AccessFlags::empty(),
    );
    // SAFETY: restores the layout `vkQueuePresentKHR` requires before the caller's own
    // (real) present call runs right after this function returns.
    unsafe {
        device.cmd_pipeline_barrier(
            r.cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_present],
        );
    }
    if unsafe { device.end_command_buffer(r.cmd) }.is_err() {
        return None;
    }
    // SAFETY: same reasoning as stage 1's own fence reset/submit/wait.
    if unsafe { device.reset_fences(&[r.fence]) }.is_err() {
        return None;
    }
    let submit2 = vk::SubmitInfo::builder().command_buffers(std::slice::from_ref(&r.cmd)).build();
    let t_stage2_start = std::time::Instant::now();
    // SAFETY: `r.cmd` was just recorded and ended above.
    if unsafe { device.queue_submit(queue, &[submit2], r.fence) }.is_err() {
        return None;
    }
    // SAFETY: `r.fence` was just submitted against above. Waiting here (rather than
    // deferring to the next frame) keeps `image` fully write-back-complete and back in
    // `PRESENT_SRC_KHR` before this function returns, which is what the caller's own
    // immediately-following real present call requires.
    if unsafe { device.wait_for_fences(&[r.fence], true, u64::MAX) }.is_err() {
        return None;
    }
    let t_stage2 = t_stage2_start.elapsed();
    crate::log!(
        "[capture] timing stage1={:?} snapshot={:?} write_proxy={:?} roundtrip={:?} compose={:?} stage2={:?} total={:?}",
        t_stage1,
        t_snapshot,
        t_write_proxy,
        t_roundtrip,
        // `t_compose_start` was captured right after the round trip; `t_stage2_start`
        // right before stage 2's own submit -- the gap between them is exactly the
        // composition work (CPU reference or GPU dispatch), with no double-counting
        // against `t_stage2` below.
        t_stage2_start.duration_since(t_compose_start),
        t_stage2,
        t_stage1_start.elapsed(),
    );
    shm.publish_frame_timing(t_stage1_start.elapsed(), answered);

    None
}

/// # Safety
/// Must only be called at device-destruction time, with no submitted work referencing
/// these handles still in flight.
pub unsafe fn destroy(resources: Option<CaptureResources>, device: &ash::Device) {
    if let Some(r) = resources {
        // SAFETY: forwarded from this function's own contract.
        unsafe { r.destroy(device) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EXTERNAL_MEMORY_HOST_EXTENSION;
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Same shape as `composition::gpu::tests::test_device` -- a real (if software)
    /// Vulkan device via whatever loader/ICD is on this machine, `None` if there
    /// isn't one. Not shared with that module (private to it, and this crate has no
    /// shared test-support module yet); small enough that duplicating it costs less
    /// than inventing one.
    fn test_device() -> Option<(ash::Entry, ash::Instance, vk::PhysicalDevice, ash::Device, vk::Queue, u32)> {
        // SAFETY: loads the system Vulkan loader; the usual caveats of loading an
        // arbitrary shared library apply and are accepted here the same way every
        // other `ash` consumer in this crate already does.
        let entry = unsafe { ash::Entry::load() }.ok()?;
        let app_info = vk::ApplicationInfo::builder().api_version(vk::API_VERSION_1_3);
        let create_info = vk::InstanceCreateInfo::builder().application_info(&app_info);
        // SAFETY: `create_info` is valid.
        let instance = unsafe { entry.create_instance(&create_info, None) }.ok()?;
        // SAFETY: `instance` was just created and outlives every use of `physical_device`.
        let physical_device = *unsafe { instance.enumerate_physical_devices() }.ok()?.first()?;
        let queue_family = 0;
        let queue_info = [vk::DeviceQueueCreateInfo::builder().queue_family_index(queue_family).queue_priorities(&[1.0]).build()];
        let device_create_info = vk::DeviceCreateInfo::builder().queue_create_infos(&queue_info);
        // SAFETY: `device_create_info` is valid; every physical device has a family 0.
        let device = unsafe { instance.create_device(physical_device, &device_create_info, None) }.ok()?;
        // SAFETY: `device`/family/index 0 match what `device_create_info` just requested.
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        Some((entry, instance, physical_device, device, queue, queue_family))
    }

    /// A standalone image standing in for a real swapchain image, already in
    /// `PRESENT_SRC_KHR` -- what `run`'s own contract requires of `image` on entry,
    /// same as any image `vkQueuePresentKHR`'s own precondition hasn't been violated
    /// on.
    fn make_present_src_image(device: &ash::Device, mem_props: &vk::PhysicalDeviceMemoryProperties, queue: vk::Queue, pool: vk::CommandPool, width: u32, height: u32) -> (vk::Image, vk::DeviceMemory) {
        let info = vk::ImageCreateInfo::builder()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::STORAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&info, None) }.expect("failed to create the test's own target image");
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let type_index = (0..mem_props.memory_type_count)
            .find(|&i| reqs.memory_type_bits & (1 << i) != 0 && mem_props.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL))
            .expect("no suitable memory type for the test's own target image");
        let memory = unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::builder().allocation_size(reqs.size).memory_type_index(type_index), None) }.unwrap();
        unsafe { device.bind_image_memory(image, memory, 0) }.unwrap();

        let alloc_info = vk::CommandBufferAllocateInfo::builder().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1);
        let cmd = unsafe { device.allocate_command_buffers(&alloc_info) }.unwrap()[0];
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::builder(), None) }.unwrap();
        let begin_info = vk::CommandBufferBeginInfo::builder().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            device.begin_command_buffer(cmd, &begin_info).unwrap();
            let to_present = barrier(image, vk::ImageLayout::UNDEFINED, vk::ImageLayout::PRESENT_SRC_KHR, vk::AccessFlags::empty(), vk::AccessFlags::empty());
            device.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::ALL_COMMANDS, vk::DependencyFlags::empty(), &[], &[], &[to_present]);
            device.end_command_buffer(cmd).unwrap();
            device.queue_submit(queue, &[vk::SubmitInfo::builder().command_buffers(std::slice::from_ref(&cmd)).build()], fence).unwrap();
            device.wait_for_fences(&[fence], true, u64::MAX).unwrap();
            device.destroy_fence(fence, None);
            device.free_command_buffers(pool, &[cmd]);
        }
        (image, memory)
    }

    fn scratch_path(tag: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        format!("{}/neuralforge-capture-test-{}-{tag}-{n}/shm.bin", std::env::temp_dir().display(), std::process::id())
    }

    /// Same shape as `test_device`, except the device is created with
    /// `VK_EXT_external_memory_host` enabled when (and only when) the physical device
    /// actually advertises it -- `None` if there's no Vulkan loader/ICD at all, or a
    /// separate flag saying whether the extension actually ended up enabled, so
    /// `direct_capture_writes_straight_into_imported_host_memory` can skip itself
    /// cleanly on a machine (this dev sandbox's software ICD, most likely) that
    /// doesn't support it, rather than fail for a reason that has nothing to do with
    /// this crate's own code.
    fn test_device_with_external_memory_host() -> Option<(ash::Entry, ash::Instance, vk::PhysicalDevice, ash::Device, vk::Queue, u32, bool)> {
        // SAFETY: same reasoning as `test_device`.
        let entry = unsafe { ash::Entry::load() }.ok()?;
        let app_info = vk::ApplicationInfo::builder().api_version(vk::API_VERSION_1_3);
        let create_info = vk::InstanceCreateInfo::builder().application_info(&app_info);
        let instance = unsafe { entry.create_instance(&create_info, None) }.ok()?;
        let physical_device = *unsafe { instance.enumerate_physical_devices() }.ok()?.first()?;
        let supported = unsafe { instance.enumerate_device_extension_properties(physical_device) }.is_ok_and(|extensions| {
            extensions.iter().any(|extension| {
                let name = unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) };
                name == EXTERNAL_MEMORY_HOST_EXTENSION
            })
        });
        let queue_family = 0;
        let queue_info = [vk::DeviceQueueCreateInfo::builder().queue_family_index(queue_family).queue_priorities(&[1.0]).build()];
        let extension_names = [EXTERNAL_MEMORY_HOST_EXTENSION.as_ptr()];
        let mut device_create_info = vk::DeviceCreateInfo::builder().queue_create_infos(&queue_info);
        if supported {
            device_create_info = device_create_info.enabled_extension_names(&extension_names);
        }
        let device = unsafe { instance.create_device(physical_device, &device_create_info, None) }.ok()?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        Some((entry, instance, physical_device, device, queue, queue_family, supported))
    }

    /// Like `make_present_src_image`, but also fills the image with a known solid
    /// color before transitioning it to `PRESENT_SRC_KHR` -- `make_present_src_image`
    /// itself leaves its image's contents undefined, fine for tests that only check
    /// *that* a copy happened, not *what* it copied. This test needs the latter: the
    /// whole point is confirming the imported-memory capture lands the *right* bytes,
    /// not just *some* bytes.
    fn make_filled_present_src_image(device: &ash::Device, mem_props: &vk::PhysicalDeviceMemoryProperties, queue: vk::Queue, pool: vk::CommandPool, width: u32, height: u32, color: [f32; 4]) -> (vk::Image, vk::DeviceMemory) {
        let info = vk::ImageCreateInfo::builder()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::STORAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&info, None) }.expect("failed to create the test's own target image");
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let type_index = (0..mem_props.memory_type_count)
            .find(|&i| reqs.memory_type_bits & (1 << i) != 0 && mem_props.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL))
            .expect("no suitable memory type for the test's own target image");
        let memory = unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::builder().allocation_size(reqs.size).memory_type_index(type_index), None) }.unwrap();
        unsafe { device.bind_image_memory(image, memory, 0) }.unwrap();

        let alloc_info = vk::CommandBufferAllocateInfo::builder().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1);
        let cmd = unsafe { device.allocate_command_buffers(&alloc_info) }.unwrap()[0];
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::builder(), None) }.unwrap();
        let begin_info = vk::CommandBufferBeginInfo::builder().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            device.begin_command_buffer(cmd, &begin_info).unwrap();
            let to_dst = barrier(image, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE);
            device.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[to_dst]);
            device.cmd_clear_color_image(cmd, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &vk::ClearColorValue { float32: color }, &[subresource()]);
            let to_present = barrier(image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::PRESENT_SRC_KHR, vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::empty());
            device.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::ALL_COMMANDS, vk::DependencyFlags::empty(), &[], &[], &[to_present]);
            device.end_command_buffer(cmd).unwrap();
            device.queue_submit(queue, &[vk::SubmitInfo::builder().command_buffers(std::slice::from_ref(&cmd)).build()], fence).unwrap();
            device.wait_for_fences(&[fence], true, u64::MAX).unwrap();
            device.destroy_fence(fence, None);
            device.free_command_buffers(pool, &[cmd]);
        }
        (image, memory)
    }

    /// The actual point of `EXTERNAL_MEMORY_HOST_DESIGN.md`, exercised end to end
    /// against a real (if software) device: a capture submitted against a
    /// `DirectCapture` slot must land its bytes directly in the imported host
    /// pointer -- not a staging buffer, not something that merely runs without
    /// crashing, but the *exact* pixels the source image held. Skips itself (not a
    /// failure) if this machine's Vulkan device doesn't advertise
    /// `VK_EXT_external_memory_host` at all.
    #[test]
    fn direct_capture_writes_straight_into_imported_host_memory() {
        let Some((_entry, instance, physical_device, device, queue, queue_family, supported)) = test_device_with_external_memory_host() else {
            eprintln!("direct_capture_writes_straight_into_imported_host_memory: no Vulkan loader/ICD, skipping");
            return;
        };
        if !supported {
            eprintln!("direct_capture_writes_straight_into_imported_host_memory: VK_EXT_external_memory_host not supported here, skipping");
            unsafe { device.destroy_device(None); instance.destroy_instance(None); }
            return;
        }
        let Some(alignment) = min_imported_host_pointer_alignment(&instance, physical_device) else {
            eprintln!("direct_capture_writes_straight_into_imported_host_memory: alignment query failed, skipping");
            unsafe { device.destroy_device(None); instance.destroy_instance(None); }
            return;
        };

        let (width, height) = (8u32, 8u32);
        let frame_bytes = (width * height * 4) as usize;
        // A real `mmap` (page-aligned, so a multiple of any real driver's alignment
        // requirement -- NVIDIA's own is 4096) standing in for the SHM proxy region
        // this path is actually meant to import; rounded up to `alignment` for
        // drivers that want more than a page.
        let region_len = frame_bytes.max(alignment as usize).div_ceil(alignment as usize) * alignment as usize;
        // SAFETY: a plain anonymous mapping, valid for the rest of this test; never
        // shared with another process, matching every other precondition
        // `ensure_direct_capture`'s own safety contract asks for.
        let host_ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), region_len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        assert_ne!(host_ptr, libc::MAP_FAILED, "mmap for the test's own host region failed");
        let host_ptr = host_ptr.cast::<u8>();
        assert_eq!(host_ptr as usize % alignment as usize, 0, "mmap must hand back at least page-aligned memory");

        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let pool_info = vk::CommandPoolCreateInfo::builder().queue_family_index(queue_family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool = unsafe { device.create_command_pool(&pool_info, None) }.expect("failed to create the test's own command pool");
        // An arbitrary, checkable, non-zero pattern -- chosen so every channel lands
        // on an *exact* u8 value (100/150/200/255), not a half-integer boundary like
        // 0.5*255=127.5, where real drivers can reasonably round either way and a
        // fixed `.round()` in this test would be guessing which.
        let color = [100.0 / 255.0, 150.0 / 255.0, 200.0 / 255.0, 1.0];
        let (image, image_memory) = make_filled_present_src_image(&device, &mem_props, queue, pool, width, height, color);

        let mut direct: Option<DirectCapture> = None;
        // SAFETY: `host_ptr` is valid for `region_len` bytes for the rest of this
        // test; nothing else writes to it. `region_len`, not `frame_bytes`, per
        // VUID-VkMemoryAllocateInfo-allocationSize-01745: the allocation itself must
        // be a multiple of `alignment` (4096 on this real NVIDIA driver) even though
        // the actual pixel copy below only ever touches the first `frame_bytes`.
        assert!(
            unsafe { ensure_direct_capture(&mut direct, &device, &instance, physical_device, queue_family, host_ptr, region_len as vk::DeviceSize) },
            "ensure_direct_capture should succeed with a supported, correctly aligned host pointer"
        );
        let d = direct.as_mut().unwrap();
        assert!(submit_direct_capture(d, &device, queue, image, vk::ImageLayout::PRESENT_SRC_KHR, width, height, neuralforge_protocol::enums::proxy_format::RGBA8));
        // A real wait (not `poll_direct_capture`'s own non-blocking check) is correct
        // here: this test cares whether the capture is *correct*, not whether `run`'s
        // own present-hook discipline of never blocking holds -- that's
        // `run_never_blocks_on_a_slow_helper_and_eventually_composites`'s job, not
        // this test's.
        unsafe { device.wait_for_fences(&[d.buf.fence], true, u64::MAX) }.expect("capture fence wait failed");
        assert_eq!(poll_direct_capture(d, &device), Some((width, height, neuralforge_protocol::enums::proxy_format::RGBA8)));

        // SAFETY: the fence wait above confirms the GPU's writes to `host_ptr` are
        // complete and visible to the CPU (host-coherent memory).
        let captured = unsafe { std::slice::from_raw_parts(host_ptr, frame_bytes) };
        // `.round()`, not a bare `as u8` truncation: the real float->UNORM8 conversion
        // `vkCmdClearColorImage`/the copy actually perform rounds to nearest (found
        // live: 0.25 * 255 = 63.75, which truncation would wrongly expect as 63
        // against the real, correct 64).
        let expected: [u8; 4] = [(color[0] * 255.0).round() as u8, (color[1] * 255.0).round() as u8, (color[2] * 255.0).round() as u8, (color[3] * 255.0).round() as u8];
        for pixel in captured.chunks_exact(4) {
            assert_eq!(pixel, expected, "every captured pixel must match the source image's own fill color, read straight out of the imported host pointer");
        }

        unsafe {
            device.device_wait_idle().unwrap();
            device.destroy_image(image, None);
            device.free_memory(image_memory, None);
            device.destroy_command_pool(pool, None);
            destroy_direct_capture(direct, &device);
            device.destroy_device(None);
            instance.destroy_instance(None);
            libc::munmap(host_ptr.cast(), region_len);
        }
    }

    /// The real point of the pipelined redesign, exercised end to end against a real
    /// (if software) Vulkan device: `run` must never block a present call waiting on
    /// the helper, even when the helper genuinely takes far longer than one frame to
    /// answer -- and once it does answer, the result must actually reach `image` via
    /// a real, verifiable composited write (not just "a semaphore came back").
    #[test]
    fn run_never_blocks_on_a_slow_helper_and_eventually_composites() {
        let Some((_entry, instance, physical_device, device, queue, queue_family)) = test_device() else {
            eprintln!("run_never_blocks_on_a_slow_helper_and_eventually_composites: no Vulkan loader/ICD, skipping");
            return;
        };

        let path = scratch_path("blocks");
        let mut shm = ShmClient::default();
        assert!(shm.test_open_at(&path), "test-only open_at should always succeed against a scratch path");
        let hdr_ptr = shm.test_header_ptr();
        // A live helper (matters for `poll_async_request`'s timeout budget: the long
        // "steady state" one, not the short "nobody's listening" one, since this test
        // deliberately answers slower than that short budget).
        unsafe { &*(hdr_ptr as *mut neuralforge_protocol::ShmHeader) }.helper_state.store(neuralforge_protocol::enums::helper_state::RUNNING, AtomicOrdering::Relaxed);

        // A fake helper that only answers `HELPER_DELAY` after it sees a new request --
        // long enough that if `run` ever blocked waiting for it, a handful of calls
        // spaced much closer together than that would visibly take just as long.
        const HELPER_DELAY: Duration = Duration::from_millis(250);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let helper = std::thread::spawn(move || {
            // SAFETY: the mapping outlives this thread (joined before the test ends).
            let hdr = unsafe { &*(hdr_ptr as *mut neuralforge_protocol::ShmHeader) };
            let mut last_seen = 0u32;
            while !stop_clone.load(AtomicOrdering::Relaxed) {
                let req = hdr.seq_req.load(AtomicOrdering::Relaxed);
                if req != 0 && req != last_seen {
                    last_seen = req;
                    std::thread::sleep(HELPER_DELAY);
                    hdr.seq_resp.store(req, AtomicOrdering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        let (width, height) = (8u32, 8u32);
        let proxy_format = neuralforge_protocol::enums::proxy_format::RGBA8;
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let pool_info = vk::CommandPoolCreateInfo::builder().queue_family_index(queue_family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool = unsafe { device.create_command_pool(&pool_info, None) }.expect("failed to create the test's own command pool");
        let (image, image_memory) = make_present_src_image(&device, &mem_props, queue, pool, width, height);

        let mut resources: Option<CaptureResources> = None;
        let mut pipeline: Option<CapturePipeline> = None;
        let mut direct: Option<DirectCapture> = None;
        let mut gpu_compose: Option<crate::composition::gpu::GpuCompose> = None;
        let mut original_scratch = Vec::new();
        let mut answer_scratch = Vec::new();
        let mut last_answer = Vec::new();
        let mut inflight = Inflight::default();

        let mut got_semaphore = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        // Real usage calls this once per present, indefinitely -- loop until either a
        // real composited result shows up or the deadline (comfortably several
        // `HELPER_DELAY`-long round trips) is exhausted, not a fixed iteration count,
        // so this can't spuriously fail just because a scratch VM's first Vulkan call
        // of the test happened to be slow.
        while Instant::now() < deadline {
            let call_start = Instant::now();
            // SAFETY: `image` is this test's own, currently `PRESENT_SRC_KHR`; `queue`
            // is used from this one thread only, exactly like `run`'s own contract
            // requires of the real present hook.
            let sem = unsafe {
                run(
                    &device,
                    &instance,
                    physical_device,
                    queue,
                    queue_family,
                    image,
                    vk::ImageLayout::PRESENT_SRC_KHR,
                    image,
                    width,
                    height,
                    proxy_format,
                    false,
                    &mut resources,
                    &mut pipeline,
                    &mut direct,
                    // This test's own device never enables `VK_EXT_external_memory_host`
                    // (see `test_device`'s minimal `DeviceCreateInfo`), so this must be
                    // `false` -- exercising `CapturePipeline`, the path this test
                    // actually validates. A `DirectCapture` equivalent needs its own
                    // test with the extension genuinely enabled, not this one lying
                    // about it.
                    false,
                    &mut gpu_compose,
                    &mut shm,
                    &mut original_scratch,
                    &mut inflight,
                    &mut answer_scratch,
                    &mut last_answer,
                )
            };
            let call_time = call_start.elapsed();
            assert!(
                call_time < HELPER_DELAY / 2,
                "a single run() call took {call_time:?} -- must never approach the helper's own {HELPER_DELAY:?} answer delay"
            );
            if let Some(sem) = sem {
                got_semaphore = true;
                // Stand in for what the real present call does: wait on the semaphore
                // before the image is considered final, exactly like
                // `composition::gpu::tests`' own async tests already establish.
                let wait_fence = unsafe { device.create_fence(&vk::FenceCreateInfo::builder(), None) }.unwrap();
                let wait_stage = vk::PipelineStageFlags::ALL_COMMANDS;
                let submit = vk::SubmitInfo::builder().wait_semaphores(std::slice::from_ref(&sem)).wait_dst_stage_mask(std::slice::from_ref(&wait_stage)).build();
                unsafe {
                    device.queue_submit(queue, &[submit], wait_fence).unwrap();
                    device.wait_for_fences(&[wait_fence], true, u64::MAX).unwrap();
                    device.destroy_fence(wait_fence, None);
                }
                break;
            }
            if !last_answer.is_empty() {
                got_semaphore = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got_semaphore, "the pipeline must eventually composite a real answer within 5s of real time, not just avoid blocking forever");

        stop.store(true, AtomicOrdering::Relaxed);
        helper.join().unwrap();

        // A capture pipeline slot can legitimately still be `pending` here (the test
        // loop can exit as soon as `last_answer` is non-empty, with no guarantee the
        // *next* speculative capture submission already resolved) -- wait for the
        // whole device idle first, the same real teardown precondition
        // `destroy_private_resources` relies on in production, before either
        // `destroy` call below touches anything.
        unsafe { device.device_wait_idle() }.unwrap();
        // SAFETY: every semaphore this test waited on has a completed, waited-for
        // fence behind it (the explicit wait above); the idle wait just above
        // confirms every capture-pipeline slot's own fence too; nothing else touched
        // `image`.
        unsafe {
            device.destroy_image(image, None);
            device.free_memory(image_memory, None);
            device.destroy_command_pool(pool, None);
            destroy(resources, &device);
            destroy_pipeline(pipeline, &device);
            destroy_direct_capture(direct, &device);
            if let Some(gpu) = gpu_compose {
                gpu.destroy(&device);
            }
            device.destroy_device(None);
            instance.destroy_instance(None);
        }
    }
}
