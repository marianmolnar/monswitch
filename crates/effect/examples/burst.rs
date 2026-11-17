//! Shows one burst, for checking the overlay by eye:
//! `cargo run -p effect --example burst -- [smoke|glow|particles] [x y] [duration_ms] [left|right|top|bottom]`
//! Without x y it uses the cursor position; without an edge it is detected
//! from the position. On macOS the overlay must run on the main thread with an
//! AppKit run loop, so this example is Windows-only.

#[cfg(windows)]
fn main() {
    use std::time::Duration;

    use ddc_core::config::EffectStyle;
    use effect::{Burst, Edge, Kind, Look, Overlay};

    effect::dpi_aware_thread();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let style = match args.first().map(String::as_str) {
        Some("particles") => EffectStyle::Particles,
        Some("glow") => EffectStyle::Glow,
        _ => EffectStyle::Smoke,
    };
    let (x, y) = match (args.get(1).and_then(|v| v.parse().ok()), args.get(2).and_then(|v| v.parse().ok())) {
        (Some(x), Some(y)) => (x, y),
        _ => effect::cursor_position().unwrap_or((500.0, 500.0)),
    };
    let ms: u64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(550);
    let edge = match args.get(4).map(String::as_str) {
        Some("left") => Some(Edge::Left),
        Some("right") => Some(Edge::Right),
        Some("top") => Some(Edge::Top),
        Some("bottom") => Some(Edge::Bottom),
        _ => effect::edge_at(x, y),
    };
    let overlay = match Overlay::new() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let look = Look { style, duration: Duration::from_millis(ms), rgb: (0xF2, 0xF6, 0xFF) };
    overlay.show(Burst { x, y, kind: Kind::Exit, look, edge });
    std::thread::sleep(Duration::from_millis(ms + 200));
}

#[cfg(not(windows))]
fn main() {
    eprintln!("this example is Windows-only");
}
