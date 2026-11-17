//! Windows: cursor queries and the overlay – a layered, click-through,
//! topmost tool window that never takes focus. It lives on its own thread
//! (created once, hidden) and is fed bursts through a channel; frames are
//! rendered in software into a DIB and pushed with `UpdateLayeredWindow`.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, MonitorFromPoint,
    ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS, HBITMAP, HDC, MONITORINFO, MONITOR_DEFAULTTONULL,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetSystemMetrics, PeekMessageW,
    RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow, HWND_TOPMOST, MSG, PM_REMOVE,
    SM_CYVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::render;
use crate::trail::Rect;
use crate::{Burst, Error};

/// Physical-pixel coordinates on the calling thread (cursor, window
/// positions) regardless of the host process' DPI awareness – the same space
/// Deskflow uses.
pub fn dpi_aware_thread() {
    // SAFETY: only changes the calling thread's DPI awareness.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn cursor_position() -> Option<(f64, f64)> {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: valid out pointer.
    (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x as f64, p.y as f64))
}

pub fn display_bounds_at(x: f64, y: f64) -> Option<Rect> {
    // SAFETY: plain Win32 calls with a correctly sized MONITORINFO.
    unsafe {
        let mon = MonitorFromPoint(POINT { x: x as i32, y: y as i32 }, MONITOR_DEFAULTTONULL);
        if mon.is_null() {
            return None;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut mi) == 0 {
            return None;
        }
        let r: RECT = mi.rcMonitor;
        Some(Rect { left: r.left as f64, top: r.top as f64, right: r.right as f64, bottom: r.bottom as f64 })
    }
}

pub struct Overlay {
    tx: Sender<Burst>,
}

impl Overlay {
    pub fn new() -> Result<Self, Error> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("effect-overlay".into())
            .spawn(move || {
                dpi_aware_thread();
                match Window::create() {
                    Ok(w) => {
                        let _ = ready_tx.send(Ok(()));
                        run(w, rx);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| Error::Overlay(e.to_string()))?;
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| Error::Overlay("overlay thread did not start".into()))??;
        Ok(Self { tx })
    }

    /// Any thread; never blocks.
    pub fn show(&self, burst: Burst) {
        let _ = self.tx.send(burst);
    }
}

struct Window {
    hwnd: HWND,
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u32,
    /// DIB is `max`×`max` pixels.
    max: usize,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl Window {
    fn create() -> Result<Self, Error> {
        // SAFETY: standard window/DIB creation; every handle is checked and
        // released in Drop.
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let class = wide("monswitch-effect-overlay");
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(DefWindowProcW),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            // fails harmlessly if the class already exists
            RegisterClassW(&wc);
            let title = wide("");
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return Err(Error::Overlay("CreateWindowExW failed".into()));
            }

            // big enough for the tallest display (physical pixels: the thread is DPI aware)
            let tallest = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1080) as f64;
            let max = (render::radius_for(tallest) * render::MAX_SIZE_FACTOR * 2.0).ceil() as usize + 2;

            let screen_dc = GetDC(std::ptr::null_mut());
            let mem_dc = CreateCompatibleDC(screen_dc);
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: max as i32,
                biHeight: -(max as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() || mem_dc.is_null() {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                if !mem_dc.is_null() {
                    DeleteDC(mem_dc);
                }
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                DestroyWindow(hwnd);
                return Err(Error::Overlay("CreateDIBSection failed".into()));
            }
            SelectObject(mem_dc, bitmap);
            Ok(Self { hwnd, screen_dc, mem_dc, bitmap, bits: bits.cast(), max })
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: the DIB holds max*max 32-bit pixels and lives as long as self.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.max * self.max) }
    }

    /// Renders frame `t` and moves/updates the window so the square is centred on (cx, cy).
    fn frame(&mut self, b: &Burst, size: usize, t: f32, seed: u32) {
        let max = self.max;
        let size = size.min(max);
        render::render(self.pixels(), max, size, t, b.look.style, b.look.rgb, seed, b.edge, b.kind);
        let half = (size / 2) as i32;
        let dst = POINT { x: b.x.round() as i32 - half, y: b.y.round() as i32 - half };
        let sz = SIZE { cx: size as i32, cy: size as i32 };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: all handles are valid for the lifetime of self.
        unsafe {
            UpdateLayeredWindow(self.hwnd, self.screen_dc, &dst, &sz, self.mem_dc, &src, 0, &blend, ULW_ALPHA);
        }
    }

    fn show(&self) {
        // SAFETY: valid window handle.
        unsafe {
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    fn hide(&self) {
        // SAFETY: valid window handle.
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: handles created in Window::create, released once.
        unsafe {
            DeleteDC(self.mem_dc);
            DeleteObject(self.bitmap);
            ReleaseDC(std::ptr::null_mut(), self.screen_dc);
            DestroyWindow(self.hwnd);
        }
    }
}

fn pump() {
    // SAFETY: standard message loop on the thread that owns the window.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

struct Anim {
    burst: Burst,
    start: Instant,
    size: usize,
    seed: u32,
}

fn run(mut w: Window, rx: Receiver<Burst>) {
    let mut anim: Option<Anim> = None;
    let mut seed: u32 = 0x5EED;
    loop {
        pump();
        let wait = if anim.is_some() { Duration::from_millis(8) } else { Duration::from_millis(250) };
        match rx.recv_timeout(wait) {
            Ok(b) => {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let factor = render::size_factor(b.look.style, b.kind);
                let size = (crate::burst_radius(b.x, b.y) * factor * 2.0).round() as usize;
                anim = Some(Anim { burst: b, start: Instant::now(), size, seed });
                if let Some(a) = &anim {
                    w.frame(&a.burst, a.size, 0.0, a.seed);
                }
                w.show();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(a) = &anim {
            let total = a.burst.look.duration.as_secs_f32().max(0.05);
            let t = a.start.elapsed().as_secs_f32() / total;
            if t >= 1.0 {
                w.hide();
                anim = None;
            } else {
                let (b, size, s) = (a.burst.clone(), a.size, a.seed);
                w.frame(&b, size, t, s);
            }
        }
    }
}
