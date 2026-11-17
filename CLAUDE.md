# monswitch

Cross-platform (Windows + macOS Apple Silicon) app that controls a monitor over DDC/CI:
input switching, PiP/PbP via monitor presets, a software KVM for PbP (Deskflow), an edge
transition effect and Sunshine/Moonlight streaming. Generic: nothing may assume a specific
monitor or machine – monitor-specific values live in presets
(`crates/ddc-core/presets/*.toml`).

## Stack
- Rust workspace, Tauri 2 (React + TypeScript + Tailwind) for the tray / menu-bar UI
- DDC: crate `ddc-hi` (Windows = dxva2 with NVAPI fallback, macOS = IOAVService)
- Config: TOML in ~/.config/monswitch/config.toml (Windows: %APPDATA%\monswitch);
  defaults in `crates/ddc-core/default_config.toml`

## Layout
crates/
  ddc-core/   # DisplayControl, DDC worker, config, presets, caps parser, probe
  cli/        # `monswitch` binary
  kvm/        # deskflow-core supervisor + log parser
  effect/     # edge transition effect (UDP link, cursor trail, native overlay)
  stream/     # Moonlight profiles, Sunshine on/off
apps/
  tray/       # Tauri app (menu bar on macOS, tray on Windows)

## Rules
- No unwrap() outside tests; errors via thiserror/anyhow
- Every DDC operation has a timeout and 1 retry (DDC is slow and sometimes fails)
- At least 50 ms between DDC commands
- Never hardcode VCP values in code – always from the config / presets
- User-facing text is English
- Commits only under the repository owner's name, no Co-Authored-By trailers
