//! Shows one burst, for checking the overlay by eye:
//! `cargo run -p effect --example burst -- [rift|smoke|glow|particles] [x y] [duration_ms] [left|right|top|bottom] [entry]`
//! Without x y it uses the cursor position; without an edge it is detected
//! from the position. On macOS the overlay runs on the main thread, animated
//! by its run loop, so the example runs that loop instead of sleeping.

#[cfg(any(windows, target_os = "macos"))]
fn main() {
    use std::time::Duration;

    use ddc_core::config::EffectStyle;
    use effect::{Burst, Edge, Kind, Look, Overlay};

    effect::dpi_aware_thread();
    #[cfg(target_os = "macos")]
    mac::init_app();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let style = match args.first().map(String::as_str) {
        Some("particles") => EffectStyle::Particles,
        Some("glow") => EffectStyle::Glow,
        Some("smoke") => EffectStyle::Smoke,
        _ => EffectStyle::Rift,
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
    let kind = if args.get(5).map(String::as_str) == Some("entry") { Kind::Entry } else { Kind::Exit };
    let overlay = match Overlay::new() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let look = Look { style, duration: Duration::from_millis(ms), rgb: (0xF2, 0xF6, 0xFF) };
    overlay.show(Burst { x, y, kind, look, edge });
    let wait = Duration::from_millis(ms + 200);
    #[cfg(windows)]
    std::thread::sleep(wait);
    #[cfg(target_os = "macos")]
    mac::run_loop(wait);
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::time::Duration;

    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: bool) -> i32;
    }

    /// The overlay window needs an NSApplication (the tray app has one).
    pub fn init_app() {
        // SAFETY: called on the main thread before any other AppKit use.
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            // accessory = no Dock icon
            let _: bool = msg_send![app, setActivationPolicy: 1isize];
        }
    }

    /// Runs the main run loop (it drives the overlay's frames) for `d`.
    pub fn run_loop(d: Duration) {
        // SAFETY: runs the current (main) thread's run loop in its default mode.
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, d.as_secs_f64(), false);
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn main() {
    eprintln!("this example needs Windows or macOS");
}
