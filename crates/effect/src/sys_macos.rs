//! macOS: cursor queries (CoreGraphics, any thread) and the overlay – a
//! borderless, transparent, click-through NSWindow above everything, animated
//! with Core Animation. AppKit requires the main thread: `Overlay::new` and
//! `Overlay::show` must run there (the tray app dispatches to it).
//!
//! Coordinates in and out of this module are CoreGraphics global display
//! coordinates (points, origin top-left of the main display), the same space
//! Deskflow uses.

use std::cell::RefCell;
use std::ffi::c_void;

use objc2::encode::{Encoding, RefEncode};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send, sel, MainThreadMarker};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::render::{particle, puff, rnd, PARTICLES, PUFFS};
use crate::trail::Rect;
use crate::{Burst, Error, Kind};
use ddc_core::config::EffectStyle;

#[repr(C)]
pub struct CGColor {
    _private: [u8; 0],
}

// SAFETY: CGColorRef is an opaque pointer to `struct CGColor`.
unsafe impl RefEncode for CGColor {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *mut c_void) -> NSPoint;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> NSRect;
    fn CGGetDisplaysWithPoint(point: NSPoint, max: u32, displays: *mut u32, count: *mut u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

/// Nothing to do: CoreGraphics coordinates are always in points.
pub fn dpi_aware_thread() {}

pub fn cursor_position() -> Option<(f64, f64)> {
    // SAFETY: CGEventCreate(NULL) returns an owned event (or NULL) that we release.
    unsafe {
        let ev = CGEventCreate(std::ptr::null());
        if ev.is_null() {
            return None;
        }
        let p = CGEventGetLocation(ev);
        CFRelease(ev);
        Some((p.x, p.y))
    }
}

pub fn display_bounds_at(x: f64, y: f64) -> Option<Rect> {
    let mut id: u32 = 0;
    let mut count: u32 = 0;
    // SAFETY: room for exactly one display id.
    unsafe {
        if CGGetDisplaysWithPoint(NSPoint::new(x, y), 1, &mut id, &mut count) != 0 || count == 0 {
            return None;
        }
        let b = CGDisplayBounds(id);
        Some(Rect {
            left: b.origin.x,
            top: b.origin.y,
            right: b.origin.x + b.size.width,
            bottom: b.origin.y + b.size.height,
        })
    }
}

/// Burst radius in points.
const RADIUS: f64 = 90.0;

pub struct Overlay {
    window: Retained<AnyObject>,
    root: Retained<AnyObject>,
    layers: RefCell<Vec<Retained<AnyObject>>>,
    _main: MainThreadMarker,
}

impl Overlay {
    pub fn new() -> Result<Self, Error> {
        let main = MainThreadMarker::new().ok_or_else(|| Error::Overlay("must be created on the main thread".into()))?;
        // SAFETY: AppKit calls on the main thread with correct argument types.
        unsafe {
            let alloc: Allocated<AnyObject> = msg_send![class!(NSWindow), alloc];
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(16.0, 16.0));
            // styleMask 0 = borderless, backing 2 = buffered
            let window: Option<Retained<AnyObject>> =
                msg_send![alloc, initWithContentRect: rect, styleMask: 0usize, backing: 2usize, defer: false];
            let window = window.ok_or_else(|| Error::Overlay("NSWindow init failed".into()))?;
            let clear: Retained<AnyObject> = msg_send![class!(NSColor), clearColor];
            let _: () = msg_send![&*window, setReleasedWhenClosed: false];
            let _: () = msg_send![&*window, setOpaque: false];
            let _: () = msg_send![&*window, setBackgroundColor: &*clear];
            let _: () = msg_send![&*window, setHasShadow: false];
            let _: () = msg_send![&*window, setIgnoresMouseEvents: true];
            // above full-screen apps and the menu bar
            let _: () = msg_send![&*window, setLevel: 1000isize];
            // all spaces | stationary | not in window cycle | full-screen auxiliary
            let _: () = msg_send![&*window, setCollectionBehavior: (1usize | 16 | 64 | 256)];

            let view: Option<Retained<AnyObject>> = msg_send![&*window, contentView];
            let view = view.ok_or_else(|| Error::Overlay("no content view".into()))?;
            let _: () = msg_send![&*view, setWantsLayer: true];
            let root: Option<Retained<AnyObject>> = msg_send![&*view, layer];
            let root = root.ok_or_else(|| Error::Overlay("content view has no layer".into()))?;
            Ok(Self { window, root, layers: RefCell::new(Vec::new()), _main: main })
        }
    }

    /// Main thread only (enforced by the type: `Overlay` is not `Send`).
    pub fn show(&self, b: Burst) {
        let r = match (b.look.style, b.kind) {
            (EffectStyle::Smoke, Kind::Exit) => RADIUS * 1.4,
            (EffectStyle::Smoke, Kind::Entry) => RADIUS * 1.12,
            (_, Kind::Exit) => RADIUS,
            (_, Kind::Entry) => RADIUS * 0.6,
        };
        let dur = b.look.duration.as_secs_f64().max(0.05);
        let (cr, cg, cb) = b.look.rgb;
        let rgb = (cr as f64 / 255.0, cg as f64 / 255.0, cb as f64 / 255.0);
        // SAFETY: AppKit / Core Animation calls on the main thread.
        unsafe {
            let main_h = CGDisplayBounds(CGMainDisplayID()).size.height;
            let frame = NSRect::new(NSPoint::new(b.x - r, main_h - b.y - r), NSSize::new(2.0 * r, 2.0 * r));
            let _: () = msg_send![class!(NSObject), cancelPreviousPerformRequestsWithTarget: &*self.window];
            let _: () = msg_send![&*self.window, setFrame: frame, display: false];

            begin_no_actions();
            for l in self.layers.borrow_mut().drain(..) {
                let _: () = msg_send![&*l, removeFromSuperlayer];
            }
            let center = NSPoint::new(r, r);
            let mut new_layers = Vec::new();
            match b.look.style {
                EffectStyle::Smoke => {
                    // light along the edge (layer y axis points up: flip screen y)
                    if let Some(edge) = b.edge {
                        let (w, h) = if edge.vertical() { (r * 0.4, r * 1.6) } else { (r * 1.6, r * 0.4) };
                        let halo = radial(center, w, h, rgb, 0.85, 0.55);
                        animate(&halo, "opacity", &number(1.0), &number(0.0), dur * 0.75);
                        let (cw, ch) = if edge.vertical() { (r * 0.1, r * 1.3) } else { (r * 1.3, r * 0.1) };
                        let core = radial(center, cw, ch, rgb, 0.9, 0.95);
                        animate(&core, "opacity", &number(1.0), &number(0.0), dur * 0.6);
                        new_layers.push(halo);
                        new_layers.push(core);
                    }
                    let inward = b.edge.map_or((0.0, -0.4), |e| e.inward());
                    let seed = (b.x as u32).wrapping_mul(31) ^ (b.y as u32);
                    for i in 0..PUFFS {
                        let (sx, sy, ex, ey, r0, r1, w) = puff(seed, i, inward);
                        let at = |px: f32, py: f32| NSPoint::new(r + px as f64 * r, r - py as f64 * r);
                        let d = 2.0 * r1 as f64 * r;
                        let p = radial(at(sx, sy), d, d, rgb, 0.3, 0.75);
                        animate(&p, "position", &point_value(at(sx, sy)), &point_value(at(ex, ey)), dur);
                        animate(&p, "transform.scale", &number((r0 / r1) as f64), &number(1.0), dur);
                        animate(&p, "opacity", &number((w as f64).min(1.0)), &number(0.0), dur);
                        new_layers.push(p);
                    }
                }
                EffectStyle::Glow => {
                    let core = disc(center, r * 0.35, rgb, 0.3, 0.9, r * 0.35);
                    animate(&core, "transform.scale", &number(0.6), &number(1.6), dur);
                    animate(&core, "opacity", &number(1.0), &number(0.0), dur);
                    let ring = ring(center, r * 0.5, r * 0.05, rgb);
                    animate(&ring, "transform.scale", &number(0.4), &number(1.8), dur);
                    animate(&ring, "opacity", &number(0.9), &number(0.0), dur);
                    new_layers.push(core);
                    new_layers.push(ring);
                }
                EffectStyle::Particles => {
                    let flash = disc(center, r * 0.22, rgb, 0.4, 0.9, r * 0.3);
                    animate(&flash, "opacity", &number(1.0), &number(0.0), dur * 0.5);
                    new_layers.push(flash);
                    let seed = (b.x as u32).wrapping_mul(31) ^ (b.y as u32);
                    for i in 0..PARTICLES {
                        let (dx, dy, speed, sz) = particle(seed, i);
                        let rad = (1.5 + 2.0 * sz as f64) * (r / 120.0).max(0.5);
                        let p = disc(center, rad, rgb, 0.35, 1.0, rad * 1.5);
                        let dist = r * 0.9 * speed as f64;
                        let to = NSPoint::new(r + dx as f64 * dist, r + dy as f64 * dist);
                        let alpha = 0.55 + 0.45 * rnd(seed, i, 4) as f64;
                        animate(&p, "position", &point_value(center), &point_value(to), dur);
                        animate(&p, "opacity", &number(alpha), &number(0.0), dur);
                        new_layers.push(p);
                    }
                }
            }
            for l in &new_layers {
                let _: () = msg_send![&*self.root, addSublayer: &**l];
            }
            *self.layers.borrow_mut() = new_layers;
            commit();

            let _: () = msg_send![&*self.window, orderFrontRegardless];
            let _: () = msg_send![
                &*self.window,
                performSelector: sel!(orderOut:),
                withObject: std::ptr::null::<AnyObject>(),
                afterDelay: dur + 0.05
            ];
        }
    }
}

unsafe fn begin_no_actions() {
    let _: () = msg_send![class!(CATransaction), begin];
    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
}

unsafe fn commit() {
    let _: () = msg_send![class!(CATransaction), commit];
}

unsafe fn ns_color(rgb: (f64, f64, f64), white: f64, alpha: f64) -> Retained<AnyObject> {
    let m = |v: f64| v + (1.0 - v) * white;
    msg_send![class!(NSColor), colorWithSRGBRed: m(rgb.0), green: m(rgb.1), blue: m(rgb.2), alpha: alpha]
}

unsafe fn number(v: f64) -> Retained<AnyObject> {
    msg_send![class!(NSNumber), numberWithDouble: v]
}

unsafe fn point_value(p: NSPoint) -> Retained<AnyObject> {
    msg_send![class!(NSValue), valueWithPoint: p]
}

unsafe fn new_layer(center: NSPoint, radius: f64) -> Retained<AnyObject> {
    let l: Retained<AnyObject> = msg_send![class!(CALayer), layer];
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(2.0 * radius, 2.0 * radius));
    let _: () = msg_send![&*l, setBounds: bounds];
    let _: () = msg_send![&*l, setPosition: center];
    let _: () = msg_send![&*l, setCornerRadius: radius];
    l
}

/// Soft ellipse: radial gradient from `alpha` in the middle to clear at the bounds.
unsafe fn radial(center: NSPoint, w: f64, h: f64, rgb: (f64, f64, f64), white: f64, alpha: f64) -> Retained<AnyObject> {
    let l: Retained<AnyObject> = msg_send![class!(CAGradientLayer), layer];
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
    let _: () = msg_send![&*l, setBounds: bounds];
    let _: () = msg_send![&*l, setPosition: center];
    let inner = ns_color(rgb, white, alpha);
    let outer = ns_color(rgb, white, 0.0);
    let c1: *const CGColor = msg_send![&*inner, CGColor];
    let c2: *const CGColor = msg_send![&*outer, CGColor];
    // CGColorRefs are valid Objective-C objects inside an NSArray
    let objs: [*const AnyObject; 2] = [c1.cast(), c2.cast()];
    let colors: Retained<AnyObject> = msg_send![class!(NSArray), arrayWithObjects: objs.as_ptr(), count: 2usize];
    let _: () = msg_send![&*l, setColors: &*colors];
    let radial = NSString::from_str("radial");
    let _: () = msg_send![&*l, setType: &*radial];
    let _: () = msg_send![&*l, setStartPoint: NSPoint::new(0.5, 0.5)];
    let _: () = msg_send![&*l, setEndPoint: NSPoint::new(1.0, 1.0)];
    l
}

/// Filled circle with a soft glow (shadow) around it.
unsafe fn disc(center: NSPoint, radius: f64, rgb: (f64, f64, f64), white: f64, alpha: f64, glow: f64) -> Retained<AnyObject> {
    let l = new_layer(center, radius);
    let fill = ns_color(rgb, white, alpha);
    let shadow = ns_color(rgb, 0.0, 1.0);
    let fill_cg: *const CGColor = msg_send![&*fill, CGColor];
    let shadow_cg: *const CGColor = msg_send![&*shadow, CGColor];
    let _: () = msg_send![&*l, setBackgroundColor: fill_cg];
    let _: () = msg_send![&*l, setShadowColor: shadow_cg];
    let _: () = msg_send![&*l, setShadowRadius: glow];
    let _: () = msg_send![&*l, setShadowOpacity: 1.0f32];
    let _: () = msg_send![&*l, setShadowOffset: NSSize::new(0.0, 0.0)];
    l
}

unsafe fn ring(center: NSPoint, radius: f64, width: f64, rgb: (f64, f64, f64)) -> Retained<AnyObject> {
    let l = new_layer(center, radius);
    let color = ns_color(rgb, 0.0, 1.0);
    let cg: *const CGColor = msg_send![&*color, CGColor];
    let _: () = msg_send![&*l, setBorderColor: cg];
    let _: () = msg_send![&*l, setBorderWidth: width];
    let _: () = msg_send![&*l, setShadowColor: cg];
    let _: () = msg_send![&*l, setShadowRadius: width * 3.0];
    let _: () = msg_send![&*l, setShadowOpacity: 1.0f32];
    let _: () = msg_send![&*l, setShadowOffset: NSSize::new(0.0, 0.0)];
    l
}

/// Explicit CABasicAnimation that stays at its end value.
unsafe fn animate(layer: &AnyObject, key: &str, from: &AnyObject, to: &AnyObject, dur: f64) {
    let key = NSString::from_str(key);
    let a: Retained<AnyObject> = msg_send![class!(CABasicAnimation), animationWithKeyPath: &*key];
    let ease = NSString::from_str("easeOut");
    let timing: Retained<AnyObject> = msg_send![class!(CAMediaTimingFunction), functionWithName: &*ease];
    let forwards = NSString::from_str("forwards");
    let _: () = msg_send![&*a, setFromValue: from];
    let _: () = msg_send![&*a, setToValue: to];
    let _: () = msg_send![&*a, setDuration: dur];
    let _: () = msg_send![&*a, setTimingFunction: &*timing];
    let _: () = msg_send![&*a, setFillMode: &*forwards];
    let _: () = msg_send![&*a, setRemovedOnCompletion: false];
    let _: () = msg_send![layer, addAnimation: &*a, forKey: &*key];
}
