# Roadmap

## Done
- **CLI + ddc-core** – input switching, PiP/PbP, probe/diff, rediscovery after the monitor
  briefly disappears, NVAPI fallback on Windows, monitor presets.
- **Tray / menu-bar app** (Tauri 2) – menu, global shortcuts (with keyboard-layout check),
  settings window, autostart, installers.
- **Software KVM in PbP** – Deskflow started only while PiP/PbP is on (server) / always (client).
- **Edge transition effect** – smoke, glow and particles; Windows overlay tested, macOS overlay
  built in CI.
- **Streaming** – Moonlight profiles, Sunshine on/off (Windows service, `brew services`).

## Next
- More monitor presets (please send [monitor reports](https://github.com/marianmolnar/monswitch/issues/new?template=monitor-report.yml)).
- Hand-drawn smoke textures for the `smoke` style (e.g. Kenney's CC0 *Smoke Particles*).
- Monitors that switch inputs only with vendor-specific commands.
- Presets selectable in the settings window.

## Later
- Signed and notarized macOS app.
- KVM over elevated windows on Windows (deskflow-core runs without admin rights, so the cursor
  may not cross over windows started as administrator or the UAC prompt).
- Linux (DDC works through ddc-hi; tray, KVM and overlay are untested).
