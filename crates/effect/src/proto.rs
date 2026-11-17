//! Tiny text protocol between the two machines (one UDP datagram per message).
//!
//! `MSFX1 hello <name>`            client → server, every few seconds
//! `MSFX1 enter <x> <y> <name>`    server → client: cursor entered screen `name` at x,y
//! `MSFX1 leave <x> <y> <name>`    server → client: cursor left screen `name`
//!                                 (x,y = entry point on the server)

const MAGIC: &str = "MSFX1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Hello { name: String },
    Enter { x: i32, y: i32, name: String },
    Leave { x: i32, y: i32, name: String },
}

impl Msg {
    pub fn encode(&self) -> String {
        match self {
            Msg::Hello { name } => format!("{MAGIC} hello {name}"),
            Msg::Enter { x, y, name } => format!("{MAGIC} enter {x} {y} {name}"),
            Msg::Leave { x, y, name } => format!("{MAGIC} leave {x} {y} {name}"),
        }
    }

    pub fn decode(data: &[u8]) -> Option<Msg> {
        if data.len() > 512 {
            return None;
        }
        let text = std::str::from_utf8(data).ok()?.trim_end();
        let rest = text.strip_prefix(MAGIC)?.strip_prefix(' ')?;
        let (kind, rest) = rest.split_once(' ')?;
        if kind == "hello" {
            return Some(Msg::Hello { name: rest.to_string() });
        }
        let mut it = rest.splitn(3, ' ');
        let x = it.next()?.parse().ok()?;
        let y = it.next()?.parse().ok()?;
        let name = it.next().unwrap_or("").to_string();
        match kind {
            "enter" => Some(Msg::Enter { x, y, name }),
            "leave" => Some(Msg::Leave { x, y, name }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for m in [
            Msg::Hello { name: "Studio Mac".into() },
            Msg::Enter { x: 0, y: -260, name: "MacBook".into() },
            Msg::Leave { x: 3829, y: 260, name: "MacBook".into() },
        ] {
            assert_eq!(Msg::decode(m.encode().as_bytes()), Some(m));
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Msg::decode(b"hello"), None);
        assert_eq!(Msg::decode(b"MSFX1 enter x 1 a"), None);
        assert_eq!(Msg::decode(b"MSFX1 jump 1 2 a"), None);
        assert_eq!(Msg::decode(&[0xff, 0xfe]), None);
    }
}
