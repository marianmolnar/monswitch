//! Events parsed from deskflow-core's log output.
//!
//! deskflow-core logs one line per event to stdout/stderr, e.g.
//! `[2026-11-17T18:27:26.064] INFO: switch from "PC" to "Mac" at 0,260`.
//! The coordinates are the entry point on the destination screen.

/// Something the edge effect cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEvent {
    /// The cursor moved from screen `from` to screen `to`, entering it at `x`,`y`.
    Switch { from: String, to: String, x: i32, y: i32 },
    /// Client: the server address it connects to (IP or host name, without port).
    ServerAddress(String),
    /// The settings file the core uses (holds this machine's screen name).
    SettingsFile(String),
    /// Client only. Deskflow logs `entering computer` / `leaving computer`
    /// here; the `switch from "…" to "…" at x,y` line is server-only.
    Presence { entered: bool },
}

/// Strips the `[timestamp] LEVEL: ` prefix.
fn message(line: &str) -> &str {
    let rest = match line.strip_prefix('[') {
        Some(r) => r.split_once("] ").map_or(r, |(_, m)| m),
        None => line,
    };
    rest.split_once(": ")
        .filter(|(lvl, _)| lvl.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
        .map_or(rest, |(_, m)| m)
        .trim_end()
}

/// `"name"` at the start of `s` → (name, rest after the closing quote).
fn quoted(s: &str) -> Option<(&str, &str)> {
    let s = s.strip_prefix('"')?;
    let end = s.find('"')?;
    Some((&s[..end], &s[end + 1..]))
}

/// Host part of "192.168.1.20:24800", "[fe80::1]:24800" or "pc.local".
fn host_of(addr: &str) -> &str {
    let addr = addr.trim();
    if let Some(r) = addr.strip_prefix('[') {
        return r.split_once(']').map_or(r, |(h, _)| h);
    }
    match addr.rsplit_once(':') {
        Some((h, port)) if !h.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => addr,
    }
}

pub fn parse_line(line: &str) -> Option<CoreEvent> {
    let msg = message(line);

    if let Some(rest) = msg.strip_prefix("switch from ") {
        let (from, rest) = quoted(rest)?;
        let (to, rest) = quoted(rest.strip_prefix(" to ")?)?;
        let (x, y) = rest.strip_prefix(" at ")?.split_once(',')?;
        return Some(CoreEvent::Switch {
            from: from.to_string(),
            to: to.to_string(),
            x: x.trim().parse().ok()?,
            y: y.trim().parse().ok()?,
        });
    }

    // client: "connecting to 'pc': 192.168.1.20:24800"
    if let Some(rest) = msg.strip_prefix("connecting to '") {
        let (name, rest) = rest.split_once('\'')?;
        let addr = rest.strip_prefix(": ").map(host_of).filter(|a| !a.is_empty());
        let host = addr.unwrap_or_else(|| host_of(name));
        return (!host.is_empty()).then(|| CoreEvent::ServerAddress(host.to_string()));
    }

    if let Some(path) = msg.strip_prefix("initial settings file: ") {
        let path = path.trim();
        return (!path.is_empty()).then(|| CoreEvent::SettingsFile(path.to_string()));
    }

    match msg {
        "entering computer" => Some(CoreEvent::Presence { entered: true }),
        "leaving computer" => Some(CoreEvent::Presence { entered: false }),
        _ => None,
    }
}

/// `computerName` from a Deskflow settings file (`[core] computerName=...`).
pub fn screen_name_from_settings(text: &str) -> Option<String> {
    let mut in_core = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_core = line.eq_ignore_ascii_case("[core]");
        } else if in_core {
            if let Some(v) = line.strip_prefix("computerName=") {
                let v = v.trim();
                return (!v.is_empty()).then(|| v.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_switch_lines() {
        assert_eq!(
            parse_line(r#"[2026-11-17T18:27:26.064] INFO: switch from "OFFICE-PC" to "MacBook" at 0,260"#),
            Some(CoreEvent::Switch {
                from: "OFFICE-PC".into(),
                to: "MacBook".into(),
                x: 0,
                y: 260
            })
        );
        assert_eq!(
            parse_line(r#"INFO: switch from "Mac Book" to "PC" at 3829,-4"#),
            Some(CoreEvent::Switch { from: "Mac Book".into(), to: "PC".into(), x: 3829, y: -4 })
        );
        assert_eq!(parse_line(r#"[x] INFO: switch from "a" to "b" at"#), None);
        assert_eq!(parse_line("[x] INFO: leaving computer"), Some(CoreEvent::Presence { entered: false }));
        assert_eq!(
            parse_line("[2026-11-17T19:35:25.279] INFO: entering computer"),
            Some(CoreEvent::Presence { entered: true })
        );
        assert_eq!(parse_line("[x] INFO: entering computer now"), None);
    }

    #[test]
    fn parses_server_address() {
        assert_eq!(
            parse_line("[t] NOTE: connecting to 'OFFICE-PC': 192.168.1.20:24800"),
            Some(CoreEvent::ServerAddress("192.168.1.20".into()))
        );
        assert_eq!(
            parse_line("[t] NOTE: connecting to 'pc.local'"),
            Some(CoreEvent::ServerAddress("pc.local".into()))
        );
        assert_eq!(
            parse_line("[t] NOTE: connecting to 'x': [fe80::1]:24800"),
            Some(CoreEvent::ServerAddress("fe80::1".into()))
        );
    }

    #[test]
    fn parses_settings_file_and_name() {
        assert_eq!(
            parse_line("[2026-11-17T18:51:47.758] INFO: initial settings file: C:/Users/x/AppData/Roaming/Deskflow/Deskflow.conf"),
            Some(CoreEvent::SettingsFile("C:/Users/x/AppData/Roaming/Deskflow/Deskflow.conf".into()))
        );
        let conf = "[log]\nlevel=Info\n\n[core]\ncomputerName=OFFICE-PC\ncoreMode=2\n";
        assert_eq!(screen_name_from_settings(conf).as_deref(), Some("OFFICE-PC"));
        assert_eq!(screen_name_from_settings("[gui]\ncomputerName=x\n"), None);
    }
}
