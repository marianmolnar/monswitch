//! macOS: cursor queries (CoreGraphics, any thread) and the overlay – a
//! borderless, transparent, click-through NSWindow above everything. Every
//! frame is drawn by the shared software renderer (the same as on Windows, so
//! all styles look identical) into a bitmap that becomes the contents of a
//! CALayer. A run-loop timer on the main thread drives the frames. AppKit
//! requires the main thread: `Overlay::new` and `Overlay::show` must run there
//! (the tray app dispatches to it).
//!
//! Coordinates in and out of this module are CoreGraphics global display
//! coordinates (points, origin top-left of the main display), the same space
//! Deskflow uses.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::time::Instant;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send, MainThreadMarker};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::render;
use crate::trail::Rect;
use crate::{Burst, Error};

type CFTypeRef = *const c_void;

#[repr(C)]
struct CFRunLoopTimerContext {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> *const c_void>,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *mut c_void) -> NSPoint;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> NSRect;
    fn CGGetDisplaysWithPoint(point: NSPoint, max: u32, displays: *mut u32, count: *mut u32) -> i32;
    static kCGColorSpaceSRGB: CFTypeRef;
    fn CGColorSpaceCreateWithName(name: CFTypeRef) -> *mut c_void;
    fn CGDataProviderCreateWithCFData(data: CFTypeRef) -> *mut c_void;
    #[allow(clippy::too_many_arguments)]
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        space: *mut c_void,
        bitmap_info: u32,
        provider: *mut c_void,
        decode: *const f64,
        should_interpolate: bool,
        intent: i32,
    ) -> *mut c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: isize) -> *mut c_void;
    fn CFAbsoluteTimeGetCurrent() -> f64;
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFRunLoopTimerCreate(
        allocator: CFTypeRef,
        fire_date: f64,
        interval: f64,
        flags: usize,
        order: isize,
        callout: extern "C" fn(*mut c_void, *mut c_void),
        context: *mut CFRunLoopTimerContext,
    ) -> *mut c_void;
    fn CFRunLoopAddTimer(rl: *mut c_void, timer: *mut c_void, mode: CFTypeRef);
    fn CFRunLoopTimerInvalidate(timer: *mut c_void);
    static kCFRunLoopCommonModes: CFTypeRef;
}

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

/// `0xAARRGGBB` words in little-endian memory = BGRA bytes, alpha premultiplied
/// (kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst).
const BITMAP_INFO: u32 = (2 << 12) | 2;
/// Frame interval of the animation timer (the overlay runs at up to 120 Hz).
const FRAME_SECS: f64 = 1.0 / 120.0;

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

/// The burst being animated.
struct Anim {
    burst: Burst,
    start: Instant,
    /// Side of the bitmap in device pixels (points × backing scale).
    px: usize,
    seed: u32,
}

/// Overlay state. Boxed so its address stays fixed: the run-loop timer keeps
/// a pointer to it.
struct Inner {
    window: Retained<AnyObject>,
    /// Sublayer of the content view's layer that shows the rendered frames.
    image: Retained<AnyObject>,
    color_space: *mut c_void,
    anim: RefCell<Option<Anim>>,
    pixels: RefCell<Vec<u32>>,
    timer: Cell<*mut c_void>,
    seed: Cell<u32>,
}

pub struct Overlay {
    inner: Box<Inner>,
    _main: MainThreadMarker,
}

impl Overlay {
    pub fn new() -> Result<Self, Error> {
        let main = MainThreadMarker::new().ok_or_else(|| Error::Overlay("must be created on the main thread".into()))?;
        // SAFETY: AppKit / Core Animation calls on the main thread with correct
        // argument types; the colour space is released in Drop.
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
            let image: Retained<AnyObject> = msg_send![class!(CALayer), layer];
            let _: () = msg_send![&*root, addSublayer: &*image];

            let color_space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
            if color_space.is_null() {
                return Err(Error::Overlay("cannot create the sRGB colour space".into()));
            }
            let inner = Box::new(Inner {
                window,
                image,
                color_space,
                anim: RefCell::new(None),
                pixels: RefCell::new(Vec::new()),
                timer: Cell::new(std::ptr::null_mut()),
                seed: Cell::new(0x5EED),
            });
            Ok(Self { inner, _main: main })
        }
    }

    /// Main thread only (enforced by the type: `Overlay` is not `Send`).
    pub fn show(&self, b: Burst) {
        let inner = &*self.inner;
        let r = render::RADIUS * render::size_factor(b.look.style, b.kind);
        let seed = inner.seed.get().wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        inner.seed.set(seed);
        // SAFETY: AppKit / Core Animation calls on the main thread.
        unsafe {
            let main_h = CGDisplayBounds(CGMainDisplayID()).size.height;
            let frame = NSRect::new(NSPoint::new(b.x - r, main_h - b.y - r), NSSize::new(2.0 * r, 2.0 * r));
            let _: () = msg_send![&*inner.window, setFrame: frame, display: false];
            let scale: f64 = msg_send![&*inner.window, backingScaleFactor];
            let scale = if scale.is_finite() && scale >= 1.0 { scale } else { 1.0 };

            begin_no_actions();
            let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(2.0 * r, 2.0 * r));
            let _: () = msg_send![&*inner.image, setFrame: bounds];
            let _: () = msg_send![&*inner.image, setContentsScale: scale];
            commit();

            let px = ((2.0 * r * scale).round() as usize).max(2);
            *inner.anim.borrow_mut() = Some(Anim { burst: b, start: Instant::now(), px, seed });
            inner.draw(0.0);
            let _: () = msg_send![&*inner.window, orderFrontRegardless];
        }
        inner.start_timer();
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.inner.stop_timer();
        // SAFETY: main thread (Overlay is not Send); created in Overlay::new.
        unsafe {
            let _: () = msg_send![&*self.inner.window, orderOut: std::ptr::null::<AnyObject>()];
            CFRelease(self.inner.color_space);
        }
    }
}

impl Inner {
    /// Called by the timer on the main run loop.
    fn tick(&self) {
        let t = match self.anim.borrow().as_ref() {
            Some(a) => a.start.elapsed().as_secs_f32() / a.burst.look.duration.as_secs_f32().max(0.05),
            None => 1.0,
        };
        if t < 1.0 {
            self.draw(t);
        } else {
            self.finish();
        }
    }

    /// Renders frame `t` of the current burst into the layer.
    fn draw(&self, t: f32) {
        let anim = self.anim.borrow();
        let Some(a) = anim.as_ref() else {
            return;
        };
        let n = a.px * a.px;
        let mut pixels = self.pixels.borrow_mut();
        if pixels.len() < n {
            pixels.resize(n, 0);
        }
        let b = &a.burst;
        render::render(&mut pixels[..n], a.px, a.px, t, b.look.style, b.look.rgb, a.seed, b.edge, b.kind);

        // SAFETY: the bytes are copied into a CFData, so the image owns its
        // pixels and the next frame cannot change one that is on screen;
        // every CF object created here is released here.
        unsafe {
            let bytes = std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), n * 4);
            let data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
            if data.is_null() {
                return;
            }
            let provider = CGDataProviderCreateWithCFData(data);
            CFRelease(data);
            if provider.is_null() {
                return;
            }
            let image = CGImageCreate(
                a.px,
                a.px,
                8,
                32,
                a.px * 4,
                self.color_space,
                BITMAP_INFO,
                provider,
                std::ptr::null(),
                false,
                0,
            );
            CFRelease(provider);
            if image.is_null() {
                return;
            }
            begin_no_actions();
            let _: () = msg_send![&*self.image, setContents: image.cast::<AnyObject>()];
            commit();
            CFRelease(image);
        }
    }

    fn finish(&self) {
        self.stop_timer();
        *self.anim.borrow_mut() = None;
        // SAFETY: AppKit / Core Animation calls on the main thread.
        unsafe {
            let _: () = msg_send![&*self.window, orderOut: std::ptr::null::<AnyObject>()];
            begin_no_actions();
            let _: () = msg_send![&*self.image, setContents: std::ptr::null::<AnyObject>()];
            commit();
        }
    }

    fn start_timer(&self) {
        if !self.timer.get().is_null() {
            return;
        }
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: (self as *const Inner).cast_mut().cast(),
            retain: None,
            release: None,
            copy_description: None,
        };
        // SAFETY: `info` points to this boxed Inner, which outlives the timer:
        // the timer is invalidated in finish() and in Overlay's Drop. The
        // timer runs on the main run loop, the thread that owns the overlay.
        unsafe {
            let timer = CFRunLoopTimerCreate(
                std::ptr::null(),
                CFAbsoluteTimeGetCurrent() + FRAME_SECS,
                FRAME_SECS,
                0,
                0,
                on_timer,
                &mut ctx,
            );
            if timer.is_null() {
                log::warn!("effect: cannot create the animation timer");
                return;
            }
            // common modes: keep animating while a menu is open
            CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopCommonModes);
            self.timer.set(timer);
        }
    }

    fn stop_timer(&self) {
        let timer = self.timer.replace(std::ptr::null_mut());
        if !timer.is_null() {
            // SAFETY: created in start_timer and not released yet.
            unsafe {
                CFRunLoopTimerInvalidate(timer);
                CFRelease(timer);
            }
        }
    }
}

extern "C" fn on_timer(_timer: *mut c_void, info: *mut c_void) {
    // SAFETY: `info` is the Inner registered in start_timer; it is alive
    // while the timer is (see start_timer), and the callback runs on the main
    // thread.
    let inner = unsafe { &*info.cast::<Inner>() };
    inner.tick();
}

unsafe fn begin_no_actions() {
    let _: () = msg_send![class!(CATransaction), begin];
    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
}

unsafe fn commit() {
    let _: () = msg_send![class!(CATransaction), commit];
}
