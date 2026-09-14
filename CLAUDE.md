# NeuralForge developer guidance

Repository: https://github.com/labj1987/NeuralForge

Read [PHASE1.md](PHASE1.md) for the current namespace, installation contract and
benchmark plan. The former app name was dlssnr; upstream DLSS5VKLayer remains a
separate application and must not be modified or uninstalled by this project.

## Build and test

- `cargo test` and `cargo build --release` build the native default members.
- Do not use `--workspace` on Linux: the helper targets Windows only.
- `cargo +stable build --release --target x86_64-pc-windows-gnu -p neuralforge-helper`
  builds the helper when the cross target is installed in the stable toolchain.
- `CARGO_HELPER='cargo +stable' bash build-appimage.sh` packages NeuralForge.
- Run `python3 scripts/check_namespace.py`, `python3 scripts/test_install.py`,
  and `bash scripts/smoke-test.sh` for namespace, installation and Vulkan checks.

## Runtime constraints

Use only NeuralForge-owned paths, `NEURALFORGE_*` variables, and the
`VK_LAYER_neuralforge_neural` identity. Keep NVIDIA DLL names, NGX exports and
`DLSSNR.*` parameters unchanged. Do not copy or move ambiguous upstream config,
shared memory, or Wine prefixes. Import DLLs explicitly into NeuralForge's data dir.

Target-process filtering and the kernel ownership lease must remain effective before
any process can resize or write a channel. Preserve the GTA baseline: helper enabled,
passes=1, model_resolution=1, motion disabled/quality 0, host SHM transport.
DMA-BUF remains experimental. Do not lower model resolution or disable the helper
without explicit user authorization.

Do not reapply the reverted capture/composition fence changes. Validate actual GPU
operations and establish matched upstream/NeuralForge measurements before performance
changes. Keep the Rust implementation independent; review licenses before source reuse.

Current target-machine evidence and unresolved Vulkan errors are in
[HARDWARE_VALIDATION.md](HARDWARE_VALIDATION.md).

## Historical evidence

[Pre-rename development notes](docs/history/development-before-neuralforge.md) retain
old commands, measured failures and toolchain investigations as historical evidence.
Those old deployment recipes are not current instructions. The historical GTA handoff
is [HANDOFF_2026-09-12.md](HANDOFF_2026-09-12.md).
