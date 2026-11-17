//! macOS: pasteboard polling + lazily provided file URLs.
//!
//! macOS has no clipboard-change notification, so a background thread polls
//! `changeCount`. Files offered by the other machine become one
//! `NSPasteboardItem` per copied file/folder whose `public.file-url` is
//! provided lazily: only when Finder (or another app) pastes is the data
//! provider asked, which downloads the file into a cache folder and returns
//! its URL.
//!
//! File *promises* (`NSFilePromiseProvider`) are a drag-and-drop mechanism –
//! Finder does not accept them on paste – so the lazy data provider is used
//! for the clipboard and promises are kept for dragging across the edge.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardType,
    NSPasteboardTypeFileURL, NSPasteboardTypeString, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSString, NSURL};

use super::{Action, Callbacks, Change, RemoteFiles, RunOnMain, State};

const POLL: Duration = Duration::from_millis(300);
/// Private pasteboard type holding the entry index of an item.
const ENTRY_TYPE: &str = "com.monswitch.xfer.entry";
const TEXT_TYPES: [&str; 4] = [
    "public.utf8-plain-text",
    "public.utf16-plain-text",
    "public.plain-text",
    "NSStringPboardType",
];

struct Shared {
    state: State,
    remote: Option<RemoteFiles>,
    /// changeCount right after our own last write.
    own_change: isize,
}

pub struct Clipboard {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    run_on_main: RunOnMain,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn general() -> Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

impl Clipboard {
    pub fn start(callbacks: Callbacks, run_on_main: Option<RunOnMain>) -> io::Result<Self> {
        let run_on_main = run_on_main.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "clipboard needs the app's main thread (tray app only)",
            )
        })?;
        let shared = Arc::new(Mutex::new(Shared {
            state: State::default(),
            remote: None,
            own_change: -1,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, st, rm) = (shared.clone(), stop.clone(), run_on_main.clone());
        std::thread::Builder::new()
            .name("xfer-clipboard".into())
            .spawn(move || poll_loop(s, st, callbacks, rm))?;
        Ok(Self {
            shared,
            stop,
            run_on_main,
        })
    }

    pub fn offer_remote(&self, files: RemoteFiles) {
        {
            let mut sh = lock(&self.shared);
            sh.state.set_remote(files.id, files.top_level_names());
            sh.remote = Some(files);
        }
        let shared = self.shared.clone();
        (self.run_on_main)(Box::new(move || put_remote(&shared)));
    }

    pub fn forget_remote(&self, id: u64) {
        let mut sh = lock(&self.shared);
        sh.state.forget_remote(id);
        if sh.remote.as_ref().is_some_and(|r| r.id == id) {
            sh.remote = None;
        }
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn poll_loop(
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    callbacks: Callbacks,
    run_on_main: RunOnMain,
) {
    let mut last = general().changeCount();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        let count = general().changeCount();
        if count == last {
            continue;
        }
        last = count;
        if count == lock(&shared).own_change {
            continue;
        }
        let change = read_change();
        log::debug!("xfer: clipboard change {change:?}");
        let action = lock(&shared).state.on_change(change);
        match action {
            Action::None => {}
            Action::Offer(paths) => (callbacks.on_copy)(paths),
            Action::Withdraw => (callbacks.on_clear)(),
            Action::RestoreLocal(paths) => {
                log::info!(
                    "xfer: Deskflow replaced the copied files on the clipboard – restoring them"
                );
                put_local(&shared, &paths);
            }
            Action::RestoreRemote => {
                log::info!(
                    "xfer: Deskflow replaced the remote files on the clipboard – restoring them"
                );
                let s = shared.clone();
                run_on_main(Box::new(move || put_remote(&s)));
            }
        }
        let mut sh = lock(&shared);
        if sh.state.remote_id().is_none() {
            sh.remote = None;
        }
    }
}

fn read_change() -> Change {
    let pb = general();
    let mut paths = Vec::new();
    if let Some(items) = pb.pasteboardItems() {
        for item in items.iter() {
            // SAFETY: NSPasteboardTypeFileURL is an immutable framework constant.
            let ty = unsafe { NSPasteboardTypeFileURL };
            let Some(s) = item.stringForType(ty) else {
                continue;
            };
            // Finder may write file reference URLs (file:///.file/id=…); filePathURL resolves them.
            let path = NSURL::URLWithString(&s)
                .and_then(|u| u.filePathURL())
                .and_then(|u| u.path());
            if let Some(p) = path {
                paths.push(PathBuf::from(p.to_string()));
            }
        }
    }
    if !paths.is_empty() {
        return Change::Files(paths);
    }
    let only_text = pb
        .types()
        .map(|types| {
            types
                .iter()
                .all(|t| TEXT_TYPES.contains(&t.to_string().as_str()))
        })
        .unwrap_or(true);
    // SAFETY: framework constant.
    let text = pb
        .stringForType(unsafe { NSPasteboardTypeString })
        .map(|s| s.to_string());
    Change::Other {
        text,
        only_text,
        from_deskflow: None,
    }
}

fn put_local(shared: &Mutex<Shared>, paths: &[PathBuf]) {
    let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
        .iter()
        .map(|p| {
            ProtocolObject::from_retained(NSURL::fileURLWithPath(&NSString::from_str(
                &p.to_string_lossy(),
            )))
        })
        .collect();
    let pb = general();
    pb.clearContents();
    let ok = pb.writeObjects(&NSArray::from_retained_slice(&urls));
    lock(shared).own_change = pb.changeCount();
    if !ok {
        log::warn!("xfer: cannot restore copied files on the clipboard");
    }
}

/// Main thread: puts one lazily provided file-url item per copied file/folder.
fn put_remote(shared: &Mutex<Shared>) {
    let Some(files) = lock(shared).remote.clone() else {
        return;
    };
    let provider = Provider::new(files.clone());
    let proto = ProtocolObject::from_ref(&*provider);
    let entry_type = NSString::from_str(ENTRY_TYPE);
    // SAFETY: framework constant.
    let file_url: &NSPasteboardType = unsafe { NSPasteboardTypeFileURL };
    let mut items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = Vec::new();
    for (i, e) in files.entries.iter().enumerate() {
        if !e.is_top_level() {
            continue;
        }
        let item = NSPasteboardItem::new();
        item.setString_forType(&NSString::from_str(&i.to_string()), &entry_type);
        if !item.setDataProvider_forTypes(proto, &NSArray::from_slice(&[file_url])) {
            log::warn!("xfer: cannot promise {}", e.path);
            continue;
        }
        items.push(ProtocolObject::from_retained(item));
    }
    let pb = general();
    pb.clearContents();
    let ok = pb.writeObjects(&NSArray::from_retained_slice(&items));
    {
        let mut sh = lock(shared);
        sh.own_change = pb.changeCount();
    }
    // The pasteboard does not keep the provider alive; it lives until the next offer.
    PROVIDER.with(|p| *p.borrow_mut() = Some(provider));
    if ok {
        log::info!("xfer: files from the other machine are on the clipboard");
    } else {
        log::warn!("xfer: cannot put the other machine's files on the clipboard");
    }
}

thread_local! {
    static PROVIDER: std::cell::RefCell<Option<Retained<Provider>>> = const { std::cell::RefCell::new(None) };
}

struct ProviderIvars {
    files: RemoteFiles,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "MonswitchXferProvider"]
    #[ivars = ProviderIvars]
    struct Provider;

    unsafe impl NSObjectProtocol for Provider {}

    unsafe impl NSPasteboardItemDataProvider for Provider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(
            &self,
            _pasteboard: Option<&NSPasteboard>,
            item: &NSPasteboardItem,
            ty: &NSPasteboardType,
        ) {
            self.provide_file_url(item, ty);
        }

        #[unsafe(method(pasteboardFinishedWithDataProvider:))]
        fn finished(&self, _pasteboard: &NSPasteboard) {}
    }
);

impl Provider {
    fn new(files: RemoteFiles) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ProviderIvars { files });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    /// Called on the main thread when an app pastes: downloads the entry
    /// (blocking – the pasting app waits) and hands out its local URL.
    fn provide_file_url(&self, item: &NSPasteboardItem, ty: &NSPasteboardType) {
        let index = item
            .stringForType(&NSString::from_str(ENTRY_TYPE))
            .and_then(|s| s.to_string().parse::<u32>().ok());
        let Some(index) = index else { return };
        let files = &self.ivars().files;
        log::info!(
            "xfer: paste requested {}",
            files
                .entries
                .get(index as usize)
                .map_or("?", |e| e.path.as_str())
        );
        match (files.materialize)(index) {
            Ok(path) => {
                let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
                if let Some(s) = url.absoluteString() {
                    item.setString_forType(&s, ty);
                }
            }
            Err(e) => log::warn!("xfer: transfer failed: {e}"),
        }
    }
}
