# Protocol v3: a second independent request/response slot

Phase 3's remaining goal after `EXTERNAL_MEMORY_HOST_DESIGN.md`'s zero-copy host
import: the wire protocol has only ever supported one outstanding request at a time
(a single `seq_req`/`seq_resp` pair). Even with `CapturePipeline`'s two GPU capture
buffers (`ASYNC_CAPTURE_DESIGN.md`), a second, already-captured frame just sits idle
once captured -- the layer cannot send it to the helper until the first request's
answer comes back, because there is only one wire slot to send it on. v3 adds a
second, fully independent slot so the layer is never blocked with a ready frame and
nowhere to send it.

## What stays serialized, and why

The helper's `FrameResources::evaluate` calls NGX's `EvaluateFeature` synchronously
(upload, evaluate, download, each waiting on its own fence before the next starts) on
a single feature handle (`snippet.feature`, created once by `ngx::ensure_feature`).
Nothing in this project's own reverse-engineering of this NVIDIA API establishes that
a single NGX feature handle is safe to evaluate concurrently from two overlapping GPU
submissions -- DLSS-family NGX features are, as far as this project has ever
observed or documented, a strictly one-evaluate-in-flight-per-feature design. Betting
on undocumented concurrent-call safety, on a *reverse-engineered* feature, running
under Wine, is exactly the kind of guess this project's own history
(`docs/history/development-before-neuralforge.md`'s reverted fence-wait "fix",
`EXTERNAL_MEMORY_HOST_DESIGN.md`'s two crashes found only after "everything passed")
says not to make without real evidence.

So v3 does **not** attempt concurrent `EvaluateFeature` calls. The helper still
processes both slots' NGX evaluation one at a time, in whichever order their
`seq_req`/`seq_req_b` bumped. What v3 actually buys:

- The layer can have a second frame's bytes already sent to the helper the moment
  they're captured, instead of holding them in `CapturePipeline`'s GPU buffer (or, for
  `DirectCapture`, not being able to start a second zero-copy capture at all -- see
  below) until the first wire request resolves. That is real, measurable dead time
  removed from the layer's own present-hook cost, independent of whatever the helper's
  own NGX evaluation time is.
- The helper's *upload* for the next request can happen (staging copy, or nothing at
  all for the imported-memory path) while the *previous* request's evaluate/download
  is still the thing occupying the GPU queue -- still ordered, but the CPU side isn't
  idle waiting for a wire slot either.

## What's duplicated for slot 1, and what isn't

New `ShmHeader` fields (appended at the end, like every prior addition — see that
struct's own layout comment): `seq_req_b`, `seq_resp_b`, `width_b`, `height_b`,
`proxy_format_b`. New memory regions: `proxy_b_offset()`, `answer_b_offset()`,
each `MAX_FRAME` bytes, appended after the existing motion region. `SHM_VERSION`
bumped to 3 -- a mismatched-version mapping is rejected and reinitialized by both
sides' existing `is_valid()`/`open()` contract, so this is a clean break, not a
migration; both processes always ship from the same build.

**Not duplicated**, deliberately:

- `format` -- always 1, written once at `init_defaults` and never read anywhere in
  this workspace (confirmed by grep). A genuinely dead field; duplicating dead code
  adds surface area for zero behavioral value.
- `hdr_encode`, `answered_w`, `answered_h` -- declared, defaulted, round-tripped by
  `reset_persisted_settings`, but (also confirmed by grep) never actually read or
  written by any live layer/helper code path today. Same reasoning as `format`.
- `frame_mvec_valid`, `frame_mvec_scale_mode` -- real fields with real read/write
  code, but that code (`ShmClient::prepare_motion`,
  `ShmClient::prepare_motion_resources`) is itself unconditionally disabled (`return`
  as the first line, `#[allow(unreachable_code)]` below it) — motion vectors are off
  in this project's known-good baseline (see `mvec_enabled`'s own doc comment) because
  of a real NVIDIA driver crash during private-device creation, unrelated to this
  feature. Re-enabling motion and wiring a per-slot motion payload is a separate,
  future change; nothing here forecloses it (the fields still exist, singular, and a
  slot-1 motion payload can be added the same way slot 1's proxy/answer were, later).

## `DirectCapture` becomes two slots

`EXTERNAL_MEMORY_HOST_DESIGN.md` explains why `DirectCapture` was deliberately a
*single* slot despite `CapturePipeline` having two: importing the shared proxy region
directly as device memory only has one region to import into under protocol v2. With
two independent proxy regions now, `DirectCapture` gets a second slot, one import per
region, mirroring `CapturePipeline`'s own two-slot shape -- see that file's own doc
comment for the aliasing/write-hazard reasoning this mirrors.

## The helper side: two `FrameResources`, still one evaluate at a time

`neuralforge_helper::main`'s loop builds two `FrameResources` instances instead of
one -- one importing (or staging into) `proxy_region`/`answer_region`, the other
`proxy_b_region`/`answer_b_region`. Each loop iteration checks both `seq_req` and
`seq_req_b` for new work and processes whichever have changed, in the order noticed,
still fully sequentially (see "What stays serialized" above) -- never both at once,
just never idle-waiting on a wire slot that a captured frame could already be
occupying.

## Validation plan

Same discipline as every other Vulkan-touching change in this project's history:
`cargo test` first (protocol layer is fully testable without any Vulkan device),
`scripts/smoke-test.sh` next (catches the class of bug validation layers can't --
see `EXTERNAL_MEMORY_HOST_DESIGN.md`'s own "third and fourth bug" section for why
that step is never optional), then real hardware on `lordnikon`: `vkcube` under
`VK_LAYER_KHRONOS_validation` with `VK_LAYER_VALIDATE_SYNC=1`, and
`crates/protocol/examples/trigger_helper_roundtrip.rs` (extended to drive both slots)
against a real running helper. GTA fps against this change is not measured as part of
this work -- that needs the user's own live session, same gate as everything else in
Phase 3/4.
