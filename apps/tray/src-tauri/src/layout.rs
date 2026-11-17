//! Does a global shortcut collide with typing on the current keyboard layout?
//!
//! On Windows, Ctrl+Alt is the same as AltGr, so e.g. Ctrl+Alt+1 on a Czech layout types `~`.
//! A registered global shortcut swallows that key, so the user can no longer type the character.

/// Parsed shortcut: modifiers + key name as written in the config ("Ctrl+Alt+1").
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Parsed<'a> {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    key: &'a str,
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse(hotkey: &str) -> Option<Parsed<'_>> {
    let mut p = Parsed { ctrl: false, alt: false, shift: false, meta: false, key: "" };
    for tok in hotkey.split('+').map(str::trim).filter(|t| !t.is_empty()) {
        match tok.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => p.ctrl = true,
            "alt" | "option" => p.alt = true,
            "shift" => p.shift = true,
            "super" | "cmd" | "command" | "meta" | "win" => p.meta = true,
            _ => p.key = tok,
        }
    }
    (!p.key.is_empty()).then_some(p)
}

/// Windows virtual-key code of a key name, the same way global-hotkey maps it.
#[cfg_attr(not(windows), allow(dead_code))]
fn virtual_key(key: &str) -> Option<u32> {
    let k = key.strip_prefix("Digit").or_else(|| key.strip_prefix("Key")).unwrap_or(key);
    if k.len() == 1 {
        let c = k.chars().next()?.to_ascii_uppercase();
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    Some(match k {
        "Minus" | "-" => 0xBD,
        "Equal" | "=" => 0xBB,
        "BracketLeft" | "[" => 0xDB,
        "BracketRight" | "]" => 0xDD,
        "Semicolon" | ";" => 0xBA,
        "Quote" | "'" => 0xDE,
        "Backslash" | "\\" => 0xDC,
        "Comma" | "," => 0xBC,
        "Period" | "." => 0xBE,
        "Slash" | "/" => 0xBF,
        "Backquote" | "`" => 0xC0,
        "IntlBackslash" => 0xE2,
        _ => return None,
    })
}

/// The character the shortcut would type on the current layout, if any.
#[cfg(windows)]
pub fn typed_char(hotkey: &str) -> Option<String> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardLayout, MapVirtualKeyExW, ToUnicodeEx, MAPVK_VK_TO_VSC,
    };

    let p = parse(hotkey)?;
    // Only Ctrl+Alt (= AltGr) produces characters; a Win-key combination never types.
    if !(p.ctrl && p.alt) || p.meta {
        return None;
    }
    let vk = virtual_key(p.key)?;

    let mut state = [0u8; 256];
    for k in [0x11usize, 0x12, 0xA2, 0xA5] {
        // VK_CONTROL, VK_MENU, VK_LCONTROL, VK_RMENU
        state[k] = 0x80;
    }
    if p.shift {
        state[0x10] = 0x80;
        state[0xA0] = 0x80;
    }
    let mut buf = [0u16; 8];
    // SAFETY: plain Win32 calls with valid, correctly sized buffers.
    let n = unsafe {
        let hkl = GetKeyboardLayout(0);
        let scan = MapVirtualKeyExW(vk, MAPVK_VK_TO_VSC, hkl);
        // flag 0x4: do not change the keyboard (dead-key) state
        ToUnicodeEx(vk, scan, state.as_ptr(), buf.as_mut_ptr(), buf.len() as i32, 0x4, hkl)
    };
    // n < 0: dead key (e.g. ˇ), the character is still in buf[0]
    let len = match n {
        0 => return None,
        n if n < 0 => 1,
        n => n as usize,
    };
    let s: String = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
    let s = s.trim_matches(|c: char| c.is_control()).to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(not(windows))]
pub fn typed_char(_hotkey: &str) -> Option<String> {
    // macOS: Option combinations are only intercepted with Ctrl/Cmd held, which don't type.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_key() {
        let p = parse("Ctrl+Alt+Shift+1").expect("parse");
        assert!(p.ctrl && p.alt && p.shift && !p.meta);
        assert_eq!(p.key, "1");
        assert!(parse("Ctrl+Alt").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn maps_virtual_keys() {
        assert_eq!(virtual_key("1"), Some(0x31));
        assert_eq!(virtual_key("Digit2"), Some(0x32));
        assert_eq!(virtual_key("KeyQ"), Some(0x51));
        assert_eq!(virtual_key("q"), Some(0x51));
        assert_eq!(virtual_key("Minus"), Some(0xBD));
        assert_eq!(virtual_key("F5"), None);
    }

    #[test]
    fn non_typing_shortcuts_are_fine() {
        assert_eq!(typed_char("Ctrl+Shift+1"), None);
        assert_eq!(typed_char("Ctrl+Alt+F5"), None);
        assert_eq!(typed_char("Super+Ctrl+Alt+1"), None);
    }

    /// Prints what the default shortcuts type on this machine's layout.
    #[test]
    #[ignore]
    fn print_current_layout() {
        for hk in ["Ctrl+Alt+1", "Ctrl+Alt+2", "Ctrl+Alt+Shift+1", "Ctrl+Alt+Shift+2", "Ctrl+Alt+Q"] {
            println!("{hk}: {:?}", typed_char(hk));
        }
    }
}
