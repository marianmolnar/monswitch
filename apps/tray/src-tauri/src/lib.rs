//! monswitch tray / menubar app.
//!
//! Menu: one entry per `[[machines]]` (switches the monitor input), PbP
//! toggle, settings, quit. The active input is read from the monitor when
//! the pointer enters the tray icon (debounced), after every action and when
//! the settings window asks. All DDC I/O runs on blocking threads through a
//! single `Controller`, which reconnects when the monitor briefly vanishes
//! (PbP toggles).

mod layout;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ddc_core::config::{self, Config, KvmRole, TraySettings};
use ddc_core::{Controller, Selector};
use effect::{Burst, Effects, Overlay};
use kvm::{CoreEvent, KvmState, Supervisor};
use stream::sunshine::{self, SunshineState};
use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{App, AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};

const TRAY_ID: &str = "monswitch";
const SETTINGS_LABEL: &str = "settings";
/// Hovering the tray icon re-reads the monitor at most this often.
const REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
enum Action {
    Machine(usize),
    PbpToggle,
}

struct MenuHandles {
    status: MenuItem<Wry>,
    kvm: MenuItem<Wry>,
    machines: Vec<CheckMenuItem<Wry>>,
    pbp: Option<CheckMenuItem<Wry>>,
    sunshine: Option<CheckMenuItem<Wry>>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct Status {
    /// Raw value of the input-source VCP.
    input: Option<u16>,
    input_alias: Option<String>,
    /// Index into `[[machines]]` of the active input (None while PiP/PbP is
    /// on: the monitor then reports the second window's source).
    machine: Option<usize>,
    pbp: Option<String>,
    pbp_on: bool,
    error: Option<String>,
    /// Human-readable software-KVM state.
    kvm: String,
    /// Sunshine running here; None = not installed.
    sunshine: Option<bool>,
}

struct AppState {
    config_path: PathBuf,
    config_error: Option<String>,
    config: Mutex<Config>,
    ctl: Mutex<Controller>,
    menu: Mutex<Option<MenuHandles>>,
    shortcuts: Mutex<Vec<(Shortcut, Action)>>,
    hotkey_errors: Mutex<Vec<String>>,
    status: Mutex<Status>,
    refreshing: AtomicBool,
    last_refresh: Mutex<Option<Instant>>,
    kvm: Mutex<Supervisor>,
    /// Edge transition effect (created in setup).
    effects: Mutex<Option<Effects>>,
    /// deskflow-core events; taken by the event thread in setup.
    core_events: Mutex<Option<Receiver<CoreEvent>>>,
}

// ---- edge transition effect ----------------------------------------------------

thread_local! {
    /// The overlay window. Lives on the main thread (required by AppKit).
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

/// Creates the overlay (main thread), the effect logic and the thread that
/// feeds it deskflow-core events.
fn setup_effects(app: &AppHandle, cfg: &Config) {
    match Overlay::new() {
        Ok(o) => OVERLAY.with(|cell| *cell.borrow_mut() = Some(o)),
        Err(e) => log::warn!("effect: {e}"),
    }
    let handle = app.clone();
    let show: effect::ShowFn = Arc::new(move |burst: Burst| {
        let r = handle.run_on_main_thread(move || {
            OVERLAY.with(|cell| {
                if let Some(o) = cell.borrow().as_ref() {
                    o.show(burst);
                }
            });
        });
        if let Err(e) = r {
            log::debug!("effect: {e}");
        }
    });
    let state = app.state::<AppState>();
    *lock(&state.effects) = Some(Effects::start(&cfg.kvm, show));

    let Some(rx) = lock(&state.core_events).take() else {
        return;
    };
    let handle = app.clone();
    let spawned = std::thread::Builder::new().name("kvm-events".into()).spawn(move || {
        for ev in rx {
            let state = handle.state::<AppState>();
            let effects = lock(&state.effects);
            if let Some(fx) = effects.as_ref() {
                fx.on_core_event(&ev);
            }
        }
    });
    if let Err(e) = spawned {
        log::warn!("effect: cannot start event thread: {e}");
    }
}

// ---- software KVM ------------------------------------------------------------

fn kvm_text(role: KvmRole, state: &KvmState) -> String {
    let role = match role {
        KvmRole::Off => return "KVM: off".into(),
        KvmRole::Server => "server",
        KvmRole::Client => "client",
    };
    match state {
        KvmState::Off => "KVM: off".into(),
        KvmState::Running { .. } => format!("KVM: running ({role})"),
        KvmState::Stopped if role == "server" => "KVM: waiting for PbP (server)".into(),
        KvmState::Stopped => format!("KVM: stopped ({role})"),
        KvmState::Failed(e) => format!("KVM: error – {e}"),
    }
}

/// Starts/stops deskflow-core for the given PbP state; returns the KVM text.
fn sync_kvm(state: &AppState, pbp_on: bool) -> String {
    let mut sup = lock(&state.kvm);
    sup.sync(pbp_on);
    let role = sup.config().role;
    kvm_text(role, &sup.state())
}

/// Background loop: the KVM server follows the PbP state (also when PbP is
/// switched from the other machine or the OSD); the client is kept alive.
fn kvm_loop(app: AppHandle) {
    loop {
        let state = app.state::<AppState>();
        let (role, poll) = {
            let cfg = lock(&state.config);
            (cfg.kvm.role, cfg.kvm.poll_ms.max(500))
        };
        match role {
            KvmRole::Server => {
                // read_status also syncs the KVM with the PbP state.
                let st = read_status(&state);
                let changed = {
                    let prev = lock(&state.status);
                    prev.pbp_on != st.pbp_on
                        || prev.machine != st.machine
                        || prev.kvm != st.kvm
                        || prev.error.is_some() != st.error.is_some()
                };
                *lock(&state.last_refresh) = Some(Instant::now());
                if changed {
                    apply_status(&app, &st);
                }
            }
            KvmRole::Client | KvmRole::Off => {
                let text = sync_kvm(&state, false);
                let changed = lock(&state.status).kvm != text;
                if changed {
                    let mut st = lock(&state.status).clone();
                    st.kvm = text;
                    apply_status(&app, &st);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(poll));
    }
}

/// Lock that survives a poisoned mutex (a panicked holder must not take
/// the whole tray down).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---- DDC -------------------------------------------------------------------

fn same_input(a: u16, b: u16) -> bool {
    a == b || (a & 0xFF) == (b & 0xFF)
}

fn read_status(state: &AppState) -> Status {
    let cfg = lock(&state.config).clone();
    let mut ctl = lock(&state.ctl);
    let mut st = Status::default();

    if let Some(p) = &cfg.pbp {
        match ctl.get_vcp(p.mode_code) {
            Ok(v) => {
                st.pbp = Some(p.mode_name(v.current));
                st.pbp_on = v.current != p.off;
            }
            Err(e) => st.error = Some(e.to_string()),
        }
    }
    let input = if st.error.is_none() {
        Some(ctl.get_vcp(cfg.vcp.input_source))
    } else {
        None
    };
    match input {
        None => {}
        Some(Ok(v)) => {
            st.input = Some(v.current);
            st.input_alias = cfg.input_alias(v.current).map(str::to_string);
            if !st.pbp_on {
                st.machine = cfg.machines.iter().position(|m| {
                    cfg.input_value(&m.input)
                        .is_some_and(|x| same_input(x, v.current))
                });
            }
        }
        Some(Err(e)) => st.error = Some(e.to_string()),
    }
    drop(ctl);
    // The monitor read can fail transiently; keep the KVM in its last state
    // then instead of stopping it.
    let pbp_on = if st.error.is_some() {
        lock(&state.status).pbp_on
    } else {
        st.pbp_on
    };
    st.kvm = sync_kvm(state, pbp_on);
    st.sunshine = sunshine_running(&cfg);
    st
}

// ---- Sunshine ------------------------------------------------------------------

fn sunshine_running(cfg: &Config) -> Option<bool> {
    match sunshine::state(&cfg.stream) {
        SunshineState::Running => Some(true),
        SunshineState::Stopped => Some(false),
        SunshineState::NotInstalled => None,
    }
}

/// Starts/stops Sunshine on a background thread, then refreshes the menu.
fn set_sunshine(app: &AppHandle, on: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cfg = lock(&state.config).clone();
        let result = if on { sunshine::start(&cfg.stream) } else { sunshine::stop(&cfg.stream) };
        if let Err(e) = &result {
            log::warn!("sunshine: {e}");
        }
        // the service needs a moment to reach RUNNING/STOPPED
        std::thread::sleep(Duration::from_millis(800));
        let mut st = lock(&state.status).clone();
        st.sunshine = sunshine_running(&cfg);
        st.error = result.err().map(|e| e.to_string());
        apply_status(&app, &st);
    });
}

fn perform(state: &AppState, action: Action) -> Result<(), String> {
    let cfg = lock(&state.config).clone();
    let mut ctl = lock(&state.ctl);
    match action {
        Action::Machine(i) => {
            let m = cfg
                .machines
                .get(i)
                .ok_or_else(|| format!("unknown computer #{i}"))?;
            let v = cfg
                .input_value(&m.input)
                .ok_or_else(|| format!("unknown input alias '{}'", m.input))?;
            log::info!("switching to {} ({}=0x{v:02X})", m.name, m.input);
            ctl.set_vcp(cfg.vcp.input_source, v)
                .map_err(|e| e.to_string())
        }
        Action::PbpToggle => {
            let p = cfg.pbp.as_ref().ok_or("the config has no [pbp] section (see `monswitch config preset`)")?;
            let cur = ctl.get_vcp(p.mode_code).map_err(|e| e.to_string())?.current;
            let next = if cur == p.off { p.pbp } else { p.off };
            log::info!("pbp {} -> {}", p.mode_name(cur), p.mode_name(next));
            ctl.set_vcp(p.mode_code, next).map_err(|e| e.to_string())
        }
    }
}

/// Runs an action on a blocking thread, then refreshes the menu.
fn run_action(app: &AppHandle, action: Action) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let result = perform(&state, action);
        let mut st = read_status(&state);
        if let Err(e) = result {
            log::warn!("{action:?} failed: {e}");
            st.error = Some(e);
        }
        *lock(&state.last_refresh) = Some(Instant::now());
        apply_status(&app, &st);
    });
}

/// Re-reads the monitor state in the background. Without `force` this is
/// debounced and skipped while another refresh runs.
fn refresh(app: &AppHandle, force: bool) {
    let state = app.state::<AppState>();
    if !force {
        if let Some(t) = *lock(&state.last_refresh) {
            if t.elapsed() < REFRESH_MIN_INTERVAL {
                return;
            }
        }
    }
    if state.refreshing.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let st = read_status(&state);
        *lock(&state.last_refresh) = Some(Instant::now());
        state.refreshing.store(false, Ordering::SeqCst);
        apply_status(&app, &st);
    });
}

// ---- menu / tray -----------------------------------------------------------

fn status_text(cfg: &Config, st: &Status) -> String {
    if let Some(e) = &st.error {
        let mut e = e.clone();
        if e.chars().count() > 70 {
            e = e.chars().take(70).collect::<String>() + "…";
        }
        return format!("Monitor not reachable: {e}");
    }
    if st.pbp_on {
        let src = st.input_alias.clone().unwrap_or_else(|| "?".into());
        return format!("{} (second window: {src})", st.pbp.as_deref().unwrap_or("pip/pbp").to_uppercase());
    }
    match st.machine.and_then(|i| cfg.machines.get(i)) {
        Some(m) => format!("Active: {}", m.name),
        None => match (&st.input_alias, st.input) {
            (Some(a), _) => format!("Active input: {a}"),
            (None, Some(v)) => format!("Active input: 0x{v:02X}"),
            (None, None) => "Loading…".into(),
        },
    }
}

fn apply_status(app: &AppHandle, st: &Status) {
    let state = app.state::<AppState>();
    let cfg = lock(&state.config).clone();
    let text = status_text(&cfg, st);
    log::debug!("status: {text} ({st:?})");
    if let Some(h) = lock(&state.menu).as_ref() {
        for (i, item) in h.machines.iter().enumerate() {
            let _ = item.set_checked(st.machine == Some(i));
        }
        if let Some(p) = &h.pbp {
            let _ = p.set_checked(st.pbp_on);
        }
        if let Some(s) = &h.sunshine {
            let _ = s.set_checked(st.sunshine == Some(true));
        }
        let _ = h.status.set_text(&text);
        let _ = h.kvm.set_text(if st.kvm.is_empty() { "KVM: …" } else { &st.kvm });
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(format!("monswitch – {text}")));
    }
    *lock(&state.status) = st.clone();
    let _ = app.emit("status", st);
}

/// "Ctrl+Alt+1" → "⌃⌥1" on macOS, unchanged elsewhere.
fn hotkey_label(hk: &str) -> String {
    let hk = hk.trim();
    if !cfg!(target_os = "macos") {
        return hk.to_string();
    }
    hk.split('+')
        .map(|p| match p.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "⌃".to_string(),
            "alt" | "option" => "⌥".to_string(),
            "shift" => "⇧".to_string(),
            "super" | "cmd" | "command" | "meta" => "⌘".to_string(),
            _ => p.trim().to_string(),
        })
        .collect()
}

fn item_label(name: &str, hotkey: &str) -> String {
    if hotkey.trim().is_empty() {
        name.to_string()
    } else {
        format!("{name}    {}", hotkey_label(hotkey))
    }
}

fn build_menu(app: &AppHandle, cfg: &Config) -> tauri::Result<(Menu<Wry>, MenuHandles)> {
    let status = MenuItem::with_id(app, "status", "Loading…", false, None::<&str>)?;
    let kvm = MenuItem::with_id(app, "kvm", "KVM: …", false, None::<&str>)?;
    let mut machines = Vec::new();
    for (i, m) in cfg.machines.iter().enumerate() {
        machines.push(CheckMenuItem::with_id(
            app,
            format!("machine:{i}"),
            item_label(&m.name, &m.hotkey),
            true,
            false,
            None::<&str>,
        )?);
    }
    let pbp = match &cfg.pbp {
        Some(_) => Some(CheckMenuItem::with_id(
            app,
            "pbp",
            item_label("PbP on/off", &cfg.tray.pbp_hotkey),
            true,
            false,
            None::<&str>,
        )?),
        None => None,
    };
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

    let menu = Menu::new(app)?;
    menu.append(&status)?;
    if cfg.kvm.role != KvmRole::Off {
        menu.append(&kvm)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    for m in &machines {
        menu.append(m)?;
    }
    if let Some(p) = &pbp {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        menu.append(p)?;
    }
    let profiles = if cfg.stream.enabled {
        stream::visible_profiles(&cfg.streams)
    } else {
        Vec::new()
    };
    if !profiles.is_empty() {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        for (i, p) in profiles {
            let has_host = !p.host.trim().is_empty();
            let label = if has_host {
                format!("Stream: {}", p.name)
            } else {
                format!("Stream: {} (set a host)", p.name)
            };
            menu.append(&MenuItem::with_id(
                app,
                format!("stream:{i}"),
                label,
                has_host,
                None::<&str>,
            )?)?;
        }
    }
    let sunshine = match sunshine_running(cfg) {
        Some(on) => Some(CheckMenuItem::with_id(
            app,
            "sunshine",
            "Sunshine – share this screen",
            true,
            on,
            None::<&str>,
        )?),
        None => None,
    };
    let moonlight = stream::find_moonlight(&cfg.stream)
        .map(|_| MenuItem::with_id(app, "moonlight", "Open Moonlight", true, None::<&str>))
        .transpose()?;
    if sunshine.is_some() || moonlight.is_some() {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        if let Some(s) = &sunshine {
            menu.append(s)?;
        }
        if let Some(m) = &moonlight {
            menu.append(m)?;
        }
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&settings)?;
    menu.append(&quit)?;
    Ok((menu, MenuHandles { status, kvm, machines, pbp, sunshine }))
}

/// Rebuilds the tray menu (machine names/hotkeys may have changed).
fn install_menu(app: &AppHandle) -> tauri::Result<()> {
    let state = app.state::<AppState>();
    let cfg = lock(&state.config).clone();
    let (menu, handles) = build_menu(app, &cfg)?;
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu))?;
    }
    *lock(&state.menu) = Some(handles);
    let st = lock(&state.status).clone();
    apply_status(app, &st);
    Ok(())
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id().as_ref();
    match id {
        "quit" => app.exit(0),
        "settings" => open_settings(app),
        "pbp" => run_action(app, Action::PbpToggle),
        "sunshine" => {
            // the check mark already flipped; act on the state we knew
            let was_on = lock(&app.state::<AppState>().status).sunshine == Some(true);
            set_sunshine(app, !was_on);
        }
        "moonlight" => {
            let cfg = lock(&app.state::<AppState>().config).clone();
            if let Err(e) = stream::open_moonlight(&cfg.stream) {
                log::warn!("moonlight: {e}");
            }
        }
        _ => {
            if let Some(i) = id.strip_prefix("machine:").and_then(|s| s.parse().ok()) {
                run_action(app, Action::Machine(i));
            } else if let Some(i) = id.strip_prefix("stream:").and_then(|s| s.parse().ok()) {
                launch_stream(app, i);
            }
        }
    }
}

/// Starts Moonlight for `[[streams]]` entry `index`; errors go to the status.
/// Off the calling (UI) thread: `--resolution native` runs system_profiler.
fn launch_stream(app: &AppHandle, index: usize) {
    let app = app.clone();
    let spawned = std::thread::Builder::new().name("stream-launch".into()).spawn(move || {
        let state = app.state::<AppState>();
        let cfg = lock(&state.config).clone();
        let result = match cfg.streams.get(index) {
            Some(p) => stream::launch(&cfg.stream, p).map_err(|e| e.to_string()),
            None => Err(format!("unknown stream #{index}")),
        };
        if let Err(e) = result {
            log::warn!("stream: {e}");
            let mut st = lock(&state.status).clone();
            st.error = Some(e);
            apply_status(&app, &st);
        }
    });
    if let Err(e) = spawned {
        log::warn!("stream: {e}");
    }
}

fn tray_icon() -> Option<Image<'static>> {
    // macOS: monochrome template image (adapts to light/dark menubar).
    // Windows: the colored app icon.
    let bytes: &[u8] = if cfg!(target_os = "macos") {
        include_bytes!("../icons/tray-template.png")
    } else {
        include_bytes!("../icons/32x32.png")
    };
    match Image::from_bytes(bytes) {
        Ok(i) => Some(i),
        Err(e) => {
            log::error!("tray icon: {e}");
            None
        }
    }
}

fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, SETTINGS_LABEL, WebviewUrl::App("index.html".into()))
        .title("monswitch – settings")
        .inner_size(640.0, 760.0)
        .min_inner_size(480.0, 480.0)
        .center()
        .build();
    match built {
        Ok(w) => {
            let _ = w.set_focus();
        }
        Err(e) => log::error!("cannot open settings window: {e}"),
    }
}

// ---- hotkeys / autostart -----------------------------------------------------

fn apply_hotkeys(app: &AppHandle, cfg: &Config) {
    let gs = app.global_shortcut();
    if let Err(e) = gs.unregister_all() {
        log::warn!("unregister hotkeys: {e}");
    }
    let mut map = Vec::new();
    let mut errors = Vec::new();
    let mut add = |hotkey: &str, action: Action, what: &str| {
        let hotkey = hotkey.trim();
        if hotkey.is_empty() {
            return;
        }
        let res = hotkey
            .parse::<Shortcut>()
            .map_err(|e| e.to_string())
            .and_then(|s| gs.register(s).map(|_| s).map_err(|e| e.to_string()));
        match res {
            Ok(s) => map.push((s, action)),
            Err(e) => {
                log::warn!("hotkey {what} ({hotkey}): {e}");
                errors.push(format!("{what} ({hotkey}): {e}"));
            }
        }
    };
    for (i, m) in cfg.machines.iter().enumerate() {
        add(&m.hotkey, Action::Machine(i), &m.name);
    }
    if cfg.pbp.is_some() {
        add(&cfg.tray.pbp_hotkey, Action::PbpToggle, "PbP");
    }
    let state = app.state::<AppState>();
    *lock(&state.shortcuts) = map;
    *lock(&state.hotkey_errors) = errors;
}

fn on_shortcut(app: &AppHandle, shortcut: &Shortcut, event: ShortcutEvent) {
    if event.state() != ShortcutState::Pressed {
        return;
    }
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let action = lock(&state.shortcuts)
        .iter()
        .find(|(s, _)| s == shortcut)
        .map(|(_, a)| *a);
    if let Some(a) = action {
        run_action(app, a);
    }
}

fn apply_autostart(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let al = app.autolaunch();
    let current = al.is_enabled().unwrap_or(false);
    if enabled {
        // Always (re)register: the entry stores the exe path, which changes when moving
        // from target/release to the installed app.
        al.enable().map_err(|e| e.to_string())?;
    } else if current {
        al.disable().map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---- commands (settings window) ----------------------------------------------

#[derive(Serialize)]
struct SettingsPayload {
    settings: TraySettings,
    config_path: String,
    config_error: Option<String>,
    has_pbp: bool,
    platform: &'static str,
    hotkey_errors: Vec<String>,
    status: Status,
    /// deskflow-core that would be used (configured or auto-detected).
    kvm_core: Option<String>,
    /// Moonlight that would be used (configured or auto-detected).
    moonlight: Option<String>,
}

fn payload(state: &AppState) -> SettingsPayload {
    let cfg = lock(&state.config).clone();
    SettingsPayload {
        moonlight: stream::find_moonlight(&cfg.stream).map(|p| p.display().to_string()),
        kvm_core: kvm::find_core(&cfg.kvm).map(|p| p.display().to_string()),
        settings: cfg.tray_settings(),
        config_path: state.config_path.display().to_string(),
        config_error: state.config_error.clone(),
        has_pbp: cfg.pbp.is_some(),
        platform: std::env::consts::OS,
        hotkey_errors: lock(&state.hotkey_errors).clone(),
        status: lock(&state.status).clone(),
    }
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> SettingsPayload {
    payload(&state)
}

#[tauri::command]
async fn save_settings(app: AppHandle, settings: TraySettings) -> Result<SettingsPayload, String> {
    let hotkeys = settings
        .machines
        .iter()
        .map(|m| m.hotkey.as_str())
        .chain(std::iter::once(settings.tray.pbp_hotkey.as_str()))
        .map(str::trim)
        .filter(|h| !h.is_empty());
    for hk in hotkeys {
        hk.parse::<Shortcut>()
            .map_err(|e| format!("invalid shortcut '{hk}': {e}"))?;
    }

    let state = app.state::<AppState>();
    let had_sunshine_autostart = lock(&state.config).stream.sunshine_autostart;
    config::save_tray_settings(&state.config_path, &settings).map_err(|e| e.to_string())?;
    if settings.stream.sunshine_autostart && !had_sunshine_autostart {
        set_sunshine(&app, true);
    }
    let (cfg, _) = Config::load_or_init(&state.config_path).map_err(|e| e.to_string())?;
    *lock(&state.config) = cfg.clone();
    lock(&state.kvm).set_config(cfg.kvm.clone());
    if let Some(fx) = lock(&state.effects).as_ref() {
        fx.set_config(&cfg.kvm);
    }

    apply_hotkeys(&app, &cfg);
    install_menu(&app).map_err(|e| e.to_string())?;
    if let Err(e) = apply_autostart(&app, cfg.tray.autostart) {
        lock(&state.hotkey_errors).push(format!("autostart: {e}"));
    }
    refresh(&app, true);
    Ok(payload(&state))
}

#[tauri::command]
async fn get_status(app: AppHandle) -> Result<Status, String> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        let st = read_status(&state);
        *lock(&state.last_refresh) = Some(Instant::now());
        apply_status(&handle, &st);
        st
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn switch_machine(app: AppHandle, index: usize) {
    run_action(&app, Action::Machine(index));
}

#[tauri::command]
fn toggle_pbp(app: AppHandle) {
    run_action(&app, Action::PbpToggle);
}

#[tauri::command]
fn start_stream(app: AppHandle, index: usize) {
    launch_stream(&app, index);
}

#[derive(Serialize)]
struct SunshineInfo {
    /// "running" | "stopped" | "missing"
    state: &'static str,
    /// Windows: monswitch may start/stop the service without admin.
    has_control: bool,
}

fn sunshine_info(cfg: &Config) -> SunshineInfo {
    SunshineInfo {
        state: match sunshine::state(&cfg.stream) {
            SunshineState::Running => "running",
            SunshineState::Stopped => "stopped",
            SunshineState::NotInstalled => "missing",
        },
        has_control: sunshine::has_control(),
    }
}

#[tauri::command]
async fn sunshine_status(app: AppHandle) -> Result<SunshineInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = lock(&app.state::<AppState>().config).clone();
        sunshine_info(&cfg)
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn sunshine_set(app: AppHandle, on: bool) -> Result<SunshineInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cfg = lock(&state.config).clone();
        let r = if on { sunshine::start(&cfg.stream) } else { sunshine::stop(&cfg.stream) };
        std::thread::sleep(Duration::from_millis(800));
        let mut st = lock(&state.status).clone();
        st.sunshine = sunshine_running(&cfg);
        apply_status(&app, &st);
        r.map(|_| sunshine_info(&cfg)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Windows: one UAC prompt so monswitch may start/stop SunshineService.
#[tauri::command]
async fn sunshine_grant(app: AppHandle) -> Result<SunshineInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = lock(&app.state::<AppState>().config).clone();
        sunshine::grant_control().map(|_| sunshine_info(&cfg)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Character the shortcut would type on the current keyboard layout (Ctrl+Alt = AltGr).
#[tauri::command]
fn hotkey_conflict(hotkey: String) -> Option<String> {
    layout::typed_char(&hotkey)
}

// ---- startup -------------------------------------------------------------------

fn setup(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    let handle = app.handle().clone();
    let cfg = lock(&app.state::<AppState>().config).clone();

    let (menu, handles) = build_menu(&handle, &cfg)?;
    *lock(&app.state::<AppState>().menu) = Some(handles);

    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("monswitch")
        .icon_as_template(cfg!(target_os = "macos"))
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(|tray, event| {
            if matches!(event, TrayIconEvent::Enter { .. } | TrayIconEvent::Click { .. }) {
                refresh(tray.app_handle(), false);
            }
        });
    if let Some(icon) = tray_icon() {
        tray = tray.icon(icon);
    }
    tray.build(app)?;

    apply_hotkeys(&handle, &cfg);
    if let Err(e) = apply_autostart(&handle, cfg.tray.autostart) {
        log::warn!("autostart: {e}");
    }
    refresh(&handle, true);
    setup_effects(&handle, &cfg);
    if cfg.stream.sunshine_autostart {
        set_sunshine(&handle, true);
    }

    let loop_handle = handle.clone();
    std::thread::Builder::new()
        .name("kvm-supervisor".into())
        .spawn(move || kvm_loop(loop_handle))?;
    Ok(())
}

pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config_path = Config::default_path().unwrap_or_else(|e| {
        log::error!("{e}; using ./config.toml");
        PathBuf::from("config.toml")
    });
    let (cfg, config_error) = match Config::load_or_init(&config_path) {
        Ok((c, _)) => (c, None),
        Err(e) => {
            log::error!("config: {e}; using built-in defaults");
            match Config::defaults() {
                Ok(c) => (c, Some(e.to_string())),
                Err(d) => {
                    eprintln!("fatal: built-in config is invalid: {d}");
                    std::process::exit(1);
                }
            }
        }
    };

    let (core_tx, core_rx) = mpsc::channel();
    let state = AppState {
        kvm: Mutex::new(Supervisor::new(cfg.kvm.clone()).with_events(core_tx)),
        effects: Mutex::new(None),
        core_events: Mutex::new(Some(core_rx)),
        config_path,
        config_error,
        ctl: Mutex::new(Controller::new(cfg.clone(), Selector::Model(cfg.monitor.model.clone()))),
        config: Mutex::new(cfg),
        menu: Mutex::new(None),
        shortcuts: Mutex::new(Vec::new()),
        hotkey_errors: Mutex::new(Vec::new()),
        status: Mutex::new(Status::default()),
        refreshing: AtomicBool::new(false),
        last_refresh: Mutex::new(None),
    };

    let app = tauri::Builder::default()
        // Must be first: a second launch just opens the settings window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| open_settings(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(on_shortcut)
                .build(),
        )
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_status,
            switch_machine,
            toggle_pbp,
            start_stream,
            hotkey_conflict,
            sunshine_status,
            sunshine_set,
            sunshine_grant
        ])
        .setup(setup)
        .build(tauri::generate_context!());

    match app {
        Ok(app) => app.run(|app, event| match event {
            // Closing the settings window must not quit the tray app;
            // only "Quit" (explicit exit code) does.
            RunEvent::ExitRequested { api, code, .. } if code.is_none() => api.prevent_exit(),
            RunEvent::Exit => {
                let state = app.state::<AppState>();
                // Never leave a deskflow-core running behind us.
                lock(&state.kvm).stop();
                // Sunshine started with monswitch stops with it.
                let cfg = lock(&state.config).clone();
                if cfg.stream.sunshine_autostart {
                    if let Err(e) = sunshine::stop(&cfg.stream) {
                        log::warn!("sunshine: {e}");
                    }
                }
            }
            _ => {}
        }),
        Err(e) => {
            log::error!("cannot start: {e}");
            eprintln!("monswitch-tray: {e}");
            std::process::exit(1);
        }
    }
}
