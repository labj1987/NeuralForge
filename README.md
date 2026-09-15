# NeuralForge

NeuralForge is a Linux Vulkan implicit layer plus Windows helper that forwards presented frames to
NVIDIA's DLSS 5 Neural Rendering model, running the model itself under Wine/Proton.
Written in Rust with GTK4 and libadwaita. A from-scratch rebuild of
[DLSS5VKLayer](https://github.com/bmitch87/DLSS5VKLayer)'s architecture, not a fork —
see [ATTRIBUTION.md](ATTRIBUTION.md) for exactly what that means and where the ideas
came from.

This is experimental, personal-use software. It works around an authorization check in
NVIDIA's proprietary NGX DLL to run the model outside its intended integration path —
see [Legal](#legal) before you use it.

Repository: [labj1987/NeuralForge](https://github.com/labj1987/NeuralForge).

See [PHASE1.md](PHASE1.md) for coexistence, installation, migration and the benchmark roadmap.
Current target-machine findings are tracked in [HARDWARE_VALIDATION.md](HARDWARE_VALIDATION.md).
NeuralForge's guarded GTA render tap has been validated with the full helper and model;
its current synchronous host transport is a correctness baseline, not a performance result.

## Screenshots

| Settings |
|---|
| ![NeuralForge settings window with Model, Motion, Composition, Debug, and Status tabs](screenshots/settings.png) |

## What it does

- A Vulkan implicit layer hooks presentation for native Linux and Proton games. It
  sends bounded captured frames to a Windows helper over private shared memory and
  presents untouched frames whenever an answer is not ready.
- Fail-open: if the helper isn't running or the model fails to initialize, the layer
  just presents the original frame — nothing about the game's rendering depends on it.
- The Windows-side helper runs NVIDIA's `nvngx_dlssnr.dll` (Feature 18) under Wine or a
  Proton build. The experimental `VK_NV_optical_flow` motion path is disabled in
  the known-good baseline.
- HDR-aware capture and composition support float16/PQ paths when a compatible
  swapchain exposes them; the validated GTA baseline is SDR B8G8R8A8.
- A composition pass blends the model's output back into the frame — tone/structure/
  skin/sharpness controls, a reversible neutral-axis proxy mode, and a choice of
  resampling filters (Lanczos, Catmull-Rom, Mitchell-Netravali, Kaiser-windowed sinc)
  for the supersampling leg. This math is rederived independently from public sources,
  not ported from any GPL-licensed code — see ATTRIBUTION.md.
- GTK4/libadwaita settings app for all of the above, live-bound to the running layer
  over the same shared-memory segment.
- A CLI (`neuralforge-cli`) for runner discovery, starting/stopping the helper, status,
  diagnostics, importing the NVIDIA NGX DLLs, and raw settings introspection
  (`shmctl status`/`set`/`toggle`/`capture`) — no bash script, no root step.
- Everything lives under `~/.local/share`, `~/.config`, and `/tmp/neuralforge-$UID/`. No
  polkit, no pkexec, no privileged install step at all.

## Requirements

- x86_64 Linux, NVIDIA GPU and driver, Vulkan loader.
- A Wine install or a Steam compatibility tool that bundles DXVK-NVAPI (e.g.
  Proton-CachyOS, Proton-GE) to run the Windows-side helper. Valve's stock Proton
  builds don't bundle DXVK-NVAPI, so they aren't a supported runner.
- NVIDIA's own NGX DLLs, which this project doesn't and can't ship — see below.

## NVIDIA NGX DLLs

`nvngx_dlssnr.dll` is NVIDIA's proprietary model binary and isn't included here. Get it
from your own NVIDIA driver/SDK install and import it with:

```bash
neuralforge-cli import-binaries /path/to/dlls
```

or from the GUI's binaries import flow. Files are copied into
`$XDG_DATA_HOME/neuralforge/binaries`; restart the helper afterward.

## Install

Download the AppImage from [Releases](https://github.com/labj1987/NeuralForge/releases):

```bash
chmod +x NeuralForge-*-x86_64.AppImage
./NeuralForge-*-x86_64.AppImage
```

For Steam games launched separately from the GUI, install the extracted AppDir into
persistent user storage so Vulkan can find the layer after the AppImage exits:

```bash
python3 scripts/install.py install --appdir build-appimage/AppDir
```

The GUI is `neuralforge`; the CLI is `neuralforge-cli`; the Windows helper is
`neuralforge-helper.exe`. Use `NEURALFORGE_ENABLE=1` to activate the layer. Config,
data, state, runtime, control mapping and helper prefix use their own `neuralforge`
locations. Upstream DLSS5VKLayer can remain installed; NeuralForge neither migrates
ambiguous upstream state nor changes its files, configuration, launch options, helper,
or runtime. See [PHASE1.md](PHASE1.md) for executable targeting, migration, uninstall
and the exact GTA baseline.

## GTA status

On the RTX 5070 target, GTA V Enhanced exposes a swapchain without transfer-source
usage. NeuralForge therefore does not read the swapchain image. It tracks GTA's own
render-to-swapchain blit, captures the demonstrated transfer-capable source only
after its layout has returned to `GENERAL`, restores that layout, and presents through
the original swapchain. Rockstar Launcher, Social Club, Wine Explorer, Xalia, and
overlays remain pass-through. The full model and helper remain enabled; the known-good
baseline uses `NEURALFORGE_DMABUF=0`.

Use this Steam launch option for the isolated GTA session:

```text
NEURALFORGE_ENABLE=1 NEURALFORGE_DMABUF=0 NEURALFORGE_TARGET_EXE=GTA5_Enhanced.exe %command%
```

The target validation uses GTA V Enhanced Steam AppID `3240220`, an RTX 5070 with
driver `615.71.09`, 2560×1440 at 288 Hz, GNOME scale 100%, passes=1, model
resolution=1, and motion disabled. The render tap keeps Rockstar Launcher, Social
Club, Wine Explorer, Xalia, Steam overlay, and other non-target processes pass-through.

The synchronous host-SHM baseline processed 475 and 474 layer frames in two separate
61-second GTA intervals: 7.73 and 7.72 layer frames/sec. Those intervals confirm the
tap works and identify the capture fence wait as the bottleneck. They are not game FPS,
1%-low metrics, or an upstream comparison because the scene was not controlled. See
[HARDWARE_VALIDATION.md](HARDWARE_VALIDATION.md) for the measurements,
[RENDER_TAP_DESIGN.md](RENDER_TAP_DESIGN.md) for the capture constraints, and
[ASYNC_CAPTURE_DESIGN.md](ASYNC_CAPTURE_DESIGN.md) for the next bounded pipeline.

## Building from source

```bash
cargo build --release          # protocol, layer, gui, cli (native Linux)
cargo +stable build --release --target x86_64-pc-windows-gnu -p neuralforge-helper
./build-appimage.sh            # packs everything into an AppImage
```

Needs `mingw-w64` and the GTK4/libadwaita dev packages; see `build-appimage.sh` for the
exact package list. `CLAUDE.md` covers toolchain gotchas in detail if you're
cross-compiling the Windows helper on a machine with its own non-rustup Rust install.

## Legal

`nvngx_dlssnr.dll` checks which module is calling into it and refuses to run outside
its intended host application. The helper here spoofs that check (an IAT hook on the
caller-identity query) so the model will initialize at all under a generic Vulkan
helper process. That is a deliberate design choice, not an accident, and it likely
falls under DMCA §1201 (circumventing an access control) and/or breaches NVIDIA's NGX
EULA, depending on jurisdiction and how you use it. There's no license grant here for
that mechanism and none implied — use it at your own legal risk, for personal,
non-commercial use.

The composition/color pipeline in `crates/layer/src/composition/` is original work
rederived from public, permissively-licensed sources (see ATTRIBUTION.md); no
GPL-licensed code was read or ported to build it.

## License

This project's own code is MIT-licensed — see [LICENSE](LICENSE). It links against and
depends on NVIDIA's proprietary NGX SDK/DLLs at runtime, which are not covered by that
license and are not redistributed here.

## Acknowledgements

Development assistance: Claude Code (Anthropic) and Codex (OpenAI).
