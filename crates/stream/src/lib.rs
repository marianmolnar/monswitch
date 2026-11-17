//! Streaming between the PC and the Mac with Sunshine (host) + Moonlight
//! (client). Pairing is done once in the Moonlight GUI; this crate only
//! launches `Moonlight [options] stream <host> <app>` for a profile.
//! [`sunshine`] turns the local Sunshine host on and off.

pub mod sunshine;

use std::path::PathBuf;
use std::process::{Command, Stdio};

use ddc_core::config::{StreamConfig, StreamProfile};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Moonlight not found (install it or set [stream].moonlight)")]
    MoonlightNotFound,
    #[error("stream '{0}' has no host – set it in the settings")]
    NoHost(String),
    #[error("cannot start Moonlight: {0}")]
    Spawn(std::io::Error),
    #[error("Sunshine: {0}")]
    Sunshine(String),
    #[error("Sunshine: monswitch may not control the service – click “Allow” in Settings → Streaming")]
    NeedsPermission,
}

/// Opens the Moonlight app itself (computer list, pairing).
pub fn open_moonlight(cfg: &StreamConfig) -> Result<u32, Error> {
    let exe = find_moonlight(cfg).ok_or(Error::MoonlightNotFound)?;
    Command::new(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|c| c.id())
        .map_err(Error::Spawn)
}

fn default_moonlight_paths() -> Vec<PathBuf> {
    if cfg!(windows) {
        let mut v = Vec::new();
        for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(p) = std::env::var_os(var) {
                v.push(
                    PathBuf::from(p)
                        .join("Moonlight Game Streaming")
                        .join("Moonlight.exe"),
                );
            }
        }
        v
    } else if cfg!(target_os = "macos") {
        vec![PathBuf::from("/Applications/Moonlight.app/Contents/MacOS/Moonlight")]
    } else {
        vec![
            PathBuf::from("/usr/bin/moonlight"),
            PathBuf::from("/usr/local/bin/moonlight"),
        ]
    }
}

/// `[stream].moonlight` if set, otherwise the first default path that exists.
pub fn find_moonlight(cfg: &StreamConfig) -> Option<PathBuf> {
    let configured = cfg.moonlight.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return p.exists().then_some(p);
    }
    default_moonlight_paths().into_iter().find(|p| p.exists())
}

/// Profiles shown on this OS.
pub fn visible_profiles(profiles: &[StreamProfile]) -> Vec<(usize, &StreamProfile)> {
    profiles
        .iter()
        .enumerate()
        .filter(|(_, p)| p.visible_on(std::env::consts::OS))
        .collect()
}

/// Value of `--resolution` that is replaced by the built-in display's native resolution.
pub const NATIVE: &str = "native";

/// Native pixel resolution ("3456x2234") of the built-in display, if there is one
/// (macOS only, via `system_profiler`; `None` with the lid closed or on a desktop).
pub fn native_resolution() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("/usr/sbin/system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_builtin_resolution(&String::from_utf8_lossy(&out.stdout))
}

/// Finds the internal display in `system_profiler SPDisplaysDataType -json` output.
fn parse_builtin_resolution(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let gpus = v.get("SPDisplaysDataType")?.as_array()?;
    let displays = gpus
        .iter()
        .filter_map(|g| g.get("spdisplays_ndrvs").and_then(|d| d.as_array()))
        .flatten();
    for d in displays {
        let internal = d
            .get("spdisplays_connection_type")
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.contains("internal"));
        if !internal {
            continue;
        }
        for key in ["_spdisplays_pixels", "spdisplays_pixelresolution"] {
            if let Some(r) = d.get(key).and_then(|p| p.as_str()).and_then(extract_wxh) {
                return Some(r);
            }
        }
    }
    None
}

/// "3456 x 2234" / "spdisplays_3456x2234Retina" → "3456x2234".
fn extract_wxh(s: &str) -> Option<String> {
    let nums: Vec<u32> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .filter(|n| *n >= 320)
        .collect();
    match nums.as_slice() {
        [w, h, ..] => Some(format!("{w}x{h}")),
        _ => None,
    }
}

/// Moonlight command-line arguments: `[options] stream <host> <app>`.
/// `--resolution native` is resolved with [`native_resolution`].
pub fn moonlight_args(p: &StreamProfile) -> Result<Vec<String>, Error> {
    moonlight_args_with(p, native_resolution)
}

fn moonlight_args_with(
    p: &StreamProfile,
    native: impl Fn() -> Option<String>,
) -> Result<Vec<String>, Error> {
    let host = p.host.trim();
    if host.is_empty() {
        return Err(Error::NoHost(p.name.clone()));
    }
    let raw: Vec<String> = p
        .args
        .iter()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect();
    let mut args = Vec::with_capacity(raw.len() + 3);
    let mut it = raw.into_iter().peekable();
    while let Some(a) = it.next() {
        if a == "--resolution" && it.peek().is_some_and(|v| v.eq_ignore_ascii_case(NATIVE)) {
            it.next();
            match native() {
                Some(res) => {
                    args.push(a);
                    args.push(res);
                }
                // Moonlight then uses its own saved resolution.
                None => log::warn!("stream '{}': native resolution unknown, --resolution dropped", p.name),
            }
            continue;
        }
        args.push(a);
    }
    args.push("stream".into());
    args.push(host.to_string());
    args.push(p.app.trim().to_string());
    Ok(args)
}

/// Starts Moonlight detached; it runs independently of the tray app.
pub fn launch(cfg: &StreamConfig, p: &StreamProfile) -> Result<u32, Error> {
    let exe = find_moonlight(cfg).ok_or(Error::MoonlightNotFound)?;
    let args = moonlight_args(p)?;
    log::info!("stream '{}': {} {}", p.name, exe.display(), args.join(" "));
    let child = Command::new(&exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Error::Spawn)?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(host: &str) -> StreamProfile {
        StreamProfile {
            name: "t".into(),
            host: host.into(),
            app: "Desktop".into(),
            args: vec!["--display-mode".into(), "windowed".into(), " ".into()],
            only_on: String::new(),
        }
    }

    #[test]
    fn args_put_options_before_stream() {
        let a = moonlight_args(&profile("mac.local")).unwrap();
        assert_eq!(a, vec!["--display-mode", "windowed", "stream", "mac.local", "Desktop"]);
    }

    #[test]
    fn native_resolution_is_resolved_or_dropped() {
        let mut p = profile("pc");
        p.args = vec!["--resolution".into(), "native".into(), "--fps".into(), "120".into()];
        let a = moonlight_args_with(&p, || Some("3456x2234".into())).unwrap();
        assert_eq!(a[..4], ["--resolution", "3456x2234", "--fps", "120"]);
        let a = moonlight_args_with(&p, || None).unwrap();
        assert_eq!(a[..2], ["--fps", "120"]);
    }

    #[test]
    fn parses_system_profiler() {
        let json = r#"{"SPDisplaysDataType":[{"sppci_model":"Apple M4 Pro","spdisplays_ndrvs":[
            {"_name":"PG27UCDM","_spdisplays_pixels":"3840 x 2160","spdisplays_connection_type":"spdisplays_displayport"},
            {"_name":"Color LCD","_spdisplays_pixels":"3456 x 2234","spdisplays_connection_type":"spdisplays_internal"}]}]}"#;
        assert_eq!(parse_builtin_resolution(json).as_deref(), Some("3456x2234"));
        let closed = r#"{"SPDisplaysDataType":[{"spdisplays_ndrvs":[
            {"_spdisplays_pixels":"3840 x 2160","spdisplays_connection_type":"spdisplays_displayport"}]}]}"#;
        assert_eq!(parse_builtin_resolution(closed), None);
        assert_eq!(extract_wxh("spdisplays_3024x1964Retina").as_deref(), Some("3024x1964"));
    }

    #[test]
    fn empty_host_is_an_error() {
        assert!(matches!(moonlight_args(&profile(" ")), Err(Error::NoHost(_))));
    }

    #[test]
    fn profiles_filter_by_os() {
        let cfg = ddc_core::Config::defaults().unwrap();
        // the generic defaults show everywhere
        assert!(cfg.streams.iter().all(|p| p.visible_on("windows") && p.visible_on("macos")));
        let mut p = profile("pc");
        p.only_on = "macos".into();
        assert!(p.visible_on("macos") && !p.visible_on("windows"));
    }
}
