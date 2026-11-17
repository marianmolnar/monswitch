use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The default config file. All defaults live here, not in code.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../default_config.toml");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub monitor: MonitorConfig,
    pub ddc: DdcConfig,
    pub vcp: VcpConfig,
    /// Alias -> value for `vcp.input_source`.
    pub inputs: BTreeMap<String, u16>,
    pub probe: ProbeConfig,
    #[serde(default)]
    pub pbp: Option<PbpConfig>,
    #[serde(default)]
    pub tray: TrayConfig,
    /// Tray menu entries / hotkeys.
    #[serde(default)]
    pub machines: Vec<Machine>,
    #[serde(default)]
    pub kvm: KvmConfig,
    #[serde(default)]
    pub stream: StreamConfig,
    /// Moonlight stream profiles (tray menu entries).
    #[serde(default)]
    pub streams: Vec<StreamProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StreamConfig {
    /// Moonlight executable; empty = default install location.
    #[serde(default)]
    pub moonlight: String,
    /// Show the `[[streams]]` entries in the tray menu (Moonlight client).
    pub enabled: bool,
    /// Start Sunshine (this machine as stream host) with monswitch and stop
    /// it again when monswitch quits.
    #[serde(default)]
    pub sunshine_autostart: bool,
    /// Sunshine executable (macOS/Linux); empty = default install location.
    /// Windows always uses the SunshineService service.
    #[serde(default)]
    pub sunshine: String,
    /// macOS: Homebrew formula of Sunshine. A Homebrew-installed Sunshine is
    /// started/stopped with `brew services` so it keeps its own Screen
    /// Recording permission. Empty = start the executable directly.
    #[serde(default)]
    pub sunshine_formula: String,
}

impl Default for StreamConfig {
    /// Only a fallback for configs without [stream]; real defaults live in
    /// default_config.toml.
    fn default() -> Self {
        Self {
            moonlight: String::new(),
            enabled: true,
            sunshine_autostart: false,
            sunshine: String::new(),
            sunshine_formula: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StreamProfile {
    pub name: String,
    /// Sunshine host as paired in Moonlight (name, IP or UUID).
    #[serde(default)]
    pub host: String,
    /// Sunshine app name ("Desktop" exists by default).
    pub app: String,
    /// Extra Moonlight options, e.g. ["--display-mode", "windowed"].
    #[serde(default)]
    pub args: Vec<String>,
    /// Show only on this OS ("windows", "macos"); empty = everywhere.
    #[serde(default)]
    pub only_on: String,
}

impl StreamProfile {
    pub fn visible_on(&self, os: &str) -> bool {
        self.only_on.trim().is_empty() || self.only_on.trim().eq_ignore_ascii_case(os)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KvmRole {
    #[default]
    Off,
    /// Gets the monitor's USB keyboard/mouse in PbP; runs only while PbP is on.
    Server,
    /// Runs all the time, waits for the server.
    Client,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct KvmConfig {
    #[serde(default)]
    pub role: KvmRole,
    /// Path to deskflow-core; empty = default install location.
    #[serde(default)]
    pub deskflow_core: String,
    /// Deskflow settings file (`--settings`); empty = Deskflow's default.
    #[serde(default)]
    pub settings: String,
    /// How often the server polls the PbP state.
    pub poll_ms: u64,
    /// Wait before restarting an exited deskflow-core.
    pub restart_delay_ms: u64,
    /// This machine's Deskflow screen name; empty = from Deskflow's settings.
    #[serde(default)]
    pub screen_name: String,
    #[serde(default)]
    pub effect: EffectConfig,
}

impl Default for KvmConfig {
    /// Only a fallback for configs without [kvm]; real defaults live in
    /// default_config.toml.
    fn default() -> Self {
        Self {
            role: KvmRole::Off,
            deskflow_core: String::new(),
            settings: String::new(),
            poll_ms: 3000,
            restart_delay_ms: 5000,
            screen_name: String::new(),
            effect: EffectConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectStyle {
    /// Puff of mist at the crossing point + light along the screen edge.
    Smoke,
    Glow,
    Particles,
    /// A portal tears open on the edge: swirling energy, chromatic rim,
    /// light streaks pulled in (exit) or pouring out (entry), shock wave.
    #[default]
    Rift,
}

/// `[kvm.effect]` – burst at the screen edge when the cursor crosses to the other machine.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct EffectConfig {
    pub enabled: bool,
    pub style: EffectStyle,
    pub duration_ms: u32,
    /// "#RRGGBB"
    pub color: String,
    /// UDP port used between the two machines.
    pub port: u16,
    /// Other machine's address; empty = learned automatically.
    pub peer: String,
}

impl Default for EffectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            style: EffectStyle::Rift,
            duration_ms: 700,
            color: "#F2F6FF".into(),
            port: 24810,
            peer: String::new(),
        }
    }
}

impl EffectConfig {
    /// `color` as (r, g, b).
    pub fn rgb(&self) -> Option<(u8, u8, u8)> {
        let h = self.color.trim().strip_prefix('#')?;
        if h.len() != 6 || !h.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        Some((byte(0)?, byte(2)?, byte(4)?))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct TrayConfig {
    #[serde(default)]
    pub autostart: bool,
    /// Empty = no hotkey.
    #[serde(default)]
    pub pbp_hotkey: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Machine {
    pub name: String,
    /// Alias from `[inputs]`.
    pub input: String,
    /// Global hotkey, e.g. "Ctrl+Alt+1". Empty = none.
    #[serde(default)]
    pub hotkey: String,
}

/// The part of the config the tray settings window edits.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TraySettings {
    pub inputs: BTreeMap<String, u16>,
    pub machines: Vec<Machine>,
    pub tray: TrayConfig,
    pub kvm: KvmConfig,
    pub stream: StreamConfig,
    pub streams: Vec<StreamProfile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MonitorConfig {
    pub model: String,
    #[serde(default)]
    pub backend: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DdcConfig {
    pub timeout_ms: u64,
    pub caps_timeout_ms: u64,
    pub enumerate_timeout_ms: u64,
    pub retries: u32,
    pub min_delay_ms: u64,
    /// How long to keep looking for a monitor that vanished (PbP toggle).
    pub rediscover_timeout_ms: u64,
    pub rediscover_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VcpConfig {
    pub input_source: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProbeConfig {
    pub range_start: u8,
    pub range_end: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PbpConfig {
    /// VCP code of the PiP/PbP mode.
    pub mode_code: u8,
    pub off: u16,
    pub pip: u16,
    pub pbp: u16,
    /// VCP code of the second-window source.
    pub source_code: u8,
    /// Source value = input value << source_shift.
    pub source_shift: u8,
}

impl PbpConfig {
    pub fn source_value(&self, input: u16) -> u16 {
        input.checked_shl(u32::from(self.source_shift)).unwrap_or(0)
    }

    pub fn source_input(&self, value: u16) -> u16 {
        value.checked_shr(u32::from(self.source_shift)).unwrap_or(0)
    }

    /// Any value other than off/pbp is a PiP variant (position).
    pub fn mode_name(&self, value: u16) -> String {
        if value == self.off {
            "off".into()
        } else if value == self.pbp {
            "pbp".into()
        } else {
            format!("pip (position {value})")
        }
    }
}

/// Built-in monitor presets: (name, TOML with [monitor], [inputs], [pbp] …).
pub const PRESETS: &[(&str, &str)] = &[("asus-pg27ucdm", include_str!("../presets/asus-pg27ucdm.toml"))];

/// First comment line of a preset (its description).
pub fn preset_description(toml: &str) -> &str {
    toml.lines()
        .find_map(|l| l.strip_prefix('#'))
        .map(str::trim)
        .unwrap_or("")
}

pub fn preset(name: &str) -> Option<&'static str> {
    PRESETS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name.trim()))
        .map(|(_, t)| *t)
}

/// Writes a preset into the config file: `[monitor]` keys are merged, every
/// other table of the preset replaces the one in the file. Comments and the
/// remaining sections are kept.
pub fn apply_preset(path: &Path, name: &str) -> Result<()> {
    use toml_edit::{DocumentMut, Item};

    let preset_text = preset(name).ok_or_else(|| {
        let names: Vec<&str> = PRESETS.iter().map(|(n, _)| *n).collect();
        Error::Config(format!("unknown preset '{name}' (known: {})", names.join(", ")))
    })?;
    let parse = |t: &str, what: &str| -> Result<DocumentMut> {
        t.parse()
            .map_err(|e: toml_edit::TomlError| Error::Config(format!("{what}: {e}")))
    };
    let text = if path.exists() {
        fs::read_to_string(path).map_err(|e| Error::io(path, e))?
    } else {
        DEFAULT_CONFIG_TOML.to_string()
    };
    let mut doc = parse(&text, &path.display().to_string())?;
    let preset_doc = parse(preset_text, name)?;

    for (key, item) in preset_doc.iter() {
        match (key, item.as_table()) {
            ("monitor", Some(t)) => {
                if !doc.contains_table("monitor") {
                    doc.insert("monitor", Item::Table(toml_edit::Table::new()));
                }
                if let Some(m) = doc["monitor"].as_table_mut() {
                    for (k, v) in t.iter() {
                        m.insert(k, v.clone());
                    }
                }
            }
            _ => {
                doc.insert(key, item.clone());
            }
        }
    }
    let out = doc.to_string();
    Config::from_toml_str(&out).map_err(|e| Error::Config(format!("preset '{name}' gives an invalid config: {e}")))?;
    write_atomic(path, &out)
}

/// Writes `text` to `path` via a temporary file + rename.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).map_err(|e| Error::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
}

/// Top-level tables that the user file replaces wholesale instead of merging
/// key by key (so removing an alias actually removes it).
const REPLACE_TABLES: &[&str] = &["inputs", "pbp", "machines", "streams"];

impl Config {
    /// `%APPDATA%\monswitch\config.toml` on Windows,
    /// `~/.config/monswitch/config.toml` elsewhere.
    pub fn default_path() -> Result<PathBuf> {
        let base = if cfg!(windows) {
            dirs::config_dir()
        } else {
            dirs::home_dir().map(|h| h.join(".config"))
        };
        base.map(|b| b.join("monswitch").join("config.toml"))
            .ok_or_else(|| Error::Config("cannot determine config directory".into()))
    }

    pub fn defaults() -> Result<Self> {
        Self::from_toml_str("")
    }

    /// Parses `user` on top of the built-in defaults.
    pub fn from_toml_str(user: &str) -> Result<Self> {
        let mut base: toml::Table = toml::from_str(DEFAULT_CONFIG_TOML)
            .map_err(|e| Error::Config(format!("built-in defaults: {e}")))?;
        let over: toml::Table =
            toml::from_str(user).map_err(|e| Error::Config(e.to_string()))?;
        for (k, v) in over {
            if REPLACE_TABLES.contains(&k.as_str()) {
                base.insert(k, v);
            } else {
                match base.get_mut(&k) {
                    Some(b) => merge(b, v),
                    None => {
                        base.insert(k, v);
                    }
                }
            }
        }
        toml::Value::Table(base)
            .try_into()
            .map_err(|e: toml::de::Error| Error::Config(e.to_string()))
    }

    /// Loads the config; writes the default file first if it does not exist.
    /// Returns the config and whether the file was just created.
    pub fn load_or_init(path: &Path) -> Result<(Self, bool)> {
        let created = if path.exists() {
            false
        } else {
            write_default(path, false)?;
            true
        };
        let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let cfg = Self::from_toml_str(&text)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        Ok((cfg, created))
    }

    /// Case-insensitive alias lookup.
    pub fn input_value(&self, alias: &str) -> Option<u16> {
        self.inputs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(alias))
            .map(|(_, v)| *v)
    }

    /// Reverse lookup. Some monitors put junk in the high byte, so the low
    /// byte is compared as a fallback.
    pub fn input_alias(&self, value: u16) -> Option<&str> {
        self.inputs
            .iter()
            .find(|(_, v)| **v == value)
            .or_else(|| self.inputs.iter().find(|(_, v)| (**v & 0xFF) == (value & 0xFF)))
            .map(|(k, _)| k.as_str())
    }

    pub fn known_aliases(&self) -> String {
        self.inputs.keys().cloned().collect::<Vec<_>>().join(", ")
    }

    pub fn tray_settings(&self) -> TraySettings {
        TraySettings {
            inputs: self.inputs.clone(),
            machines: self.machines.clone(),
            tray: self.tray.clone(),
            kvm: self.kvm.clone(),
            stream: self.stream.clone(),
            streams: self.streams.clone(),
        }
    }
}

impl TraySettings {
    /// Checks aliases, machine inputs and hotkey uniqueness. Hotkey syntax
    /// is checked by the tray app (it owns the hotkey parser).
    pub fn validate(&self) -> Result<()> {
        if self.inputs.is_empty() {
            return Err(Error::Config("at least one input alias is required".into()));
        }
        for alias in self.inputs.keys() {
            let ok = !alias.is_empty()
                && alias
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !ok {
                return Err(Error::Config(format!(
                    "invalid alias '{alias}' (letters, digits, - and _ only)"
                )));
            }
        }
        let mut hotkeys: Vec<String> = Vec::new();
        for m in &self.machines {
            if m.name.trim().is_empty() {
                return Err(Error::Config("machine name must not be empty".into()));
            }
            if !self.inputs.keys().any(|k| k.eq_ignore_ascii_case(&m.input)) {
                return Err(Error::Config(format!(
                    "'{}' uses unknown input alias '{}'",
                    m.name, m.input
                )));
            }
            hotkeys.push(m.hotkey.trim().to_lowercase());
        }
        for s in &self.streams {
            if s.name.trim().is_empty() || s.app.trim().is_empty() {
                return Err(Error::Config("stream profile needs a name and an app".into()));
            }
            let os = s.only_on.trim();
            if !(os.is_empty() || os == "windows" || os == "macos" || os == "linux") {
                return Err(Error::Config(format!(
                    "stream '{}': only_on must be empty, windows, macos or linux",
                    s.name
                )));
            }
        }
        let fx = &self.kvm.effect;
        if fx.rgb().is_none() {
            return Err(Error::Config(format!("effect color '{}' is not #RRGGBB", fx.color)));
        }
        if !(50..=3000).contains(&fx.duration_ms) {
            return Err(Error::Config("effect duration must be 50–3000 ms".into()));
        }
        if fx.port == 0 {
            return Err(Error::Config("effect port must not be 0".into()));
        }
        hotkeys.push(self.tray.pbp_hotkey.trim().to_lowercase());
        hotkeys.retain(|h| !h.is_empty());
        let total = hotkeys.len();
        hotkeys.sort();
        hotkeys.dedup();
        if hotkeys.len() != total {
            return Err(Error::Config("the same hotkey is used twice".into()));
        }
        Ok(())
    }
}

/// Writes the tray settings into the config file, keeping comments and all
/// other sections. Creates the file from the defaults if it is missing.
pub fn save_tray_settings(path: &Path, s: &TraySettings) -> Result<()> {
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

    s.validate()?;
    let text = if path.exists() {
        fs::read_to_string(path).map_err(|e| Error::io(path, e))?
    } else {
        DEFAULT_CONFIG_TOML.to_string()
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| Error::Config(format!("{}: {e}", path.display())))?;

    let table = |doc: &mut DocumentMut, key: &str| -> Result<()> {
        if !doc.contains_table(key) {
            doc.insert(key, Item::Table(Table::new()));
        }
        Ok(())
    };

    // [inputs] – clear entries but keep the table's header comments.
    table(&mut doc, "inputs")?;
    let inputs = doc["inputs"]
        .as_table_mut()
        .ok_or_else(|| Error::Config("[inputs] is not a table".into()))?;
    inputs.clear();
    for (alias, v) in &s.inputs {
        let hex: toml_edit::Value = format!("0x{v:02X}")
            .parse()
            .map_err(|e: toml_edit::TomlError| Error::Config(e.to_string()))?;
        inputs.insert(&alias.to_lowercase(), Item::Value(hex));
    }

    // [tray]
    table(&mut doc, "tray")?;
    let tray = doc["tray"]
        .as_table_mut()
        .ok_or_else(|| Error::Config("[tray] is not a table".into()))?;
    tray.insert("autostart", value(s.tray.autostart));
    tray.insert("pbp_hotkey", value(s.tray.pbp_hotkey.trim()));

    // [kvm]
    table(&mut doc, "kvm")?;
    let kvm = doc["kvm"]
        .as_table_mut()
        .ok_or_else(|| Error::Config("[kvm] is not a table".into()))?;
    let role = match s.kvm.role {
        KvmRole::Off => "off",
        KvmRole::Server => "server",
        KvmRole::Client => "client",
    };
    kvm.insert("role", value(role));
    kvm.insert("deskflow_core", value(s.kvm.deskflow_core.trim()));
    kvm.insert("settings", value(s.kvm.settings.trim()));
    kvm.insert("poll_ms", value(i64::try_from(s.kvm.poll_ms).unwrap_or(i64::MAX)));
    kvm.insert(
        "restart_delay_ms",
        value(i64::try_from(s.kvm.restart_delay_ms).unwrap_or(i64::MAX)),
    );
    kvm.insert("screen_name", value(s.kvm.screen_name.trim()));

    // [kvm.effect]
    if !kvm.get("effect").is_some_and(Item::is_table) {
        kvm.insert("effect", Item::Table(Table::new()));
    }
    let fx = kvm["effect"]
        .as_table_mut()
        .ok_or_else(|| Error::Config("[kvm.effect] is not a table".into()))?;
    let e = &s.kvm.effect;
    fx.insert("enabled", value(e.enabled));
    fx.insert(
        "style",
        value(match e.style {
            EffectStyle::Smoke => "smoke",
            EffectStyle::Glow => "glow",
            EffectStyle::Particles => "particles",
            EffectStyle::Rift => "rift",
        }),
    );
    fx.insert("duration_ms", value(i64::from(e.duration_ms)));
    fx.insert("color", value(e.color.trim().to_uppercase()));
    fx.insert("port", value(i64::from(e.port)));
    fx.insert("peer", value(e.peer.trim()));

    // [[machines]]
    let mut machines = ArrayOfTables::new();
    for m in &s.machines {
        let mut t = Table::new();
        t.insert("name", value(m.name.trim()));
        t.insert("input", value(m.input.to_lowercase()));
        t.insert("hotkey", value(m.hotkey.trim()));
        machines.push(t);
    }
    doc.insert("machines", Item::ArrayOfTables(machines));

    // [stream] + [[streams]]
    table(&mut doc, "stream")?;
    let stream = doc["stream"]
        .as_table_mut()
        .ok_or_else(|| Error::Config("[stream] is not a table".into()))?;
    stream.insert("moonlight", value(s.stream.moonlight.trim()));
    stream.insert("enabled", value(s.stream.enabled));
    stream.insert("sunshine_autostart", value(s.stream.sunshine_autostart));
    stream.insert("sunshine", value(s.stream.sunshine.trim()));
    stream.insert("sunshine_formula", value(s.stream.sunshine_formula.trim()));

    let mut streams = ArrayOfTables::new();
    for p in &s.streams {
        let mut t = Table::new();
        t.insert("name", value(p.name.trim()));
        t.insert("host", value(p.host.trim()));
        t.insert("app", value(p.app.trim()));
        let mut args = toml_edit::Array::new();
        for a in p.args.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
            args.push(a);
        }
        t.insert("args", value(args));
        t.insert("only_on", value(p.only_on.trim()));
        streams.push(t);
    }
    doc.insert("streams", Item::ArrayOfTables(streams));

    let out = doc.to_string();
    // Never write a file we could not load back.
    Config::from_toml_str(&out)?;

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, out).map_err(|e| Error::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
}

/// Writes the default config file. Refuses to overwrite unless `force`.
pub fn write_default(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        return Err(Error::Config(format!(
            "{} already exists (use --force to overwrite)",
            path.display()
        )));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    fs::write(path, DEFAULT_CONFIG_TOML).map_err(|e| Error::io(path, e))
}

fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) => merge(bv, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let c = Config::defaults().unwrap();
        assert_eq!(c.monitor.backend, "winapi");
        assert_eq!(c.vcp.input_source, 0x60);
        assert_eq!(c.input_value("DP1"), Some(0x0F));
        assert_eq!(c.input_alias(0x12), Some("hdmi2"));

        let c = Config::from_toml_str(preset("asus-pg27ucdm").unwrap()).unwrap();
        assert_eq!(c.input_alias(0x1A), Some("usbc"));
        let p = c.pbp.as_ref().unwrap();
        assert_eq!((p.mode_code, p.off, p.pip, p.pbp), (0xF4, 0, 4, 5));
        assert_eq!(p.source_value(0x1A), 0x1A00);
        assert_eq!(p.source_input(0x1100), 0x11);
        assert_eq!(p.mode_name(5), "pbp");
        assert_eq!(p.mode_name(4), "pip (position 4)");
    }

    #[test]
    fn user_overrides_merge_and_inputs_replace() {
        let c = Config::from_toml_str(
            "[ddc]\ntimeout_ms = 500\n[inputs]\ndp = 0x10\n[pbp]\nmode_code = 0xE5\noff = 0\npip = 1\npbp = 2\nsource_code = 0xE6\nsource_shift = 0\n",
        )
        .unwrap();
        assert_eq!(c.ddc.timeout_ms, 500);
        assert_eq!(c.ddc.min_delay_ms, 50); // kept from defaults
        assert_eq!(c.inputs.len(), 1);
        assert_eq!(c.input_value("dp"), Some(0x10));
        assert_eq!(c.pbp.as_ref().map(|p| p.mode_code), Some(0xE5));
    }

    fn temp_config(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("monswitch-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn defaults_have_tray_machines() {
        let c = Config::defaults().unwrap();
        assert_eq!(c.machines.len(), 2);
        assert_eq!(c.machines[0].input, "dp1");
        assert_eq!(c.machines[1].hotkey, "Ctrl+Alt+Shift+2");
        assert!(!c.tray.autostart);
        assert_eq!(c.kvm.role, KvmRole::Off);
        assert_eq!(c.kvm.poll_ms, 3000);
        assert!(c.kvm.effect.enabled);
        assert_eq!(c.kvm.effect.duration_ms, 700);
        assert_eq!(c.kvm.effect.style, EffectStyle::Rift);
        assert!(c.stream.enabled);
        assert!(!c.stream.sunshine_autostart);
        assert_eq!(c.stream.sunshine_formula, "lizardbyte/homebrew/sunshine");
        c.tray_settings().validate().unwrap();
    }

    #[test]
    fn presets_parse_and_apply() {
        for (name, text) in PRESETS {
            assert!(!preset_description(text).is_empty(), "{name} has no description");
            Config::from_toml_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let p = temp_config("preset", "# keep me\n[ddc]\ntimeout_ms = 1234\n[monitor]\nbackend = \"nvapi\"\n");
        apply_preset(&p, "ASUS-PG27UCDM").unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("# keep me"));
        let c = Config::from_toml_str(&text).unwrap();
        assert_eq!(c.monitor.model, "PG27UCDM");
        assert_eq!(c.monitor.backend, "nvapi");
        assert_eq!(c.ddc.timeout_ms, 1234);
        assert_eq!(c.input_value("usbc"), Some(0x1A));
        assert_eq!(c.pbp.as_ref().map(|p| p.mode_code), Some(0xF4));
        assert!(apply_preset(&p, "nope").is_err());
        fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn generic_defaults_have_no_pbp() {
        let c = Config::defaults().unwrap();
        assert!(c.pbp.is_none());
        assert_eq!(c.monitor.model, "");
        assert_eq!(c.input_value("hdmi1"), Some(0x11));
    }

    #[test]
    fn effect_color_and_validation() {
        let mut fx = EffectConfig { color: "#38BDF8".into(), ..EffectConfig::default() };
        assert_eq!(fx.rgb(), Some((0x38, 0xBD, 0xF8)));
        fx.color = "38BDF8".into();
        assert_eq!(fx.rgb(), None);
        let mut s = Config::defaults().unwrap().tray_settings();
        s.kvm.effect.color = "#12345".into();
        assert!(s.validate().is_err());
        s.kvm.effect.color = "#123456".into();
        s.kvm.effect.duration_ms = 10;
        assert!(s.validate().is_err());
    }

    #[test]
    fn save_tray_settings_keeps_comments_and_other_sections() {
        let p = temp_config(
            "save",
            "# my comment\n[ddc]\ntimeout_ms = 1234\n\n# aliases\n[inputs]\ndp1 = 0x0F # inline\nusbc = 0x1A\n",
        );
        let mut s = Config::from_toml_str(&fs::read_to_string(&p).unwrap()).unwrap().tray_settings();
        s.inputs.insert("hdmi1".into(), 0x11);
        s.tray.autostart = true;
        s.machines[1].hotkey = "Ctrl+Alt+9".into();
        s.kvm.role = KvmRole::Server;
        s.kvm.settings = "C:/x/Deskflow.conf".into();
        s.kvm.effect.style = EffectStyle::Particles;
        s.kvm.effect.duration_ms = 500;
        s.kvm.effect.color = "#FF00AA".into();
        s.stream.enabled = false;
        s.stream.sunshine_formula = String::new();
        save_tray_settings(&p, &s).unwrap();

        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("# my comment"));
        assert!(text.contains("# aliases"));
        assert!(text.contains("hdmi1 = 0x11"));
        let c = Config::from_toml_str(&text).unwrap();
        assert_eq!(c.ddc.timeout_ms, 1234);
        assert_eq!(c.tray_settings(), s);
        fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn validate_rejects_bad_settings() {
        let base = Config::defaults().unwrap().tray_settings();
        let mut s = base.clone();
        s.machines[0].input = "vga".into();
        assert!(s.validate().is_err());
        let mut s = base.clone();
        s.machines[1].hotkey = s.machines[0].hotkey.clone();
        assert!(s.validate().is_err());
        let mut s = base;
        s.inputs.insert("bad alias".into(), 1);
        assert!(s.validate().is_err());
    }

    #[test]
    fn alias_low_byte_fallback() {
        let c = Config::defaults().unwrap();
        assert_eq!(c.input_alias(0x0111), Some("hdmi1"));
    }
}
