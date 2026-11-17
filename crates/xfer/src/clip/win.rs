//! Windows: clipboard listener + virtual files.
//!
//! One STA thread owns a message-only window that receives
//! `WM_CLIPBOARDUPDATE`, and the OLE clipboard object for remote files.
//! Remote files are an `IDataObject` with `FileGroupDescriptorW` (names,
//! sizes, folders) and `FileContents` (one `IStream` per file, read from the
//! network only when Explorer or another app pastes).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read};
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use windows::core::{implement, w, Ref, BOOL, HRESULT, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, DV_E_FORMATETC, E_FAIL, E_NOTIMPL, FILETIME, HANDLE, HGLOBAL, HWND, LPARAM,
    LRESULT, OLE_E_ADVISENOTSUPPORTED, POINT, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION, S_FALSE,
    S_OK, WPARAM,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
    ISequentialStream_Impl, IStream, IStream_Impl, DATADIR_GET, DVASPECT_CONTENT, FORMATETC,
    LOCKTYPE, STATFLAG, STATFLAG_NONAME, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, GetClipboardFormatNameW, GetClipboardOwner, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use windows::Win32::System::Ole::{
    OleInitialize, OleIsCurrentClipboard, OleSetClipboard, OleUninitialize, ReleaseStgMedium,
    CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{
    DragQueryFileW, SHCreateStdEnumFmtEtc, CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW,
    CFSTR_PREFERREDDROPEFFECT, DROPFILES, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE,
    FD_WRITESTIME, FILEDESCRIPTORW, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowThreadProcessId, PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage,
    HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_CLOSE,
    WM_DESTROY, WNDCLASSW,
};

use super::{Action, Callbacks, Change, RemoteFiles, RunOnMain, State};

const WM_XFER_CMD: u32 = WM_APP + 0x58;
/// FILEDESCRIPTORW.cFileName holds 260 UTF-16 units including the NUL.
const MAX_DESCRIPTOR_PATH: usize = 259;

enum Cmd {
    Offer(RemoteFiles),
    Forget(u64),
}

pub struct Clipboard {
    hwnd: isize,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn hwnd_of(v: isize) -> HWND {
    HWND(v as *mut core::ffi::c_void)
}

impl Clipboard {
    pub fn start(callbacks: Callbacks, _run_on_main: Option<RunOnMain>) -> io::Result<Self> {
        let cmds = Arc::new(Mutex::new(VecDeque::new()));
        let (tx, rx) = mpsc::channel();
        let thread_cmds = cmds.clone();
        let thread = std::thread::Builder::new()
            .name("xfer-clipboard".into())
            .spawn(move || clipboard_thread(callbacks, thread_cmds, tx))?;
        let hwnd = rx
            .recv()
            .map_err(|_| io::Error::other("clipboard thread exited"))?
            .map_err(|e| io::Error::other(format!("clipboard: {e}")))?;
        Ok(Self {
            hwnd,
            cmds,
            thread: Some(thread),
        })
    }

    fn post(&self, cmd: Cmd) {
        lock(&self.cmds).push_back(cmd);
        // SAFETY: plain message post to our own window (may already be gone – then it fails harmlessly).
        if let Err(e) =
            unsafe { PostMessageW(Some(hwnd_of(self.hwnd)), WM_XFER_CMD, WPARAM(0), LPARAM(0)) }
        {
            log::warn!("xfer: clipboard thread unreachable: {e}");
        }
    }

    pub fn offer_remote(&self, files: RemoteFiles) {
        self.post(Cmd::Offer(files));
    }

    pub fn forget_remote(&self, id: u64) {
        self.post(Cmd::Forget(id));
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: see post().
        let _ = unsafe { PostMessageW(Some(hwnd_of(self.hwnd)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- clipboard thread -----------------------------------------------------------

struct Ctx {
    hwnd: HWND,
    callbacks: Callbacks,
    state: State,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    /// Remote files currently promised on the clipboard.
    remote: Option<RemoteFiles>,
    /// The data object we put on the OLE clipboard.
    object: Option<IDataObject>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

fn clipboard_thread(
    callbacks: Callbacks,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    ready: mpsc::Sender<Result<isize, String>>,
) {
    // SAFETY: called once on this new thread; balanced by OleUninitialize below.
    if let Err(e) = unsafe { OleInitialize(None) } {
        let _ = ready.send(Err(format!("OleInitialize: {e}")));
        return;
    }
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            // SAFETY: OleInitialize succeeded above.
            unsafe { OleUninitialize() };
            return;
        }
    };
    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx {
            hwnd,
            callbacks,
            state: State::default(),
            cmds,
            remote: None,
            object: None,
        });
    });
    let _ = ready.send(Ok(hwnd.0 as isize));

    let mut msg = MSG::default();
    // SAFETY: standard message loop on the thread that owns the window.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Leave the clipboard empty rather than holding promises nobody can serve.
    CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().take() {
            if let Some(obj) = &ctx.object {
                // SAFETY: obj is a live data object created on this thread.
                if unsafe { OleIsCurrentClipboard(obj) }.is_ok() {
                    let _ = unsafe { OleSetClipboard(None) };
                }
            }
        }
    });
    // SAFETY: matches the OleInitialize above.
    unsafe { OleUninitialize() };
}

fn create_window() -> windows::core::Result<HWND> {
    // SAFETY: registering a class and creating a message-only window owned by this thread.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("MonswitchXferClipboard");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // Fails harmlessly when the class is already registered (second start).
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("monswitch xfer"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )?;
        AddClipboardFormatListener(hwnd)?;
        Ok(hwnd)
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE | WM_XFER_CMD => {
            let handled = CTX.with(|c| match c.try_borrow_mut() {
                Ok(mut guard) => {
                    if let Some(ctx) = guard.as_mut() {
                        if msg == WM_CLIPBOARDUPDATE {
                            on_clipboard_update(ctx);
                        } else {
                            run_commands(ctx);
                        }
                    }
                    true
                }
                Err(_) => false,
            });
            if !handled {
                // Re-entered while busy (COM pumps messages during OLE calls): try again later.
                // SAFETY: posting to our own window.
                let _ = unsafe { PostMessageW(Some(hwnd), msg, wp, lp) };
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: our own window, on its thread.
            unsafe {
                let _ = RemoveClipboardFormatListener(hwnd);
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn run_commands(ctx: &mut Ctx) {
    loop {
        let cmd = lock(&ctx.cmds).pop_front();
        match cmd {
            None => break,
            Some(Cmd::Offer(files)) => {
                ctx.state.set_remote(files.id, files.top_level_names());
                ctx.remote = Some(files);
                put_remote(ctx);
            }
            Some(Cmd::Forget(id)) => {
                ctx.state.forget_remote(id);
                if ctx.remote.as_ref().is_some_and(|r| r.id == id) {
                    ctx.remote = None;
                }
            }
        }
    }
}

fn put_remote(ctx: &mut Ctx) {
    let Some(files) = ctx.remote.clone() else {
        return;
    };
    let obj: IDataObject = VirtualFiles::new(files).into();
    // SAFETY: obj lives in ctx while it is on the clipboard; this is an STA thread.
    match unsafe { OleSetClipboard(&obj) } {
        Ok(()) => {
            log::info!("xfer: files from the other machine are on the clipboard");
            ctx.object = Some(obj);
        }
        Err(e) => log::warn!("xfer: cannot set clipboard: {e}"),
    }
}

/// Process id of the clipboard owner window.
fn owner_pid() -> Option<u32> {
    // SAFETY: plain queries; a missing owner is an error/NULL handled here.
    unsafe {
        let owner = GetClipboardOwner().ok().filter(|h| !h.is_invalid())?;
        let mut pid = 0u32;
        GetWindowThreadProcessId(owner, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

/// Lower-case executable file name of process `pid`.
fn exe_name(pid: u32) -> Option<String> {
    // SAFETY: the handle is closed before returning; the buffer outlives the call.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        let path = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
        Some(path.file_name()?.to_string_lossy().to_lowercase())
    }
}

/// Opens the clipboard, retrying briefly – another app may hold it.
fn open_clipboard(hwnd: HWND) -> bool {
    for _ in 0..10 {
        // SAFETY: closed by the caller via CloseClipboard.
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Formats that only accompany text (synthesized by Windows, or Deskflow's
/// ownership marker).
fn is_text_like(format: u32) -> bool {
    const CF_TEXT: u32 = 1;
    const CF_OEMTEXT: u32 = 7;
    const CF_LOCALE: u32 = 16;
    if matches!(format, CF_TEXT | CF_OEMTEXT | CF_LOCALE) || format == u32::from(CF_UNICODETEXT.0) {
        return true;
    }
    format_name(format).is_some_and(|n| n.to_ascii_lowercase().ends_with("ownership"))
}

fn format_name(format: u32) -> Option<String> {
    let mut buf = [0u16; 256];
    // SAFETY: buffer is valid for its length.
    let n = unsafe { GetClipboardFormatNameW(format, &mut buf) };
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n as usize]))
}

/// Reads the clipboard (must be open).
fn read_change(from_deskflow: bool) -> Option<Change> {
    // SAFETY: the clipboard is open; handles returned by GetClipboardData are
    // owned by the clipboard and only read here.
    unsafe {
        if IsClipboardFormatAvailable(u32::from(CF_HDROP.0)).is_ok() {
            let h = GetClipboardData(u32::from(CF_HDROP.0)).ok()?;
            let hdrop = HDROP(h.0);
            let n = DragQueryFileW(hdrop, u32::MAX, None);
            let mut paths = Vec::new();
            for i in 0..n {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                paths.push(PathBuf::from(String::from_utf16_lossy(&buf[..got])));
            }
            return (!paths.is_empty()).then_some(Change::Files(paths));
        }
        let mut only_text = true;
        let mut f = EnumClipboardFormats(0);
        while f != 0 {
            if !is_text_like(f) {
                only_text = false;
                break;
            }
            f = EnumClipboardFormats(f);
        }
        let text = if IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)).is_ok() {
            GetClipboardData(u32::from(CF_UNICODETEXT.0))
                .ok()
                .and_then(|h| global_text(HGLOBAL(h.0)))
        } else {
            None
        };
        Some(Change::Other {
            text,
            only_text,
            from_deskflow: Some(from_deskflow),
        })
    }
}

/// NUL-terminated UTF-16 text in a global memory block.
unsafe fn global_text(h: HGLOBAL) -> Option<String> {
    let p = GlobalLock(h) as *const u16;
    if p.is_null() {
        return None;
    }
    let size = windows::Win32::System::Memory::GlobalSize(h) / 2;
    let slice = std::slice::from_raw_parts(p, size);
    let end = slice.iter().position(|&c| c == 0).unwrap_or(size);
    let s = String::from_utf16_lossy(&slice[..end]);
    let _ = GlobalUnlock(h);
    Some(s)
}

fn on_clipboard_update(ctx: &mut Ctx) {
    let owner = owner_pid();
    if owner == Some(std::process::id()) {
        return; // our own change (OLE clipboard window or CF_HDROP restore)
    }
    let from_deskflow = owner
        .and_then(exe_name)
        .is_some_and(|n| n.starts_with("deskflow"));
    if !open_clipboard(ctx.hwnd) {
        log::debug!("xfer: clipboard busy, change ignored");
        return;
    }
    let change = read_change(from_deskflow);
    // SAFETY: opened above.
    let _ = unsafe { CloseClipboard() };
    let Some(change) = change else { return };
    log::debug!("xfer: clipboard change {change:?}");
    match ctx.state.on_change(change) {
        Action::None => {}
        Action::Offer(paths) => (ctx.callbacks.on_copy)(paths),
        Action::Withdraw => {
            ctx.remote = None;
            (ctx.callbacks.on_clear)();
        }
        Action::RestoreLocal(paths) => {
            log::info!(
                "xfer: Deskflow replaced the copied files on the clipboard – restoring them"
            );
            if let Err(e) = put_hdrop(ctx.hwnd, &paths) {
                log::warn!("xfer: cannot restore clipboard: {e}");
            }
        }
        Action::RestoreRemote => {
            log::info!(
                "xfer: Deskflow replaced the remote files on the clipboard – restoring them"
            );
            put_remote(ctx);
        }
    }
    if ctx.state.remote_id().is_none() {
        ctx.remote = None;
    }
}

/// Global memory block holding `bytes`.
fn hglobal_from(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: the block is at least bytes.len() long; unlocked before returning.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            return Err(E_FAIL.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h); // "fails" with NO_ERROR once unlocked
        Ok(h)
    }
}

/// Puts local files on the clipboard as CF_HDROP (+ preferred effect: copy).
fn put_hdrop(hwnd: HWND, paths: &[PathBuf]) -> windows::core::Result<()> {
    let header = std::mem::size_of::<DROPFILES>();
    let df = DROPFILES {
        pFiles: header as u32,
        pt: POINT::default(),
        fNC: BOOL(0),
        fWide: BOOL(1),
    };
    let mut bytes = Vec::with_capacity(header + 512);
    // SAFETY: DROPFILES is plain old data.
    bytes.extend_from_slice(unsafe {
        std::slice::from_raw_parts((&df as *const DROPFILES).cast::<u8>(), header)
    });
    for p in paths {
        for u in p.as_os_str().to_string_lossy().encode_utf16().chain([0]) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    let drop = hglobal_from(&bytes)?;
    let effect = hglobal_from(&DROPEFFECT_COPY.0.to_le_bytes())?;
    if !open_clipboard(hwnd) {
        return Err(E_FAIL.into());
    }
    // SAFETY: clipboard opened above; on success the clipboard owns the blocks.
    unsafe {
        let r = EmptyClipboard()
            .and_then(|_| SetClipboardData(u32::from(CF_HDROP.0), Some(HANDLE(drop.0))))
            .and_then(|_| {
                SetClipboardData(
                    RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT),
                    Some(HANDLE(effect.0)),
                )
            });
        let _ = CloseClipboard();
        r.map(|_| ())
    }
}

// ---- IDataObject with virtual files -------------------------------------------------

/// One FILEDESCRIPTORW: an entry with a path short enough for cFileName.
struct Item {
    entry: u32,
    path: Vec<u16>,
    dir: bool,
    size: u64,
    mtime: u64,
}

#[implement(IDataObject)]
struct VirtualFiles {
    files: RemoteFiles,
    items: Vec<Item>,
    cf_descriptor: u16,
    cf_contents: u16,
    cf_effect: u16,
}

fn register(name: windows::core::PCWSTR) -> u16 {
    // SAFETY: registering a well-known format name.
    let id = unsafe { RegisterClipboardFormatW(name) };
    u16::try_from(id).unwrap_or(0)
}

impl VirtualFiles {
    fn new(files: RemoteFiles) -> Self {
        let mut items = Vec::new();
        let mut skipped_dirs: Vec<String> = Vec::new();
        for (i, e) in files.entries.iter().enumerate() {
            if skipped_dirs
                .iter()
                .any(|d| e.path.starts_with(&format!("{d}/")))
            {
                continue;
            }
            let path: Vec<u16> = e.path.replace('/', "\\").encode_utf16().collect();
            if path.len() > MAX_DESCRIPTOR_PATH {
                log::warn!(
                    "xfer: path too long for a Windows virtual file, skipped: {}",
                    e.path
                );
                if e.dir {
                    skipped_dirs.push(e.path.clone());
                }
                continue;
            }
            items.push(Item {
                entry: i as u32,
                path,
                dir: e.dir,
                size: e.size,
                mtime: e.mtime,
            });
        }
        Self {
            files,
            items,
            cf_descriptor: register(CFSTR_FILEDESCRIPTORW),
            cf_contents: register(CFSTR_FILECONTENTS),
            cf_effect: register(CFSTR_PREFERREDDROPEFFECT),
        }
    }

    fn formats(&self) -> [FORMATETC; 3] {
        let f = |cf: u16, tymed: i32| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: tymed as u32,
        };
        [
            f(self.cf_descriptor, TYMED_HGLOBAL.0),
            f(self.cf_contents, TYMED_ISTREAM.0),
            f(self.cf_effect, TYMED_HGLOBAL.0),
        ]
    }

    fn descriptor(&self) -> Vec<u8> {
        let n = self.items.len();
        let mut out = Vec::with_capacity(4 + n * std::mem::size_of::<FILEDESCRIPTORW>());
        out.extend_from_slice(&(n as u32).to_le_bytes());
        for it in &self.items {
            let mut fd = FILEDESCRIPTORW::default();
            let mut flags = FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0 | FD_UNICODE.0;
            if it.mtime != 0 {
                flags |= FD_WRITESTIME.0;
                fd.ftLastWriteTime = filetime(it.mtime);
            }
            fd.dwFlags = flags as u32;
            fd.dwFileAttributes = if it.dir {
                FILE_ATTRIBUTE_DIRECTORY.0
            } else {
                FILE_ATTRIBUTE_NORMAL.0
            };
            fd.nFileSizeHigh = (it.size >> 32) as u32;
            fd.nFileSizeLow = it.size as u32;
            // packed struct: build the name first, then assign the whole field
            let mut name = [0u16; 260];
            name[..it.path.len()].copy_from_slice(&it.path);
            fd.cFileName = name;
            // SAFETY: FILEDESCRIPTORW is plain old data.
            out.extend_from_slice(unsafe {
                std::slice::from_raw_parts(
                    (&fd as *const FILEDESCRIPTORW).cast::<u8>(),
                    std::mem::size_of::<FILEDESCRIPTORW>(),
                )
            });
        }
        out
    }

    /// Which of our formats `f` asks for, if any.
    fn matches(&self, f: &FORMATETC) -> Option<u16> {
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return None;
        }
        self.formats()
            .iter()
            .find(|ours| ours.cfFormat == f.cfFormat && f.tymed & ours.tymed != 0)
            .map(|o| o.cfFormat)
    }
}

/// Unix seconds → FILETIME.
fn filetime(unix_secs: u64) -> FILETIME {
    let t = (unix_secs + 11_644_473_600) * 10_000_000;
    FILETIME {
        dwLowDateTime: t as u32,
        dwHighDateTime: (t >> 32) as u32,
    }
}

fn hglobal_medium(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 {
            hGlobal: hglobal_from(bytes)?,
        },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

impl IDataObject_Impl for VirtualFiles_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: COM guarantees a valid pointer (checked for NULL anyway).
        let f =
            unsafe { pformatetcin.as_ref() }.ok_or(windows::core::Error::from(DV_E_FORMATETC))?;
        let cf = self
            .matches(f)
            .ok_or(windows::core::Error::from(DV_E_FORMATETC))?;
        if cf == self.cf_descriptor {
            return hglobal_medium(&self.descriptor());
        }
        if cf == self.cf_effect {
            return hglobal_medium(&DROPEFFECT_COPY.0.to_le_bytes());
        }
        let item = usize::try_from(f.lindex)
            .ok()
            .and_then(|i| self.items.get(i))
            .filter(|it| !it.dir)
            .ok_or(windows::core::Error::from(DV_E_FORMATETC))?;
        log::info!(
            "xfer: paste requested {}",
            String::from_utf16_lossy(&item.path)
        );
        let stream: IStream = NetStream {
            state: Mutex::new(StreamState {
                reader: None,
                pos: 0,
            }),
            open: self.files.open.clone(),
            entry: item.entry,
            size: item.size,
            mtime: item.mtime,
            name: item.path.clone(),
        }
        .into();
        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        // SAFETY: see GetData.
        match unsafe { pformatetc.as_ref() }.and_then(|f| self.matches(f)) {
            Some(_) => S_OK,
            None => DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        // SAFETY: COM out-pointer; only its ptd field is cleared as the docs require.
        if let Some(out) = unsafe { pformatetcout.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> windows::core::Result<()> {
        // Explorer reports "Performed DropEffect" / "Paste Succeeded" here; nothing to keep.
        if frelease.as_bool() && !pmedium.is_null() {
            // SAFETY: we own the medium when fRelease is TRUE.
            unsafe { ReleaseStgMedium(pmedium as *mut STGMEDIUM) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection as i32 != DATADIR_GET.0 {
            return Err(E_NOTIMPL.into());
        }
        // SAFETY: the array is copied by the enumerator.
        unsafe { SHCreateStdEnumFmtEtc(&self.formats()) }
    }

    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

// ---- IStream reading from the other machine ---------------------------------------

struct StreamState {
    reader: Option<Box<dyn Read + Send>>,
    pos: u64,
}

#[implement(IStream)]
struct NetStream {
    state: Mutex<StreamState>,
    open: super::Opener,
    entry: u32,
    size: u64,
    mtime: u64,
    name: Vec<u16>,
}

impl ISequentialStream_Impl for NetStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return STG_E_INVALIDFUNCTION;
        }
        // SAFETY: COM guarantees pv points to cb writable bytes.
        let buf = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), cb as usize) };
        let mut st = lock(&self.state);
        if st.reader.is_none() {
            match (self.open)(self.entry) {
                Ok(r) => st.reader = Some(r),
                Err(e) => {
                    log::warn!("xfer: {e}");
                    return E_FAIL;
                }
            }
        }
        let mut filled = 0;
        let mut failed = false;
        if let Some(reader) = st.reader.as_mut() {
            while filled < buf.len() {
                match reader.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        log::warn!("xfer: transfer failed: {e}");
                        failed = true;
                        break;
                    }
                }
            }
        }
        st.pos += filled as u64;
        if !pcbread.is_null() {
            // SAFETY: optional out-pointer, checked for NULL.
            unsafe { *pcbread = filled as u32 };
        }
        if failed && filled == 0 {
            E_FAIL
        } else if filled < buf.len() {
            S_FALSE
        } else {
            S_OK
        }
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for NetStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        let pos = lock(&self.state).pos;
        let target = match dworigin {
            STREAM_SEEK_SET => Some(dlibmove),
            STREAM_SEEK_CUR => (pos as i64).checked_add(dlibmove),
            STREAM_SEEK_END => (self.size as i64).checked_add(dlibmove),
            _ => None,
        };
        // Sequential network stream: only "seek" to where we already are.
        if target != Some(pos as i64) {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        if !plibnewposition.is_null() {
            // SAFETY: optional out-pointer, checked for NULL.
            unsafe { *plibnewposition = pos };
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        _: Ref<IStream>,
        _: u64,
        _: *mut u64,
        _: *mut u64,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        // SAFETY: COM out-pointer, checked for NULL.
        let out = unsafe { pstatstg.as_mut() }
            .ok_or(windows::core::Error::from(STG_E_INVALIDFUNCTION))?;
        let mut st = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: self.size,
            ..Default::default()
        };
        if self.mtime != 0 {
            st.mtime = filetime(self.mtime);
        }
        if *grfstatflag != STATFLAG_NONAME {
            let name: Vec<u16> = self
                .name
                .rsplit(|&c| c == u16::from(b'\\'))
                .next()
                .unwrap_or(&self.name)
                .to_vec();
            // SAFETY: the caller frees pwcsName with CoTaskMemFree.
            unsafe {
                let p = CoTaskMemAlloc((name.len() + 1) * 2) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(name.as_ptr(), p, name.len());
                    *p.add(name.len()) = 0;
                    st.pwcsName = PWSTR(p);
                }
            }
        }
        *out = st;
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}
