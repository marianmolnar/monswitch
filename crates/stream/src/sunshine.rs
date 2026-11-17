//! Turning the Sunshine host on and off from monswitch.
//!
//! - **Windows:** Sunshine is the `SunshineService` service. Regular users may
//!   not start/stop services, so [`grant_control`] adds a start/stop right for
//!   interactive users to that one service (one UAC prompt, once).
//! - **macOS / Linux:** Sunshine is a normal process (`sunshine`), started
//!   detached and stopped with a signal.
//! - **macOS + Homebrew:** a Sunshine installed from `[stream].sunshine_formula`
//!   is started/stopped with `brew services`. It then runs as its own
//!   LaunchAgent and keeps its Screen Recording permission; started by
//!   monswitch, macOS would check that permission for monswitch instead.

#[cfg(not(windows))]
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ddc_core::config::StreamConfig;

use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SunshineState {
    Running,
    /// Includes "starting"/"stopping".
    Stopped,
    NotInstalled,
}

fn quiet(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

// ---- Windows: service ------------------------------------------------------------

#[cfg(windows)]
const SERVICE: &str = "SunshineService";
/// Start (RP) + stop (WP) for interactive users (IU).
#[cfg(windows)]
const GRANT_ACE: &str = "(A;;RPWP;;;IU)";

#[cfg(windows)]
fn sc(args: &[&str]) -> Result<(i32, String), Error> {
    let out = quiet(Command::new("sc.exe").args(args))
        .output()
        .map_err(|e| Error::Sunshine(format!("sc.exe: {e}")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.code().unwrap_or(-1), text))
}

/// `STATE : 4  RUNNING` → 4 (the number is not localized, the word may be).
#[cfg(any(windows, test))]
fn service_state_code(sc_query: &str) -> Option<u32> {
    sc_query
        .lines()
        .find(|l| l.trim_start().starts_with("STATE"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

/// SDDL with `ace` added to the DACL (before a SACL if there is one).
#[cfg(any(windows, test))]
fn sddl_with(sddl: &str, ace: &str) -> String {
    let sddl = sddl.trim();
    if sddl.contains(ace) {
        return sddl.to_string();
    }
    match sddl.find("S:") {
        Some(i) => format!("{}{ace}{}", &sddl[..i], &sddl[i..]),
        None => format!("{sddl}{ace}"),
    }
}

#[cfg(windows)]
pub fn state(_cfg: &StreamConfig) -> SunshineState {
    match sc(&["query", SERVICE]) {
        Ok((1060, _)) => SunshineState::NotInstalled,
        Ok((_, text)) => match service_state_code(&text) {
            Some(4) => SunshineState::Running,
            Some(_) => SunshineState::Stopped,
            None => SunshineState::NotInstalled,
        },
        Err(_) => SunshineState::NotInstalled,
    }
}

#[cfg(windows)]
pub fn start(_cfg: &StreamConfig) -> Result<(), Error> {
    match sc(&["start", SERVICE])? {
        (0, _) | (1056, _) => Ok(()), // started / already running
        (5, _) => Err(Error::NeedsPermission),
        (1060, _) => Err(Error::Sunshine("Sunshine is not installed".into())),
        (code, text) => Err(Error::Sunshine(format!("sc start: {code} {}", text.trim()))),
    }
}

#[cfg(windows)]
pub fn stop(_cfg: &StreamConfig) -> Result<(), Error> {
    match sc(&["stop", SERVICE])? {
        (0, _) | (1062, _) => Ok(()), // stopped / was not running
        (5, _) => Err(Error::NeedsPermission),
        (code, text) => Err(Error::Sunshine(format!("sc stop: {code} {}", text.trim()))),
    }
}

/// True when this user may start/stop the service without admin rights.
#[cfg(windows)]
pub fn has_control() -> bool {
    sc(&["sdshow", SERVICE]).is_ok_and(|(code, text)| code == 0 && text.contains(GRANT_ACE))
}

/// Lets interactive users start/stop SunshineService. Shows one UAC prompt.
#[cfg(windows)]
pub fn grant_control() -> Result<(), Error> {
    let (code, text) = sc(&["sdshow", SERVICE])?;
    if code != 0 {
        return Err(Error::Sunshine(format!("sc sdshow: {}", text.trim())));
    }
    let current = text.lines().map(str::trim).find(|l| l.starts_with("D:")).unwrap_or("").to_string();
    if current.is_empty() {
        return Err(Error::Sunshine("cannot read the service permissions".into()));
    }
    let wanted = sddl_with(&current, GRANT_ACE);
    if wanted == current {
        return Ok(());
    }
    // SDDL contains no spaces or quotes, so it is safe inside a single-quoted PS string.
    let ps = format!(
        "$p = Start-Process -FilePath sc.exe -ArgumentList 'sdset','{SERVICE}','{wanted}' -Verb RunAs -Wait -PassThru -WindowStyle Hidden; exit $p.ExitCode"
    );
    let status = quiet(Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", &ps]))
        .status()
        .map_err(|e| Error::Sunshine(format!("powershell: {e}")))?;
    if !status.success() || !has_control() {
        return Err(Error::Sunshine("permission was not granted (UAC cancelled?)".into()));
    }
    Ok(())
}

// ---- macOS / Linux: process -------------------------------------------------------

#[cfg(not(windows))]
fn find_sunshine(cfg: &StreamConfig) -> Option<PathBuf> {
    let configured = cfg.sunshine.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return p.exists().then_some(p);
    }
    [
        "/opt/homebrew/bin/sunshine",
        "/usr/local/bin/sunshine",
        "/Applications/Sunshine.app/Contents/MacOS/sunshine",
        "/usr/bin/sunshine",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

#[cfg(not(windows))]
fn running() -> bool {
    quiet(Command::new("pgrep").args(["-x", "sunshine"]))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(windows))]
pub fn state(cfg: &StreamConfig) -> SunshineState {
    if running() {
        SunshineState::Running
    } else if find_sunshine(cfg).is_some() {
        SunshineState::Stopped
    } else {
        SunshineState::NotInstalled
    }
}

/// `brew` and the formula when Sunshine should go through `brew services`:
/// macOS, `[stream].sunshine_formula` set and the executable we would run
/// lives in that Homebrew prefix.
#[cfg(not(windows))]
fn brew_services(cfg: &StreamConfig) -> Option<(PathBuf, String)> {
    let formula = cfg.sunshine_formula.trim();
    if !cfg!(target_os = "macos") || formula.is_empty() {
        return None;
    }
    let brew = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())?;
    let prefix = brew.parent()?.parent()?.to_path_buf();
    let exe = std::fs::canonicalize(find_sunshine(cfg)?).ok()?;
    exe.starts_with(&prefix).then(|| (brew, formula.to_string()))
}

#[cfg(not(windows))]
fn brew_service(brew: &Path, verb: &str, formula: &str) -> Result<(), Error> {
    log::info!("sunshine: brew services {verb} {formula}");
    let out = quiet(Command::new(brew).args(["services", verb, formula]))
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        .output()
        .map_err(|e| Error::Sunshine(format!("brew: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let line = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    Err(Error::Sunshine(format!("brew services {verb}: {line}")))
}

#[cfg(not(windows))]
pub fn start(cfg: &StreamConfig) -> Result<(), Error> {
    if running() {
        return Ok(());
    }
    if let Some((brew, formula)) = brew_services(cfg) {
        return brew_service(&brew, "start", &formula);
    }
    let exe = find_sunshine(cfg).ok_or_else(|| Error::Sunshine("Sunshine is not installed".into()))?;
    quiet(&mut Command::new(&exe))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::Sunshine(format!("{}: {e}", exe.display())))
}

#[cfg(not(windows))]
pub fn stop(cfg: &StreamConfig) -> Result<(), Error> {
    // Unload the LaunchAgent first (no-op when not loaded), so launchd does
    // not start it again at login; then end a directly started process.
    if let Some((brew, formula)) = brew_services(cfg) {
        brew_service(&brew, "stop", &formula)?;
    }
    // pkill exits 1 when nothing matched – that is fine too
    quiet(Command::new("pkill").args(["-x", "sunshine"]))
        .status()
        .map(|_| ())
        .map_err(|e| Error::Sunshine(format!("pkill: {e}")))
}

#[cfg(not(windows))]
pub fn has_control() -> bool {
    true
}

#[cfg(not(windows))]
pub fn grant_control() -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sc_query_state() {
        let q = "SERVICE_NAME: SunshineService\r\n        TYPE               : 10  WIN32_OWN_PROCESS\r\n        STATE              : 4  RUNNING\r\n";
        assert_eq!(service_state_code(q), Some(4));
        assert_eq!(service_state_code("STATE : 1  STOPPED"), Some(1));
        assert_eq!(service_state_code("nothing"), None);
    }

    #[test]
    fn adds_ace_once_before_sacl() {
        let d = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCLCSWLOCRRC;;;IU)";
        let with = sddl_with(d, "(A;;RPWP;;;IU)");
        assert_eq!(with, format!("{d}(A;;RPWP;;;IU)"));
        assert_eq!(sddl_with(&with, "(A;;RPWP;;;IU)"), with);
        assert_eq!(
            sddl_with("D:(A;;CC;;;SY)S:(AU;FA;CCDC;;;WD)", "(A;;RPWP;;;IU)"),
            "D:(A;;CC;;;SY)(A;;RPWP;;;IU)S:(AU;FA;CCDC;;;WD)"
        );
    }
}
