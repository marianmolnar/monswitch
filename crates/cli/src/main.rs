use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use ddc_core::caps::feature_name;
use ddc_core::config::{self, Config};
use ddc_core::probe::{self, Entry};
use ddc_core::{connect_monitor, connect_session, Capabilities, DdcSession, DisplayControl, Selector};

#[derive(Parser)]
#[command(name = "monswitch", version, about = "Monitor control over DDC/CI")]
struct Cli {
    /// Config file (default: %APPDATA%\monswitch\config.toml or ~/.config/monswitch/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Monitor: index from `list` or model substring (default: [monitor].model from config)
    #[arg(short, long, global = true)]
    monitor: Option<String>,

    /// More logging (-v, -vv)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List monitors, models and current input
    List,
    /// Show the parsed MCCS capabilities string
    Caps {
        /// Print the raw string only
        #[arg(long)]
        raw: bool,
    },
    /// Read VCP features (codes in hex: 60 or 0x60; several allowed)
    Get {
        #[arg(value_parser = parse_code, required = true, num_args = 1..)]
        codes: Vec<u8>,
    },
    /// Write a VCP feature (code in hex; value decimal, or hex with 0x prefix)
    Set {
        #[arg(value_parser = parse_code)]
        code: u8,
        #[arg(value_parser = parse_value)]
        value: u16,
    },
    /// Switch input by alias from config (dp, hdmi1, …) or raw value (0x0F); no argument = show current
    Input { name: Option<String> },
    /// Dump VCP values to JSON (to find vendor codes, e.g. PbP), or diff two dumps
    Probe {
        /// Compare two snapshots instead of probing
        #[arg(long, num_args = 2, value_names = ["A", "B"])]
        diff: Option<Vec<PathBuf>>,
        /// Write JSON here instead of stdout (recommended in PowerShell)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Also read every code listed in the capabilities string
        #[arg(long)]
        all_caps: bool,
        /// First code of the probed range (hex; default [probe].range_start)
        #[arg(long, value_parser = parse_code)]
        from: Option<u8>,
        /// Last code of the probed range (hex; default [probe].range_end)
        #[arg(long, value_parser = parse_code)]
        to: Option<u8>,
        /// Interactively switch through every input value from capabilities, then restore
        #[arg(long)]
        cycle_inputs: bool,
    },
    /// PiP/PbP: show state, or on/off/pip/toggle/source (codes from [pbp] in config)
    Pbp {
        #[command(subcommand)]
        action: Option<PbpCmd>,
    },
    /// Config file helpers
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
    /// File transfer to/from the other machine (TLS with Deskflow's pairing)
    Xfer {
        #[command(subcommand)]
        action: XferCmd,
    },
}

#[derive(Subcommand)]
enum XferCmd {
    /// Print this machine's certificate fingerprint and the trusted ones
    Fingerprint,
    /// Offer files/folders to the other machine and serve them until Ctrl+C
    Send {
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
        /// Other machine (host or host:port; default [xfer].peer)
        #[arg(long)]
        peer: Option<String>,
    },
    /// Wait for offers from the other machine and download them into DIR
    Receive {
        dir: PathBuf,
        /// Other machine (host or host:port; default [xfer].peer)
        #[arg(long)]
        peer: Option<String>,
    },
}

#[derive(Subcommand)]
enum PbpCmd {
    /// Side-by-side PbP
    On,
    /// Single picture
    Off,
    /// Picture in picture; optional position (1–4, default [pbp].pip)
    Pip { position: Option<u16> },
    /// PbP if off, otherwise off
    Toggle,
    /// Source of the second window: input alias (usbc, hdmi1, …) or raw input value
    Source { name: String },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the config path
    Path,
    /// Print the effective config
    Show,
    /// Write the default config file
    Init {
        #[arg(long)]
        force: bool,
    },
    /// List built-in monitor presets, or apply one to the config file
    Preset {
        /// Preset name (see `monswitch config preset`)
        name: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    env_logger::Builder::new()
        .filter_level(match cli.verbose {
            0 => log::LevelFilter::Warn,
            1 => log::LevelFilter::Info,
            _ => log::LevelFilter::Debug,
        })
        .format_timestamp_millis()
        .init();

    // Panics inside the DDC worker are caught and turned into per-operation
    // errors; don't spray the default panic message over the output.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some(ddc_core::WORKER_THREAD_NAME) {
            log::info!("DDC backend panic (handled): {info}");
        } else {
            default_hook(info);
        }
    }));

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let config_path = match &cli.config {
        Some(p) => p.clone(),
        None => Config::default_path()?,
    };

    // Commands that need no monitor access.
    match &cli.cmd {
        Cmd::Config { action } => return cmd_config(&config_path, action.as_ref()),
        Cmd::Xfer { action } => {
            let (cfg, _) = Config::load_or_init(&config_path)?;
            return cmd_xfer(&cfg, action);
        }
        Cmd::Probe { diff: Some(files), .. } => return cmd_diff(&files[0], &files[1]),
        _ => {}
    }

    let (cfg, created) = Config::load_or_init(&config_path)?;
    if created {
        eprintln!("created default config: {}", config_path.display());
    }
    let selector = match &cli.monitor {
        Some(s) => Selector::parse(s),
        None => Selector::Model(cfg.monitor.model.clone()),
    };
    match cli.cmd {
        Cmd::List => cmd_list(&cfg, &mut connect_session(&cfg)?, &selector),
        Cmd::Caps { raw } => {
            let mon = connect_monitor(&cfg, &selector)?;
            cmd_caps(&cfg, &mon, raw)
        }
        Cmd::Get { codes } => {
            let mon = connect_monitor(&cfg, &selector)?;
            let mut failed = 0;
            for code in codes.iter().copied() {
                match mon.get_vcp(code) {
                    Ok(v) => {
                        let alias = (code == cfg.vcp.input_source)
                            .then(|| cfg.input_alias(v.current))
                            .flatten()
                            .map(|a| format!("  [{a}]"))
                            .unwrap_or_default();
                        println!(
                            "VCP {}{}: current = {} ({}), max = {} ({}){}",
                            hex8(code),
                            name_suffix(code),
                            v.current,
                            hex(v.current),
                            v.maximum,
                            hex(v.maximum),
                            alias
                        );
                    }
                    Err(e) => {
                        failed += 1;
                        println!("VCP {}: error: {e}", hex8(code));
                    }
                }
            }
            if failed == codes.len() {
                bail!("no VCP code could be read");
            }
            Ok(())
        }
        Cmd::Set { code, value } => {
            let mon = connect_monitor(&cfg, &selector)?;
            mon.set_vcp(code, value)?;
            println!("VCP {} <- {} ({})", hex8(code), value, hex(value));
            Ok(())
        }
        Cmd::Input { name } => {
            let mon = connect_monitor(&cfg, &selector)?;
            cmd_input(&cfg, &mon, name.as_deref())
        }
        Cmd::Probe {
            output,
            all_caps,
            cycle_inputs,
            from,
            to,
            ..
        } => {
            let range = (
                from.unwrap_or(cfg.probe.range_start),
                to.unwrap_or(cfg.probe.range_end),
            );
            if range.0 > range.1 {
                bail!("--from {} is above --to {}", hex8(range.0), hex8(range.1));
            }
            let mon = connect_monitor(&cfg, &selector)?;
            cmd_probe(&cfg, &mon, output, all_caps, cycle_inputs, range)
        }
        Cmd::Pbp { action } => {
            let mon = connect_monitor(&cfg, &selector)?;
            cmd_pbp(&cfg, &mon, action)
        }
        Cmd::Config { .. } | Cmd::Xfer { .. } => Ok(()), // handled above
    }
}

fn cmd_pbp(cfg: &Config, mon: &impl DisplayControl, action: Option<PbpCmd>) -> Result<()> {
    let p = cfg
        .pbp
        .as_ref()
        .ok_or_else(|| anyhow!("no [pbp] section in config"))?;

    let print_state = || -> Result<()> {
        let mode = mon.get_vcp(p.mode_code)?.current;
        let src = mon.get_vcp(p.source_code)?.current;
        let input = p.source_input(src);
        println!("mode:   {}", p.mode_name(mode));
        println!("source: {} (second window)", format_input(cfg, input));
        Ok(())
    };

    let mode = match action {
        None => return print_state(),
        Some(PbpCmd::On) => p.pbp,
        Some(PbpCmd::Off) => p.off,
        Some(PbpCmd::Pip { position }) => {
            let pos = position.unwrap_or(p.pip);
            if pos == p.off || pos == p.pbp {
                bail!("PiP position {pos} collides with the off/pbp value in [pbp]");
            }
            pos
        }
        Some(PbpCmd::Toggle) => {
            if mon.get_vcp(p.mode_code)?.current == p.off {
                p.pbp
            } else {
                p.off
            }
        }
        Some(PbpCmd::Source { name }) => {
            let input = resolve_input(cfg, &name)?;
            mon.set_vcp(p.source_code, p.source_value(input))?;
            println!("second window source -> {}", format_input(cfg, input));
            return Ok(());
        }
    };
    mon.set_vcp(p.mode_code, mode)?;
    println!("pip/pbp -> {}", p.mode_name(mode));
    Ok(())
}

fn cmd_config(path: &Path, action: Option<&ConfigCmd>) -> Result<()> {
    match action {
        None | Some(ConfigCmd::Path) => println!("{}", path.display()),
        Some(ConfigCmd::Show) => {
            let (cfg, _) = Config::load_or_init(path)?;
            println!("# {}", path.display());
            println!("{cfg:#?}");
        }
        Some(ConfigCmd::Init { force }) => {
            config::write_default(path, *force)?;
            println!("wrote {}", path.display());
        }
        Some(ConfigCmd::Preset { name: None }) => {
            for (name, text) in config::PRESETS {
                println!("{name:<20} {}", config::preset_description(text));
            }
        }
        Some(ConfigCmd::Preset { name: Some(name) }) => {
            config::apply_preset(path, name)?;
            println!("applied preset '{name}' to {}", path.display());
        }
    }
    Ok(())
}

fn cmd_list(cfg: &Config, session: &mut DdcSession, sel: &Selector) -> Result<()> {
    // Resolving the selector may read capabilities to learn real model names.
    let selected = match session.find(sel) {
        Ok(i) => Some(i),
        Err(e) => {
            eprintln!("note: {e}");
            None
        }
    };
    println!(
        "{:<4} {:<8} {:<24} {:<5} {:<16} id",
        "#", "backend", "model", "mfr", "input"
    );
    for m in session.monitors().to_vec() {
        let input = match session.get_vcp_at(m.index, cfg.vcp.input_source) {
            Ok(v) => format_input(cfg, v.current),
            Err(e) => {
                log::info!("monitor #{}: {e}", m.index);
                "n/a".into()
            }
        };
        let mark = if selected == Some(m.index) { "*" } else { " " };
        println!(
            "{:<4} {:<8} {:<24} {:<5} {:<16} {}",
            format!("{}{mark}", m.index),
            m.backend,
            m.model(),
            m.manufacturer.as_deref().unwrap_or("-"),
            input,
            m.id
        );
    }
    if selected.is_some() {
        println!("\n* = monitor used by default (model \"{}\")", cfg.monitor.model);
    }
    Ok(())
}

fn cmd_caps(cfg: &Config, mon: &impl DisplayControl, raw: bool) -> Result<()> {
    let caps = mon.capabilities()?;
    if raw {
        println!("{}", caps.raw);
        return Ok(());
    }
    let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "-".into());
    println!("Monitor:   #{} ({})", mon.info().index, mon.info().backend);
    println!("Model:     {}", opt(&caps.model));
    println!("Type:      {}", opt(&caps.kind));
    println!("Protocol:  {}", opt(&caps.prot));
    println!("MCCS ver:  {}", opt(&caps.mccs_ver));
    println!(
        "Commands:  {}",
        caps.cmds.iter().map(|c| format!("{c:02X}")).collect::<Vec<_>>().join(" ")
    );
    for (k, v) in &caps.other {
        println!("{:<10} {}", format!("{k}:"), v);
    }
    println!("\nVCP features ({}):", caps.vcp.len());
    for (code, values) in &caps.vcp {
        let name = feature_name(*code).unwrap_or("");
        let vals = if values.is_empty() {
            String::new()
        } else if *code == cfg.vcp.input_source {
            values.iter().map(|v| format_input(cfg, *v)).collect::<Vec<_>>().join(", ")
        } else {
            values.iter().map(|v| caps_value(*v)).collect::<Vec<_>>().join(" ")
        };
        println!("  {}  {:<34} {}", hex8(*code), name, vals);
    }
    print_input_hint(cfg, &caps);
    Ok(())
}

fn print_input_hint(cfg: &Config, caps: &Capabilities) {
    let code = cfg.vcp.input_source;
    if !caps.supports(code) {
        println!("\nwarning: VCP {} (input source) not listed in capabilities", hex8(code));
        return;
    }
    let listed = caps.values(code);
    let missing: Vec<String> = cfg
        .inputs
        .iter()
        .filter(|(_, v)| !listed.is_empty() && !listed.contains(v))
        .map(|(k, v)| format!("{k}={}", hex(*v)))
        .collect();
    if !missing.is_empty() {
        println!(
            "\nnote: config aliases not in capabilities list: {} – check [inputs] in config",
            missing.join(", ")
        );
    }
}

fn cmd_input(cfg: &Config, mon: &impl DisplayControl, name: Option<&str>) -> Result<()> {
    let code = cfg.vcp.input_source;
    let Some(name) = name else {
        let v = mon.get_vcp(code)?;
        println!("current input: {}", format_input(cfg, v.current));
        println!("aliases: {}", alias_list(cfg));
        return Ok(());
    };
    let value = resolve_input(cfg, name)?;
    mon.set_vcp(code, value)?;
    println!("switched to {}", format_input(cfg, value));
    Ok(())
}

fn cmd_probe(
    cfg: &Config,
    mon: &impl DisplayControl,
    output: Option<PathBuf>,
    all_caps: bool,
    cycle_inputs: bool,
    (range_start, range_end): (u8, u8),
) -> Result<()> {
    let input_code = cfg.vcp.input_source;
    let caps = match mon.capabilities() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("warning: cannot read capabilities: {e}");
            None
        }
    };

    let current_input = mon.get_vcp(input_code).ok().map(|v| v.current);
    if let Some(c) = &caps {
        let vals = c.values(input_code);
        eprintln!(
            "input values in capabilities (VCP {}): {}",
            hex8(input_code),
            if vals.is_empty() {
                "none listed".to_string()
            } else {
                vals.iter()
                    .map(|v| {
                        let cur = if Some(*v) == current_input { " <- current" } else { "" };
                        format!("{}{cur}", format_input(cfg, *v))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }

    if cycle_inputs {
        let values = caps.as_ref().map(|c| c.values(input_code).to_vec()).unwrap_or_default();
        cycle(cfg, mon, &values, current_input)?;
    }

    let mut codes: Vec<u8> = (range_start..=range_end).collect();
    codes.push(input_code);
    if let Some(c) = caps.as_ref().filter(|_| all_caps) {
        codes.extend(c.vcp.keys().copied());
    }
    codes.sort_unstable();
    codes.dedup();

    eprintln!("reading {} VCP codes…", codes.len());
    let snap = probe::snapshot(mon, &codes, |code, e| match (e.current, &e.error) {
        (Some(v), _) => eprintln!("  {}  {:>5} ({})  max {}", hex8(code), v, hex(v), e.maximum.unwrap_or(0)),
        (None, Some(err)) => log::info!("  {}  error: {err}", hex8(code)),
        _ => {}
    });
    let ok = snap.codes.values().filter(|e| e.current.is_some()).count();
    eprintln!("{ok}/{} codes answered", codes.len());

    let json = serde_json::to_string_pretty(&snap)?;
    match output {
        Some(path) => {
            fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("saved {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}

fn cycle(cfg: &Config, mon: &impl DisplayControl, values: &[u16], original: Option<u16>) -> Result<()> {
    if values.is_empty() {
        bail!("capabilities list no input values to cycle through");
    }
    let code = cfg.vcp.input_source;
    eprintln!(
        "cycling inputs – the screen may go dark; press Enter to continue each step.\n\
         (this also tests whether DDC works while another input is shown)"
    );
    let stdin = io::stdin();
    let mut line = String::new();
    for &v in values {
        eprint!("-> {} … ", format_input(cfg, v));
        io::stderr().flush().ok();
        match mon.set_vcp(code, v) {
            Ok(()) => eprint!("set. Enter = next "),
            Err(e) => eprint!("failed ({e}). Enter = next "),
        }
        io::stderr().flush().ok();
        line.clear();
        stdin.lock().read_line(&mut line).context("reading stdin")?;
    }
    if let Some(orig) = original {
        mon.set_vcp(code, orig)
            .with_context(|| format!("restoring input {}", format_input(cfg, orig)))?;
        eprintln!("restored {}", format_input(cfg, orig));
    }
    Ok(())
}

fn cmd_diff(a: &Path, b: &Path) -> Result<()> {
    let sa = probe::load(a)?;
    let sb = probe::load(b)?;
    let changes = probe::diff(&sa, &sb);
    if changes.is_empty() {
        println!("no differences");
        return Ok(());
    }
    let fmt = |e: &Option<Entry>| match e {
        None => "absent".to_string(),
        Some(Entry { current: Some(v), maximum, .. }) => {
            format!("{v} ({}) max {}", hex(*v), maximum.unwrap_or(0))
        }
        Some(Entry { error: Some(_), .. }) => "error".to_string(),
        Some(_) => "?".to_string(),
    };
    println!("{:<6} {:<24} B", "code", "A");
    for c in changes {
        println!("{:<6} {:<24} {}", c.code, fmt(&c.before), fmt(&c.after));
    }
    Ok(())
}

// ---- helpers ---------------------------------------------------------------

/// Input alias from config, or a raw value (`0x1A`, `26`).
fn resolve_input(cfg: &Config, name: &str) -> Result<u16> {
    match cfg.input_value(name) {
        Some(v) => Ok(v),
        None => parse_value(name).map_err(|_| {
            anyhow!(ddc_core::Error::UnknownInput {
                name: name.to_string(),
                known: cfg.known_aliases(),
            })
        }),
    }
}

fn format_input(cfg: &Config, v: u16) -> String {
    match cfg.input_alias(v) {
        Some(a) => format!("{} ({a})", hex(v)),
        None => hex(v),
    }
}

fn alias_list(cfg: &Config) -> String {
    cfg.inputs
        .iter()
        .map(|(k, v)| format!("{k}={}", hex(*v)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn hex(v: u16) -> String {
    if v <= 0xFF {
        format!("0x{v:02X}")
    } else {
        format!("0x{v:04X}")
    }
}

/// Capability value as listed by the monitor: 2 digits, or 4 for 16-bit values.
fn caps_value(v: u16) -> String {
    if v <= 0xFF {
        format!("{v:02X}")
    } else {
        format!("{v:04X}")
    }
}

fn hex8(v: u8) -> String {
    format!("0x{v:02X}")
}

fn name_suffix(code: u8) -> String {
    feature_name(code).map(|n| format!(" ({n})")).unwrap_or_default()
}

/// VCP codes are hex by convention (like ddcutil): `60`, `0x60`, `60h`.
fn parse_code(s: &str) -> std::result::Result<u8, String> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .or_else(|| t.strip_suffix(['h', 'H']))
        .unwrap_or(t);
    u8::from_str_radix(t, 16).map_err(|e| format!("invalid VCP code '{s}' (hex, e.g. 60 or 0x60): {e}"))
}

/// Values are decimal unless prefixed with 0x (or suffixed with h).
fn parse_value(s: &str) -> std::result::Result<u16, String> {
    let t = s.trim();
    let r = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u16::from_str_radix(h, 16)
    } else if let Some(h) = t.strip_suffix(['h', 'H']) {
        u16::from_str_radix(h, 16)
    } else {
        t.parse::<u16>()
    };
    r.map_err(|e| format!("invalid value '{s}' (decimal, or hex as 0x0F): {e}"))
}

fn cmd_xfer(cfg: &Config, action: &XferCmd) -> Result<()> {
    use xfer::{trust, Event};

    let (identity, trust_list) = xfer::load_tls(&cfg.xfer, None)?;
    if let XferCmd::Fingerprint = action {
        println!("this machine: {}", trust::pretty(&identity.fingerprint()));
        println!("TLS folder:   {}", trust_list.dir.display());
        let fps = trust_list.fingerprints();
        if fps.is_empty() {
            println!("trusted:      none – pair the machines in Deskflow or add [xfer].trusted");
        }
        for f in fps {
            println!("trusted:      {}", trust::pretty(&f));
        }
        return Ok(());
    }
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "monswitch".into());
    let (tx, rx) = std::sync::mpsc::channel();
    let svc = xfer::Service::start(&cfg.xfer, &name, &identity, trust_list, tx)?;
    let peer = match action {
        XferCmd::Send { peer, .. } | XferCmd::Receive { peer, .. } => peer.clone(),
        XferCmd::Fingerprint => None,
    };
    if let Some(p) = peer {
        svc.set_peer(&p);
    }
    eprintln!("listening on port {} – Ctrl+C to stop", svc.port());
    if let XferCmd::Send { paths, .. } = action {
        let peer = svc.peer().ok_or_else(|| anyhow!("no peer – use --peer or set [xfer].peer"))?;
        eprintln!("offering {} item(s) to {peer}", paths.len());
        svc.offer_local(paths.clone());
    }
    let mut last_line = String::new();
    for ev in rx {
        match (&ev, action) {
            (Event::RemoteOffer(o), XferCmd::Receive { dir, .. }) => {
                eprintln!("{} offers {} item(s)", o.from, o.entries.iter().filter(|e| e.is_top_level()).count());
                let client = svc.client();
                let (o, dir) = (o.clone(), dir.clone());
                std::thread::spawn(move || match client.download_all(&o, &dir) {
                    Ok(paths) => paths.iter().for_each(|p| eprintln!("received {}", p.display())),
                    Err(e) => eprintln!("download failed: {e}"),
                });
            }
            (Event::Progress(p), _) => {
                let pct = p.done.saturating_mul(100).checked_div(p.total).unwrap_or(100);
                let line = format!("{:?} {} {pct} %", p.dir, p.name);
                if line != last_line {
                    eprintln!("{line}");
                    last_line = line;
                }
            }
            (Event::Done { dir, name, result, .. }, _) => match result {
                Ok(()) => eprintln!("{dir:?} {name} done"),
                Err(e) => eprintln!("{dir:?} {name} failed: {e}"),
            },
            (Event::Peer(p), _) => eprintln!("other machine at {p}"),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_parsing() {
        assert_eq!(parse_code("60"), Ok(0x60));
        assert_eq!(parse_code("0x60"), Ok(0x60));
        assert_eq!(parse_code("E0h"), Ok(0xE0));
        assert!(parse_code("160").is_err());
    }

    #[test]
    fn value_parsing() {
        assert_eq!(parse_value("15"), Ok(15));
        assert_eq!(parse_value("0x0F"), Ok(15));
        assert_eq!(parse_value("1Bh"), Ok(0x1B));
        assert!(parse_value("dp").is_err());
    }

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
