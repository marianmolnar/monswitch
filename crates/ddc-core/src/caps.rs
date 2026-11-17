//! Tolerant parser for MCCS capabilities strings, e.g.
//! `(prot(monitor)type(LCD)model(PG27UCDM)cmds(01 02 03)vcp(10 12 60(0F 11 12 1B))mccs_ver(2.2))`

use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Capabilities {
    pub raw: String,
    pub prot: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub model: Option<String>,
    pub mccs_ver: Option<String>,
    pub cmds: Vec<u8>,
    /// VCP code -> list of allowed values (empty = continuous / not enumerated).
    pub vcp: BTreeMap<u8, Vec<u16>>,
    /// Any other top-level sections (`vcpname`, `mswhql`, `asset_eep`, …).
    pub other: BTreeMap<String, String>,
}

impl Capabilities {
    pub fn parse(raw: &str) -> Self {
        let cleaned: String = raw
            .chars()
            .filter(|c| *c != '\0')
            .collect::<String>()
            .trim()
            .to_string();

        let mut caps = Capabilities {
            raw: cleaned.clone(),
            ..Default::default()
        };

        for (key, value) in split_sections(unwrap_outer(&cleaned)) {
            match key.to_ascii_lowercase().as_str() {
                "prot" => caps.prot = Some(value.trim().to_string()),
                "type" => caps.kind = Some(value.trim().to_string()),
                "model" => caps.model = Some(value.trim().to_string()),
                "mccs_ver" => caps.mccs_ver = Some(value.trim().to_string()),
                "cmds" => caps.cmds = hex_tokens(&value).filter_map(|v| u8::try_from(v).ok()).collect(),
                "vcp" => caps.vcp = parse_vcp(&value),
                _ if key.is_empty() => {}
                _ => {
                    caps.other.insert(key, value);
                }
            }
        }
        caps
    }

    pub fn supports(&self, code: u8) -> bool {
        self.vcp.contains_key(&code)
    }

    pub fn values(&self, code: u8) -> &[u16] {
        self.vcp.get(&code).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Strips the outer `( … )` wrapper if present. Trailing garbage after the
/// matching paren is ignored; an unterminated wrapper takes the rest.
fn unwrap_outer(s: &str) -> &str {
    let Some(rest) = s.strip_prefix('(') else {
        return s;
    };
    let mut depth = 1usize;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..i];
                }
            }
            _ => {}
        }
    }
    rest
}

/// Splits `key(value)key2(value2)` at the top nesting level.
fn split_sections(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let key_start = i;
        while i < bytes.len() && bytes[i] != '(' && bytes[i] != ')' {
            i += 1;
        }
        if i >= bytes.len() {
            break; // trailing text without a value
        }
        if bytes[i] == ')' {
            i += 1; // stray close paren
            continue;
        }
        let key: String = bytes[key_start..i].iter().collect::<String>().trim().to_string();
        i += 1; // '('
        let value_start = i;
        let mut depth = 1usize;
        while i < bytes.len() {
            match bytes[i] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let value: String = bytes[value_start..i.min(bytes.len())].iter().collect();
        out.push((key, value));
        i += 1; // ')'
    }
    out
}

/// Parses value tokens separated by whitespace. Tokens of up to 4 hex digits
/// are single (up to 16-bit) values – ASUS writes e.g. `FC(099F)` where
/// 0x099F is the feature maximum. Longer runs are split into byte pairs.
fn hex_tokens(s: &str) -> impl Iterator<Item = u16> + '_ {
    s.split(|c: char| !c.is_ascii_hexdigit())
        .filter(|t| !t.is_empty())
        .flat_map(|t| {
            if t.len() <= 4 {
                u16::from_str_radix(t, 16).into_iter().collect()
            } else {
                split_hex_run(t)
            }
        })
}

/// Splits a run of hex digits into byte pairs (`1012` -> 10, 12). Used for
/// VCP codes, which are always one byte, and overlong value runs.
fn split_hex_run(tok: &str) -> Vec<u16> {
    if tok.len() > 2 && tok.len() % 2 == 0 {
        tok.as_bytes()
            .chunks(2)
            .filter_map(|pair| std::str::from_utf8(pair).ok())
            .filter_map(|p| u16::from_str_radix(p, 16).ok())
            .collect()
    } else {
        u16::from_str_radix(tok, 16).into_iter().collect()
    }
}

fn parse_vcp(s: &str) -> BTreeMap<u8, Vec<u16>> {
    let mut map = BTreeMap::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut last_code: Option<u8> = None;

    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_hexdigit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_hexdigit() {
                i += 1;
            }
            let tok: String = chars[start..i].iter().collect();
            for code in split_hex_run(&tok) {
                if let Ok(code) = u8::try_from(code) {
                    map.entry(code).or_insert_with(Vec::new);
                    last_code = Some(code);
                }
            }
        } else if c == '(' {
            // Value list belongs to the last code read.
            let start = i + 1;
            let mut depth = 1usize;
            i += 1;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            let end = if depth == 0 { i - 1 } else { i };
            let inner: String = chars[start..end].iter().collect();
            if let Some(code) = last_code {
                let values = map.entry(code).or_insert_with(Vec::new);
                // Only top-level values; nested groups are flattened.
                values.extend(hex_tokens(&inner));
                values.dedup();
            }
        } else {
            i += 1;
        }
    }
    map
}

/// Human-readable names for common MCCS features (display only).
pub fn feature_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x02 => "New control value",
        0x04 => "Restore factory defaults",
        0x05 => "Restore factory luminance/contrast",
        0x08 => "Restore factory color",
        0x0B => "Color temperature increment",
        0x0C => "Color temperature request",
        0x10 => "Brightness",
        0x12 => "Contrast",
        0x14 => "Select color preset",
        0x16 => "Video gain: red",
        0x18 => "Video gain: green",
        0x1A => "Video gain: blue",
        0x52 => "Active control",
        0x60 => "Input source",
        0x62 => "Audio speaker volume",
        0x6C => "Video black level: red",
        0x6E => "Video black level: green",
        0x70 => "Video black level: blue",
        0x86 => "Display scaling",
        0x87 => "Sharpness",
        0x8D => "Audio mute",
        0xAA => "Screen orientation",
        0xAC => "Horizontal frequency",
        0xAE => "Vertical frequency",
        0xB6 => "Display technology type",
        0xC0 => "Display usage time",
        0xC6 => "Application enable key",
        0xC8 => "Display controller type",
        0xC9 => "Display firmware level",
        0xCA => "OSD",
        0xCC => "OSD language",
        0xD6 => "Power mode",
        0xDC => "Display mode",
        0xDF => "VCP version",
        0xE0..=0xFF => "Manufacturer specific",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "(prot(monitor)type(LCD)model(PG27UCDM)cmds(01 02 03 07 0C E3 F3)\
vcp(02 04 05 08 10 12 14(05 06 08 0B) 16 18 1A 52 60( 0F 11 12 1B) 62 6C 6E 70 86(02 05) 8D(01 02) \
AC AE B6 C6 C8 C9 D6(01 04) DF E0 E1(00 01 02) FF)mswhql(1)asset_eep(40)mccs_ver(2.2))";

    #[test]
    fn parses_sample() {
        let c = Capabilities::parse(SAMPLE);
        assert_eq!(c.model.as_deref(), Some("PG27UCDM"));
        assert_eq!(c.kind.as_deref(), Some("LCD"));
        assert_eq!(c.mccs_ver.as_deref(), Some("2.2"));
        assert_eq!(c.cmds, vec![0x01, 0x02, 0x03, 0x07, 0x0C, 0xE3, 0xF3]);
        assert_eq!(c.values(0x60), &[0x0F, 0x11, 0x12, 0x1B]);
        assert_eq!(c.values(0x14), &[0x05, 0x06, 0x08, 0x0B]);
        assert_eq!(c.values(0xE1), &[0x00, 0x01, 0x02]);
        assert!(c.supports(0x10));
        assert!(c.values(0x10).is_empty());
        assert!(c.supports(0xFF));
        assert_eq!(c.other.get("mswhql").map(String::as_str), Some("1"));
    }

    #[test]
    fn tolerates_missing_outer_parens_nul_and_packed_codes() {
        let c = Capabilities::parse("model(X)vcp(1012 60(0F 11) 61(0F1112))\0\0");
        assert_eq!(c.model.as_deref(), Some("X"));
        assert!(c.supports(0x10) && c.supports(0x12));
        assert_eq!(c.values(0x60), &[0x0F, 0x11]);
        // >4 digits: split into bytes
        assert_eq!(c.values(0x61), &[0x0F, 0x11, 0x12]);
    }

    /// Real string from the PG27UCDM (firmware as of 2026-11).
    const PG27UCDM: &str = "(prot(monitor)type(LCD)model(PG27UCDM)cmds(01 02 03 07 0C E3 F3)vcp(02 04 05 08 10 12 14(03 04 05 06 07 08 09 0B) 16 18 1A 52 59 5A 5B 5C 5D 5E 5F(0005) 60(0F 11 12 1A) 61(01 02 03) 62 72(5000 6400 7800 8C00 A000) 87 8A 8D(01 02) AC AE B2 B6 C6 C8 CC(01 02 03 04 05 06 07 08 09 0A 0C 0D 11 12 14 1A 1E 1F 23 24 25 26 27) D6(01 05) DC(0000 0100 0200 01 02 04 05 06 07 08 09 0A) DF E1(00 01) E2(0000 0100 0200 01 02 03 04 05 06 07) E3(00 07 08 09 0A 0B 0C 0D 0E 0F) E4(00 01 02 03 04 05) E5(00 01 02 03 04) E6(00 01 02 03 04) E7(00 01) E8(01 02 03 04) E9 EA(00 01 02) EB(00 01 02 03 04 05 06 07) EC(01 FF) ED(0000 0500 0A00 0F00 00 01 02 03 FF) EE(00 01) EF F2(0000 0100 0200 0300 0400 0500 0600 00 01 02 03 04 05 06) F3(02 03 04) F4(00 01 02 03 04 05) F5(0F00 1100 1200 1A00 0F 11 12 1A) F6 F7(0000 0100 0200 0300 0400 00 01 02 03) F8(00 02 04 08) F9(00 01 02 03) FA(01 02 03 04) FB FC(099F) FD(7879) FF)mswhql(1)asset_eep(40)mccs_ver(2.5))";

    #[test]
    fn parses_pg27ucdm_16bit_values() {
        let c = Capabilities::parse(PG27UCDM);
        assert_eq!(c.model.as_deref(), Some("PG27UCDM"));
        assert_eq!(c.mccs_ver.as_deref(), Some("2.5"));
        assert_eq!(c.vcp.len(), 63);
        assert_eq!(c.values(0x60), &[0x0F, 0x11, 0x12, 0x1A]);
        assert_eq!(c.values(0x5F), &[0x0005]);
        assert_eq!(c.values(0xFC), &[0x099F]);
        assert_eq!(c.values(0xFD), &[0x7879]);
        assert_eq!(c.values(0x72), &[0x5000, 0x6400, 0x7800, 0x8C00, 0xA000]);
        assert_eq!(
            c.values(0xF5),
            &[0x0F00, 0x1100, 0x1200, 0x1A00, 0x0F, 0x11, 0x12, 0x1A]
        );
    }

    #[test]
    fn tolerates_unterminated() {
        let c = Capabilities::parse("(model(Y)vcp(10 60(0F 11");
        assert_eq!(c.model.as_deref(), Some("Y"));
        assert_eq!(c.values(0x60), &[0x0F, 0x11]);
    }
}
