//! Windows: one invisible, topmost strip window per edge, registered as an
//! OLE drop target. A strip is shown only while a mouse button has been held
//! and dragged near its edge, so normal clicks never land on it.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{implement, w, Ref, Result as WinResult};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, POINTL, WPARAM};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
    VK_LBUTTON, VK_RBUTTON,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetSystemMetrics, PeekMessageW, RegisterClassW, SetCursorPos, SetLayeredWindowAttributes,
    SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG, PM_REMOVE,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_SWAPBUTTON, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::{Callbacks, Dropped, Edge, Error};

/// Strip width at 96 DPI.
const STRIP_96DPI: f64 = 6.0;
/// The cursor must be this close to the edge (96 DPI) for the strip to appear.
const ARM_96DPI: f64 = 64.0;
/// A held button counts as a drag after this long and this far (96 DPI).
const DRAG_AFTER: Duration = Duration::from_millis(120);
const DRAG_MOVE_96DPI: f64 = 8.0;
const POLL: Duration = Duration::from_millis(15);

/// Watches the given edges for file drags while it lives.
pub struct EdgeDrop {
    tx: Sender<Vec<Edge>>,
}

impl EdgeDrop {
    pub fn start(edges: Vec<Edge>, callbacks: Callbacks) -> Result<Self, Error> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("dnd-edge".into())
            .spawn(move || run(edges, rx, callbacks, ready_tx))
            .map_err(|e| Error::Sys(e.to_string()))?;
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| Error::Sys("edge drop thread did not start".into()))??;
        Ok(Self { tx })
    }

    /// Edges that lead to the other machine (e.g. after the Deskflow layout changed).
    pub fn set_edges(&self, edges: Vec<Edge>) {
        let _ = self.tx.send(edges);
    }
}

/// Hands the cursor over to the other machine through `edge`: puts it on the
/// edge and pushes it outwards, which Deskflow turns into a screen switch.
/// Call it after a drop has finished (no mouse button held).
pub fn cross_edge(edge: Edge) -> Result<(), Error> {
    // physical pixels for this call, whatever the caller's thread uses
    // SAFETY: restored below.
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let result = push_out(edge);
    if !previous.is_invalid() {
        // SAFETY: restores the caller's context.
        unsafe { SetThreadDpiAwarenessContext(previous) };
    }
    result
}

fn push_out(edge: Edge) -> Result<(), Error> {
    let vs = virtual_screen();
    let mut p = POINT::default();
    // SAFETY: plain Win32 calls with valid pointers.
    unsafe {
        GetCursorPos(&mut p).map_err(|e| Error::Sys(format!("GetCursorPos: {e}")))?;
        let (x, y) = match edge {
            Edge::Left => (vs.left, p.y),
            Edge::Right => (vs.right - 1, p.y),
            Edge::Top => (p.x, vs.top),
            Edge::Bottom => (p.x, vs.bottom - 1),
        };
        SetCursorPos(x, y).map_err(|e| Error::Sys(format!("SetCursorPos: {e}")))?;
    }
    let (dx, dy) = edge.outward();
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(15));
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: dx * 6,
                    dy: dy * 6,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: one valid INPUT.
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        if sent != 1 {
            return Err(Error::Sys("SendInput was blocked".into()));
        }
    }
    Ok(())
}

/// Cursor position in physical pixels.
pub fn cursor_position() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    // SAFETY: valid out pointer; the DPI context is restored.
    unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let ok = GetCursorPos(&mut p).is_ok();
        if !previous.is_invalid() {
            SetThreadDpiAwarenessContext(previous);
        }
        ok.then_some((p.x, p.y))
    }
}

#[derive(Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

fn virtual_screen() -> Rect {
    // SAFETY: GetSystemMetrics has no preconditions.
    unsafe {
        let (x, y) = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        );
        Rect {
            left: x,
            top: y,
            right: x + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: y + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// Distance from `p` to `edge` of `r` (0 on the edge pixel).
fn distance(edge: Edge, r: Rect, p: POINT) -> i32 {
    match edge {
        Edge::Left => p.x - r.left,
        Edge::Right => r.right - 1 - p.x,
        Edge::Top => p.y - r.top,
        Edge::Bottom => r.bottom - 1 - p.y,
    }
}

/// The strip along `edge` of `r`: (x, y, width, height).
fn strip(edge: Edge, r: Rect, width: i32) -> (i32, i32, i32, i32) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    match edge {
        Edge::Left => (r.left, r.top, width, h),
        Edge::Right => (r.right - width, r.top, width, h),
        Edge::Top => (r.left, r.top, w, width),
        Edge::Bottom => (r.left, r.bottom - width, w, width),
    }
}

struct Shared {
    callbacks: Callbacks,
    /// A file drag is over one of the strips.
    hovering: AtomicBool,
}

#[implement(IDropTarget)]
struct DropTarget {
    edge: Edge,
    shared: Arc<Shared>,
    accept: Cell<bool>,
}

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn has_files(data: &IDataObject) -> bool {
    let fmt = hdrop_format();
    // SAFETY: valid FORMATETC.
    unsafe { data.QueryGetData(&fmt).is_ok() }
}

fn files_of(data: &IDataObject) -> Vec<PathBuf> {
    let fmt = hdrop_format();
    let mut out = Vec::new();
    // SAFETY: the HGLOBAL of a CF_HDROP medium is an HDROP; the medium is
    // released after use.
    unsafe {
        let Ok(mut medium) = data.GetData(&fmt) else {
            return out;
        };
        let hdrop = HDROP(medium.u.hGlobal.0);
        let n = DragQueryFileW(hdrop, u32::MAX, None);
        for i in 0..n {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
            if got > 0 {
                out.push(PathBuf::from(String::from_utf16_lossy(&buf[..got])));
            }
        }
        ReleaseStgMedium(&mut medium);
    }
    out
}

fn set_effect(effect: *mut DROPEFFECT, accept: bool) {
    if !effect.is_null() {
        // SAFETY: OLE passes a valid out pointer.
        unsafe {
            *effect = if accept {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            }
        };
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let accept = data.as_ref().is_some_and(has_files);
        self.accept.set(accept);
        set_effect(effect, accept);
        if accept {
            self.shared.hovering.store(true, Ordering::Relaxed);
            (self.shared.callbacks.on_hover)(Some(self.edge));
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        set_effect(effect, self.accept.get());
        Ok(())
    }

    fn DragLeave(&self) -> WinResult<()> {
        if self.accept.replace(false) {
            self.shared.hovering.store(false, Ordering::Relaxed);
            (self.shared.callbacks.on_hover)(None);
        }
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let files = if self.accept.replace(false) {
            data.as_ref().map(files_of).unwrap_or_default()
        } else {
            Vec::new()
        };
        set_effect(effect, !files.is_empty());
        self.shared.hovering.store(false, Ordering::Relaxed);
        (self.shared.callbacks.on_hover)(None);
        if !files.is_empty() {
            log::info!(
                "dnd: {} file(s) dropped on the {:?} edge",
                files.len(),
                self.edge
            );
            (self.shared.callbacks.on_drop)(Dropped {
                files,
                edge: self.edge,
                x: pt.x,
                y: pt.y,
            });
        }
        Ok(())
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: default handling of our own window's messages.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

struct StripWindow {
    edge: Edge,
    hwnd: HWND,
    shown: bool,
}

fn create_strips(shared: &Arc<Shared>) -> Result<Vec<StripWindow>, Error> {
    let sys = |what: &str, e: windows::core::Error| Error::Sys(format!("{what}: {e}"));
    // SAFETY: window class and windows owned by this thread; the drop targets
    // are reference counted by OLE.
    unsafe {
        let hinst = GetModuleHandleW(None).map_err(|e| sys("GetModuleHandleW", e))?;
        let class = w!("monswitch-dnd-edge");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // fails harmlessly when already registered
        RegisterClassW(&wc);
        let mut out = Vec::new();
        for edge in Edge::ALL {
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .map_err(|e| sys("CreateWindowExW", e))?;
            // alpha 1: invisible, but still hit-tested (alpha 0 is click-through)
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA)
                .map_err(|e| sys("SetLayeredWindowAttributes", e))?;
            let target: IDropTarget = DropTarget {
                edge,
                shared: shared.clone(),
                accept: Cell::new(false),
            }
            .into();
            RegisterDragDrop(hwnd, &target).map_err(|e| sys("RegisterDragDrop", e))?;
            out.push(StripWindow {
                edge,
                hwnd,
                shown: false,
            });
        }
        Ok(out)
    }
}

fn pump() {
    // SAFETY: standard message loop on this thread's queue.
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn primary_button_down() -> bool {
    // SAFETY: no preconditions.
    unsafe {
        let vk = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
            VK_RBUTTON
        } else {
            VK_LBUTTON
        };
        GetAsyncKeyState(i32::from(vk.0)) < 0
    }
}

fn run(
    mut edges: Vec<Edge>,
    rx: Receiver<Vec<Edge>>,
    callbacks: Callbacks,
    ready: Sender<Result<(), Error>>,
) {
    // SAFETY: per-thread DPI awareness and OLE (STA) for this thread.
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if let Err(e) = OleInitialize(None) {
            let _ = ready.send(Err(Error::Sys(format!("OleInitialize: {e}"))));
            return;
        }
    }
    let shared = Arc::new(Shared {
        callbacks,
        hovering: AtomicBool::new(false),
    });
    let mut strips = match create_strips(&shared) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e));
            // SAFETY: balanced with OleInitialize above.
            unsafe { OleUninitialize() };
            return;
        }
    };
    let _ = ready.send(Ok(()));
    // SAFETY: no preconditions.
    let scale = f64::from(unsafe { GetDpiForSystem() }.max(96)) / 96.0;
    let (width, arm, drag_move) = (
        (STRIP_96DPI * scale).round() as i32,
        (ARM_96DPI * scale) as i32,
        DRAG_MOVE_96DPI * scale,
    );
    // where and when the current button press started
    let mut press: Option<(Instant, POINT)> = None;

    loop {
        pump();
        match rx.try_recv() {
            Ok(e) => edges = e,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
        }
        let mut p = POINT::default();
        // SAFETY: valid out pointer.
        let have_pos = unsafe { GetCursorPos(&mut p) }.is_ok();
        let down = primary_button_down();
        press = match (down, press) {
            (false, _) => None,
            (true, None) => Some((Instant::now(), p)),
            (true, some) => some,
        };
        let dragging = press.is_some_and(|(at, from)| {
            let moved = f64::from((p.x - from.x).pow(2) + (p.y - from.y).pow(2)).sqrt();
            at.elapsed() >= DRAG_AFTER && moved >= drag_move
        });
        let hovering = shared.hovering.load(Ordering::Relaxed);
        let vs = virtual_screen();
        for s in &mut strips {
            let near = have_pos && distance(s.edge, vs, p) <= arm;
            let want = edges.contains(&s.edge) && (hovering || (dragging && near));
            if want == s.shown {
                continue;
            }
            // SAFETY: our own window.
            unsafe {
                if want {
                    let (x, y, w, h) = strip(s.edge, vs, width);
                    if let Err(e) = SetWindowPos(
                        s.hwnd,
                        Some(HWND_TOPMOST),
                        x,
                        y,
                        w,
                        h,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    ) {
                        log::warn!("dnd: cannot show the {:?} strip: {e}", s.edge);
                        continue;
                    }
                } else {
                    let _ = ShowWindow(s.hwnd, SW_HIDE);
                }
            }
            s.shown = want;
        }
        std::thread::sleep(POLL);
    }

    // SAFETY: our own windows; balanced with OleInitialize.
    unsafe {
        for s in &strips {
            let _ = RevokeDragDrop(s.hwnd);
            let _ = DestroyWindow(s.hwnd);
        }
        OleUninitialize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_geometry() {
        let r = Rect {
            left: -1920,
            top: 0,
            right: 1920,
            bottom: 2160,
        };
        assert_eq!(strip(Edge::Right, r, 6), (1914, 0, 6, 2160));
        assert_eq!(strip(Edge::Left, r, 6), (-1920, 0, 6, 2160));
        assert_eq!(strip(Edge::Bottom, r, 6), (-1920, 2154, 3840, 6));
        assert_eq!(distance(Edge::Right, r, POINT { x: 1919, y: 5 }), 0);
        assert_eq!(distance(Edge::Left, r, POINT { x: -1900, y: 5 }), 20);
    }
}
